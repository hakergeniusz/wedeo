//! HEVC parameter-set parsing (VPS / SPS / PPS), slice headers, NAL units.
//!
//! FFmpeg reference: `libavcodec/hevc/ps.c`, `libavcodec/hevc/parse.c`,
//! `libavcodec/hevc/sei.c`, `libavcodec/hevc/hevc.h`.
//!
//! This crate is a *library* for the other HEVC work units. It does not decode
//! video: it parses the RBSP of parameter sets and slice headers into typed,
//! publicly accessible Rust structs and exposes a [`ParamSets`] container that
//! stores them by id, mirroring FFmpeg's `HEVCParamSets`.
//!
//! All parsing is pure Rust and returns [`wedeo_core::Result`]. No FFI, no
//! bindgen, no decoder state is kept here.

use inventory as _;
use wedeo_codec as _;
use wedeo_core as _;

mod bitreader;
mod nal;
mod pps;
mod ps;
mod ptl;
mod rps;
mod scaling;
mod scan;
mod sei;
mod slice;
mod sps;
mod vps;
mod vui;

#[cfg(test)]
mod tests;

pub use bitreader::BitReader;
pub use nal::{
    NalUnitHeader, NalUnitType, ParsedNalUnit, annex_b_split, ebsp_to_rbsp, parse_nal_header,
    parse_nal_unit,
};
pub use pps::{Pps, TileInfo, parse_pps};
pub use ps::{ParamSets, ParameterSetKind, decode_extradata, decode_nal_units};
pub use ptl::{ProfileTierLevel, SubLayerProfileTierLevel};
pub use rps::{ShortTermRps, parse_short_term_rps};
pub use scaling::{ScalingList, parse_scaling_list_data, scaling_list_defaults};
pub use scan::{DIAG_SCAN4X4_X, DIAG_SCAN4X4_Y, DIAG_SCAN8X8_X, DIAG_SCAN8X8_Y};
pub use sei::{SeiMessage, SeiPayloadType, parse_sei_message};
pub use slice::{L0, L1, SliceHeader, SliceType, compute_poc, parse_slice_header};
pub use sps::{Sps, parse_sps};
pub use vps::{Vps, parse_vps};
pub use vui::{
    CommonVui, HrdParams, SubLayerHrdParams, Vui, decode_hrd, parse_common_vui, parse_vui,
    read_window,
};

// ---------------------------------------------------------------------------
// Constants from libavcodec/hevc/hevc.h
// ---------------------------------------------------------------------------

/// `HEVC_MAX_LAYERS`: vps_max_layers_minus1 is in [0, 62].
pub const MAX_LAYERS: usize = 63;
/// `HEVC_MAX_SUB_LAYERS`: vps_max_sub_layers_minus1 is in [0, 6].
pub const MAX_SUB_LAYERS: usize = 7;
/// `HEVC_MAX_LAYER_SETS`: vps_num_layer_sets_minus1 is in [0, 1023].
pub const MAX_LAYER_SETS: usize = 1024;
/// `HEVC_MAX_LAYER_ID`: vps_max_layer_id is in [0, 63].
pub const MAX_LAYER_ID: usize = 63;
/// `HEVC_MAX_NUH_LAYER_ID`.
pub const MAX_NUH_LAYER_ID: usize = 62;

/// `HEVC_MAX_VPS_COUNT`: vps_video_parameter_set_id is u(4).
pub const MAX_VPS_COUNT: usize = 16;
/// `HEVC_MAX_SPS_COUNT`: sps_seq_parameter_set_id is in [0, 15].
pub const MAX_SPS_COUNT: usize = 16;
/// `HEVC_MAX_PPS_COUNT`: pps_pic_parameter_set_id is in [0, 63].
pub const MAX_PPS_COUNT: usize = 64;

/// `HEVC_MAX_DPB_SIZE`: MaxDpbSize is bounded above by 16.
pub const MAX_DPB_SIZE: usize = 16;
/// `HEVC_MAX_REFS`.
pub const MAX_REFS: usize = MAX_DPB_SIZE;
/// `HEVC_MAX_SHORT_TERM_REF_PIC_SETS`.
pub const MAX_SHORT_TERM_REF_PIC_SETS: usize = 64;
/// `HEVC_MAX_LONG_TERM_REF_PICS`.
pub const MAX_LONG_TERM_REF_PICS: usize = 32;
/// `HEVC_MIN_LOG2_CTB_SIZE`.
pub const MIN_LOG2_CTB_SIZE: u32 = 4;
/// `HEVC_MAX_LOG2_CTB_SIZE`.
pub const MAX_LOG2_CTB_SIZE: u32 = 6;
/// `HEVC_MAX_CPB_CNT`.
pub const MAX_CPB_CNT: usize = 32;
/// `HEVC_MAX_LUMA_PS`.
pub const MAX_LUMA_PS: u32 = 35_651_584;
/// `HEVC_MAX_WIDTH`.
pub const MAX_WIDTH: u32 = 16_888;
/// `HEVC_MAX_HEIGHT`.
pub const MAX_HEIGHT: u32 = 16_888;
/// `HEVC_MAX_TILE_ROWS`.
pub const MAX_TILE_ROWS: usize = 22;
/// `HEVC_MAX_TILE_COLUMNS`.
pub const MAX_TILE_COLUMNS: usize = 20;
/// `HEVC_MAX_SLICE_SEGMENTS`.
pub const MAX_SLICE_SEGMENTS: usize = 600;
/// `HEVC_MAX_PALETTE_PREDICTOR_SIZE`.
pub const MAX_PALETTE_PREDICTOR_SIZE: usize = 128;

/// Chroma sub-sampling width factors indexed by `chroma_format_idc`
/// (`hevc_sub_width_c` in `ps.c`).
pub const SUB_WIDTH_C: [u32; 4] = [1, 2, 2, 1];
/// Chroma sub-sampling height factors indexed by `chroma_format_idc`
/// (`hevc_sub_height_c` in `ps.c`).
pub const SUB_HEIGHT_C: [u32; 4] = [1, 2, 1, 1];

/// `AV_PROFILE_HEVC_REXT`: range extensions profile idc, used to gate PPS
/// range extensions (see `pps_range_extensions` in `ps.c`).
pub const PROFILE_HEVC_REXT: u8 = 4;

/// A display/output window, FFmpeg's `HEVCWindow`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Window {
    pub left_offset: u32,
    pub right_offset: u32,
    pub top_offset: u32,
    pub bottom_offset: u32,
}
