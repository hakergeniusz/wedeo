//! Intra prediction: MPM derivation and neighbour sample gathering.
//!
//! FFmpeg references: `hevcdec.c` (`luma_intra_pred_mode`,
//! `intra_prediction_unit`) and `pred_template.c` (`FUNC(intra_pred)`).

use wedeo_codec_hevc_cabac::Cabac;
use wedeo_codec_hevc_dsp::intra::{
    IntraFilterOptions, IntraNeighbors, IntraPredMode, intra_predict_with_neighbors,
};
use wedeo_codec_hevc_ps::{Pps, Sps};
use wedeo_codec_hevc_refs::{
    NeighbourAvailability, NeighbourCtbFlags, RefGeometry, z_scan_block_avail,
};
use wedeo_core::Result;

use crate::picture::Picture;
use crate::tables::{INTRA_CHROMA_TABLE, TAB_MODE_IDX};

/// Per-picture intra prediction state (FFmpeg's `tab_ipm` and `pu.intra_pred_mode[]`).
#[derive(Debug, Clone)]
pub struct IntraState {
    pub tab_ipm: Vec<u8>,
    pub min_pu_width: usize,
    pub modes: [u8; 4],
    pub modes_c: [u8; 4],
    pub chroma_modes: [u8; 4],
    /// Mode used by the current transform unit.
    pub mode: u8,
    pub mode_c: u8,
}

impl IntraState {
    pub fn new(min_pu_width: usize, min_pu_height: usize) -> Self {
        Self {
            tab_ipm: vec![1; min_pu_width * min_pu_height],
            min_pu_width,
            modes: [1; 4],
            modes_c: [1; 4],
            chroma_modes: [0; 4],
            mode: 1,
            mode_c: 1,
        }
    }
}

/// Parse the intra prediction unit syntax (8.4.1) at the start of an intra CU.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's intra_prediction_unit
pub fn intra_prediction_unit(
    cabac: &mut Cabac<'_>,
    sps: &Sps,
    pps: &Pps,
    state: &mut IntraState,
    x0: i32,
    y0: i32,
    log2_cb_size: u32,
    intra_split: bool,
    ctb_up_flag: bool,
    ctb_left_flag: bool,
) -> Result<()> {
    let side = usize::from(intra_split) + 1;
    let pb_size = (1i32 << log2_cb_size) >> usize::from(intra_split);
    let mut prev_flag = [0u8; 4];
    let mut mpm_idx = 0i32;
    let mut rem_mode = 0i32;

    for i in 0..side {
        for j in 0..side {
            prev_flag[2 * i + j] = cabac.decode_prev_intra_luma_pred_flag();
        }
    }
    for i in 0..side {
        for j in 0..side {
            let k = 2 * i + j;
            if prev_flag[k] != 0 {
                mpm_idx = cabac.decode_mpm_idx();
            } else {
                rem_mode = cabac.decode_rem_intra_luma_pred_mode();
            }
            let mode = luma_intra_pred_mode(
                sps,
                state,
                x0 + pb_size * j as i32,
                y0 + pb_size * i as i32,
                pb_size,
                prev_flag[k] != 0,
                mpm_idx,
                rem_mode,
                ctb_up_flag,
                ctb_left_flag,
            );
            state.modes[k] = mode;
        }
    }

    // Chroma mode derivation (8.4.1.2.1).
    let chroma_mode = cabac.decode_intra_chroma_pred_mode() as u8;
    let n = if sps.chroma_format_idc == 3 {
        side * side
    } else {
        1
    };
    for k in 0..n {
        state.chroma_modes[k] = chroma_mode;
        let idx = chroma_mode as usize;
        let mapped = if idx == 4 {
            state.modes[k.min(3)]
        } else if state.modes[k.min(3)] == INTRA_CHROMA_TABLE[idx] {
            34
        } else {
            INTRA_CHROMA_TABLE[idx]
        };
        let mapped = if sps.chroma_format_idc == 2 && mapped < TAB_MODE_IDX.len() as u8 {
            TAB_MODE_IDX[mapped as usize]
        } else {
            mapped
        };
        state.modes_c[k] = mapped;
    }
    let _ = pps;
    Ok(())
}

/// Default intra mode/flag state for inter or skip CUs (FFmpeg's
/// `intra_prediction_unit_default_value`).
pub fn intra_defaults(
    state: &mut IntraState,
    sps: &Sps,
    x0: i32,
    y0: i32,
    log2_cb_size: u32,
    picture: &mut Picture,
) {
    let pb_size = 1i32 << log2_cb_size;
    let mut size_in_pus = (pb_size as u32) >> sps.log2_min_pu_size;
    if size_in_pus == 0 {
        size_in_pus = 1;
    }
    let x_pu = (x0 as u32 >> sps.log2_min_pu_size) as usize;
    let y_pu = (y0 as u32 >> sps.log2_min_pu_size) as usize;
    for j in 0..size_in_pus as usize {
        for i in 0..state.min_pu_width.min(x_pu + size_in_pus as usize) {
            let idx = (y_pu + j) * state.min_pu_width + x_pu + i;
            if idx < state.tab_ipm.len() {
                state.tab_ipm[idx] = 1; // INTRA_DC
            }
        }
    }
    // Mark the PUs as intra in the MV field table.
    for j in 0..size_in_pus as usize {
        for i in 0..size_in_pus as usize {
            if let Some(f) = picture.tab_mvf.get_mut(x_pu + i, y_pu + j) {
                f.pred_flag = wedeo_codec_hevc_refs::PredFlag::INTRA;
            }
        }
    }
    state.modes = [1; 4];
    state.modes_c = [1; 4];
}

#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's luma_intra_pred_mode
fn luma_intra_pred_mode(
    sps: &Sps,
    state: &mut IntraState,
    x0: i32,
    y0: i32,
    pu_size: i32,
    prev_flag: bool,
    mpm_idx: i32,
    rem_mode: i32,
    ctb_up_flag: bool,
    ctb_left_flag: bool,
) -> u8 {
    const PLANAR: u8 = 0;
    const DC: u8 = 1;
    const ANG26: u8 = 26;

    let x_pu = (x0 as u32 >> sps.log2_min_pu_size) as usize;
    let y_pu = (y0 as u32 >> sps.log2_min_pu_size) as usize;
    let x0b = x0 & ((1 << sps.log2_ctb_size) - 1);
    let y0b = y0 & ((1 << sps.log2_ctb_size) - 1);
    let y_ctb = y0 & !((1 << sps.log2_ctb_size) - 1);

    let tab = |x: usize, y: usize| -> u8 {
        state
            .tab_ipm
            .get(y * state.min_pu_width + x)
            .copied()
            .unwrap_or(DC)
    };
    let mut cand_up = if ctb_up_flag || y0b != 0 {
        tab(x_pu, y_pu.saturating_sub(1))
    } else {
        DC
    };
    let cand_left = if ctb_left_flag || x0b != 0 {
        tab(x_pu.saturating_sub(1), y_pu)
    } else {
        DC
    };
    if y0 - 1 < y_ctb {
        cand_up = DC;
    }

    let mut candidates = if cand_left == cand_up {
        if cand_left < 2 {
            [PLANAR, DC, ANG26]
        } else {
            [
                cand_left,
                (2i32 + ((i32::from(cand_left) - 2 - 1 + 32) & 31)) as u8,
                2 + ((cand_left - 2 + 1) & 31),
            ]
        }
    } else {
        let third = if cand_left != PLANAR && cand_up != PLANAR {
            PLANAR
        } else if cand_left != DC && cand_up != DC {
            DC
        } else {
            ANG26
        };
        [cand_left, cand_up, third]
    };

    let mode = if prev_flag {
        candidates
            .get(mpm_idx as usize)
            .copied()
            .unwrap_or(candidates[0])
    } else {
        candidates.sort_unstable();
        let mut m = rem_mode as u8;
        for &c in &candidates {
            if m >= c {
                m += 1;
            }
        }
        m
    };

    let size_in_pus = ((pu_size as u32) >> sps.log2_min_pu_size).max(1) as usize;
    for i in 0..size_in_pus {
        for j in 0..size_in_pus {
            if let Some(v) = state
                .tab_ipm
                .get_mut((y_pu + i) * state.min_pu_width + x_pu + j)
            {
                *v = mode;
            }
        }
    }
    mode
}

/// FFmpeg's `MIN_TB_ADDR_ZS(x, y)` for *signed* TB coordinates.
///
/// The PS table has a -1 sentinel row and column, so an unavailable neighbour
/// at x == -1 or y == -1 reads -1 instead of wrapping into the CTB.
fn min_tb_addr_zs(geo: &RefGeometry, x: i32, y: i32) -> i32 {
    let side = geo.tb_mask as usize + 2;
    let (x, y) = (x + 1, y + 1);
    if x < 0 || y < 0 {
        -1
    } else {
        geo.min_tb_addr_zs_tab
            .get(y as usize * side + x as usize)
            .copied()
            .unwrap_or(-1)
    }
}

/// Predict one transform block using the reconstructed neighbours of `picture`.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's FUNC(intra_pred)
pub fn intra_predict_tu(
    picture: &mut Picture,
    sps: &Sps,
    pps: &Pps,
    geo: &RefGeometry,
    na: NeighbourAvailability,
    x0: i32,
    y0: i32,
    log2_size: u32,
    c_idx: usize,
    mode: u8,
) -> Result<()> {
    let size = 1usize << log2_size;
    let hshift = if c_idx == 0 { 0 } else { sps.hshift[1] };
    let vshift = if c_idx == 0 { 0 } else { sps.vshift[1] };
    let x = x0 >> hshift;
    let y = y0 >> vshift;
    let width = picture.width as i32;
    let height = picture.height as i32;

    let x_tb = ((x0 as u32) >> sps.log2_min_tb_size) & sps.tb_mask;
    let y_tb = ((y0 as u32) >> sps.log2_min_tb_size) & sps.tb_mask;
    let size_in_tbs_h = (size << hshift as usize) >> sps.log2_min_tb_size;
    let size_in_tbs_v = (size << vshift as usize) >> sps.log2_min_tb_size;
    // pred_template.c:131 - spin uses the *luma* TB height for chroma.
    let spin = c_idx != 0 && size_in_tbs_v == 0 && ((2 * y0) & (1 << sps.log2_min_tb_size)) != 0;
    // FFmpeg's MIN_TB_ADDR_ZS takes signed neighbours. A negative coordinate
    // lands on the -1 sentinel row/column; wrapping it into tb_mask would make
    // unavailable top-right / bottom-left samples look available.
    let cur_tb_addr = min_tb_addr_zs(geo, x_tb as i32, y_tb as i32);

    let mut cand_up_left = na.cand_up_left;
    let mut cand_up = na.cand_up;
    let mut cand_left = na.cand_left;
    // pred_template.c:154,158. The spin term in the bottom-left y coordinate
    // is required for 4:2:0 chroma when the TB is smaller than a chroma sample.
    let mut cand_up_right = na.cand_up_right
        && !spin
        && cur_tb_addr
            > min_tb_addr_zs(
                geo,
                (x_tb as i32 + size_in_tbs_h as i32) & sps.tb_mask as i32,
                y_tb as i32 - 1,
            );
    let mut cand_bottom_left = na.cand_bottom_left
        && cur_tb_addr
            > min_tb_addr_zs(
                geo,
                x_tb as i32 - 1,
                (y_tb as i32 + size_in_tbs_v as i32 + spin as i32) & sps.tb_mask as i32,
            );

    let bottom_left_size = ((height.min(y0 + 2 * (size as i32) * (1 << vshift))
        - (y0 + size as i32 * (1 << vshift)))
        >> vshift) as usize;
    let top_right_size = ((width.min(x0 + 2 * (size as i32) * (1 << hshift))
        - (x0 + size as i32 * (1 << hshift)))
        >> hshift) as usize;

    // Constrained intra prediction: a neighbour is usable only if its PU is intra.
    if pps.constrained_intra_pred_flag {
        let pu = |v: i32| (v as u32 >> sps.log2_min_pu_size) as usize;
        let is_intra = |px: i32, py: i32| -> bool {
            picture
                .tab_mvf
                .get(pu(px), pu(py))
                .is_some_and(|f| f.pred_flag.is_intra())
        };
        if cand_left {
            let mut ok = true;
            for i in 0..size as i32 {
                if !is_intra(x0 - 1, y0 + i * (1 << vshift)) {
                    ok = false;
                    break;
                }
            }
            cand_left = ok;
        }
        if cand_up {
            let mut ok = true;
            for i in 0..size as i32 {
                if !is_intra(x0 + i * (1 << hshift), y0 - 1) {
                    ok = false;
                    break;
                }
            }
            cand_up = ok;
        }
        if cand_up_left {
            cand_up_left = is_intra(x0 - 1, y0 - 1);
        }
        if cand_up_right {
            let mut ok = true;
            for i in 0..size as i32 {
                if !is_intra(x0 + size as i32 * (1 << hshift) + i, y0 - 1) {
                    ok = false;
                    break;
                }
            }
            cand_up_right = ok;
        }
        if cand_bottom_left {
            let mut ok = true;
            for i in 0..size as i32 {
                if !is_intra(x0 - 1, y0 + size as i32 * (1 << vshift) + i) {
                    ok = false;
                    break;
                }
            }
            cand_bottom_left = ok;
        }
    }

    let mut top_buf = vec![0u16; size];
    let mut left_buf = vec![0u16; size];
    let mut top_right_buf = vec![0u16; size];
    let mut bottom_left_buf = vec![0u16; size];

    if cand_up {
        for i in 0..size {
            top_buf[i] = picture.sample(c_idx, x as usize + i, y as usize - 1);
        }
    }
    if cand_left {
        for i in 0..size {
            left_buf[i] = picture.sample(c_idx, x as usize - 1, y as usize + i);
        }
    }
    if cand_up_right {
        let n = top_right_size.min(size);
        for i in 0..n {
            top_right_buf[i] = picture.sample(c_idx, x as usize + size + i, y as usize - 1);
        }
        top_right_buf.truncate(n);
    } else {
        top_right_buf.clear();
    }
    if cand_bottom_left {
        let n = bottom_left_size.min(size);
        for i in 0..n {
            bottom_left_buf[i] = picture.sample(c_idx, x as usize - 1, y as usize + size + i);
        }
        bottom_left_buf.truncate(n);
    } else {
        bottom_left_buf.clear();
    }

    let mut dst = vec![0u16; size * size];
    let neighbors = IntraNeighbors {
        top_left: cand_up_left.then(|| picture.sample(c_idx, x as usize - 1, y as usize - 1)),
        top: cand_up.then_some(top_buf.as_slice()),
        top_right: (!top_right_buf.is_empty()).then_some(top_right_buf.as_slice()),
        left: cand_left.then_some(left_buf.as_slice()),
        bottom_left: (!bottom_left_buf.is_empty()).then_some(bottom_left_buf.as_slice()),
    };
    let options = IntraFilterOptions {
        smoothing_disabled: sps.range_ext.intra_smoothing_disabled,
        strong_smoothing: sps.strong_intra_smoothing_enabled,
        chroma_format_idc: sps.chroma_format_idc as u8,
    };
    intra_predict_with_neighbors(
        &mut dst,
        size,
        log2_size as u8,
        IntraPredMode::try_from(mode).map_err(|e| wedeo_core::Error::Other(e.to_string()))?,
        c_idx,
        picture.bit_depth,
        neighbors,
        options,
    )
    .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;

    for j in 0..size {
        for i in 0..size {
            picture.set_sample(c_idx, x as usize + i, y as usize + j, dst[j * size + i]);
        }
    }
    Ok(())
}

/// Compute neighbour CTB flags (FFmpeg's `hls_decode_neighbour`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's hls_decode_neighbour inputs
pub fn neighbour_flags(
    geo: &RefGeometry,
    x_ctb: i32,
    y_ctb: i32,
    ctb_addr_rs: usize,
    slice_addr: i32,
    left_slice_addr: Option<i32>,
    up_slice_addr: Option<i32>,
    up_left_slice_addr: Option<i32>,
    end_of_tiles_x: i32,
    end_of_tiles_y: i32,
) -> (NeighbourAvailability, NeighbourCtbFlags) {
    let ctb_addr_in_slice = ctb_addr_rs as i32 - slice_addr;
    let ctb_size = 1 << geo.log2_ctb_size;
    let flags = NeighbourCtbFlags {
        ctb_left_flag: x_ctb > 0 && ctb_addr_in_slice > 0,
        ctb_up_flag: y_ctb > 0 && ctb_addr_in_slice >= geo.ctb_width as i32,
        ctb_up_left_flag: x_ctb > 0
            && y_ctb > 0
            && ctb_addr_in_slice > geo.ctb_width as i32
            && up_left_slice_addr.is_some(),
        ctb_up_right_flag: y_ctb > 0
            && ctb_addr_in_slice + 1 >= geo.ctb_width as i32
            && up_slice_addr.is_some(),
        end_of_tiles_x,
        end_of_tiles_y,
    };
    let _ = (ctb_size, left_slice_addr);
    let na = wedeo_codec_hevc_refs::set_neighbour_available(0, 0, 0, 0, geo.log2_ctb_size, flags);
    // The caller recomputes per-block availability with the actual block origin.
    (na, flags)
}

/// Compute per-block neighbour availability (FFmpeg's `ff_hevc_set_neighbour_available`).
pub fn block_neighbours(
    geo: &RefGeometry,
    flags: NeighbourCtbFlags,
    x0: i32,
    y0: i32,
    size: i32,
) -> NeighbourAvailability {
    wedeo_codec_hevc_refs::set_neighbour_available(x0, y0, size, size, geo.log2_ctb_size, flags)
}

/// z-scan availability helper exposed for the deblocking code.
pub fn z_available(geo: &RefGeometry, x0: i32, y0: i32, xn: i32, yn: i32) -> bool {
    z_scan_block_avail(geo, x0, y0, xn, yn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> RefGeometry {
        let tb_mask = 15u32; // 64x64 CTB with 4x4 minimum TBs
        let side = tb_mask as usize + 2;
        let mut tab = vec![0i32; side * side];
        for y in 0..side {
            tab[y * side] = -1;
            tab[y] = -1;
        }
        for y in 1..=tb_mask as usize {
            for x in 1..=tb_mask as usize {
                tab[y * side + x] = (y * side + x) as i32;
            }
        }
        RefGeometry {
            width: 64,
            height: 64,
            log2_ctb_size: 6,
            log2_min_pu_size: 2,
            log2_min_tb_size: 2,
            ctb_width: 1,
            tb_mask,
            log2_parallel_merge_level: 2,
            ctb_addr_rs_to_ts: vec![0],
            min_tb_addr_zs_offset: side + 1,
            min_tb_addr_zs_tab: tab,
        }
    }

    #[test]
    fn min_tb_addr_zs_matches_ffmpeg_table_and_sentinels() {
        let geo = geometry();
        // Non-negative coordinates read the same value as RefGeometry's
        // (already offset) accessor: pps->min_tb_addr_zs points at (1, 1).
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(
                    min_tb_addr_zs(&geo, x, y),
                    geo.min_tb_addr_zs(x as usize, y as usize).unwrap()
                );
            }
        }
        // The row/column before (0, 0) are -1 sentinels, never wrapped.
        assert_eq!(min_tb_addr_zs(&geo, -1, 0), -1);
        assert_eq!(min_tb_addr_zs(&geo, 0, -1), -1);
        assert_eq!(min_tb_addr_zs(&geo, -1, -1), -1);
    }
}
