//! Block dequantization primitives for MPEG-1/2 and H.263.
//!
//! Faithful port of `mpegvideo_unquantize.c`. All arithmetic uses the same
//! integer widths and rounding as the C source, which is required for
//! bit-exact reconstruction parity.

use crate::tables::MPEG2_NON_LINEAR_QSCALE;

/// MPEG-1 intra block dequantization (FFmpeg `dct_unquantize_mpeg1_intra_c`).
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn dct_unquantize_mpeg1_intra(
    block: &mut [i16; 64],
    n: usize,
    qscale: i32,
    quant_matrix: &[u16; 64],
    permutated: &[u8; 64],
    block_last_index: &[i32; 12],
    y_dc_scale: i32,
    c_dc_scale: i32,
) {
    let n_coeffs = block_last_index[n];
    block[0] = block[0].wrapping_mul(if n < 4 { y_dc_scale } else { c_dc_scale } as i16);
    for i in 1..=n_coeffs {
        let j = permutated[i as usize] as usize;
        let mut level = block[j] as i32;
        if level != 0 {
            if level < 0 {
                level = -level;
                level = (level * qscale * quant_matrix[j] as i32) >> 3;
                level = (level - 1) | 1;
                level = -level;
            } else {
                level = (level * qscale * quant_matrix[j] as i32) >> 3;
                level = (level - 1) | 1;
            }
            block[j] = level as i16;
        }
    }
}

/// MPEG-1 inter block dequantization (FFmpeg `dct_unquantize_mpeg1_inter_c`).
pub fn dct_unquantize_mpeg1_inter(
    block: &mut [i16; 64],
    n: usize,
    qscale: i32,
    quant_matrix: &[u16; 64],
    permutated: &[u8; 64],
    block_last_index: &[i32; 12],
) {
    let n_coeffs = block_last_index[n];
    for i in 0..=n_coeffs {
        let j = permutated[i as usize] as usize;
        let mut level = block[j] as i32;
        if level != 0 {
            if level < 0 {
                level = -level;
                level = (((level << 1) + 1) * qscale * quant_matrix[j] as i32) >> 4;
                level = (level - 1) | 1;
                level = -level;
            } else {
                level = (((level << 1) + 1) * qscale * quant_matrix[j] as i32) >> 4;
                level = (level - 1) | 1;
            }
            block[j] = level as i16;
        }
    }
}

/// Resolve a quantizer scale using `q_scale_type` (FFmpeg: linear scale is
/// doubled, non-linear uses `ff_mpeg2_non_linear_qscale`).
#[inline]
pub fn mpeg2_qscale(qscale: i32, q_scale_type: bool) -> i32 {
    if q_scale_type {
        MPEG2_NON_LINEAR_QSCALE[qscale as usize] as i32
    } else {
        qscale << 1
    }
}

/// MPEG-2 intra block dequantization (FFmpeg `dct_unquantize_mpeg2_intra_c`
/// and `..._bitexact`).
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn dct_unquantize_mpeg2_intra(
    block: &mut [i16; 64],
    n: usize,
    qscale: i32,
    q_scale_type: bool,
    bitexact: bool,
    quant_matrix: &[u16; 64],
    permutated: &[u8; 64],
    block_last_index: &[i32; 12],
    y_dc_scale: i32,
    c_dc_scale: i32,
) {
    let qscale = mpeg2_qscale(qscale, q_scale_type);
    let n_coeffs = block_last_index[n];
    block[0] = block[0].wrapping_mul(if n < 4 { y_dc_scale } else { c_dc_scale } as i16);
    let mut sum = -1i32;
    if bitexact {
        sum += block[0] as i32;
    }
    for i in 1..=n_coeffs {
        let j = permutated[i as usize] as usize;
        let mut level = block[j] as i32;
        if level != 0 {
            if level < 0 {
                level = -level;
                level = (level * qscale * quant_matrix[j] as i32) >> 4;
                level = -level;
            } else {
                level = (level * qscale * quant_matrix[j] as i32) >> 4;
            }
            block[j] = level as i16;
            if bitexact {
                sum += level;
            }
        }
    }
    if bitexact {
        block[63] ^= (sum & 1) as i16;
    }
}

/// MPEG-2 inter block dequantization (FFmpeg `dct_unquantize_mpeg2_inter_c`).
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn dct_unquantize_mpeg2_inter(
    block: &mut [i16; 64],
    n: usize,
    qscale: i32,
    q_scale_type: bool,
    quant_matrix: &[u16; 64],
    permutated: &[u8; 64],
    block_last_index: &[i32; 12],
) {
    let qscale = mpeg2_qscale(qscale, q_scale_type);
    let n_coeffs = block_last_index[n];
    let mut sum = -1i32;
    for i in 0..=n_coeffs {
        let j = permutated[i as usize] as usize;
        let mut level = block[j] as i32;
        if level != 0 {
            if level < 0 {
                level = -level;
                level = (((level << 1) + 1) * qscale * quant_matrix[j] as i32) >> 5;
                level = -level;
            } else {
                level = (((level << 1) + 1) * qscale * quant_matrix[j] as i32) >> 5;
            }
            block[j] = level as i16;
            sum += level;
        }
    }
    block[63] ^= (sum & 1) as i16;
}

/// H.263 intra block dequantization (FFmpeg `dct_unquantize_h263_intra_c`).
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn dct_unquantize_h263_intra(
    block: &mut [i16; 64],
    n: usize,
    qscale: i32,
    h263_aic: bool,
    ac_pred: bool,
    raster_end: &[u8; 64],
    block_last_index: &[i32; 12],
    y_dc_scale: i32,
    c_dc_scale: i32,
) {
    let qmul = qscale << 1;
    let qadd = if !h263_aic {
        block[0] = block[0].wrapping_mul(if n < 4 { y_dc_scale } else { c_dc_scale } as i16);
        (qscale - 1) | 1
    } else {
        0
    };
    let n_coeffs = if ac_pred {
        63
    } else {
        raster_end[block_last_index[n] as usize] as i32
    };
    for i in 1..=n_coeffs {
        let mut level = block[i as usize] as i32;
        if level != 0 {
            if level < 0 {
                level = level * qmul - qadd;
            } else {
                level = level * qmul + qadd;
            }
            block[i as usize] = level as i16;
        }
    }
}

/// H.263 inter block dequantization (FFmpeg `dct_unquantize_h263_inter_c`).
pub fn dct_unquantize_h263_inter(
    block: &mut [i16; 64],
    n: usize,
    qscale: i32,
    raster_end: &[u8; 64],
    block_last_index: &[i32; 12],
) {
    let qadd = (qscale - 1) | 1;
    let qmul = qscale << 1;
    let n_coeffs = raster_end[block_last_index[n] as usize] as i32;
    for i in 0..=n_coeffs {
        let mut level = block[i as usize] as i32;
        if level != 0 {
            if level < 0 {
                level = level * qmul - qadd;
            } else {
                level = level * qmul + qadd;
            }
            block[i as usize] = level as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::init_scantable_identity;
    use crate::tables::{MPEG1_DEFAULT_INTRA_MATRIX, ZIGZAG_DIRECT};

    fn context() -> ([u8; 64], [i32; 12]) {
        let st = init_scantable_identity(&ZIGZAG_DIRECT);
        let last = [63i32; 12];
        (st.permutated, last)
    }

    #[test]
    fn mpeg1_intra_dc_and_ac() {
        let (perm, last) = context();
        let mut block = [0i16; 64];
        block[0] = 2;
        block[perm[1] as usize] = 1;
        let mut quant = MPEG1_DEFAULT_INTRA_MATRIX;
        quant[perm[1] as usize] = 16;
        dct_unquantize_mpeg1_intra(&mut block, 0, 1, &quant, &perm, &last, 8, 8);
        assert_eq!(block[0], 16);
        // level=1, qscale=1, matrix=16: (1*1*16)>>3 = 2, (2-1)|1 = 1.
        assert_eq!(block[perm[1] as usize], 1);
    }

    #[test]
    fn mpeg2_linear_qscale_is_doubled() {
        assert_eq!(mpeg2_qscale(3, false), 6);
        assert_eq!(mpeg2_qscale(9, true), 10);
    }

    #[test]
    fn mpeg2_intra_bitexact_toggles_coeff63() {
        let (perm, last) = context();
        let mut a = [0i16; 64];
        a[0] = 1;
        let mut b = a;
        dct_unquantize_mpeg2_intra(
            &mut a,
            0,
            1,
            false,
            true,
            &MPEG1_DEFAULT_INTRA_MATRIX,
            &perm,
            &last,
            8,
            8,
        );
        dct_unquantize_mpeg2_intra(
            &mut b,
            0,
            1,
            false,
            false,
            &MPEG1_DEFAULT_INTRA_MATRIX,
            &perm,
            &last,
            8,
            8,
        );
        assert_ne!(a[63], b[63]);
    }
}
