//! HEVC scaling list parsing (FFmpeg `ps.c`: `default_scaling_list_*`,
//! `set_default_scaling_list_data`, `scaling_list_data`).

use wedeo_core::Result;

use crate::bitreader::BitReader;
use crate::scan::{DIAG_SCAN4X4_X, DIAG_SCAN4X4_Y, DIAG_SCAN8X8_X, DIAG_SCAN8X8_Y};

/// `default_scaling_list_intra` from `ps.c`.
pub const DEFAULT_SCALING_LIST_INTRA: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 21, 24, 16, 16, 16, 16, 17, 19, 22, 25, 16, 16, 17, 18, 20, 22, 25, 29,
    16, 16, 18, 21, 24, 27, 31, 36, 17, 17, 20, 24, 30, 35, 41, 47, 18, 19, 22, 27, 35, 44, 54, 65,
    21, 22, 25, 31, 41, 54, 70, 88, 24, 25, 29, 36, 47, 65, 88, 115,
];

/// `default_scaling_list_inter` from `ps.c`.
pub const DEFAULT_SCALING_LIST_INTER: [u8; 64] = [
    16, 16, 16, 16, 17, 18, 20, 24, 16, 16, 16, 17, 18, 20, 24, 25, 16, 16, 17, 18, 20, 24, 25, 28,
    16, 17, 18, 20, 24, 25, 28, 33, 17, 18, 20, 24, 25, 28, 33, 41, 18, 20, 24, 25, 28, 33, 41, 54,
    20, 24, 25, 28, 33, 41, 54, 71, 24, 25, 28, 33, 41, 54, 71, 91,
];

/// FFmpeg's `ScalingList`: `sl[size_id][matrix_id][coeff]` and the two 16x16 /
/// 32x32 DC arrays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingList {
    /// `sl[size_id][matrix_id]`: 4x4 lists have 16 valid entries, all others 64.
    pub sl: [[[u8; 64]; 6]; 4],
    /// `sl_dc[0]` is for 16x16, `sl_dc[1]` for 32x32 lists.
    pub sl_dc: [[u8; 6]; 2],
}

impl Default for ScalingList {
    fn default() -> Self {
        scaling_list_defaults()
    }
}

/// `set_default_scaling_list_data()` from `ps.c`.
pub fn scaling_list_defaults() -> ScalingList {
    let mut sl = [[[0u8; 64]; 6]; 4];
    // 4x4 default is 16 for all six matrices.
    for matrix in 0..6 {
        sl[0][matrix][..16].fill(16);
    }
    for size_id in 1..4 {
        for matrix in 0..6 {
            let default = if matrix < 3 {
                DEFAULT_SCALING_LIST_INTRA
            } else {
                DEFAULT_SCALING_LIST_INTER
            };
            sl[size_id][matrix] = default;
        }
    }
    ScalingList {
        sl,
        sl_dc: [[16; 6], [16; 6]],
    }
}

/// Parse `scaling_list_data()` from `ps.c`.
///
/// `chroma_format_idc == 3` triggers the 4:4:4 DC overrides at the end.
pub fn parse_scaling_list_data(
    br: &mut BitReader<'_>,
    sl: &mut ScalingList,
    chroma_format_idc: u32,
) -> Result<()> {
    for size_id in 0..4 {
        let matrix_step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            let pred_mode_flag = br.read_flag()?;
            if !pred_mode_flag {
                let mut delta = br.read_ue()? as usize;
                if delta != 0 {
                    // Copy from previous array. For size_id 3, the delta is
                    // scaled by 3 (only matrices 0 and 3 are coded).
                    delta *= matrix_step;
                    if matrix_id < delta {
                        return Err(wedeo_core::Error::InvalidData);
                    }
                    let count = if size_id > 0 { 64 } else { 16 };
                    let mut source = [0u8; 64];
                    source[..count].copy_from_slice(&sl.sl[size_id][matrix_id - delta][..count]);
                    sl.sl[size_id][matrix_id][..count].copy_from_slice(&source[..count]);
                    if size_id > 1 {
                        sl.sl_dc[size_id - 2][matrix_id] = sl.sl_dc[size_id - 2][matrix_id - delta];
                    }
                }
            } else {
                let mut next_coef = 8i32;
                let coef_num = 64.min(1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    let dc = br.read_se()?;
                    if !(-7..=247).contains(&dc) {
                        return Err(wedeo_core::Error::InvalidData);
                    }
                    next_coef = dc + 8;
                    sl.sl_dc[size_id - 2][matrix_id] = next_coef as u8;
                }
                for i in 0..coef_num {
                    let pos = if size_id == 0 {
                        4 * DIAG_SCAN4X4_Y[i] as usize + DIAG_SCAN4X4_X[i] as usize
                    } else {
                        8 * DIAG_SCAN8X8_Y[i] as usize + DIAG_SCAN8X8_X[i] as usize
                    };
                    let delta_coef = br.read_se()?;
                    next_coef = (next_coef + 256 + delta_coef) % 256;
                    sl.sl[size_id][matrix_id][pos] = next_coef as u8;
                }
            }
            matrix_id += matrix_step;
        }
    }

    if chroma_format_idc == 3 {
        for i in 0..64 {
            sl.sl[3][1][i] = sl.sl[2][1][i];
            sl.sl[3][2][i] = sl.sl[2][2][i];
            sl.sl[3][4][i] = sl.sl[2][4][i];
            sl.sl[3][5][i] = sl.sl[2][5][i];
        }
        sl.sl_dc[1][1] = sl.sl_dc[0][1];
        sl.sl_dc[1][2] = sl.sl_dc[0][2];
        sl.sl_dc[1][4] = sl.sl_dc[0][4];
        sl.sl_dc[1][5] = sl.sl_dc[0][5];
    }

    Ok(())
}
