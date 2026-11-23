//! Weighted prediction tables and sample weighting helpers.
//!
//! FFmpeg reference: `libavcodec/hevc/hevcdec.c` (`pred_weight_table()`)
//! and `libavcodec/hevc/dsp_template.c` (`put_hevc_pel_uni_w_pixels`,
//! `put_hevc_pel_bi_w_pixels`). The slice-header parser lives in the merged
//! `wedeo-codec-hevc-ps` crate; this crate owns the reusable table accessors
//! and sample weighting (HEVC spec 8.4.3).

use wedeo_codec_hevc_ps::{L0, L1, MAX_REFS, SliceHeader};

/// FFmpeg's `PredWeightTable` (parsed by `wedeo-codec-hevc-ps`).
///
/// The PS crate currently keeps this type private; [`Self::from_slice_header`]
/// copies the parsed values out of `SliceHeader` without naming that type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub chroma_log2_weight_denom: u32,
    pub luma_weight_l0: [i32; MAX_REFS],
    pub luma_offset_l0: [i32; MAX_REFS],
    pub luma_weight_l1: [i32; MAX_REFS],
    pub luma_offset_l1: [i32; MAX_REFS],
    pub chroma_weight_l0: [[i32; 2]; MAX_REFS],
    pub chroma_offset_l0: [[i32; 2]; MAX_REFS],
    pub chroma_weight_l1: [[i32; 2]; MAX_REFS],
    pub chroma_offset_l1: [[i32; 2]; MAX_REFS],
}

impl PredWeightTable {
    /// Build a table whose entries all carry the default weights (`1 << denom`)
    /// and zero offsets, as FFmpeg's `pred_weight_table()` does when the
    /// per-reference flags are not set.
    pub fn new(luma_log2_weight_denom: u32, chroma_log2_weight_denom: u32) -> Self {
        let mut table = Self {
            luma_log2_weight_denom,
            chroma_log2_weight_denom,
            ..Default::default()
        };
        for i in 0..MAX_REFS {
            table.luma_weight_l0[i] = 1 << luma_log2_weight_denom;
            table.luma_weight_l1[i] = 1 << luma_log2_weight_denom;
            table.chroma_weight_l0[i] = [1 << chroma_log2_weight_denom; 2];
            table.chroma_weight_l1[i] = [1 << chroma_log2_weight_denom; 2];
        }
        table
    }

    /// Copy the weighted-prediction table out of a parsed slice header.
    /// Returns `None` when the slice has no explicit table (default weights
    /// apply, see [`Self::default_luma_weight`]).
    pub fn from_slice_header(sh: &SliceHeader) -> Option<Self> {
        let t = sh.pred_weight_table.as_ref()?;
        Some(Self {
            luma_log2_weight_denom: t.luma_log2_weight_denom,
            chroma_log2_weight_denom: t.chroma_log2_weight_denom,
            luma_weight_l0: t.luma_weight_l0,
            luma_offset_l0: t.luma_offset_l0,
            luma_weight_l1: t.luma_weight_l1,
            luma_offset_l1: t.luma_offset_l1,
            chroma_weight_l0: t.chroma_weight_l0,
            chroma_offset_l0: t.chroma_offset_l0,
            chroma_weight_l1: t.chroma_weight_l1,
            chroma_offset_l1: t.chroma_offset_l1,
        })
    }

    /// Default luma weight when `luma_weight_lX_flag` is 0: `1 << denom`.
    pub fn default_luma_weight(&self) -> LumaWeight {
        LumaWeight {
            weight: 1 << self.luma_log2_weight_denom,
            offset: 0,
            log2_denom: self.luma_log2_weight_denom,
        }
    }

    /// Default chroma weights when `chroma_weight_lX_flag` is 0.
    pub fn default_chroma_weight(&self) -> ChromaWeight {
        ChromaWeight {
            weight: [1 << self.chroma_log2_weight_denom; 2],
            offset: [0; 2],
            log2_denom: self.chroma_log2_weight_denom,
        }
    }

    /// Luma weight for `list` / `ref_idx`, falling back to the default when an
    /// index is out of range.
    pub fn luma_weight(&self, list: usize, ref_idx: usize) -> LumaWeight {
        let (w, o) = match list {
            L0 => (
                self.luma_weight_l0.get(ref_idx).copied(),
                self.luma_offset_l0.get(ref_idx).copied(),
            ),
            L1 => (
                self.luma_weight_l1.get(ref_idx).copied(),
                self.luma_offset_l1.get(ref_idx).copied(),
            ),
            _ => (None, None),
        };
        match (w, o) {
            (Some(weight), Some(offset)) => LumaWeight {
                weight,
                offset,
                log2_denom: self.luma_log2_weight_denom,
            },
            _ => self.default_luma_weight(),
        }
    }

    /// Chroma weights for `list` / `ref_idx`.
    pub fn chroma_weight(&self, list: usize, ref_idx: usize) -> ChromaWeight {
        let (w, o) = match list {
            L0 => (
                self.chroma_weight_l0.get(ref_idx).copied(),
                self.chroma_offset_l0.get(ref_idx).copied(),
            ),
            L1 => (
                self.chroma_weight_l1.get(ref_idx).copied(),
                self.chroma_offset_l1.get(ref_idx).copied(),
            ),
            _ => (None, None),
        };
        match (w, o) {
            (Some(weight), Some(offset)) => ChromaWeight {
                weight,
                offset,
                log2_denom: self.chroma_log2_weight_denom,
            },
            _ => self.default_chroma_weight(),
        }
    }
}

/// Luma weight / offset for one reference entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LumaWeight {
    pub weight: i32,
    pub offset: i32,
    pub log2_denom: u32,
}

/// Chroma (Cb + Cr) weights / offsets for one reference entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromaWeight {
    pub weight: [i32; 2],
    pub offset: [i32; 2],
    pub log2_denom: u32,
}

/// `Clip1Y` / `Clip1C` to `[0, (1 << bit_depth) - 1]`.
#[inline]
fn clip_sample(v: i32, bit_depth: u32) -> i32 {
    v.clamp(0, (1i32 << bit_depth) - 1)
}

/// HEVC spec 8.4.3.1: uni-directional weighted sample prediction.
///
/// `pred` is an unweighted predicted sample, `bit_depth` the component bit
/// depth (8..14). The offset is scaled by `2^(bit_depth - 8)` and the final
/// shift uses `denom = log2_denom + bit_depth - 8` with a half-denom rounding
/// term, matching FFmpeg's `put_hevc_pel_uni_w_pixels`.
pub fn weighted_luma_sample(pred: i32, w: LumaWeight, bit_depth: u32) -> i32 {
    let bd_shift = bit_depth.saturating_sub(8);
    let denom = w.log2_denom + bd_shift;
    let offset = w.offset << bd_shift;
    // Round by half a denom step; there is no rounding term when denom == 0.
    let rounding = if denom > 0 { 1 << (denom - 1) } else { 0 };
    let num = w.weight * pred + offset + rounding;
    clip_sample(num >> denom, bit_depth)
}

/// Uni-directional weighted chroma prediction for component `c` (0 = Cb,
/// 1 = Cr).
pub fn weighted_chroma_sample(pred: i32, c: usize, w: ChromaWeight, bit_depth: u32) -> i32 {
    let bd_shift = bit_depth.saturating_sub(8);
    let denom = w.log2_denom + bd_shift;
    let offset = w.offset[c] << bd_shift;
    let rounding = if denom > 0 { 1 << (denom - 1) } else { 0 };
    let num = w.weight[c] * pred + offset + rounding;
    clip_sample(num >> denom, bit_depth)
}

/// HEVC spec 8.4.3.2: bi-directional weighted luma sample prediction.
///
/// `w0`/`w1` are the L0/L1 weights for the same reference index, `bit_depth`
/// the luma bit depth.
pub fn weighted_bi_luma_sample(
    pred0: i32,
    w0: LumaWeight,
    pred1: i32,
    w1: LumaWeight,
    bit_depth: u32,
) -> i32 {
    let bd_shift = bit_depth.saturating_sub(8);
    let denom = w0.log2_denom + bd_shift;
    let o0 = w0.offset << bd_shift;
    let o1 = w1.offset << bd_shift;
    let num = w0.weight * pred0 + o0 + w1.weight * pred1 + o1 + (1 << denom);
    clip_sample(num >> (denom + 1), bit_depth)
}

/// Validate that active reference counts fit the fixed table size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WeightValidation {
    pub l0_refs: usize,
    pub l1_refs: usize,
}

impl WeightValidation {
    pub fn validate(counts: WeightValidation) -> bool {
        counts.l0_refs <= MAX_REFS && counts.l1_refs <= MAX_REFS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_weight_is_identity() {
        let table = PredWeightTable::new(0, 0);
        assert_eq!(table.luma_weight(L0, 0).weight, 1);
        assert_eq!(weighted_luma_sample(100, table.luma_weight(L0, 0), 8), 100);
        assert_eq!(
            weighted_chroma_sample(100, 0, table.chroma_weight(L1, 0), 8),
            100
        );
    }

    #[test]
    fn explicit_weight_scales_and_offsets() {
        let mut table = PredWeightTable::new(2, 2); // default weight = 4
        table.luma_weight_l0[0] = 6;
        table.luma_offset_l0[0] = 4;
        let w = table.luma_weight(L0, 0);
        assert_eq!(w.weight, 6);
        // (6*100 + 4 + 2) >> 2 = 606 >> 2 = 151
        assert_eq!(weighted_luma_sample(100, w, 8), 151);
        // out-of-range index falls back to default (weight = 4, offset 0)
        let d = table.luma_weight(L0, MAX_REFS);
        assert_eq!(d.weight, 4);
        assert_eq!(weighted_luma_sample(100, d, 8), 100);
    }

    #[test]
    fn bi_weight_averages_equal_predictions() {
        let table = PredWeightTable::new(1, 1);
        let w = table.luma_weight(L0, 0);
        // identical weights/offsets -> average of two identical samples
        assert_eq!(weighted_bi_luma_sample(80, w, 80, w, 8), 80);
    }
}
