//! Diagonal scan tables (from `libavcodec/hevc/data.c`).
//!
//! Used by `scaling_list_data()` to map the coded coefficient order to the
//! stored matrix position (`pos = width * y + x`).

/// `ff_hevc_diag_scan4x4_x`.
pub const DIAG_SCAN4X4_X: [u8; 16] = [0, 0, 1, 0, 1, 2, 0, 1, 2, 3, 1, 2, 3, 2, 3, 3];

/// `ff_hevc_diag_scan4x4_y`.
pub const DIAG_SCAN4X4_Y: [u8; 16] = [0, 1, 0, 2, 1, 0, 3, 2, 1, 0, 3, 2, 1, 3, 2, 3];

/// `ff_hevc_diag_scan8x8_x`.
pub const DIAG_SCAN8X8_X: [u8; 64] = [
    0, 0, 1, 0, // row 0
    1, 2, 0, 1, // row 1
    2, 3, 0, 1, // row 2
    2, 3, 4, 0, // row 3
    1, 2, 3, 4, // row 4
    5, 0, 1, 2, // row 5
    3, 4, 5, 6, // row 6
    0, 1, 2, 3, // row 7
    4, 5, 6, 7, // row 8
    1, 2, 3, 4, // row 9
    5, 6, 7, 2, // row 10
    3, 4, 5, 6, // row 11
    7, 3, 4, 5, // row 12
    6, 7, 4, 5, // row 13
    6, 7, 5, 6, // row 14
    7, 6, 7, 7, // row 15
];

/// `ff_hevc_diag_scan8x8_y`.
pub const DIAG_SCAN8X8_Y: [u8; 64] = [
    0, 1, 0, 2, // row 0
    1, 0, 3, 2, // row 1
    1, 0, 4, 3, // row 2
    2, 1, 0, 5, // row 3
    4, 3, 2, 1, // row 4
    0, 6, 5, 4, // row 5
    3, 2, 1, 0, // row 6
    7, 6, 5, 4, // row 7
    3, 2, 1, 0, // row 8
    7, 6, 5, 4, // row 9
    3, 2, 1, 7, // row 10
    6, 5, 4, 3, // row 11
    2, 7, 6, 5, // row 12
    4, 3, 7, 6, // row 13
    5, 4, 7, 6, // row 14
    5, 7, 6, 7, // row 15
];

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn scans_have_expected_values() {
        assert_eq!(DIAG_SCAN4X4_X.len(), 16);
        assert_eq!(DIAG_SCAN8X8_X.len(), 64);
        assert_eq!(DIAG_SCAN8X8_Y.len(), 64);
        // The last diagonal-scan position in an 8x8 block is (7, 7).
        assert_eq!(DIAG_SCAN8X8_X[63], 7);
        assert_eq!(DIAG_SCAN8X8_Y[63], 7);
    }
}
