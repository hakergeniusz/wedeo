#![forbid(unsafe_code)]
//! HEVC DSP primitives, translated directly from FFmpeg's C source.
//!
//! References:
//! - `libavcodec/hevc/pred_template.c` (intra prediction and reference filtering)
//! - `libavcodec/h26x/h2656_inter_template.c` (8-tap luma / 4-tap chroma MC)
//! - `libavcodec/hevc/dsp_template.c` (transforms, bi-prediction deblock wrappers)
//! - `libavcodec/h26x/h2656_deblock_template.c` (loop filter kernels)
//! - `libavcodec/h26x/h2656_sao_template.c` (SAO band / edge filters)
//! - `libavcodec/hevc/dsp.c` (transform and interpolation filter tables)
//! - `libavcodec/hevc/filter.c` (`tctable`, `betatable`, chroma `tc`)
//!
//! Every routine is usable standalone; the caller owns the picture buffers.
//! Samples are stored as `u16` at the picture's bit depth (8, 9, 10 or 12),
//! except motion-compensation intermediates, which are `i16` in FFmpeg's
//! 14-bit domain. Buffer indices are checked and errors are returned instead
//! of panicking on malformed geometry.

use std::fmt;

use inventory as _;
use wedeo_codec as _;
use wedeo_core as _;

pub mod deblock;
pub mod inter;
pub mod intra;
pub mod sao;
pub mod transform;

/// Errors returned by the DSP routines when their inputs cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspError {
    /// `log2_size` outside FFmpeg's 2..=5 range (4x4 .. 32x32 transform blocks).
    InvalidLog2Size(u8),
    /// An intra prediction mode outside 0..=34.
    InvalidIntraMode(u8),
    /// A motion-vector fraction outside the filter's index range.
    InvalidFraction(usize),
    /// A bit depth outside FFmpeg's supported 8..=12 range.
    InvalidBitDepth(u8),
    /// A SAO edge-offset class outside 0..=3.
    InvalidEoClass(u8),
    /// A requested sample lies outside the supplied slice.
    OutOfBounds {
        what: &'static str,
        index: usize,
        len: usize,
    },
    /// A slice is smaller than the block geometry requires.
    BufferTooSmall {
        what: &'static str,
        need: usize,
        have: usize,
    },
}

impl fmt::Display for DspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLog2Size(v) => write!(f, "invalid HEVC log2 block size: {v}"),
            Self::InvalidIntraMode(v) => write!(f, "invalid HEVC intra prediction mode: {v}"),
            Self::InvalidFraction(v) => write!(f, "invalid motion vector fraction: {v}"),
            Self::InvalidBitDepth(v) => write!(f, "invalid HEVC bit depth: {v}"),
            Self::InvalidEoClass(v) => write!(f, "invalid SAO edge-offset class: {v}"),
            Self::OutOfBounds { what, index, len } => {
                write!(f, "{what} index {index} out of bounds (len {len})")
            }
            Self::BufferTooSmall { what, need, have } => {
                write!(f, "{what} buffer too small: need {need}, have {have}")
            }
        }
    }
}

impl std::error::Error for DspError {}

/// Convenience alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, DspError>;

/// Maximum HEVC transform block size (`MAX_TB_SIZE` in FFmpeg).
pub const MAX_TB_SIZE: usize = 32;
/// Maximum prediction block size (`MAX_PB_SIZE` in FFmpeg).
pub const MAX_PB_SIZE: usize = 64;
/// FFmpeg's input buffer padding, used by the SAO edge filter.
pub const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;

#[inline]
pub(crate) fn check_size(log2_size: u8) -> Result<usize> {
    if !(2..=5).contains(&log2_size) {
        return Err(DspError::InvalidLog2Size(log2_size));
    }
    Ok(1usize << log2_size)
}

#[inline]
pub(crate) fn check_bit_depth(bit_depth: u8) -> Result<()> {
    if !(8..=12).contains(&bit_depth) {
        return Err(DspError::InvalidBitDepth(bit_depth));
    }
    Ok(())
}

/// `av_clip_pixel()` for a sample at `bit_depth`: clip to 0 .. (1 << depth) - 1.
#[inline]
pub fn clip_pixel(v: i32, bit_depth: u8) -> u16 {
    let max = ((1u32 << bit_depth) - 1) as i32;
    v.clamp(0, max) as u16
}

/// `av_clip_int16()` from `libavutil/common.h`.
#[inline]
pub fn clip_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// `av_clip()` from `libavutil/common.h` with C integer arguments.
#[inline]
pub fn clip(v: i32, lo: i32, hi: i32) -> i32 {
    v.clamp(lo, hi)
}

#[cfg(test)]
mod tests;

/// Reference vectors generated from FFmpeg's C source by `tools/gen_vectors.py`.
///
/// Exposed so the FATE conformance harness can replay the exact same vectors.
pub mod reference_vectors;
