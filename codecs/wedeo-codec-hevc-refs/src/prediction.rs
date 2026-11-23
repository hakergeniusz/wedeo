//! Motion-vector prediction: merge candidates (spatial, temporal, combined,
//! zero) and AMVP candidates.
//!
//! FFmpeg reference: `libavcodec/hevc/mvs.c`, sections 8.5.3.1.1, 8.5.3.1.2,
//! 8.5.3.1.7, 8.5.3.1.8 (`ff_hevc_luma_mv_merge_mode`,
//! `derive_spatial_merge_candidates`, `temporal_luma_motion_vector`,
//! `ff_hevc_luma_mv_mvp_mode`, `mv_mp_mode_mx{, _lt}`, `dist_scale`).

use wedeo_codec_hevc_ps::{L0, L1, SliceType};

use crate::context::{PartMode, RefGeometry};
use crate::frame::{HevcFrame, MvFieldTable};
use crate::mv::{
    Mv, MvField, PredFlag, compare_mv_ref_idx, derive_temporal_colocated_mvs, mv_scale,
};
use crate::neighbors::{NeighbourAvailability, is_diff_mer, z_scan_block_avail};
use crate::rpl::RefPicList;

/// FFmpeg's `MRG_MAX_NUM_CANDS`.
pub const MRG_MAX_NUM_CANDS: usize = 5;

/// FFmpeg's `l0_l1_cand_idx[12][2]`.
const L0_L1_CAND_IDX: [[usize; 2]; 12] = [
    [0, 1],
    [1, 0],
    [0, 2],
    [2, 0],
    [1, 2],
    [2, 1],
    [0, 3],
    [3, 0],
    [1, 3],
    [3, 1],
    [2, 3],
    [3, 2],
];

/// Inputs needed to derive a collocated temporal MV (8.5.3.1.7).
pub struct TemporalCtx<'a> {
    /// Current picture POC (`s->poc`).
    pub poc: i32,
    /// Collocated reference frame (`lc->collocated_ref`).
    pub col_frame: &'a HevcFrame,
    /// Current picture's active reference lists (`lc->rpl`).
    pub cur_rpl: &'a [RefPicList; 2],
    /// `sh->collocated_list` (`L0` or `L1`).
    pub collocated_list: usize,
}

/// FFmpeg's `temporal_luma_motion_vector()`.
///
/// Tries the bottom-right collocated PU first, then the center PU. Returns
/// `(availableFlagLXCol, mvLXCol)`.
// Mirrors FFmpeg's eight-argument `temporal_luma_motion_vector()`.
#[allow(clippy::too_many_arguments)]
pub fn temporal_luma_motion_vector(
    geo: &RefGeometry,
    temporal: &TemporalCtx<'_>,
    x0: i32,
    y0: i32,
    n_pb_w: i32,
    n_pb_h: i32,
    ref_idx_lx: usize,
    x: usize,
) -> (bool, Mv) {
    let tab_mvf = &temporal.col_frame.tab_mvf;
    if tab_mvf.data.is_empty() {
        return (false, Mv::ZERO);
    }
    let col_poc = temporal.col_frame.poc;

    // bottom right collocated motion vector
    let mut px = x0 + n_pb_w;
    let mut py = y0 + n_pb_h;
    if (y0 >> geo.log2_ctb_size) == (py >> geo.log2_ctb_size)
        && py < geo.height as i32
        && px < geo.width as i32
    {
        px &= !15;
        py &= !15;
        let pu_x = (px >> geo.log2_min_pu_size) as usize;
        let pu_y = (py >> geo.log2_min_pu_size) as usize;
        if let (Some(temp_col), Some(col_rpl)) = (
            tab_mvf.get(pu_x, pu_y),
            temporal.col_frame.get_ref_list(
                px,
                py,
                geo.log2_ctb_size,
                geo.ctb_width,
                &geo.ctb_addr_rs_to_ts,
            ),
        ) {
            let (avail, mv) = derive_temporal_colocated_mvs(
                *temp_col,
                ref_idx_lx,
                x,
                temporal.poc,
                col_poc,
                temporal.cur_rpl,
                col_rpl,
                temporal.collocated_list,
            );
            if avail {
                return (true, mv);
            }
        }
    }

    // derive center collocated motion vector
    px = x0 + (n_pb_w >> 1);
    py = y0 + (n_pb_h >> 1);
    px &= !15;
    py &= !15;
    let pu_x = (px >> geo.log2_min_pu_size) as usize;
    let pu_y = (py >> geo.log2_min_pu_size) as usize;
    if let (Some(temp_col), Some(col_rpl)) = (
        tab_mvf.get(pu_x, pu_y),
        temporal.col_frame.get_ref_list(
            px,
            py,
            geo.log2_ctb_size,
            geo.ctb_width,
            &geo.ctb_addr_rs_to_ts,
        ),
    ) {
        return derive_temporal_colocated_mvs(
            *temp_col,
            ref_idx_lx,
            x,
            temporal.poc,
            col_poc,
            temporal.cur_rpl,
            col_rpl,
            temporal.collocated_list,
        );
    }

    (false, Mv::ZERO)
}

/// Parameters for [`derive_spatial_merge_candidates`].
pub struct SpatialMergeParams<'a> {
    pub geo: &'a RefGeometry,
    pub na: NeighbourAvailability,
    pub cur_mvf: &'a MvFieldTable,
    pub cur_rpl: &'a [RefPicList; 2],
    pub sh: &'a wedeo_codec_hevc_ps::SliceHeader,
    pub part_mode: PartMode,
    pub single_mcl_flag: bool,
    pub part_idx: usize,
    pub merge_idx: usize,
    pub x0: i32,
    pub y0: i32,
    pub n_pb_w: i32,
    pub n_pb_h: i32,
    pub log2_cb_size: u32,
    pub temporal: Option<&'a TemporalCtx<'a>>,
}

/// FFmpeg's `derive_spatial_merge_candidates()` (8.5.3.1.2).
///
/// Returns the merge candidate list. Entries beyond the first unavailable one
/// are zero; callers use `merge_idx` to select one.
pub fn derive_spatial_merge_candidates(
    params: &SpatialMergeParams<'_>,
) -> [MvField; MRG_MAX_NUM_CANDS] {
    let geo = params.geo;
    let mut list = [MvField::INTRA; MRG_MAX_NUM_CANDS];
    let mut nb = 0usize;

    // FFmpeg returns early as soon as the requested merge_idx is filled.
    macro_rules! push {
        ($field:expr) => {{
            if nb >= list.len() {
                return list;
            }
            list[nb] = $field;
            if params.merge_idx == nb {
                return list;
            }
            nb += 1;
        }};
    }

    let pu = |v: i32| -> usize { (v >> geo.log2_min_pu_size) as usize };
    let field_at =
        |x: i32, y: i32| -> Option<MvField> { params.cur_mvf.get(pu(x), pu(y)).copied() };
    // AVAILABLE(cand, v): (cand && !(TAB_MVF_PU(v).pred_flag == PF_INTRA))
    let available = |cand: bool, x: i32, y: i32| -> bool {
        cand && field_at(x, y).is_some_and(|f| !f.pred_flag.is_intra())
    };
    let compare = |a: (i32, i32), b: (i32, i32)| -> bool {
        match (field_at(a.0, a.1), field_at(b.0, b.1)) {
            (Some(fa), Some(fb)) => compare_mv_ref_idx(fa, fb),
            _ => false,
        }
    };
    let z_avail = |x: i32, y: i32| -> bool { z_scan_block_avail(geo, params.x0, params.y0, x, y) };
    let diff_mer = |x: i32, y: i32| -> bool { is_diff_mer(geo, x, y, params.x0, params.y0) };

    let x_a1 = params.x0 - 1;
    let y_a1 = params.y0 + params.n_pb_h - 1;
    let x_b1 = params.x0 + params.n_pb_w - 1;
    let y_b1 = params.y0 - 1;
    let x_b0 = params.x0 + params.n_pb_w;
    let y_b0 = params.y0 - 1;
    let x_a0 = params.x0 - 1;
    let y_a0 = params.y0 + params.n_pb_h;
    let x_b2 = params.x0 - 1;
    let y_b2 = params.y0 - 1;

    let nb_refs = if params.sh.slice_type == SliceType::P {
        params.sh.nb_refs[L0] as usize
    } else {
        (params.sh.nb_refs[L0] as usize).min(params.sh.nb_refs[L1] as usize)
    };

    // left spatial merge candidate (A1)
    let is_available_a1 = if (!params.single_mcl_flag
        && params.part_idx == 1
        && matches!(
            params.part_mode,
            PartMode::Nx2N | PartMode::NLx2N | PartMode::NRx2N
        ))
        || diff_mer(x_a1, y_a1)
    {
        false
    } else {
        let ok = available(params.na.cand_left, x_a1, y_a1);
        if ok {
            push!(field_at(x_a1, y_a1).unwrap_or(MvField::INTRA));
        }
        ok
    };

    // above spatial merge candidate (B1)
    let is_available_b1 = if (!params.single_mcl_flag
        && params.part_idx == 1
        && matches!(
            params.part_mode,
            PartMode::N2xN | PartMode::N2xnU | PartMode::N2xnD
        ))
        || diff_mer(x_b1, y_b1)
    {
        false
    } else {
        let ok = available(params.na.cand_up, x_b1, y_b1);
        if ok && !(is_available_a1 && compare((x_b1, y_b1), (x_a1, y_a1))) {
            push!(field_at(x_b1, y_b1).unwrap_or(MvField::INTRA));
        }
        ok
    };

    // above right spatial merge candidate (B0)
    let is_available_b0 = x_b0 < geo.width as i32
        && available(params.na.cand_up_right_sap, x_b0, y_b0)
        && z_avail(x_b0, y_b0)
        && !diff_mer(x_b0, y_b0);
    if is_available_b0 && !(is_available_b1 && compare((x_b0, y_b0), (x_b1, y_b1))) {
        push!(field_at(x_b0, y_b0).unwrap_or(MvField::INTRA));
    }

    // left bottom spatial merge candidate (A0)
    let is_available_a0 = available(params.na.cand_bottom_left, x_a0, y_a0)
        && y_a0 < geo.height as i32
        && z_avail(x_a0, y_a0)
        && !diff_mer(x_a0, y_a0);
    if is_available_a0 && !(is_available_a1 && compare((x_a0, y_a0), (x_a1, y_a1))) {
        push!(field_at(x_a0, y_a0).unwrap_or(MvField::INTRA));
    }

    // above left spatial merge candidate (B2)
    let is_available_b2 = available(params.na.cand_up_left, x_b2, y_b2) && !diff_mer(x_b2, y_b2);
    if is_available_b2
        && !(is_available_a1 && compare((x_b2, y_b2), (x_a1, y_a1)))
        && !(is_available_b1 && compare((x_b2, y_b2), (x_b1, y_b1)))
        && nb != 4
    {
        push!(field_at(x_b2, y_b2).unwrap_or(MvField::INTRA));
    }

    // temporal motion vector candidate
    if params.sh.slice_temporal_mvp_enabled_flag && nb < params.sh.max_num_merge_cand as usize {
        let mut mv_l0_col = Mv::ZERO;
        let mut mv_l1_col = Mv::ZERO;
        let mut available_l0 = false;
        let mut available_l1 = false;
        if let Some(temporal) = params.temporal {
            let (ok, mv) = temporal_luma_motion_vector(
                geo,
                temporal,
                params.x0,
                params.y0,
                params.n_pb_w,
                params.n_pb_h,
                0,
                L0,
            );
            available_l0 = ok;
            mv_l0_col = mv;
            if params.sh.slice_type == SliceType::B {
                let (ok, mv) = temporal_luma_motion_vector(
                    geo,
                    temporal,
                    params.x0,
                    params.y0,
                    params.n_pb_w,
                    params.n_pb_h,
                    0,
                    L1,
                );
                available_l1 = ok;
                mv_l1_col = mv;
            }
        }
        if available_l0 || available_l1 {
            let mut field = MvField::INTRA;
            field.pred_flag = PredFlag(available_l0 as u8 + ((available_l1 as u8) << 1));
            field.ref_idx = [0; 2];
            field.mv = [mv_l0_col, mv_l1_col];
            push!(field);
        }
    }

    let nb_orig = nb;

    // combined bi-predictive merge candidates (applies for B slices)
    if params.sh.slice_type == SliceType::B
        && nb_orig > 1
        && nb_orig < params.sh.max_num_merge_cand as usize
    {
        let mut comb_idx = 0usize;
        while nb < params.sh.max_num_merge_cand as usize && comb_idx < nb_orig * (nb_orig - 1) {
            let l0_cand_idx = L0_L1_CAND_IDX[comb_idx][0];
            let l1_cand_idx = L0_L1_CAND_IDX[comb_idx][1];
            let l0_cand = list[l0_cand_idx];
            let l1_cand = list[l1_cand_idx];
            if l0_cand.pred_flag.has_l0()
                && l1_cand.pred_flag.has_l1()
                && l0_cand.ref_idx[0] >= 0
                && l1_cand.ref_idx[1] >= 0
            {
                let p0 = l0_cand.ref_idx[0] as usize;
                let p1 = l1_cand.ref_idx[1] as usize;
                let different = params.cur_rpl[L0]
                    .list
                    .get(p0)
                    .zip(params.cur_rpl[L1].list.get(p1))
                    .is_some_and(|(a, b)| a != b)
                    || l0_cand.mv[0] != l1_cand.mv[1];
                if different {
                    let mut field = MvField::INTRA;
                    field.ref_idx = [l0_cand.ref_idx[0], l1_cand.ref_idx[1]];
                    field.pred_flag = PredFlag::BI;
                    field.mv = [l0_cand.mv[0], l1_cand.mv[1]];
                    push!(field);
                }
            }
            comb_idx += 1;
        }
    }

    // append Zero motion vector candidates
    let mut zero_idx = 0usize;
    while nb < params.sh.max_num_merge_cand as usize {
        let mut field = MvField::INTRA;
        // PF_L0 + ((slice_type == HEVC_SLICE_B) << 1)
        field.pred_flag = PredFlag(if params.sh.slice_type == SliceType::B {
            3
        } else {
            1
        });
        field.mv = [Mv::ZERO; 2];
        let r = if zero_idx < nb_refs { zero_idx } else { 0 };
        field.ref_idx = [r as i8, r as i8];
        push!(field);
        zero_idx += 1;
    }

    list
}

/// FFmpeg's `ff_hevc_luma_mv_merge_mode()` (8.5.3.1.1).
// Keeps a one-to-one argument mapping with FFmpeg's eight-parameter helper
// (plus the explicit neighbour/geometry/temporal context).
#[allow(clippy::too_many_arguments)]
pub fn luma_mv_merge_mode(
    geo: &RefGeometry,
    na: NeighbourAvailability,
    cur_mvf: &MvFieldTable,
    cur_rpl: &[RefPicList; 2],
    sh: &wedeo_codec_hevc_ps::SliceHeader,
    part_mode: PartMode,
    part_idx: usize,
    mut x0: i32,
    mut y0: i32,
    mut n_pb_w: i32,
    mut n_pb_h: i32,
    log2_cb_size: u32,
    cu_x: i32,
    cu_y: i32,
    merge_idx: usize,
    temporal: Option<&TemporalCtx<'_>>,
) -> MvField {
    let mut single_mcl_flag = false;
    let n_cs = 1i32 << log2_cb_size;
    let n_pb_w2 = n_pb_w;
    let n_pb_h2 = n_pb_h;

    if geo.log2_parallel_merge_level > 2 && n_cs == 8 {
        single_mcl_flag = true;
        x0 = cu_x;
        y0 = cu_y;
        n_pb_w = n_cs;
        n_pb_h = n_cs;
    }

    let params = SpatialMergeParams {
        geo,
        na,
        cur_mvf,
        cur_rpl,
        sh,
        part_mode,
        single_mcl_flag,
        part_idx: if single_mcl_flag { 0 } else { part_idx },
        merge_idx,
        x0,
        y0,
        n_pb_w,
        n_pb_h,
        log2_cb_size,
        temporal,
    };
    let mut cand_list = derive_spatial_merge_candidates(&params);

    if cand_list[merge_idx].pred_flag == PredFlag::BI && (n_pb_w2 + n_pb_h2) == 12 {
        cand_list[merge_idx].pred_flag = PredFlag::L0;
    }

    cand_list[merge_idx]
}

/// Internal lookup used by AMVP (FFmpeg's `mv_mp_mode_mx{,_lt}`).
struct MvpLookup<'a> {
    cur_mvf: &'a MvFieldTable,
    cur_rpl: &'a [RefPicList; 2],
    poc: i32,
}

impl MvpLookup<'_> {
    /// FFmpeg's `mv_mp_mode_mx()`: exact reference match.
    fn mx(
        &self,
        x: usize,
        y: usize,
        pred_flag_index: usize,
        ref_idx_curr: usize,
        ref_idx: usize,
    ) -> Option<Mv> {
        let field = self.cur_mvf.get(x, y)?;
        if !(field.pred_flag.bits() & (1 << pred_flag_index) != 0) {
            return None;
        }
        let col_idx = field.ref_idx[pred_flag_index];
        if col_idx < 0 {
            return None;
        }
        let col_poc = *self
            .cur_rpl
            .get(pred_flag_index)?
            .list
            .get(col_idx as usize)?;
        let cur_poc = *self.cur_rpl.get(ref_idx_curr)?.list.get(ref_idx)?;
        if col_poc != cur_poc {
            return None;
        }
        Some(field.mv[pred_flag_index])
    }

    /// FFmpeg's `dist_scale()`.
    fn dist_scale(
        &self,
        mv: Mv,
        x: usize,
        y: usize,
        elist: usize,
        ref_idx_curr: usize,
        ref_idx: usize,
    ) -> Mv {
        let Some(field) = self.cur_mvf.get(x, y) else {
            return mv;
        };
        let col_idx = field.ref_idx[elist];
        if col_idx < 0 {
            return mv;
        }
        let (Some(ref_pic_elist), Some(ref_pic_curr)) = (
            self.cur_rpl
                .get(elist)
                .and_then(|r| r.list.get(col_idx as usize))
                .copied(),
            self.cur_rpl
                .get(ref_idx_curr)
                .and_then(|r| r.list.get(ref_idx))
                .copied(),
        ) else {
            return mv;
        };
        if ref_pic_elist == ref_pic_curr {
            return mv;
        }
        let mut poc_diff = self.poc.wrapping_sub(ref_pic_elist);
        if poc_diff == 0 {
            poc_diff = 1;
        }
        mv_scale(mv, poc_diff, self.poc.wrapping_sub(ref_pic_curr))
    }

    /// FFmpeg's `mv_mp_mode_mx_lt()`: long-term-aware candidate with distance
    /// scaling when the reference differs.
    fn mx_lt(
        &self,
        x: usize,
        y: usize,
        pred_flag_index: usize,
        ref_idx_curr: usize,
        ref_idx: usize,
    ) -> Option<Mv> {
        let field = self.cur_mvf.get(x, y)?;
        if field.pred_flag.bits() & (1 << pred_flag_index) == 0 {
            return None;
        }
        let col_idx = field.ref_idx[pred_flag_index];
        if col_idx < 0 {
            return None;
        }
        let curr_is_lt = *self.cur_rpl.get(ref_idx_curr)?.is_long_term.get(ref_idx)?;
        let col_is_lt = *self
            .cur_rpl
            .get(pred_flag_index)?
            .is_long_term
            .get(col_idx as usize)?;
        if col_is_lt != curr_is_lt {
            return None;
        }
        let mv = field.mv[pred_flag_index];
        Some(if curr_is_lt {
            mv
        } else {
            self.dist_scale(mv, x, y, pred_flag_index, ref_idx_curr, ref_idx)
        })
    }
}

/// Try the non-long-term AMVP candidates in order (FFmpeg's `MP_MX` calls).
fn find_mx(
    lookup: &MvpLookup<'_>,
    geo: &RefGeometry,
    candidates: &[(i32, i32, bool)],
    pred_l0: usize,
    pred_l1: usize,
    lx: usize,
    ref_idx: usize,
) -> Option<Mv> {
    for &(x, y, avail) in candidates {
        if !avail {
            continue;
        }
        let px = (x >> geo.log2_min_pu_size) as usize;
        let py = (y >> geo.log2_min_pu_size) as usize;
        if let Some(v) = lookup.mx(px, py, pred_l0, lx, ref_idx) {
            return Some(v);
        }
        if let Some(v) = lookup.mx(px, py, pred_l1, lx, ref_idx) {
            return Some(v);
        }
    }
    None
}

/// Try the long-term AMVP candidates in order (FFmpeg's `MP_MX_LT` calls).
fn find_mx_lt(
    lookup: &MvpLookup<'_>,
    geo: &RefGeometry,
    candidates: &[(i32, i32, bool)],
    pred_l0: usize,
    pred_l1: usize,
    lx: usize,
    ref_idx: usize,
) -> Option<Mv> {
    for &(x, y, avail) in candidates {
        if !avail {
            continue;
        }
        let px = (x >> geo.log2_min_pu_size) as usize;
        let py = (y >> geo.log2_min_pu_size) as usize;
        if let Some(v) = lookup.mx_lt(px, py, pred_l0, lx, ref_idx) {
            return Some(v);
        }
        if let Some(v) = lookup.mx_lt(px, py, pred_l1, lx, ref_idx) {
            return Some(v);
        }
    }
    None
}

/// Parameters for [`luma_mv_mvp_mode`].
pub struct MvpParams<'a> {
    pub geo: &'a RefGeometry,
    pub na: NeighbourAvailability,
    pub cur_mvf: &'a MvFieldTable,
    pub cur_rpl: &'a [RefPicList; 2],
    pub poc: i32,
    pub temporal: Option<&'a TemporalCtx<'a>>,
}

/// FFmpeg's `ff_hevc_luma_mv_mvp_mode()` (AMVP).
///
/// Writes the selected predictor into `mv.mv[lx]`; the reference index must
/// already be set in `mv.ref_idx[lx]`.
// Mirrors FFmpeg's nine-parameter `ff_hevc_luma_mv_mvp_mode()` signature.
#[allow(clippy::too_many_arguments)]
pub fn luma_mv_mvp_mode(
    params: &MvpParams<'_>,
    x0: i32,
    y0: i32,
    n_pb_w: i32,
    n_pb_h: i32,
    mv: &mut MvField,
    mvp_lx_flag: usize,
    lx: usize,
) {
    let geo = params.geo;
    let lookup = MvpLookup {
        cur_mvf: params.cur_mvf,
        cur_rpl: params.cur_rpl,
        poc: params.poc,
    };
    let pu = |v: i32| -> usize { (v >> geo.log2_min_pu_size) as usize };
    let field_at =
        |x: i32, y: i32| -> Option<MvField> { params.cur_mvf.get(pu(x), pu(y)).copied() };
    let available = |cand: bool, x: i32, y: i32| -> bool {
        cand && field_at(x, y).is_some_and(|f| !f.pred_flag.is_intra())
    };
    let ref_idx = usize::try_from(mv.ref_idx[lx]).unwrap_or(0);
    let pred_flag_index_l0 = lx;
    let pred_flag_index_l1 = 1 - lx;

    // left bottom spatial candidate (A0)
    let x_a0 = x0 - 1;
    let y_a0 = y0 + n_pb_h;
    let is_available_a0 = available(params.na.cand_bottom_left, x_a0, y_a0)
        && y_a0 < geo.height as i32
        && z_scan_block_avail(geo, x0, y0, x_a0, y_a0);

    // left spatial merge candidate (A1)
    let x_a1 = x0 - 1;
    let y_a1 = y0 + n_pb_h - 1;
    let is_available_a1 = available(params.na.cand_left, x_a1, y_a1);

    // isScaledFlag_L0
    let is_scaled_flag_l0 = is_available_a0 || is_available_a1;

    let a_candidates = [(x_a0, y_a0, is_available_a0), (x_a1, y_a1, is_available_a1)];
    let mut mx_a = find_mx(
        &lookup,
        geo,
        &a_candidates,
        pred_flag_index_l0,
        pred_flag_index_l1,
        lx,
        ref_idx,
    );
    if mx_a.is_none() {
        mx_a = find_mx_lt(
            &lookup,
            geo,
            &a_candidates,
            pred_flag_index_l0,
            pred_flag_index_l1,
            lx,
            ref_idx,
        );
    }
    // availableFlagLXA0 = (mxA found)
    let mut available_flag_lx_a0 = mx_a.is_some();

    // B candidates
    let x_b0 = x0 + n_pb_w;
    let y_b0 = y0 - 1;
    let is_available_b0 = x_b0 < geo.width as i32
        && available(params.na.cand_up_right_sap, x_b0, y_b0)
        && z_scan_block_avail(geo, x0, y0, x_b0, y_b0);
    let x_b1 = x0 + n_pb_w - 1;
    let y_b1 = y0 - 1;
    let is_available_b1 = available(params.na.cand_up, x_b1, y_b1);
    let x_b2 = x0 - 1;
    let y_b2 = y0 - 1;
    let is_available_b2 = available(params.na.cand_up_left, x_b2, y_b2);

    let b_candidates = [
        (x_b0, y_b0, is_available_b0),
        (x_b1, y_b1, is_available_b1),
        (x_b2, y_b2, is_available_b2),
    ];
    let mut mx_b = find_mx(
        &lookup,
        geo,
        &b_candidates,
        pred_flag_index_l0,
        pred_flag_index_l1,
        lx,
        ref_idx,
    );
    let mut available_flag_lx_b0 = mx_b.is_some();

    if !is_scaled_flag_l0 {
        if available_flag_lx_b0 {
            available_flag_lx_a0 = true;
            mx_a = mx_b;
        }

        // XB0 and L1 (long-term)
        mx_b = find_mx_lt(
            &lookup,
            geo,
            &b_candidates,
            pred_flag_index_l0,
            pred_flag_index_l1,
            lx,
            ref_idx,
        );
        available_flag_lx_b0 = mx_b.is_some();
    }

    let mut mvp_cands = [Mv::ZERO; 2];
    let mut num = 0usize;
    if available_flag_lx_a0 {
        mvp_cands[num] = mx_a.unwrap_or(Mv::ZERO);
        num += 1;
    }
    if available_flag_lx_b0 && (!available_flag_lx_a0 || mx_a != mx_b) && num < 2 {
        mvp_cands[num] = mx_b.unwrap_or(Mv::ZERO);
        num += 1;
    }

    // temporal motion vector prediction candidate
    if num < 2
        && mvp_lx_flag == num
        && let Some(temporal) = params.temporal
    {
        let (available_col, mv_col) =
            temporal_luma_motion_vector(geo, temporal, x0, y0, n_pb_w, n_pb_h, ref_idx, lx);
        if available_col && num < 2 {
            mvp_cands[num] = mv_col;
        }
    }

    mv.mv[lx] = mvp_cands.get(mvp_lx_flag).copied().unwrap_or(Mv::ZERO);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PartMode;
    use crate::neighbors::NeighbourAvailability;
    use wedeo_codec_hevc_ps::{SliceHeader, SliceType};

    fn geometry() -> RefGeometry {
        RefGeometry {
            width: 64,
            height: 64,
            log2_ctb_size: 6,
            log2_min_pu_size: 2,
            log2_min_tb_size: 2,
            ctb_width: 1,
            tb_mask: 15,
            // A1 (x=15) and the current block (x=16) fall in different MERs.
            log2_parallel_merge_level: 2,
            ctb_addr_rs_to_ts: vec![0],
            min_tb_addr_zs_tab: vec![0; 17 * 17],
            min_tb_addr_zs_offset: 17,
        }
    }

    fn rpl_with_pocs(pocs: &[i32]) -> [RefPicList; 2] {
        let mut r = [RefPicList::default(), RefPicList::default()];
        for (i, &poc) in pocs.iter().enumerate() {
            r[0].list[i] = poc;
            r[0].nb_refs += 1;
        }
        r
    }

    #[test]
    fn spatial_merge_collects_a1_b1_then_zero() {
        let geo = geometry();
        let mut tab = MvFieldTable::new(16, 16);
        // A1 at pixel (x0-1, y0+nPbH-1) = (15, 47) -> PU (3, 11).
        *tab.get_mut(3, 11).unwrap() = MvField {
            mv: [Mv { x: 4, y: 8 }, Mv::ZERO],
            ref_idx: [0, 0],
            pred_flag: PredFlag::L0,
        };
        // B1 at pixel (x0+nPbW-1, y0-1) = (31, 31) -> PU (7, 7).
        *tab.get_mut(7, 7).unwrap() = MvField {
            mv: [Mv { x: 16, y: 32 }, Mv::ZERO],
            ref_idx: [0, 0],
            pred_flag: PredFlag::L0,
        };
        let rpl = rpl_with_pocs(&[8]);
        let sh = SliceHeader {
            slice_type: SliceType::P,
            nb_refs: [1, 0],
            max_num_merge_cand: 5,
            ..Default::default()
        };
        let na = NeighbourAvailability {
            cand_left: true,
            cand_up: true,
            ..Default::default()
        };
        let params = SpatialMergeParams {
            geo: &geo,
            na,
            cur_mvf: &tab,
            cur_rpl: &rpl,
            sh: &sh,
            part_mode: PartMode::N2Nx2N,
            single_mcl_flag: false,
            part_idx: 0,
            // Use an index past the spatial candidates so the full candidate
            // list is built (a smaller index returns early).
            merge_idx: 5,
            x0: 16,
            y0: 32,
            n_pb_w: 16,
            n_pb_h: 16,
            log2_cb_size: 4,
            temporal: None,
        };
        let list = derive_spatial_merge_candidates(&params);
        assert_eq!(list[0].mv[0], Mv { x: 4, y: 8 });
        assert_eq!(list[1].mv[0], Mv { x: 16, y: 32 });
        assert_eq!(list[2].pred_flag, PredFlag::L0); // zero candidate
        assert_eq!(list[2].mv[0], Mv::ZERO);
    }

    #[test]
    fn amvp_picks_left_candidate() {
        let geo = geometry();
        let mut tab = MvFieldTable::new(16, 16);
        // A1 at pixel (x0-1, y0+nPbH-1) = (15, 47) -> PU (3, 11), POC 8.
        *tab.get_mut(3, 11).unwrap() = MvField {
            mv: [Mv { x: 11, y: -7 }, Mv::ZERO],
            ref_idx: [0, 0],
            pred_flag: PredFlag::L0,
        };
        let rpl = rpl_with_pocs(&[8]);
        let na = NeighbourAvailability {
            cand_left: true,
            ..Default::default()
        };
        let params = MvpParams {
            geo: &geo,
            na,
            cur_mvf: &tab,
            cur_rpl: &rpl,
            poc: 16,
            temporal: None,
        };
        let mut mv = MvField {
            ref_idx: [0, 0],
            ..Default::default()
        };
        luma_mv_mvp_mode(&params, 16, 32, 16, 16, &mut mv, 0, L0);
        assert_eq!(mv.mv[L0], Mv { x: 11, y: -7 });
    }
}
