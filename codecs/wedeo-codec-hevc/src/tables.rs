//! Quantisation tables used by HEVC residual dequantisation.
//!
//! FFmpeg reference: `libavcodec/hevc/cabac.c`,
//! `ff_hevc_hls_residual_coding()` (the "Derive QP for dequant" block).

/// `level_scale` from `ff_hevc_hls_residual_coding()`.
pub const LEVEL_SCALE: [u8; 6] = [40, 45, 51, 57, 64, 72];

/// `qp_c` from `ff_hevc_hls_residual_coding()`.
pub const QP_C: [u8; 14] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37];

/// `rem6` from `ff_hevc_hls_residual_coding()` (size `51 + 4 * 6 + 1`).
pub const REM6: [u8; 76] = [
    0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1,
    2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3,
    4, 5, 0, 1, 2, 3, 4, 5, 0, 1, 2, 3,
];

/// `div6` from `ff_hevc_hls_residual_coding()` (size `51 + 4 * 6 + 1`).
pub const DIV6: [u8; 76] = [
    0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 4, 4, 4, 4, 4, 4, 5, 5,
    5, 5, 5, 5, 6, 6, 6, 6, 6, 6, 7, 7, 7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9, 10, 10, 10,
    10, 10, 10, 11, 11, 11, 11, 11, 11, 12, 12, 12, 12,
];

/// `ff_hevc_luma_mv_mvp_mode` style chroma tables are in the refs crate.
/// `tab_mode_idx` from `intra_prediction_unit()` (4:2:2 chroma mode mapping).
pub const TAB_MODE_IDX: [u8; 35] = [
    0, 1, 2, 2, 2, 2, 3, 5, 7, 8, 10, 12, 13, 15, 17, 18, 19, 20, 21, 22, 23, 23, 24, 24, 25, 25,
    26, 27, 27, 28, 28, 29, 29, 30, 31,
];

/// `intra_chroma_table` from `intra_prediction_unit()`.
pub const INTRA_CHROMA_TABLE: [u8; 4] = [0, 26, 10, 1];

/// Derive the luma QP used for dequantisation (FFmpeg's `qp_y + qp_bd_offset`).
///
/// Returns `None` when `qp` is outside the bounds of the lookup tables.
pub fn scale_for_qp(qp: i32) -> Option<i32> {
    let rem = *REM6.get(usize::try_from(qp).ok()?)? as i32;
    let div = *DIV6.get(usize::try_from(qp).ok()?)? as i32;
    Some(i32::from(LEVEL_SCALE[rem as usize]) << div)
}

/// Chroma QP derivation (ITU-T H.265 7.4.5 / FFmpeg cabac.c lines 1146-1171).
///
/// `qp_y` is the current luma QP, `offset` the sum of PPS, slice and CU
/// chroma QP offsets, `qp_bd_offset = 6 * (bit_depth - 8)`.
pub fn chroma_qp(qp_y: i32, offset: i32, chroma_format_idc: u32, qp_bd_offset: u32) -> i32 {
    let lo = -(qp_bd_offset as i32);
    let qp_i = (qp_y + offset).clamp(lo, 57);
    let qp = if chroma_format_idc == 1 {
        if qp_i < 30 {
            qp_i
        } else if qp_i > 43 {
            qp_i - 6
        } else {
            i32::from(QP_C[(qp_i - 30) as usize])
        }
    } else if qp_i > 51 {
        51
    } else {
        qp_i
    };
    qp + qp_bd_offset as i32
}
