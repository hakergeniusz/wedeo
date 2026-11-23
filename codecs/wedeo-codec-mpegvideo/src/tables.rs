//! Lookup tables shared by the MPEG-1/2/4 codecs.
//!
//! Transcribed verbatim from FFmpeg `libavcodec/mpeg12data.c`,
//! `mpegvideodata.c`, `mathtables.c` and `mpeg12vlc.h`.

/// Zigzag scan order for an 8x8 block (FFmpeg `ff_zigzag_direct`).
///
/// Maps a coefficient's position in the scan to its raster index.
pub const ZIGZAG_DIRECT: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Zigzag scan order for a 4x4 block (FFmpeg `ff_zigzag_scan`).
///
/// Used by MPEG-4/H.263 4x4 transforms; kept here for completeness.
pub const ZIGZAG_SCAN_4X4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// Alternate horizontal scan (FFmpeg `ff_alternate_horizontal_scan`).
pub const ALTERNATE_HORIZONTAL_SCAN: [u8; 64] = [
    0, 1, 2, 3, 8, 9, 16, 17, 10, 11, 4, 5, 6, 7, 15, 14, 13, 12, 19, 18, 24, 25, 32, 33, 26, 27,
    20, 21, 22, 23, 28, 29, 30, 31, 34, 35, 40, 41, 48, 49, 42, 43, 36, 37, 38, 39, 44, 45, 46, 47,
    50, 51, 56, 57, 58, 59, 52, 53, 54, 55, 60, 61, 62, 63,
];

/// Alternate vertical scan (FFmpeg `ff_alternate_vertical_scan`).
///
/// Selected when `alternate_scan` is set in the picture coding extension.
pub const ALTERNATE_VERTICAL_SCAN: [u8; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19,
    27, 34, 42, 50, 58, 35, 43, 51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53,
    61, 22, 30, 7, 15, 23, 31, 38, 46, 54, 62, 39, 47, 55, 63,
];

/// Default MPEG-1 intra quantizer matrix (FFmpeg
/// `ff_mpeg1_default_intra_matrix`).
pub const MPEG1_DEFAULT_INTRA_MATRIX: [u16; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
];

/// Default MPEG-1 non-intra (inter) quantizer matrix (FFmpeg
/// `ff_mpeg1_default_non_intra_matrix`).
pub const MPEG1_DEFAULT_NON_INTRA_MATRIX: [u16; 64] = [16; 64];

/// MPEG-2 non-linear quantizer scale table (FFmpeg
/// `ff_mpeg2_non_linear_qscale`).
pub const MPEG2_NON_LINEAR_QSCALE: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64,
    72, 80, 88, 96, 104, 112,
];

/// DC scale by `intra_dc_precision` then qscale (FFmpeg
/// `ff_mpeg12_dc_scale_table`). Indexed `[precision][qscale]`.
pub const MPEG12_DC_SCALE_TABLE: [[u8; 32]; 4] = [[8; 32], [4; 32], [2; 32], [1; 32]];

/// Identity luma/chroma qscale table (FFmpeg `ff_default_chroma_qscale_table`).
pub const DEFAULT_CHROMA_QSCALE_TABLE: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, 30, 31,
];

/// DC luminance VLC code lengths (FFmpeg `ff_mpeg12_vlc_dc_lum_bits`).
pub const DC_LUM_BITS: [u8; 12] = [3, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 9];
/// DC luminance VLC codes (FFmpeg `ff_mpeg12_vlc_dc_lum_code`).
pub const DC_LUM_CODE: [u16; 12] = [
    0x4, 0x0, 0x1, 0x5, 0x6, 0xe, 0x1e, 0x3e, 0x7e, 0xfe, 0x1fe, 0x1ff,
];

/// DC chrominance VLC code lengths (FFmpeg `ff_mpeg12_vlc_dc_chroma_bits`).
pub const DC_CHROMA_BITS: [u8; 12] = [2, 2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10];
/// DC chrominance VLC codes (FFmpeg `ff_mpeg12_vlc_dc_chroma_code`).
pub const DC_CHROMA_CODE: [u16; 12] = [
    0x0, 0x1, 0x2, 0x6, 0xe, 0x1e, 0x3e, 0x7e, 0xfe, 0x1fe, 0x3fe, 0x3ff,
];

/// Motion-vector VLC table: `[code, bits]` (FFmpeg
/// `ff_mpeg12_mbMotionVectorTable`).
pub const MB_MOTION_VECTOR_TABLE: [[u8; 2]; 17] = [
    [0x1, 1],
    [0x1, 2],
    [0x1, 3],
    [0x1, 4],
    [0x3, 6],
    [0x5, 7],
    [0x4, 7],
    [0x3, 7],
    [0xb, 9],
    [0xa, 9],
    [0x9, 9],
    [0x11, 10],
    [0x10, 10],
    [0xf, 10],
    [0xe, 10],
    [0xd, 10],
    [0xc, 10],
];

/// Macroblock address increment VLC table: `[code, bits]` (FFmpeg
/// `ff_mpeg12_mbAddrIncrTable`).
pub const MB_ADDR_INCR_TABLE: [[u8; 2]; 36] = [
    [0x1, 1],
    [0x3, 3],
    [0x2, 3],
    [0x3, 4],
    [0x2, 4],
    [0x3, 5],
    [0x2, 5],
    [0x7, 7],
    [0x6, 7],
    [0xb, 8],
    [0xa, 8],
    [0x9, 8],
    [0x8, 8],
    [0x7, 8],
    [0x6, 8],
    [0x17, 10],
    [0x16, 10],
    [0x15, 10],
    [0x14, 10],
    [0x13, 10],
    [0x12, 10],
    [0x23, 11],
    [0x22, 11],
    [0x21, 11],
    [0x20, 11],
    [0x1f, 11],
    [0x1e, 11],
    [0x1d, 11],
    [0x1c, 11],
    [0x1b, 11],
    [0x1a, 11],
    [0x19, 11],
    [0x18, 11],
    [0x8, 11],
    [0xf, 11],
    [0x0, 8],
];

/// Coded block pattern VLC table: `[code, bits]` (FFmpeg `ff_mpeg12_mbPatTable`).
pub const MB_PAT_TABLE: [[u8; 2]; 64] = [
    [0x1, 9],
    [0xb, 5],
    [0x9, 5],
    [0xd, 6],
    [0xd, 4],
    [0x17, 7],
    [0x13, 7],
    [0x1f, 8],
    [0xc, 4],
    [0x16, 7],
    [0x12, 7],
    [0x1e, 8],
    [0x13, 5],
    [0x1b, 8],
    [0x17, 8],
    [0x13, 8],
    [0xb, 4],
    [0x15, 7],
    [0x11, 7],
    [0x1d, 8],
    [0x11, 5],
    [0x19, 8],
    [0x15, 8],
    [0x11, 8],
    [0xf, 6],
    [0xf, 8],
    [0xd, 8],
    [0x3, 9],
    [0xf, 5],
    [0xb, 8],
    [0x7, 8],
    [0x7, 9],
    [0xa, 4],
    [0x14, 7],
    [0x10, 7],
    [0x1c, 8],
    [0xe, 6],
    [0xe, 8],
    [0xc, 8],
    [0x2, 9],
    [0x10, 5],
    [0x18, 8],
    [0x14, 8],
    [0x10, 8],
    [0xe, 5],
    [0xa, 8],
    [0x6, 8],
    [0x6, 9],
    [0x12, 5],
    [0x1a, 8],
    [0x16, 8],
    [0x12, 8],
    [0xd, 5],
    [0x9, 8],
    [0x5, 8],
    [0x5, 9],
    [0xc, 5],
    [0x8, 8],
    [0x4, 8],
    [0x4, 9],
    [0x7, 3],
    [0xa, 5],
    [0x8, 5],
    [0xc, 6],
];

/// Number of run/level VLC symbols (FFmpeg `MPEG12_RL_NB_ELEMS`).
pub const MPEG12_RL_NB_ELEMS: usize = 111;

/// Run/level VLC level values (FFmpeg `ff_mpeg12_level`).
pub const MPEG12_LEVEL: [i8; MPEG12_RL_NB_ELEMS] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15, 16, 17, 18, 1, 2, 3, 4, 5, 1, 2, 3, 4, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 1, 2, 1, 2,
    1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

/// Run/level VLC run values (FFmpeg `ff_mpeg12_run`).
pub const MPEG12_RUN: [i8; MPEG12_RL_NB_ELEMS] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 5, 6, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14,
    15, 15, 16, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
];

/// MPEG-1 run/level VLC table `[code, bits]`, followed by the escape and EOB
/// entries (FFmpeg `ff_mpeg1_vlc_table`).
pub const MPEG1_VLC_TABLE: [[u16; 2]; MPEG12_RL_NB_ELEMS + 2] = [
    [0x3, 2],
    [0x4, 4],
    [0x5, 5],
    [0x6, 7],
    [0x26, 8],
    [0x21, 8],
    [0xa, 10],
    [0x1d, 12],
    [0x18, 12],
    [0x13, 12],
    [0x10, 12],
    [0x1a, 13],
    [0x19, 13],
    [0x18, 13],
    [0x17, 13],
    [0x1f, 14],
    [0x1e, 14],
    [0x1d, 14],
    [0x1c, 14],
    [0x1b, 14],
    [0x1a, 14],
    [0x19, 14],
    [0x18, 14],
    [0x17, 14],
    [0x16, 14],
    [0x15, 14],
    [0x14, 14],
    [0x13, 14],
    [0x12, 14],
    [0x11, 14],
    [0x10, 14],
    [0x18, 15],
    [0x17, 15],
    [0x16, 15],
    [0x15, 15],
    [0x14, 15],
    [0x13, 15],
    [0x12, 15],
    [0x11, 15],
    [0x10, 15],
    [0x3, 3],
    [0x6, 6],
    [0x25, 8],
    [0xc, 10],
    [0x1b, 12],
    [0x16, 13],
    [0x15, 13],
    [0x1f, 15],
    [0x1e, 15],
    [0x1d, 15],
    [0x1c, 15],
    [0x1b, 15],
    [0x1a, 15],
    [0x19, 15],
    [0x13, 16],
    [0x12, 16],
    [0x11, 16],
    [0x10, 16],
    [0x5, 4],
    [0x4, 7],
    [0xb, 10],
    [0x14, 12],
    [0x14, 13],
    [0x7, 5],
    [0x24, 8],
    [0x1c, 12],
    [0x13, 13],
    [0x6, 5],
    [0xf, 10],
    [0x12, 12],
    [0x7, 6],
    [0x9, 10],
    [0x12, 13],
    [0x5, 6],
    [0x1e, 12],
    [0x14, 16],
    [0x4, 6],
    [0x15, 12],
    [0x7, 7],
    [0x11, 12],
    [0x5, 7],
    [0x11, 13],
    [0x27, 8],
    [0x10, 13],
    [0x23, 8],
    [0x1a, 16],
    [0x22, 8],
    [0x19, 16],
    [0x20, 8],
    [0x18, 16],
    [0xe, 10],
    [0x17, 16],
    [0xd, 10],
    [0x16, 16],
    [0x8, 10],
    [0x15, 16],
    [0x1f, 12],
    [0x1a, 12],
    [0x19, 12],
    [0x17, 12],
    [0x16, 12],
    [0x1f, 13],
    [0x1e, 13],
    [0x1d, 13],
    [0x1c, 13],
    [0x1b, 13],
    [0x1f, 16],
    [0x1e, 16],
    [0x1d, 16],
    [0x1c, 16],
    [0x1b, 16],
    [0x1, 6],
    [0x2, 2],
];

/// MPEG-2 run/level VLC table `[code, bits]`, followed by the escape and EOB
/// entries (FFmpeg `ff_mpeg2_vlc_table`).
pub const MPEG2_VLC_TABLE: [[u16; 2]; MPEG12_RL_NB_ELEMS + 2] = [
    [0x02, 2],
    [0x06, 3],
    [0x07, 4],
    [0x1c, 5],
    [0x1d, 5],
    [0x05, 6],
    [0x04, 6],
    [0x7b, 7],
    [0x7c, 7],
    [0x23, 8],
    [0x22, 8],
    [0xfa, 8],
    [0xfb, 8],
    [0xfe, 8],
    [0xff, 8],
    [0x1f, 14],
    [0x1e, 14],
    [0x1d, 14],
    [0x1c, 14],
    [0x1b, 14],
    [0x1a, 14],
    [0x19, 14],
    [0x18, 14],
    [0x17, 14],
    [0x16, 14],
    [0x15, 14],
    [0x14, 14],
    [0x13, 14],
    [0x12, 14],
    [0x11, 14],
    [0x10, 14],
    [0x18, 15],
    [0x17, 15],
    [0x16, 15],
    [0x15, 15],
    [0x14, 15],
    [0x13, 15],
    [0x12, 15],
    [0x11, 15],
    [0x10, 15],
    [0x02, 3],
    [0x06, 5],
    [0x79, 7],
    [0x27, 8],
    [0x20, 8],
    [0x16, 13],
    [0x15, 13],
    [0x1f, 15],
    [0x1e, 15],
    [0x1d, 15],
    [0x1c, 15],
    [0x1b, 15],
    [0x1a, 15],
    [0x19, 15],
    [0x13, 16],
    [0x12, 16],
    [0x11, 16],
    [0x10, 16],
    [0x05, 5],
    [0x07, 7],
    [0xfc, 8],
    [0x0c, 10],
    [0x14, 13],
    [0x07, 5],
    [0x26, 8],
    [0x1c, 12],
    [0x13, 13],
    [0x06, 6],
    [0xfd, 8],
    [0x12, 12],
    [0x07, 6],
    [0x04, 9],
    [0x12, 13],
    [0x06, 7],
    [0x1e, 12],
    [0x14, 16],
    [0x04, 7],
    [0x15, 12],
    [0x05, 7],
    [0x11, 12],
    [0x78, 7],
    [0x11, 13],
    [0x7a, 7],
    [0x10, 13],
    [0x21, 8],
    [0x1a, 16],
    [0x25, 8],
    [0x19, 16],
    [0x24, 8],
    [0x18, 16],
    [0x05, 9],
    [0x17, 16],
    [0x07, 9],
    [0x16, 16],
    [0x0d, 10],
    [0x15, 16],
    [0x1f, 12],
    [0x1a, 12],
    [0x19, 12],
    [0x17, 12],
    [0x16, 12],
    [0x1f, 13],
    [0x1e, 13],
    [0x1d, 13],
    [0x1c, 13],
    [0x1b, 13],
    [0x1f, 16],
    [0x1e, 16],
    [0x1d, 16],
    [0x1c, 16],
    [0x1b, 16],
    [0x01, 6],
    [0x06, 4],
];

/// MPEG-2 frame-rate codes as `(numerator, denominator)` pairs (FFmpeg
/// `ff_mpeg2_frame_rate_tab`). The final `(0, 0)` entry marks an invalid code.
pub const MPEG2_FRAME_RATE: [(u16, u16); 63] = [
    (1, 1),
    (2, 1),
    (3, 1),
    (4, 1),
    (5, 1),
    (6, 1),
    (8, 1),
    (9, 1),
    (10, 1),
    (12, 1),
    (15, 1),
    (16, 1),
    (18, 1),
    (20, 1),
    (24, 1),
    (25, 1),
    (30, 1),
    (32, 1),
    (36, 1),
    (40, 1),
    (45, 1),
    (48, 1),
    (50, 1),
    (60, 1),
    (72, 1),
    (75, 1),
    (80, 1),
    (90, 1),
    (96, 1),
    (100, 1),
    (120, 1),
    (150, 1),
    (180, 1),
    (200, 1),
    (240, 1),
    (750, 1001),
    (800, 1001),
    (960, 1001),
    (1000, 1001),
    (1200, 1001),
    (1250, 1001),
    (1500, 1001),
    (1600, 1001),
    (1875, 1001),
    (2000, 1001),
    (2400, 1001),
    (2500, 1001),
    (3000, 1001),
    (3750, 1001),
    (4000, 1001),
    (4800, 1001),
    (5000, 1001),
    (6000, 1001),
    (7500, 1001),
    (8000, 1001),
    (10000, 1001),
    (12000, 1001),
    (15000, 1001),
    (20000, 1001),
    (24000, 1001),
    (30000, 1001),
    (60000, 1001),
    (0, 0),
];

/// Sequence-header frame-rate code table (FFmpeg `ff_mpeg12_frame_rate_tab`
/// from `mpeg12framerate.c`). Indexed directly by the 4-bit
/// `frame_rate_code`; index 0 is invalid.
pub const MPEG12_FRAME_RATE: [(u16, u16); 16] = [
    (0, 0),
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (15, 1),
    (5, 1),
    (10, 1),
    (12, 1),
    (15, 1),
    (0, 0),
    (0, 0),
];

/// MPEG-1 aspect ratio codes as floating-point values (FFmpeg
/// `ff_mpeg1_aspect`).
pub const MPEG1_ASPECT: [f32; 16] = [
    0.0000, 1.0000, 0.6735, 0.7031, 0.7615, 0.8055, 0.8437, 0.8935, 0.9157, 0.9815, 1.0255, 1.0695,
    1.0950, 1.1575, 1.2015, 0.0,
];

/// MPEG-2 aspect ratio codes as `(numerator, denominator)` pairs (FFmpeg
/// `ff_mpeg2_aspect`).
pub const MPEG2_ASPECT: [(u16, u16); 16] = [
    (0, 1),
    (1, 1),
    (4, 3),
    (16, 9),
    (221, 100),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
    (0, 1),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_is_a_permutation() {
        let mut seen = [false; 64];
        for &v in &ZIGZAG_DIRECT {
            let i = v as usize;
            assert!(i < 64 && !seen[i]);
            seen[i] = true;
        }
        assert!(seen.iter().all(|&x| x));
    }

    #[test]
    fn mpeg12_matrix_lengths() {
        assert_eq!(MPEG12_LEVEL.len(), MPEG12_RL_NB_ELEMS);
        assert_eq!(MPEG12_RUN.len(), MPEG12_RL_NB_ELEMS);
        assert_eq!(MPEG1_VLC_TABLE.len(), MPEG12_RL_NB_ELEMS + 2);
        assert_eq!(MPEG2_VLC_TABLE.len(), MPEG12_RL_NB_ELEMS + 2);
        assert_eq!(MPEG1_DEFAULT_INTRA_MATRIX[0], 8);
    }
}
