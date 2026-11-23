//! Reusable HEVC reference-picture and motion-vector library.
//!
//! FFmpeg reference: `libavcodec/hevc/refs.c`, `libavcodec/hevc/mvs.c`,
//! `libavcodec/hevc/hevcdec.h` and `libavutil/refstruct.c`.
//!
//! This crate is a *library* for the other HEVC work units, mirroring FFmpeg's
//! decoded-picture-buffer (DPB) and MV-prediction helpers. It provides:
//!
//! - [`Dpb`]: short-term / long-term RPS management (`frame_rps`), reference
//!   marking, missing-reference generation and output ordering.
//! - [`RefPicList`] / [`slice_rpl`]: L0/L1 construction with list modification.
//! - [`prediction`]: merge candidates (spatial / temporal / combined / zero)
//!   and AMVP candidates, including collocated-derived temporal MVs.
//! - [`weights`]: weighted-prediction table accessors and sample weighting.
//!
//! All parsing of `ShortTermRps`, `LongTermRps`, `SliceHeader` and
//! `PredWeightTable` comes from the merged `wedeo-codec-hevc-ps` crate.
//! Everything here is pure Rust: no FFI, bindgen or c2rust.
//!
//! Do not implement this work unit in any other crate.

use inventory as _;
use wedeo_codec as _;

pub mod context;
pub mod dpb;
pub mod frame;
pub mod mv;
pub mod neighbors;
pub mod prediction;
pub mod rpl;
pub mod weights;

#[cfg(test)]
mod tests;

pub use context::{PartMode, RefGeometry};
pub use dpb::{Dpb, FrameRpsParams, NewRefParams};
pub use frame::{FrameFlags, FrameRef, HevcFrame, MvFieldTable};
pub use mv::{
    Mv, MvField, PredFlag, check_mvset, compare_mv_ref_idx, derive_temporal_colocated_mvs, mv_scale,
};
pub use neighbors::{
    NeighbourAvailability, NeighbourCtbFlags, is_diff_mer, set_neighbour_available,
    z_scan_block_avail,
};
pub use prediction::{
    MRG_MAX_NUM_CANDS, MvpParams, SpatialMergeParams, TemporalCtx, derive_spatial_merge_candidates,
    luma_mv_merge_mode, luma_mv_mvp_mode, temporal_luma_motion_vector,
};
pub use rpl::{
    NB_RPS_TYPE, RefPicList, RpsType, SliceRplParams, frame_nb_refs, init_slice_rpl, slice_rpl,
};
pub use weights::{
    ChromaWeight, LumaWeight, PredWeightTable, WeightValidation, weighted_bi_luma_sample,
    weighted_chroma_sample, weighted_luma_sample,
};

// Constants re-exported so downstream HEVC crates do not need a direct
// dependency on `wedeo-codec-hevc-ps` just for these.
pub use wedeo_codec_hevc_ps::{L0, L1, MAX_REFS};
