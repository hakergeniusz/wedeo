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

    derive_chroma_modes(
        sps.chroma_format_idc,
        side,
        &state.modes,
        || cabac.decode_intra_chroma_pred_mode() as u8,
        &mut state.chroma_modes,
        &mut state.modes_c,
    );
    let _ = pps;
    Ok(())
}

/// Chroma mode derivation (8.4.1.2.1) from FFmpeg's
/// `intra_prediction_unit()`:
///
/// ```c
/// if (sps->chroma_format_idc == 3) {
///     for (i = 0; i < side; i++) {
///         for (j = 0; j < side; j++) {
///             lc->pu.chroma_mode_c[2 * i + j] = chroma_mode = ff_hevc_intra_chroma_pred_mode_decode(lc);
///             ...
///         }
///     }
/// } else if (sps->chroma_format_idc == 2) {
///     lc->pu.chroma_mode_c[0] = chroma_mode = ff_hevc_intra_chroma_pred_mode_decode(lc);
///     ...
///     lc->pu.intra_pred_mode_c[0] = tab_mode_idx[mode_idx];
/// } else if (sps->chroma_format_idc != 0) {
///     chroma_mode = ff_hevc_intra_chroma_pred_mode_decode(lc);
///     ...
/// }
/// ```
///
/// The decode closure is called once per prediction unit for 4:4:4 and once
/// per coding unit otherwise (never for monochrome).
fn derive_chroma_modes(
    chroma_format_idc: u32,
    side: usize,
    modes: &[u8; 4],
    mut decode_mode: impl FnMut() -> u8,
    chroma_modes: &mut [u8; 4],
    modes_c: &mut [u8; 4],
) {
    let mapped_mode = |k: usize, chroma_mode: u8| -> u8 {
        let idx = chroma_mode as usize;
        if idx == 4 {
            modes[k]
        } else if modes[k] == INTRA_CHROMA_TABLE[idx] {
            34
        } else {
            INTRA_CHROMA_TABLE[idx]
        }
    };
    if chroma_format_idc == 3 {
        for i in 0..side {
            for j in 0..side {
                let k = 2 * i + j;
                let chroma_mode = decode_mode();
                chroma_modes[k] = chroma_mode;
                modes_c[k] = mapped_mode(k, chroma_mode);
            }
        }
    } else if chroma_format_idc == 2 {
        let chroma_mode = decode_mode();
        chroma_modes[0] = chroma_mode;
        modes_c[0] = TAB_MODE_IDX[mapped_mode(0, chroma_mode) as usize];
    } else if chroma_format_idc != 0 {
        let chroma_mode = decode_mode();
        chroma_modes[0] = chroma_mode;
        modes_c[0] = mapped_mode(0, chroma_mode);
    }
}

/// Default intra mode/flag state for inter or skip CUs (FFmpeg's
/// `intra_prediction_unit_default_value`).
pub fn intra_defaults(
    state: &mut IntraState,
    sps: &Sps,
    x0: i32,
    y0: i32,
    log2_cb_size: u32,
    _picture: &mut Picture,
) {
    let pb_size = 1i32 << log2_cb_size;
    let mut size_in_pus = (pb_size as u32) >> sps.log2_min_pu_size;
    if size_in_pus == 0 {
        size_in_pus = 1;
    }
    let x_pu = (x0 as u32 >> sps.log2_min_pu_size) as usize;
    let y_pu = (y0 as u32 >> sps.log2_min_pu_size) as usize;
    for j in 0..size_in_pus as usize {
        for i in 0..size_in_pus as usize {
            let idx = (y_pu + j) * state.min_pu_width + x_pu + i;
            if idx < state.tab_ipm.len() {
                state.tab_ipm[idx] = 1; // INTRA_DC
            }
        }
    }
    // hevcdec.c: "if (lc->cu.pred_mode == MODE_INTRA) ... pred_flag = PF_INTRA".
    // The per-frame `tab_mvf` starts as PF_INTRA and each PU is decoded once,
    // so an intra CU never needs to re-mark it. Marking here unconditionally
    // would erase the MVs of a skip/inter CU (called after `prediction_unit`
    // in `hls_coding_unit`), which FFmpeg does not do and which breaks the
    // following block's AMVP/merge candidates.
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

/// `EXTEND_LEFT_CIP` from FFmpeg's `pred_template.c`.
///
/// Replaces each top sample with its right neighbour when the PU above it is
/// not intra. Iteration runs right to left, so a chain of non-intra PUs copies
/// the first usable sample all the way down.
fn extend_left_cip(
    top: &mut [u16],
    corner: &mut u16,
    start: i32,
    length: i32,
    is_intra: &impl Fn(i32, i32) -> bool,
) {
    let mut i = start;
    while i > start - length {
        if !is_intra(i - 1, -1) {
            if i == 0 {
                // FFmpeg's `ptr[i - 1] = ptr[i]` also writes `top[-1]`.
                if let Some(v) = top.first().copied() {
                    *corner = v;
                }
            } else {
                let src = top.get(i as usize).copied();
                if let (Some(dst), Some(v)) = (top.get_mut((i - 1) as usize), src) {
                    *dst = v;
                }
            }
        }
        i -= 1;
    }
}

/// `EXTEND_RIGHT_CIP` from FFmpeg's `pred_template.c`.
///
/// Writes the carried value to groups of four top samples whose PU above is
/// not intra; an intra PU refreshes the carried value from `ptr[i + 3]`.
fn extend_right_cip(
    top: &mut [u16],
    start: i32,
    length: i32,
    mut a: u16,
    is_intra: &impl Fn(i32, i32) -> bool,
) {
    let mut i = start;
    while i < start + length {
        if i >= 0 && !is_intra(i, -1) {
            for k in 0..4 {
                if let Some(v) = top.get_mut((i + k) as usize) {
                    *v = a;
                }
            }
        } else if let Some(v) = top.get((i + 3) as usize) {
            a = *v;
        }
        i += 4;
    }
}

/// `EXTEND_DOWN_CIP` from FFmpeg's `pred_template.c`.
fn extend_down_cip(
    left: &mut [u16],
    start: i32,
    length: i32,
    mut a: u16,
    is_intra: &impl Fn(i32, i32) -> bool,
) {
    let mut i = start;
    while i < start + length {
        if i >= 0 && !is_intra(-1, i) {
            for k in 0..4 {
                if let Some(v) = left.get_mut((i + k) as usize) {
                    *v = a;
                }
            }
        } else if let Some(v) = left.get((i + 3) as usize) {
            a = *v;
        }
        i += 4;
    }
}

/// `EXTEND_UP_CIP` from FFmpeg's `pred_template.c`.
fn extend_up_cip(
    left: &mut [u16],
    corner: &mut u16,
    start: i32,
    length: i32,
    mut a: u16,
    is_intra: &impl Fn(i32, i32) -> bool,
) {
    let mut i = start;
    while i > start - length {
        if !is_intra(-1, i - 3) {
            for k in 0..4 {
                let idx = i - 3 + k;
                if idx >= 0 {
                    if let Some(v) = left.get_mut(idx as usize) {
                        *v = a;
                    }
                } else if idx == -1 {
                    // `AV_WN4P(&ptr[i - 3], a)` also covers `left[-1]`.
                    *corner = a;
                }
            }
        } else if let Some(v) = left.get((i - 3) as usize) {
            a = *v;
        }
        i -= 4;
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

    // Constrained intra prediction: FFmpeg's `pred_template.c:167-207` rechecks
    // each candidate at PU granularity. Every second PU along the neighbour is
    // tested and any intra PU makes the candidate available; testing every
    // sample and requiring *all* of them to be intra is too strict.
    if pps.constrained_intra_pred_flag {
        let pu = |v: i32| (v as u32 >> sps.log2_min_pu_size) as usize;
        let is_intra = |px_pu: usize, py_pu: usize| -> bool {
            picture
                .tab_mvf
                .get(px_pu, py_pu)
                .is_some_and(|f| f.pred_flag.is_intra())
        };
        let min_pu_size = 1i32 << sps.log2_min_pu_size;
        let on_pu_edge_x = x0 % min_pu_size == 0;
        let on_pu_edge_y = y0 % min_pu_size == 0;
        let size_luma_h = size as i32 * (1 << hshift);
        let size_luma_v = size as i32 * (1 << vshift);
        let mut size_pu_h = size_luma_h >> sps.log2_min_pu_size;
        let size_pu_v = size_luma_v >> sps.log2_min_pu_size;
        if size_pu_h == 0 {
            size_pu_h = 1;
        }
        let any_intra = |first: usize, fixed: usize, count: i32, vertical: bool| -> bool {
            let max = count.min(if vertical {
                sps.min_pu_height as i32 - fixed as i32
            } else {
                sps.min_pu_width as i32 - fixed as i32
            });
            (0..max).step_by(2).any(|i| {
                let idx = fixed + i as usize;
                if vertical {
                    is_intra(first, idx)
                } else {
                    is_intra(idx, first)
                }
            })
        };
        if cand_bottom_left && on_pu_edge_x {
            let x_left = pu(x0 - 1);
            let y_bottom = pu(y0 + size_luma_v);
            cand_bottom_left = any_intra(x_left, y_bottom, size_pu_v, true);
        }
        if cand_left && on_pu_edge_x {
            let x_left = pu(x0 - 1);
            let y_left = pu(y0);
            cand_left = any_intra(x_left, y_left, size_pu_v, true);
        }
        if cand_up_left {
            cand_up_left = is_intra(pu(x0 - 1), pu(y0 - 1));
        }
        if cand_up && on_pu_edge_y {
            let x_top = pu(x0);
            let y_top = pu(y0 - 1);
            cand_up = any_intra(y_top, x_top, size_pu_h, false);
        }
        if cand_up_right && on_pu_edge_y {
            let x_right = pu(x0 + size_luma_h);
            let y_top = pu(y0 - 1);
            cand_up_right = any_intra(y_top, x_right, size_pu_h, false);
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

    if pps.constrained_intra_pred_flag {
        // FFmpeg's `FUNC(intra_pred)` replaces samples belonging to non-intra
        // PUs before the "Infer the unavailable samples" block. Operate on the
        // combined top/left layout used by `pred_template.c`, then copy the
        // substituted samples back; the existing DSP path still performs the
        // inference step.
        let default = 1u16 << (picture.bit_depth - 1);
        let mut top = vec![default; 2 * size];
        let mut left = vec![default; 2 * size];
        if cand_up {
            top[..size].copy_from_slice(&top_buf);
        }
        if cand_up_right && !top_right_buf.is_empty() {
            let n = top_right_buf.len().min(size);
            top[size..size + n].copy_from_slice(&top_right_buf[..n]);
            if n > 0 && n < size {
                let v = top[size + n - 1];
                for k in n..size {
                    top[size + k] = v;
                }
            }
        }
        if cand_left {
            left[..size].copy_from_slice(&left_buf);
        }
        if cand_bottom_left && !bottom_left_buf.is_empty() {
            let n = bottom_left_buf.len().min(size);
            left[size..size + n].copy_from_slice(&bottom_left_buf[..n]);
            if n > 0 && n < size {
                let v = left[size + n - 1];
                for k in n..size {
                    left[size + k] = v;
                }
            }
        }

        let mut top_corner = if cand_up_left {
            picture.sample(c_idx, x as usize - 1, y as usize - 1)
        } else {
            default
        };
        let mut left_corner;
        let size_i = size as i32;
        let is_intra = |sx: i32, sy: i32| -> bool {
            let lx = x0 + sx * (1 << hshift);
            let ly = y0 + sy * (1 << vshift);
            let px = (lx as u32 >> sps.log2_min_pu_size) as usize;
            let py = (ly as u32 >> sps.log2_min_pu_size) as usize;
            picture
                .tab_mvf
                .get(px, py)
                .is_some_and(|f| f.pred_flag.is_intra())
        };

        if cand_bottom_left || cand_left || cand_up_left || cand_up || cand_up_right {
            let mut size_max_x = if x0 + ((2 * size_i) << hshift) < width {
                2 * size
            } else {
                ((width - x0) >> hshift) as usize
            };
            let mut size_max_y = if y0 + ((2 * size_i) << vshift) < height {
                2 * size
            } else {
                ((height - y0) >> vshift) as usize
            };
            let mut j = size_i + i32::from(cand_bottom_left) * bottom_left_size as i32 - 1;
            if !cand_up_right {
                size_max_x = if x0 + (size_i << hshift) < width {
                    size
                } else {
                    ((width - x0) >> hshift) as usize
                };
            }
            if !cand_bottom_left {
                size_max_y = if y0 + (size_i << vshift) < height {
                    size
                } else {
                    ((height - y0) >> vshift) as usize
                };
            }
            if cand_bottom_left || cand_left || cand_up_left {
                while j > -1 && !is_intra(-1, j) {
                    j -= 1;
                }
                if !is_intra(-1, j) {
                    j = 0;
                    while (j as usize) < size_max_x && !is_intra(j, -1) {
                        j += 1;
                    }
                    extend_left_cip(&mut top, &mut top_corner, j, j + 1, &is_intra);
                }
            } else {
                j = 0;
                while (j as usize) < size_max_x && !is_intra(j, -1) {
                    j += 1;
                }
                if j > 0 {
                    extend_left_cip(&mut top, &mut top_corner, j, j, &is_intra);
                    top_corner = top[0];
                }
            }
            // FFmpeg's unconditional `left[-1] = top[-1]` after both branches.
            left_corner = top_corner;
            if cand_bottom_left || cand_left {
                extend_down_cip(&mut left, 0, size_max_y as i32, left_corner, &is_intra);
            }
            if !cand_left {
                for v in left[..size].iter_mut() {
                    *v = left_corner;
                }
            }
            if !cand_bottom_left {
                let v = left[size - 1];
                for k in size..2 * size {
                    left[k] = v;
                }
            }
            if x0 != 0 && y0 != 0 {
                let a = left[size_max_y - 1];
                extend_up_cip(
                    &mut left,
                    &mut left_corner,
                    size_max_y as i32 - 1,
                    size_max_y as i32,
                    a,
                    &is_intra,
                );
                if !is_intra(-1, -1) {
                    left_corner = left[0];
                }
            } else if x0 == 0 {
                for v in left[..size_max_y].iter_mut() {
                    *v = 0;
                }
            } else {
                let a = left[size_max_y - 1];
                extend_up_cip(
                    &mut left,
                    &mut left_corner,
                    size_max_y as i32 - 1,
                    size_max_y as i32,
                    a,
                    &is_intra,
                );
            }
            top_corner = left_corner;
            if y0 != 0 {
                extend_right_cip(&mut top, 0, size_max_x as i32, top_corner, &is_intra);
            }
            if cand_bottom_left && bottom_left_size < size {
                let v = left[size + bottom_left_size - 1];
                for k in bottom_left_size..size {
                    left[size + k] = v;
                }
            }
            if cand_up_right && top_right_size < size {
                let v = top[size + top_right_size - 1];
                for k in top_right_size..size {
                    top[size + k] = v;
                }
            }
        }

        top_buf.copy_from_slice(&top[..size]);
        left_buf.copy_from_slice(&left[..size]);
        let n = top_right_buf.len();
        if n > 0 {
            top_right_buf.copy_from_slice(&top[size..size + n]);
        }
        let n = bottom_left_buf.len();
        if n > 0 {
            bottom_left_buf.copy_from_slice(&left[size..size + n]);
        }
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

    #[test]
    fn chroma_modes_444_decode_one_per_pu() {
        let mut chroma_modes = [0u8; 4];
        let mut modes_c = [0u8; 4];
        let mut decoded = [0u8; 4];
        let mut i = 0;
        derive_chroma_modes(
            3,
            2,
            &[1, 1, 1, 1],
            || {
                decoded[i] = [3, 1, 4, 2][i];
                let v = decoded[i];
                i += 1;
                v
            },
            &mut chroma_modes,
            &mut modes_c,
        );
        assert_eq!(i, 4, "4:4:4 NxN must decode side*side chroma modes");
        assert_eq!(chroma_modes, [3, 1, 4, 2]);
        // table = { 0, 26, 10, 1 }; mode 0 matches luma mode 1 -> 34,
        // mode 4 copies the luma mode (1).
        assert_eq!(modes_c, [34, 26, 1, 10]);
    }

    #[test]
    fn chroma_modes_422_uses_tab_mode_idx_and_420_decodes_once() {
        let mut chroma_modes = [0u8; 4];
        let mut modes_c = [9u8; 4];
        let mut calls = 0;
        derive_chroma_modes(
            2,
            2,
            &[1, 1, 1, 1],
            || {
                calls += 1;
                2
            },
            &mut chroma_modes,
            &mut modes_c,
        );
        assert_eq!(calls, 1);
        assert_eq!(chroma_modes[0], 2);
        assert_eq!(modes_c[0], TAB_MODE_IDX[10]);
        assert_eq!(modes_c[1], 9, "unused PUs keep their previous mode");

        let mut chroma_modes = [7u8; 4];
        let mut modes_c = [8u8; 4];
        let mut calls = 0;
        derive_chroma_modes(
            1,
            2,
            &[26, 1, 1, 1],
            || {
                calls += 1;
                1
            },
            &mut chroma_modes,
            &mut modes_c,
        );
        assert_eq!(calls, 1);
        assert_eq!(chroma_modes[0], 1);
        assert_eq!(
            modes_c[0], 34,
            "same as luma maps to intra_pred_mode_c = 34"
        );
        assert_eq!(modes_c[2], 8);
    }

    #[test]
    fn chroma_modes_400_decodes_nothing() {
        let mut chroma_modes = [5u8; 4];
        let mut modes_c = [6u8; 4];
        derive_chroma_modes(
            0,
            1,
            &[0; 4],
            || panic!("monochrome must not decode intra_chroma_pred_mode"),
            &mut chroma_modes,
            &mut modes_c,
        );
        assert_eq!(chroma_modes, [5; 4]);
        assert_eq!(modes_c, [6; 4]);
    }
}
