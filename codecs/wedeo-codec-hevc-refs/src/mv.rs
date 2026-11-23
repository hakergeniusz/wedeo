//! Motion vectors, motion-vector fields and temporal collocated derivation.
//!
//! FFmpeg reference: `libavcodec/hevc/mvs.c`, `hevcdec.h`
//! (`Mv`, `MvField`, `mv_scale`, `check_mvset`,
//! `derive_temporal_colocated_mvs`).

use crate::RefPicList;

/// FFmpeg's `Mv`: `int16_t x, y` (quarter-pel units).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mv {
    pub x: i16,
    pub y: i16,
}

impl Mv {
    pub const ZERO: Self = Self { x: 0, y: 0 };
}

/// FFmpeg's `enum { PF_INTRA, PF_L0, PF_L1, PF_BI }`.
///
/// Kept as a bitmask so the C idioms `pred_flag & PF_L0` and
/// `pred_flag == PF_BI` both remain meaningful.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PredFlag(pub u8);

impl PredFlag {
    pub const INTRA: Self = Self(0);
    pub const L0: Self = Self(1);
    pub const L1: Self = Self(2);
    pub const BI: Self = Self(3);

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn is_intra(self) -> bool {
        self.0 == 0
    }

    pub fn has_l0(self) -> bool {
        self.0 & Self::L0.0 != 0
    }

    pub fn has_l1(self) -> bool {
        self.0 & Self::L1.0 != 0
    }
}

/// FFmpeg's `MvField`: two motion vectors with reference indices and a
/// prediction flag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MvField {
    pub mv: [Mv; 2],
    pub ref_idx: [i8; 2],
    pub pred_flag: PredFlag,
}

impl MvField {
    pub const INTRA: Self = Self {
        mv: [Mv::ZERO; 2],
        ref_idx: [0; 2],
        pred_flag: PredFlag::INTRA,
    };
}

/// `av_clip(a, amin, amax)`.
#[inline]
fn clip(a: i32, amin: i32, amax: i32) -> i32 {
    a.clamp(amin, amax)
}

/// FFmpeg's `av_clip_int8()`.
#[inline]
fn av_clip_int8(a: i32) -> i32 {
    clip(a, -128, 127)
}

/// FFmpeg's `av_clip_intp2(a, p)` (bit-exact for the ranges used here).
#[inline]
fn av_clip_intp2(a: i32, p: u32) -> i32 {
    clip(a, -(1 << p), (1 << p) - 1)
}

/// FFmpeg's `av_clip_int16()`.
#[inline]
fn av_clip_int16(a: i32) -> i32 {
    clip(a, -32_768, 32_767)
}

/// FFmpeg's `mv_scale()`.
///
/// `td` is the collocated picture distance and `tb` the current picture
/// distance. Both are clipped to `int8_t` before scaling; the resulting scale
/// factor is clipped to 12 signed bits and the motion-vector components are
/// rounded with `(v + 127 + (v < 0)) >> 8` before an `int16_t` clip.
pub fn mv_scale(src: Mv, td: i32, tb: i32) -> Mv {
    // FFmpeg divides by `td` below; callers guarantee it is non-zero. Avoid a
    // Rust divide-by-zero panic for malformed synthetic input by copying.
    if td == 0 {
        return src;
    }

    let td = av_clip_int8(td);
    let tb = av_clip_int8(tb);
    // C's `abs(td / 2)`: `td / 2` truncates toward zero, then abs.
    let tx = (0x4000_i32.wrapping_add((td / 2).wrapping_abs())) / td;
    let scale_factor = av_clip_intp2((tb.wrapping_mul(tx).wrapping_add(32)) >> 6, 12);

    let scale = |v: i16| -> i16 {
        let prod = scale_factor.wrapping_mul(i32::from(v));
        let rounded = (prod.wrapping_add(127).wrapping_add(i32::from(prod < 0))) >> 8;
        av_clip_int16(rounded) as i16
    };

    Mv {
        x: scale(src.x),
        y: scale(src.y),
    }
}

/// FFmpeg's `compare_mv_ref_idx()`.
///
/// Check that the motion vectors and reference indices of two fields match,
/// comparing only the lists that are actually active for their prediction
/// flag.
pub fn compare_mv_ref_idx(a: MvField, b: MvField) -> bool {
    if a.pred_flag != b.pred_flag {
        return false;
    }
    if a.pred_flag == PredFlag::BI {
        a.ref_idx[0] == b.ref_idx[0]
            && a.mv[0] == b.mv[0]
            && a.ref_idx[1] == b.ref_idx[1]
            && a.mv[1] == b.mv[1]
    } else if a.pred_flag == PredFlag::L0 {
        a.ref_idx[0] == b.ref_idx[0] && a.mv[0] == b.mv[0]
    } else if a.pred_flag == PredFlag::L1 {
        a.ref_idx[1] == b.ref_idx[1] && a.mv[1] == b.mv[1]
    } else {
        false
    }
}

/// FFmpeg's `check_mvset()` from `mvs.c` (section 8.5.3.1.8).
///
/// `x` is the current list index (`X` in C), `list_col` the collocated list
/// index (`L0`/`L1`). Returns `(available_flag, mvLXCol)`.
// Mirrors FFmpeg's nine-argument `check_mvset()` signature.
#[allow(clippy::too_many_arguments)]
pub fn check_mvset(
    mv_col: Mv,
    col_poc: i32,
    poc: i32,
    cur_rpl: &[RefPicList; 2],
    x: usize,
    ref_idx_lx: usize,
    col_rpl: &[RefPicList; 2],
    list_col: usize,
    ref_idx_col: usize,
) -> (bool, Mv) {
    let (Some(cur), Some(col)) = (cur_rpl.get(x), col_rpl.get(list_col)) else {
        return (false, Mv::ZERO);
    };
    let (Some(&cur_lt), Some(&col_lt)) = (
        cur.is_long_term.get(ref_idx_lx),
        col.is_long_term.get(ref_idx_col),
    ) else {
        return (false, Mv::ZERO);
    };

    if cur_lt != col_lt {
        return (false, Mv::ZERO);
    }

    let (Some(&col_ref_poc), Some(&cur_ref_poc)) =
        (col.list.get(ref_idx_col), cur.list.get(ref_idx_lx))
    else {
        return (false, Mv::ZERO);
    };

    let col_poc_diff = col_poc.wrapping_sub(col_ref_poc);
    let cur_poc_diff = poc.wrapping_sub(cur_ref_poc);

    if cur_lt || col_poc_diff == cur_poc_diff || col_poc_diff == 0 {
        (true, mv_col)
    } else {
        (true, mv_scale(mv_col, col_poc_diff, cur_poc_diff))
    }
}

/// FFmpeg's `derive_temporal_colocated_mvs()` from `mvs.c`.
///
/// Derives the temporal collocated MV for list `x` from a collocated `MvField`.
/// `collocated_list` is `sh->collocated_list` (`L0` or `L1`). Returns
/// `(availableFlagLXCol, mvLXCol)`.
// Mirrors FFmpeg's eight-argument `derive_temporal_colocated_mvs()`.
#[allow(clippy::too_many_arguments)]
pub fn derive_temporal_colocated_mvs(
    temp_col: MvField,
    ref_idx_lx: usize,
    x: usize,
    poc: i32,
    col_poc: i32,
    cur_rpl: &[RefPicList; 2],
    col_rpl: &[RefPicList; 2],
    collocated_list: usize,
) -> (bool, Mv) {
    if temp_col.pred_flag.is_intra() {
        return (false, Mv::ZERO);
    }

    let coll_idx = |list: usize| -> (bool, Mv) {
        let idx = temp_col.ref_idx[list];
        if idx < 0 {
            return (false, Mv::ZERO);
        }
        check_mvset(
            temp_col.mv[list],
            col_poc,
            poc,
            cur_rpl,
            x,
            ref_idx_lx,
            col_rpl,
            list,
            idx as usize,
        )
    };

    if !temp_col.pred_flag.has_l0() {
        return coll_idx(1);
    }
    if temp_col.pred_flag == PredFlag::L0 {
        return coll_idx(0);
    }
    if temp_col.pred_flag == PredFlag::BI {
        // check_diffpicount: number of lists containing a picture with POC
        // greater than the current POC.
        let mut check_diffpicount = 0;
        for list in 0..2 {
            let Some(rpl) = cur_rpl.get(list) else {
                continue;
            };
            for i in 0..rpl.nb_refs.min(rpl.list.len()) {
                if rpl.list[i] > poc {
                    check_diffpicount += 1;
                    break;
                }
            }
        }
        if check_diffpicount == 0 {
            return coll_idx(x);
        }
        if collocated_list == 1 {
            return coll_idx(0);
        }
        return coll_idx(1);
    }

    (false, Mv::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mv_scale_identity_when_distances_match() {
        let mv = Mv { x: 100, y: -50 };
        assert_eq!(mv_scale(mv, 4, 4), mv);
    }

    #[test]
    fn mv_scale_halves_when_current_distance_is_half() {
        // td = 8, tb = 4 -> scale factor ~= 0.5 (rounded).
        let out = mv_scale(Mv { x: 100, y: 100 }, 8, 4);
        assert_eq!(out.x, 50);
        assert_eq!(out.y, 50);
    }

    #[test]
    fn mv_scale_zero_td_copies() {
        let mv = Mv { x: 7, y: -9 };
        assert_eq!(mv_scale(mv, 0, 4), mv);
    }

    #[test]
    fn compare_mv_ref_idx_respects_prediction_flag() {
        let a = MvField {
            mv: [Mv { x: 1, y: 2 }, Mv::ZERO],
            ref_idx: [3, 0],
            pred_flag: PredFlag::L0,
        };
        let b = a;
        assert!(compare_mv_ref_idx(a, b));
        let mut c = a;
        c.mv[1] = Mv { x: 9, y: 9 }; // unused for L0
        assert!(compare_mv_ref_idx(a, c));
        c.pred_flag = PredFlag::BI;
        assert!(!compare_mv_ref_idx(a, c));
    }
}
