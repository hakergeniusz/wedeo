//! Zigzag scan tables and permutation helpers.
//!
//! Ports FFmpeg's `ScanTable` (from `mpegvideo.h`) and the
//! `ff_init_scantable` / `ff_permute_scantable` functions.

/// Scantable (FFmpeg `ScanTable`).
///
/// - `scantable`: the raw scan order (scan index -> raster index).
/// - `permutated`: permutation applied on top of `scantable`
///   (`permutated[i] = permutation[scantable[i]]`).
/// - `raster_end`: for each scan index `i`, the largest raster position seen
///   in `permutated[0..=i]`; FFmpeg uses this to iterate only the relevant
///   coefficients of a block.
#[derive(Debug, Clone)]
pub struct ScanTable {
    pub scantable: Vec<u8>,
    pub permutated: [u8; 64],
    pub raster_end: [u8; 64],
}

impl Default for ScanTable {
    fn default() -> Self {
        Self {
            scantable: Vec::new(),
            permutated: [0; 64],
            raster_end: [0; 64],
        }
    }
}

/// Initialize a scantable with an explicit permutation (FFmpeg
/// `ff_init_scantable`).
///
/// `permutation` maps a raster index to its IDCT-permuted index. Use the
/// identity permutation (0,1,2,...) when no IDCT permutation is desired.
pub fn init_scantable(permutation: &[u8; 64], src_scantable: &[u8; 64]) -> ScanTable {
    let mut permutated = [0u8; 64];
    let mut raster_end = [0u8; 64];
    let mut end: u8 = 0;
    for i in 0..64 {
        let j = src_scantable[i] as usize;
        permutated[i] = permutation[j];
        if permutation[j] > end {
            end = permutation[j];
        }
        raster_end[i] = end;
    }
    ScanTable {
        scantable: src_scantable.to_vec(),
        permutated,
        raster_end,
    }
}

/// Initialize a scantable with the identity permutation, i.e.
/// `permutated == scantable`.
pub fn init_scantable_identity(src_scantable: &[u8; 64]) -> ScanTable {
    let identity: [u8; 64] = core::array::from_fn(|i| i as u8);
    init_scantable(&identity, src_scantable)
}

/// Permute a scan table using the given IDCT permutation (FFmpeg
/// `ff_permute_scantable`).
///
/// Maps a scan-table position to a raster position:
/// `dst[i] = permutation[scan[i]]`.
pub fn permute_scantable(dst: &mut [u8; 64], scan: &[u8; 64], permutation: &[u8; 64]) {
    for i in 0..64 {
        dst[i] = permutation[scan[i] as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{ALTERNATE_HORIZONTAL_SCAN, ALTERNATE_VERTICAL_SCAN, ZIGZAG_DIRECT};

    #[test]
    fn identity_scantable_matches_source() {
        let st = init_scantable_identity(&ZIGZAG_DIRECT);
        assert_eq!(st.scantable, ZIGZAG_DIRECT.to_vec());
        assert_eq!(st.permutated, ZIGZAG_DIRECT);
        // raster_end is monotonically non-decreasing and ends at 63.
        for w in st.raster_end.windows(2) {
            assert!(w[0] <= w[1]);
        }
        assert_eq!(st.raster_end[63], 63);
    }

    #[test]
    fn identity_scantable_permuted_equals_scantable() {
        let st = init_scantable_identity(&ALTERNATE_HORIZONTAL_SCAN);
        assert_eq!(st.permutated, st.scantable.as_slice());
    }

    #[test]
    fn init_scantable_with_transpose_permutation() {
        // Transpose permutation: raster index (y*8+x) -> (x*8+y).
        let mut transpose = [0u8; 64];
        for y in 0..8 {
            for x in 0..8 {
                transpose[y * 8 + x] = (x * 8 + y) as u8;
            }
        }
        let st = init_scantable(&transpose, &ZIGZAG_DIRECT);
        for i in 0..64 {
            assert_eq!(st.permutated[i], transpose[ZIGZAG_DIRECT[i] as usize]);
        }
    }

    #[test]
    fn permute_scantable_matches_init() {
        let mut identity = [0u8; 64];
        for i in 0..8 {
            for x in 0..8 {
                identity[i * 8 + x] = (i * 8 + x) as u8;
            }
        }
        let st = init_scantable(&identity, &ALTERNATE_VERTICAL_SCAN);
        let mut out = [0u8; 64];
        permute_scantable(&mut out, &ALTERNATE_VERTICAL_SCAN, &identity);
        assert_eq!(out, st.permutated);
    }
}
