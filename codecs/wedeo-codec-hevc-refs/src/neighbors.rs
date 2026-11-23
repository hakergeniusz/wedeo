//! Neighbour availability and z-scan block availability.
//!
//! FFmpeg reference: `libavcodec/hevc/mvs.c`
//! (`ff_hevc_set_neighbour_available`, `z_scan_block_avail`, `is_diff_mer`)
//! and `hevcdec.h` (`NeighbourAvailable`).

use crate::context::RefGeometry;

/// FFmpeg's `NeighbourAvailable` (`lc->na`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NeighbourAvailability {
    pub cand_bottom_left: bool,
    pub cand_left: bool,
    pub cand_up: bool,
    pub cand_up_left: bool,
    pub cand_up_right: bool,
    pub cand_up_right_sap: bool,
}

/// CTB/tile flags fed into [`set_neighbour_available`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NeighbourCtbFlags {
    pub ctb_up_flag: bool,
    pub ctb_left_flag: bool,
    pub ctb_up_left_flag: bool,
    pub ctb_up_right_flag: bool,
    /// `lc->end_of_tiles_x`.
    pub end_of_tiles_x: i32,
    /// `lc->end_of_tiles_y`.
    pub end_of_tiles_y: i32,
}

/// FFmpeg's `av_zero_extend(value, bits)` for non-negative coordinates.
fn av_zero_extend(value: i32, bits: u32) -> i32 {
    value & ((1i32 << bits) - 1)
}

/// FFmpeg's `ff_hevc_set_neighbour_available()`.
pub fn set_neighbour_available(
    x0: i32,
    y0: i32,
    n_pb_w: i32,
    n_pb_h: i32,
    log2_ctb_size: u32,
    flags: NeighbourCtbFlags,
) -> NeighbourAvailability {
    let x0b = av_zero_extend(x0, log2_ctb_size);
    let y0b = av_zero_extend(y0, log2_ctb_size);

    let cand_up = flags.ctb_up_flag || y0b != 0;
    let cand_left = flags.ctb_left_flag || x0b != 0;
    let cand_up_left = if x0b != 0 || y0b != 0 {
        cand_left && cand_up
    } else {
        flags.ctb_up_left_flag
    };
    let cand_up_right_sap = if x0b + n_pb_w == 1 << log2_ctb_size {
        flags.ctb_up_right_flag && y0b == 0
    } else {
        cand_up
    };
    let cand_up_right = cand_up_right_sap && x0 + n_pb_w < flags.end_of_tiles_x;
    let cand_bottom_left = if y0 + n_pb_h >= flags.end_of_tiles_y {
        false
    } else {
        cand_left
    };

    NeighbourAvailability {
        cand_bottom_left,
        cand_left,
        cand_up,
        cand_up_left,
        cand_up_right,
        cand_up_right_sap,
    }
}

/// FFmpeg's `z_scan_block_avail()` (6.4.1 Derivation process for z-scan order
/// block availability).
pub fn z_scan_block_avail(geo: &RefGeometry, x_curr: i32, y_curr: i32, x_n: i32, y_n: i32) -> bool {
    let ctb = geo.log2_ctb_size as i32;
    let x_curr_ctb = x_curr >> ctb;
    let y_curr_ctb = y_curr >> ctb;
    let x_n_ctb = x_n >> ctb;
    let y_n_ctb = y_n >> ctb;
    if y_n_ctb < y_curr_ctb || x_n_ctb < x_curr_ctb {
        return true;
    }

    let mask = geo.tb_mask as i32;
    let ltb = geo.log2_min_tb_size as i32;
    let curr_x = ((x_curr >> ltb) & mask) as usize;
    let curr_y = ((y_curr >> ltb) & mask) as usize;
    let n_x = ((x_n >> ltb) & mask) as usize;
    let n_y = ((y_n >> ltb) & mask) as usize;

    match (
        geo.min_tb_addr_zs(curr_x, curr_y),
        geo.min_tb_addr_zs(n_x, n_y),
    ) {
        (Some(curr), Some(n)) => n <= curr,
        // FFmpeg would read out of bounds on malformed geometry; treat it as
        // unavailable rather than panicking.
        _ => false,
    }
}

/// FFmpeg's `is_diff_mer()`: check if the two luma locations belong to the
/// same motion estimation region.
pub fn is_diff_mer(geo: &RefGeometry, x_n: i32, y_n: i32, x_p: i32, y_p: i32) -> bool {
    let plevel = geo.log2_parallel_merge_level as i32;
    x_n >> plevel == x_p >> plevel && y_n >> plevel == y_p >> plevel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_neighbour_available_inside_ctb() {
        // 64x64 CTB, PU at (16, 16): all four inside-neighbours available.
        let na = set_neighbour_available(
            16,
            16,
            8,
            8,
            6,
            NeighbourCtbFlags {
                end_of_tiles_x: 64,
                end_of_tiles_y: 64,
                ..Default::default()
            },
        );
        assert!(na.cand_up && na.cand_left && na.cand_up_left && na.cand_bottom_left);
        assert!(na.cand_up_right_sap); // x0b + nPbW == 64
        assert!(na.cand_up_right); // x0 + nPbW = 24 < 64
    }

    #[test]
    fn set_neighbour_available_top_left_ctb() {
        let na = set_neighbour_available(
            0,
            0,
            8,
            8,
            6,
            NeighbourCtbFlags {
                ctb_up_left_flag: true,
                end_of_tiles_x: 128,
                end_of_tiles_y: 128,
                ..Default::default()
            },
        );
        assert!(!na.cand_up && !na.cand_left);
        assert!(na.cand_up_left); // ctb_up_left_flag
        assert!(!na.cand_bottom_left);
    }

    #[test]
    fn z_scan_earlier_ctb_is_always_available() {
        let geo = RefGeometry::default();
        assert!(z_scan_block_avail(&geo, 64, 64, 0, 0));
    }
}
