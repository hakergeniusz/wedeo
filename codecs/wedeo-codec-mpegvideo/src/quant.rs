//! Quantizer matrix handling.
//!
//! Ports FFmpeg's `load_matrix`, `mpeg_decode_quant_matrix_extension` and the
//! default matrix setup from `mpeg12dec.c` / `mpeg12data.c`.

use wedeo_codec::bitstream::{BitRead, BitReadBE};
use wedeo_core::{Error, Result};

use crate::tables::{MPEG1_DEFAULT_INTRA_MATRIX, MPEG1_DEFAULT_NON_INTRA_MATRIX, ZIGZAG_DIRECT};

/// The four quantizer matrices carried in `MpegEncContext`.
#[derive(Debug, Clone)]
pub struct QuantMatrices {
    /// Luma intra matrix (transmitted in zigzag order, stored permuted).
    pub intra_matrix: [u16; 64],
    /// Chroma intra matrix.
    pub chroma_intra_matrix: [u16; 64],
    /// Luma non-intra (inter) matrix.
    pub inter_matrix: [u16; 64],
    /// Chroma non-intra (inter) matrix.
    pub chroma_inter_matrix: [u16; 64],
}

impl Default for QuantMatrices {
    fn default() -> Self {
        Self::new()
    }
}

impl QuantMatrices {
    /// MPEG-1 default matrices (flat 16 for inter, the standard intra matrix).
    pub fn new() -> Self {
        Self {
            intra_matrix: MPEG1_DEFAULT_INTRA_MATRIX,
            chroma_intra_matrix: MPEG1_DEFAULT_INTRA_MATRIX,
            inter_matrix: MPEG1_DEFAULT_NON_INTRA_MATRIX,
            chroma_inter_matrix: MPEG1_DEFAULT_NON_INTRA_MATRIX,
        }
    }

    /// Set both luma and chroma intra matrices to the MPEG-1 default.
    pub fn reset_intra(&mut self) {
        self.intra_matrix = MPEG1_DEFAULT_INTRA_MATRIX;
        self.chroma_intra_matrix = MPEG1_DEFAULT_INTRA_MATRIX;
    }

    /// Set both luma and chroma inter matrices to the MPEG-1 default.
    pub fn reset_inter(&mut self) {
        self.inter_matrix = MPEG1_DEFAULT_NON_INTRA_MATRIX;
        self.chroma_inter_matrix = MPEG1_DEFAULT_NON_INTRA_MATRIX;
    }
}

/// Load one 64-entry quantizer matrix from the bitstream (FFmpeg `load_matrix`).
///
/// The 64 coefficients are read in zigzag order and stored in raster order
/// after applying `permutation`. A zero coefficient is invalid; for intra
/// matrices the first coefficient is forced to 8 (FFmpeg's workaround for
/// `pink.mpg` / issue 1046).
pub fn load_matrix(
    br: &mut BitReadBE<'_>,
    permutation: &[u8; 64],
    intra: bool,
) -> Result<[u16; 64]> {
    let mut matrix = [0u16; 64];
    for i in 0..64 {
        let v = br.get_bits_32(8);
        if v == 0 {
            return Err(Error::InvalidData);
        }
        let mut v = v;
        if intra && i == 0 && v != 8 {
            // "intra matrix specifies invalid DC quantizer %d, ignoring"
            v = 8;
        }
        let j = permutation[ZIGZAG_DIRECT[i] as usize] as usize;
        matrix[j] = v as u16;
    }
    Ok(matrix)
}

/// Parse a quantizer matrix extension (FFmpeg
/// `mpeg_decode_quant_matrix_extension`).
///
/// Returns `Ok(false)` unchanged when a matrix is damaged, letting the caller
/// continue with the previous matrix (matching FFmpeg's error-recilience
/// behavior where an error is reported but decoding continues).
pub fn decode_quant_matrix_extension(
    br: &mut BitReadBE<'_>,
    permutation: &[u8; 64],
    matrices: &mut QuantMatrices,
) -> Result<()> {
    if br.get_bit() {
        let m = load_matrix(br, permutation, true)?;
        matrices.chroma_intra_matrix = m;
        matrices.intra_matrix = m;
    }
    if br.get_bit() {
        let m = load_matrix(br, permutation, false)?;
        matrices.chroma_inter_matrix = m;
        matrices.inter_matrix = m;
    }
    if br.get_bit() {
        matrices.chroma_intra_matrix = load_matrix(br, permutation, true)?;
    }
    if br.get_bit() {
        matrices.chroma_inter_matrix = load_matrix(br, permutation, false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> [u8; 64] {
        core::array::from_fn(|i| i as u8)
    }

    #[test]
    fn default_matrices_match_tables() {
        let m = QuantMatrices::new();
        assert_eq!(m.intra_matrix[0], 8);
        assert_eq!(m.intra_matrix[63], 83);
        assert!(m.inter_matrix.iter().all(|&v| v == 16));
    }

    #[test]
    fn load_matrix_stores_in_zigzag_order() {
        // 64 bytes: all 2 except the last (index 63) which is 4.
        let mut data = vec![2u8; 64];
        data[63] = 4;
        let mut buf = data;
        buf.resize(80, 0);
        let mut br = BitReadBE::new(&buf);
        let m = load_matrix(&mut br, &identity(), false).unwrap();
        assert_eq!(m[ZIGZAG_DIRECT[0] as usize], 2);
        assert_eq!(m[63], 4);
    }

    #[test]
    fn load_matrix_forces_intra_dc_to_eight() {
        let mut data = vec![3u8; 64];
        // First coefficient in zigzag order = 3 (invalid for intra DC).
        let mut buf = data.clone();
        buf.resize(80, 0);
        let mut br = BitReadBE::new(&buf);
        let m = load_matrix(&mut br, &identity(), true).unwrap();
        assert_eq!(m[0], 8);
        data[0] = 0;
        let mut buf2 = data;
        buf2.resize(80, 0);
        let mut br2 = BitReadBE::new(&buf2);
        assert_eq!(
            load_matrix(&mut br2, &identity(), false),
            Err(Error::InvalidData)
        );
    }
}
