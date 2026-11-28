// VP9 intra and inter reconstruction: prediction + inverse transform + add.
//
// Translated from FFmpeg's libavcodec/vp9recon.c (`intra_recon`,
// `check_intra_mode`, `inter_recon`). The 8-bit path keeps its original
// arithmetic; 10/12-bit frames run the same generic kernels on `Plane::U16`.
//
// LGPL-2.1-or-later — same licence as FFmpeg.

use std::ops::{Deref, DerefMut, Index, IndexMut, RangeFrom};
use std::sync::Arc;

use bytemuck::{cast_slice, cast_slice_mut};
use wedeo_core::error::Result;

use crate::block::BlockInfo;
use crate::data::{BWH_TAB, INTRA_TXFM_TYPE};
use crate::header::FrameHeader;
use crate::idct::{ItxfmSample, itxfm_add, itxfm_add_lossless};
use crate::intra_pred::{Sample, intra_pred};
use crate::mc;
use crate::refs::RefFrame;
use crate::types::{IntraMode, TxSize, TxType};

// ---------------------------------------------------------------------------
// Frame buffer
// ---------------------------------------------------------------------------

/// Sample storage for one plane of a decoded frame.
///
/// FFmpeg keeps plane pointers as `uint8_t *` and scales every sample offset
/// by `s->bytesperpixel` (`vp9dec.h:120`, `vp9.c:1136`). Keeping the two
/// widths in separate variants lets the reconstruction math be generalised
/// over `u8`/`u16` later without changing the 8-bit byte path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plane {
    /// One byte per sample (VP9 profiles 0 and 1).
    U8(Vec<u8>),
    /// Two bytes per sample, native little-endian (VP9 profiles 2 and 3).
    U16(Vec<u16>),
}

impl Plane {
    /// Allocate `samples` zeroed samples for the given bit depth.
    ///
    /// VP9 defines only 8, 10 and 12-bit profiles. Any depth above 8 uses the
    /// 16-bit container so samples are never truncated.
    pub fn zeros(samples: usize, bit_depth: u8) -> Self {
        if bit_depth == 8 {
            Self::U8(vec![0u8; samples])
        } else {
            Self::U16(vec![0u16; samples])
        }
    }

    /// Bytes occupied by one sample.
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            Self::U8(_) => 1,
            Self::U16(_) => 2,
        }
    }

    /// Number of samples in the plane.
    pub fn len_samples(&self) -> usize {
        match self {
            Self::U8(data) => data.len(),
            Self::U16(data) => data.len(),
        }
    }

    /// Number of bytes in the plane (`stride_samples * rows * bytes_per_sample`).
    pub fn len_bytes(&self) -> usize {
        self.len_samples() * self.bytes_per_sample()
    }

    /// Raw byte view used by output frames and the legacy 8-bit math.
    pub fn as_u8(&self) -> &[u8] {
        match self {
            Self::U8(data) => data,
            // Native-endian byte view, matching FFmpeg's `uint8_t *` plane
            // pointers on little-endian hosts (the output formats are *le).
            Self::U16(data) => cast_slice(data),
        }
    }

    /// Mutable raw byte view used by the legacy 8-bit math.
    pub fn as_u8_mut(&mut self) -> &mut [u8] {
        match self {
            Self::U8(data) => data,
            Self::U16(data) => cast_slice_mut(data),
        }
    }

    /// Typed 16-bit view, available only for high-depth planes.
    pub fn as_u16(&self) -> Option<&[u16]> {
        match self {
            Self::U8(_) => None,
            Self::U16(data) => Some(data),
        }
    }

    /// Mutable typed 16-bit view, available only for high-depth planes.
    pub fn as_u16_mut(&mut self) -> Option<&mut [u16]> {
        match self {
            Self::U8(_) => None,
            Self::U16(data) => Some(data),
        }
    }
}

/// Connect FFmpeg's `uint8_t` / `uint16_t` sample instantiations to the
/// `Plane` variants so one reconstruction routine can serve both depths.
///
/// The 8-bit instantiation borrows `Plane::U8`; 10/12-bit borrows
/// `Plane::U16`. A mismatch is a decoder bug, not a stream error.
pub trait PlaneSample: Sample {
    /// Borrow a plane as a typed sample slice.
    fn plane_slice(plane: &Plane) -> Option<&[Self]>;

    /// Mutably borrow a plane as a typed sample slice.
    fn plane_slice_mut(plane: &mut Plane) -> Option<&mut [Self]>;
}

impl PlaneSample for u8 {
    fn plane_slice(plane: &Plane) -> Option<&[u8]> {
        match plane {
            Plane::U8(data) => Some(data),
            Plane::U16(_) => None,
        }
    }

    fn plane_slice_mut(plane: &mut Plane) -> Option<&mut [u8]> {
        match plane {
            Plane::U8(data) => Some(data),
            Plane::U16(_) => None,
        }
    }
}

impl PlaneSample for u16 {
    fn plane_slice(plane: &Plane) -> Option<&[u16]> {
        match plane {
            Plane::U8(_) => None,
            Plane::U16(data) => Some(data),
        }
    }

    fn plane_slice_mut(plane: &mut Plane) -> Option<&mut [u16]> {
        match plane {
            Plane::U8(_) => None,
            Plane::U16(data) => Some(data),
        }
    }
}

// These byte views keep the existing 8-bit call sites (loop filter, MC and
// output framing) source-compatible while stage 3 moves the reconstruction
// math to `as_u16`/`as_u16_mut`.
impl Deref for Plane {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_u8()
    }
}

impl DerefMut for Plane {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_u8_mut()
    }
}

impl Index<RangeFrom<usize>> for Plane {
    type Output = [u8];

    fn index(&self, range: RangeFrom<usize>) -> &[u8] {
        &self.as_u8()[range]
    }
}

impl IndexMut<RangeFrom<usize>> for Plane {
    fn index_mut(&mut self, range: RangeFrom<usize>) -> &mut [u8] {
        &mut self.as_u8_mut()[range]
    }
}

/// YUV frame buffer holding one decoded frame.
///
/// Chroma plane geometry follows the frame's subsampling flags
/// (`s->ss_h` / `s->ss_v` in FFmpeg): 4:2:0 shifts both dimensions,
/// 4:2:2 only height, 4:4:4 neither. Strides are in samples, as in FFmpeg;
/// only the output `linesize` is converted to bytes.
#[derive(Clone)]
pub struct FrameBuffer {
    pub y: Plane,
    pub u: Plane,
    pub v: Plane,
    /// Luma row stride in samples.
    pub y_stride: usize,
    /// Chroma row stride in samples.
    pub uv_stride: usize,
    pub width: u32,
    pub height: u32,
    /// Sample bit depth of this frame (8, 10 or 12).
    pub bit_depth: u8,
}

impl FrameBuffer {
    /// Allocate a zeroed frame buffer for a frame of the given dimensions.
    pub fn new(
        width: u32,
        height: u32,
        bit_depth: u8,
        subsampling_x: bool,
        subsampling_y: bool,
    ) -> Self {
        let w = width as usize;
        let h = height as usize;
        // Round strides up to a multiple of 64 for alignment headroom.
        let y_stride = (w + 63) & !63;
        let shift_h = usize::from(subsampling_x);
        let shift_v = usize::from(subsampling_y);
        let uv_w = (w + ((1 << shift_h) - 1)) >> shift_h;
        let uv_h = (h + ((1 << shift_v) - 1)) >> shift_v;
        let uv_stride = (uv_w + 63) & !63;
        // Pad rows to the next superblock boundary (64 luma, 32 chroma)
        // so edge-SB intra prediction can write past the visible frame.
        let y_rows = (h + 63) & !63;
        let uv_rows = (uv_h + 31) & !31;
        Self {
            y: Plane::zeros(y_stride * y_rows, bit_depth),
            u: Plane::zeros(uv_stride * uv_rows, bit_depth),
            v: Plane::zeros(uv_stride * uv_rows, bit_depth),
            y_stride,
            uv_stride,
            width,
            height,
            bit_depth,
        }
    }

    /// Luma `linesize` in bytes (FFmpeg `f->linesize[0]`).
    pub fn y_byte_stride(&self) -> usize {
        self.y_stride * self.y.bytes_per_sample()
    }

    /// Chroma `linesize` in bytes (FFmpeg `f->linesize[1]`).
    pub fn uv_byte_stride(&self) -> usize {
        self.uv_stride * self.u.bytes_per_sample()
    }
}

// ---------------------------------------------------------------------------
// Mode conversion table  (check_intra_mode in vp9recon.c)
// ---------------------------------------------------------------------------

/// `mode_conv[mode][have_left][have_top]` — maps a bitstream intra mode to
/// the appropriate fallback when some neighbors are missing.
///
/// Indices: 0 = no-left/no-top → left/top DC variants; same layout as FFmpeg.
static MODE_CONV: [[[IntraMode; 2]; 2]; 10] = [
    // VERT_PRED
    [
        [IntraMode::Dc127Pred, IntraMode::VertPred],
        [IntraMode::Dc127Pred, IntraMode::VertPred],
    ],
    // HOR_PRED
    [
        [IntraMode::Dc129Pred, IntraMode::Dc129Pred],
        [IntraMode::HorPred, IntraMode::HorPred],
    ],
    // DC_PRED
    [
        [IntraMode::Dc128Pred, IntraMode::TopDcPred],
        [IntraMode::LeftDcPred, IntraMode::DcPred],
    ],
    // DIAG_DOWN_LEFT_PRED
    [
        [IntraMode::Dc127Pred, IntraMode::DiagDownLeftPred],
        [IntraMode::Dc127Pred, IntraMode::DiagDownLeftPred],
    ],
    // DIAG_DOWN_RIGHT_PRED
    [
        [IntraMode::DiagDownRightPred, IntraMode::DiagDownRightPred],
        [IntraMode::DiagDownRightPred, IntraMode::DiagDownRightPred],
    ],
    // VERT_RIGHT_PRED
    [
        [IntraMode::VertRightPred, IntraMode::VertRightPred],
        [IntraMode::VertRightPred, IntraMode::VertRightPred],
    ],
    // HOR_DOWN_PRED
    [
        [IntraMode::HorDownPred, IntraMode::HorDownPred],
        [IntraMode::HorDownPred, IntraMode::HorDownPred],
    ],
    // VERT_LEFT_PRED
    [
        [IntraMode::Dc127Pred, IntraMode::VertLeftPred],
        [IntraMode::Dc127Pred, IntraMode::VertLeftPred],
    ],
    // HOR_UP_PRED
    [
        [IntraMode::Dc129Pred, IntraMode::Dc129Pred],
        [IntraMode::HorUpPred, IntraMode::HorUpPred],
    ],
    // TM_VP8_PRED
    [
        [IntraMode::Dc129Pred, IntraMode::VertPred],
        [IntraMode::HorPred, IntraMode::TmVp8Pred],
    ],
];

/// Which neighbor pixels each mode requires.
struct EdgeNeeds {
    needs_left: bool,
    needs_top: bool,
    needs_topleft: bool,
    needs_topright: bool,
    /// HOR_UP_PRED: collect left pixels top-to-bottom instead of bottom-to-top.
    invert_left: bool,
}

static EDGE_NEEDS: [EdgeNeeds; 15] = [
    // VERT_PRED
    EdgeNeeds {
        needs_left: false,
        needs_top: true,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // HOR_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: false,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // DC_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: true,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // DIAG_DOWN_LEFT_PRED
    EdgeNeeds {
        needs_left: false,
        needs_top: true,
        needs_topleft: false,
        needs_topright: true,
        invert_left: false,
    },
    // DIAG_DOWN_RIGHT_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: true,
        needs_topleft: true,
        needs_topright: false,
        invert_left: false,
    },
    // VERT_RIGHT_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: true,
        needs_topleft: true,
        needs_topright: false,
        invert_left: false,
    },
    // HOR_DOWN_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: true,
        needs_topleft: true,
        needs_topright: false,
        invert_left: false,
    },
    // VERT_LEFT_PRED
    EdgeNeeds {
        needs_left: false,
        needs_top: true,
        needs_topleft: false,
        needs_topright: true,
        invert_left: false,
    },
    // HOR_UP_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: false,
        needs_topleft: false,
        needs_topright: false,
        invert_left: true,
    },
    // TM_VP8_PRED
    EdgeNeeds {
        needs_left: true,
        needs_top: true,
        needs_topleft: true,
        needs_topright: false,
        invert_left: false,
    },
    // LEFT_DC_PRED (10)
    EdgeNeeds {
        needs_left: true,
        needs_top: false,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // TOP_DC_PRED (11)
    EdgeNeeds {
        needs_left: false,
        needs_top: true,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // DC_128_PRED (12)
    EdgeNeeds {
        needs_left: false,
        needs_top: false,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // DC_127_PRED (13)
    EdgeNeeds {
        needs_left: false,
        needs_top: false,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
    // DC_129_PRED (14)
    EdgeNeeds {
        needs_left: false,
        needs_top: false,
        needs_topleft: false,
        needs_topright: false,
        invert_left: false,
    },
];

// ---------------------------------------------------------------------------
// Neighbor pixel preparation  (check_intra_mode)
// ---------------------------------------------------------------------------

/// Prepare the `above` and `left` neighbor pixel arrays for one transform-block
/// and return the (possibly adjusted) intra mode.
///
/// * `plane_buf`   — the plane pixel buffer (Y, U, or V).
/// * `stride`      — row stride for `plane_buf`.
/// * `px`, `py`    — pixel-coordinate top-left corner of this transform block.
/// * `frame_w`, `frame_h` — frame dimensions for the plane (pixels).
/// * `tile_col_start_px` — first pixel column in the current tile.
/// * `tx_px`       — transform block size in pixels (4/8/16/32).
/// * `above_buf`   — caller-provided scratch buffer (≥ `tx_px + 1` entries;
///   entry `[0]` is the top-left, `[1..]` are the above pixels).
/// * `left_buf`    — caller-provided scratch buffer (≥ `tx_px` entries).
/// * `mode`        — bitstream intra mode (0..9).
///
/// Returns the resolved mode.
#[allow(clippy::too_many_arguments)]
fn prepare_intra_edges<P: Sample>(
    plane_buf: &[P],
    stride: usize,
    px: usize,
    py: usize,
    frame_w: usize,
    frame_h: usize,
    tile_col_start_px: usize,
    tx_px: usize,
    above_buf: &mut [P], // length >= tx_px + 1; above_buf[1..] = above pixels; above_buf[0] = top-left
    left_buf: &mut [P],  // length >= tx_px
    mode: IntraMode,
    have_right: bool, // FFmpeg: x < w - 1 (within-block check)
    bit_depth: u8,
) -> IntraMode {
    // FFmpeg vp9recon.c:153,173,212 uses `(128 << (bpp - 8)) -/+ 1`.
    let mid = 128i32 << (bit_depth - 8);
    let low = P::from_i32(mid - 1);
    let high = P::from_i32(mid + 1);
    let mode_idx = mode as usize;
    debug_assert!(
        mode_idx < 10,
        "prepare_intra_edges called with non-bitstream mode"
    );

    let have_top = py > 0;
    let have_left = px > tile_col_start_px;

    // Apply mode_conv: map bitstream mode to the mode suitable for the
    // available neighbors.
    let resolved = MODE_CONV[mode_idx][have_left as usize][have_top as usize];
    let r = resolved as usize;
    let needs = &EDGE_NEEDS[r];

    // --- above pixels ---
    if needs.needs_top {
        // FFmpeg: n_px_have = (((s->cols - col) << !ss_h) - x) * 4
        // Total pixels available from px to right edge of frame.
        let n_px_have = frame_w.saturating_sub(px);
        let n_px_need = tx_px;
        // Include top-right pixels if needed (only for TX_4X4).
        let n_need_tr = if tx_px == 4 && needs.needs_topright && have_right {
            4
        } else {
            0
        };

        // FFmpeg fast-path: direct pointer when all required pixels are available.
        //   have_top && (!needs_topleft || (have_left && top == topleft))
        //   && (tx != TX_4X4 || !needs_topright || have_right)
        //   && n_px_need + n_px_need_tr <= n_px_have
        if have_top
            && (!needs.needs_topleft || have_left)
            && (tx_px != 4 || !needs.needs_topright || have_right)
            && n_px_need + n_need_tr <= n_px_have
        {
            // Fast path: can point directly; but since we own above_buf, copy.
            let src_row = py - 1;
            for i in 0..tx_px {
                above_buf[i + 1] = plane_buf[src_row * stride + px + i];
            }
            if n_need_tr > 0 {
                for i in 0..n_need_tr {
                    above_buf[tx_px + 1 + i] = plane_buf[src_row * stride + px + tx_px + i];
                }
            }
        } else {
            // Slow path: copy what we have, pad the rest.
            if have_top {
                let src_row = py - 1;
                if n_px_have >= n_px_need {
                    for i in 0..n_px_need {
                        above_buf[i + 1] = plane_buf[src_row * stride + px + i];
                    }
                } else {
                    for i in 0..n_px_have {
                        above_buf[i + 1] = plane_buf[src_row * stride + px + i];
                    }
                    // Pad with last available pixel.
                    let pad = if n_px_have > 0 {
                        plane_buf[src_row * stride + px + n_px_have - 1]
                    } else {
                        low
                    };
                    for i in n_px_have..n_px_need {
                        above_buf[i + 1] = pad;
                    }
                }
            } else {
                // No top neighbor — fill with `(128 << (bpp - 8)) - 1`.
                for i in 0..n_px_need {
                    above_buf[i + 1] = low;
                }
            }
            if needs.needs_topright {
                if have_top && have_right && n_px_need + n_need_tr <= n_px_have {
                    // FFmpeg: memcpy(&(*a)[4], &top[4], 4)
                    let src_row = py - 1;
                    for i in 0..4 {
                        above_buf[tx_px + 1 + i] = plane_buf[src_row * stride + px + tx_px + i];
                    }
                } else {
                    // Replicate last above pixel.
                    let last = above_buf[tx_px];
                    for i in 0..4 {
                        above_buf[tx_px + 1 + i] = last;
                    }
                }
            }
        }

        // Top-left pixel.
        if needs.needs_topleft {
            if have_left && have_top {
                above_buf[0] = plane_buf[(py - 1) * stride + px - 1];
            } else if have_top {
                above_buf[0] = high; // `(128 << (bpp - 8)) + 1`
            } else {
                above_buf[0] = low; // `(128 << (bpp - 8)) - 1`
            }
        }
    }

    // --- left pixels (bottom-to-top ordering expected by intra_pred) ---
    if needs.needs_left {
        let n_have_rows = frame_h.saturating_sub(py).min(tx_px);

        if have_left {
            if needs.invert_left {
                // HOR_UP_PRED: collect top-to-bottom (left[0] = topmost row).
                if n_have_rows >= tx_px {
                    for i in 0..tx_px {
                        left_buf[i] = plane_buf[(py + i) * stride + px - 1];
                    }
                } else {
                    for i in 0..n_have_rows {
                        left_buf[i] = plane_buf[(py + i) * stride + px - 1];
                    }
                    let pad = if n_have_rows > 0 {
                        plane_buf[(py + n_have_rows - 1) * stride + px - 1]
                    } else {
                        high
                    };
                    left_buf[n_have_rows..tx_px].fill(pad);
                }
            } else {
                // Normal: bottom-to-top (left[0] = bottom-most row).
                if n_have_rows >= tx_px {
                    for i in 0..tx_px {
                        left_buf[tx_px - 1 - i] = plane_buf[(py + i) * stride + px - 1];
                    }
                } else {
                    for i in 0..n_have_rows {
                        left_buf[tx_px - 1 - i] = plane_buf[(py + i) * stride + px - 1];
                    }
                    let pad = if n_have_rows > 0 {
                        plane_buf[(py + n_have_rows - 1) * stride + px - 1]
                    } else {
                        high
                    };
                    left_buf[0..(tx_px - n_have_rows)].fill(pad);
                }
            }
        } else {
            // No left neighbor — fill with `(128 << (bpp - 8)) + 1`.
            left_buf[..tx_px].fill(high);
        }
    }

    resolved
}

// ---------------------------------------------------------------------------
// Public reconstruction entry point
// ---------------------------------------------------------------------------

/// Reconstruct one decoded block: run intra prediction then add the inverse
/// transform of the dequantized coefficients.
///
/// # Arguments
/// * `fb`     — frame buffer (modified in-place).
/// * `block`  — decoded block info from the entropy-coding phase.
/// * `header` — frame header (for lossless flag, dimensions, tile info).
/// * `tile_col_start` — first 4×4 column of the current tile.
fn reconstruct_intra_block_impl<P: ItxfmSample + PlaneSample>(
    fb: &mut FrameBuffer,
    block: &BlockInfo,
    header: &FrameHeader,
    tile_col_start: usize,
) -> Result<()> {
    let y = P::plane_slice_mut(&mut fb.y).expect("luma plane sample type matches bit depth");
    let u = P::plane_slice_mut(&mut fb.u).expect("U plane sample type matches bit depth");
    let v = P::plane_slice_mut(&mut fb.v).expect("V plane sample type matches bit depth");

    let bit_depth = header.bit_depth;
    let bs = block.bs as usize;

    // Block dimensions in 4×4 "double" units — matches FFmpeg's
    // `w4 = bwh_tab[1][bs][0] << 1` in intra_recon (vp9recon.c:225).
    // This is the FULL coefficient grid, not the sub-block size.
    let w4 = (BWH_TAB[1][bs][0] as usize) * 2;
    let h4 = (BWH_TAB[1][bs][1] as usize) * 2;

    // Frame-boundary clamping — FFmpeg:
    //   end_x = FFMIN(2 * (s->cols - col), w4)
    //   end_y = FFMIN(2 * (s->rows - row), h4)
    // where cols/rows are in 8×8 units and col/row are also 8×8.
    // Our col/row are in 4×4 units and cols_4x4 is also 4×4, so we
    // convert: 2 * (cols_8x8 - col_8x8) = cols_4x4*2 - col_4x4*2
    //        = 2 * (header.cols_4x4 - block.col)  (treating block.col as 4×4).
    let cols_8x8 = (header.width as usize).div_ceil(8);
    let rows_8x8 = (header.height as usize).div_ceil(8);
    let col_8x8 = block.col / 2;
    let row_8x8 = block.row / 2;
    let end_x = w4.min(2 * (cols_8x8.saturating_sub(col_8x8)));
    let end_y = h4.min(2 * (rows_8x8.saturating_sub(row_8x8)));

    // Luma transform step in 4×4 units.
    let tx_step = 1usize << (block.tx_size as usize);
    // TX size in pixels.
    let tx_px = tx_step * 4;

    // Luma reconstruction.
    {
        // Scratch buffers for neighbor pixels.  Above needs tx_px + 4 entries
        // (one for top-left at index 0, tx_px for above, and up to 4 for
        // top-right).  Left needs tx_px entries.
        let mid = P::from_i32(128i32 << (bit_depth - 8));
        let mut above_scratch = vec![mid; tx_px + 5];
        let mut left_scratch = vec![P::from_i32((128i32 << (bit_depth - 8)) + 1); tx_px];

        let mut coef_idx = 0usize;
        let coef_per_tx = tx_px * tx_px;

        for ty in (0..end_y).step_by(tx_step) {
            for tx in (0..end_x).step_by(tx_step) {
                // Pixel coordinates of this transform block.
                let px = (block.col + tx) * 4;
                let py = (block.row + ty) * 4;

                // For blocks larger than 8×8 and using 4×4 transforms, each
                // 4×4 within the 16-element y_mode array is indexed by
                // `ty * 2 + tx` (matching `b->mode[b->bs > BS_8x8 && b->tx == TX_4X4 ? y * 2 + x : 0]`
                // from vp9recon.c where x and y are in tx steps).
                let mode_idx = if bs > 9 && block.tx_size == TxSize::Tx4x4 {
                    // bs > 9 means sub-8×8 blocks (Bs8x4=10, Bs4x8=11, Bs4x4=12).
                    // FFmpeg: b->mode[b->bs > BS_8x8 && b->tx == TX_4X4 ? y*2+x : 0]
                    let sub_y = ty / tx_step;
                    let sub_x = tx / tx_step;
                    (sub_y * 2 + sub_x).min(3)
                } else {
                    0
                };
                let mode = block.y_mode[mode_idx];

                // Prepare neighbors and resolve the mode.
                // Use 8×8-grid-based dimensions (not pixel-exact) to match
                // FFmpeg's n_px_have = (((s->cols - col) << !ss_h) - x) * 4.
                let frame_w = cols_8x8 * 8;
                let frame_h = rows_8x8 * 8;
                let tile_col_px = tile_col_start * 4;

                // FFmpeg: have_right = x < w - 1 (against full block width, not end_x).
                let have_right = tx + tx_step < w4;

                let resolved = prepare_intra_edges(
                    y,
                    fb.y_stride,
                    px,
                    py,
                    frame_w,
                    frame_h,
                    tile_col_px,
                    tx_px,
                    &mut above_scratch,
                    &mut left_scratch,
                    mode,
                    have_right,
                    bit_depth,
                );

                // intra_pred writes to a sub-slice of the frame buffer.
                let dst_offset = py * fb.y_stride + px;
                // above = &above_scratch[1..] (above_scratch[0] is top-left).
                let above_slice = &above_scratch[1..];
                let top_left = above_scratch[0];

                intra_pred(
                    &mut y[dst_offset..],
                    fb.y_stride,
                    above_slice,
                    &left_scratch,
                    top_left,
                    resolved,
                    tx_px,
                    bit_depth,
                );

                // Inverse transform + add.
                if !block.skip {
                    let coefs = &block.coefs_y[coef_idx..coef_idx + coef_per_tx];
                    // Determine EOB (last non-zero coefficient + 1).
                    let eob = coefs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
                    if eob > 0 {
                        if header.lossless {
                            itxfm_add_lossless(&mut y[dst_offset..], fb.y_stride, coefs, bit_depth);
                        } else {
                            let tx_type_raw = INTRA_TXFM_TYPE[mode as usize];
                            let tx_type = TxType::try_from(tx_type_raw).unwrap_or(TxType::DctDct);
                            itxfm_add(
                                &mut y[dst_offset..],
                                fb.y_stride,
                                coefs,
                                block.tx_size,
                                tx_type,
                                bit_depth,
                                eob,
                            );
                        }
                    }
                }

                coef_idx += coef_per_tx;
            }
        }
    }

    // Chroma (U and V) reconstruction.
    // For 4:2:0: chroma dimensions are half luma (rounded up).
    let ss_h = if header.subsampling_x { 1usize } else { 0 };
    let ss_v = if header.subsampling_y { 1usize } else { 0 };

    // Chroma end dimensions derived from luma end, matching FFmpeg:
    //   w4 >>= s->ss_h; end_x >>= s->ss_h; end_y >>= s->ss_v;
    let uv_end_x = end_x >> ss_h;
    let uv_end_y = end_y >> ss_v;
    let uv_w4 = w4 >> ss_h; // FFmpeg: w4 >>= s->ss_h
    let uv_tx_step = 1usize << (block.uv_tx_size as usize);
    let uv_tx_px = uv_tx_step * 4;

    // Grid-based chroma dimensions matching FFmpeg's n_px_have formula.
    let uv_frame_w = cols_8x8 * (8 >> ss_h);
    let uv_frame_h = rows_8x8 * (8 >> ss_v);
    let uv_tile_col_px = (tile_col_start * 4) >> ss_h;

    let uv_mid = P::from_i32(128i32 << (bit_depth - 8));
    let mut uv_above_scratch = vec![uv_mid; uv_tx_px + 5];
    let mut uv_left_scratch = vec![P::from_i32((128i32 << (bit_depth - 8)) + 1); uv_tx_px];
    let uv_stride = fb.uv_stride;
    let uv_mode = block.uv_mode;
    let coef_per_uv_tx = uv_tx_px * uv_tx_px;

    // Process U (Cb) plane.
    {
        let mut coef_idx = 0usize;
        for ty in (0..uv_end_y).step_by(uv_tx_step) {
            for tx in (0..uv_end_x).step_by(uv_tx_step) {
                let px = ((block.col >> ss_h) + tx) * 4;
                let py = ((block.row >> ss_v) + ty) * 4;

                let dst_end = py
                    .saturating_add(uv_tx_px)
                    .saturating_sub(1)
                    .saturating_mul(uv_stride)
                    .saturating_add(px)
                    .saturating_add(uv_tx_px);
                // The frame buffer is padded to superblock boundaries, so a
                // transform block may extend past the visible frame. FFmpeg's
                // intra_recon still predicts such edge blocks, replicating the
                // last available neighbor pixel (prepare_intra_edges does this
                // via n_px_have). Only guard against writing outside the
                // allocated buffer; never skip a valid edge block.
                if dst_end > u.len() || dst_end > v.len() {
                    coef_idx += coef_per_uv_tx;
                    continue;
                }

                let uv_have_right = tx + uv_tx_step < uv_w4;

                let resolved = prepare_intra_edges(
                    u,
                    uv_stride,
                    px,
                    py,
                    uv_frame_w,
                    uv_frame_h,
                    uv_tile_col_px,
                    uv_tx_px,
                    &mut uv_above_scratch,
                    &mut uv_left_scratch,
                    uv_mode,
                    uv_have_right,
                    bit_depth,
                );

                let dst_offset = py * uv_stride + px;
                let above_slice = &uv_above_scratch[1..];
                let top_left = uv_above_scratch[0];

                intra_pred(
                    &mut u[dst_offset..],
                    uv_stride,
                    above_slice,
                    &uv_left_scratch,
                    top_left,
                    resolved,
                    uv_tx_px,
                    bit_depth,
                );

                if !block.skip {
                    let coefs = &block.coefs_u[coef_idx..coef_idx + coef_per_uv_tx];
                    let eob = coefs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
                    if eob > 0 {
                        if header.lossless {
                            itxfm_add_lossless(&mut u[dst_offset..], uv_stride, coefs, bit_depth);
                        } else {
                            itxfm_add(
                                &mut u[dst_offset..],
                                uv_stride,
                                coefs,
                                block.uv_tx_size,
                                TxType::DctDct,
                                bit_depth,
                                eob,
                            );
                        }
                    }
                }

                coef_idx += coef_per_uv_tx;
            }
        }
    }

    // Process V (Cr) plane.
    {
        let mut coef_idx = 0usize;
        for ty in (0..uv_end_y).step_by(uv_tx_step) {
            for tx in (0..uv_end_x).step_by(uv_tx_step) {
                let px = ((block.col >> ss_h) + tx) * 4;
                let py = ((block.row >> ss_v) + ty) * 4;

                let dst_end = py
                    .saturating_add(uv_tx_px)
                    .saturating_sub(1)
                    .saturating_mul(uv_stride)
                    .saturating_add(px)
                    .saturating_add(uv_tx_px);
                // See the U-plane loop: edge blocks are predicted with
                // replicated pixels instead of skipped.
                if dst_end > u.len() || dst_end > v.len() {
                    coef_idx += coef_per_uv_tx;
                    continue;
                }

                let uv_have_right = tx + uv_tx_step < uv_w4;

                let resolved = prepare_intra_edges(
                    v,
                    uv_stride,
                    px,
                    py,
                    uv_frame_w,
                    uv_frame_h,
                    uv_tile_col_px,
                    uv_tx_px,
                    &mut uv_above_scratch,
                    &mut uv_left_scratch,
                    uv_mode,
                    uv_have_right,
                    bit_depth,
                );

                let dst_offset = py * uv_stride + px;
                let above_slice = &uv_above_scratch[1..];
                let top_left = uv_above_scratch[0];

                intra_pred(
                    &mut v[dst_offset..],
                    uv_stride,
                    above_slice,
                    &uv_left_scratch,
                    top_left,
                    resolved,
                    uv_tx_px,
                    bit_depth,
                );

                if !block.skip {
                    let coefs = &block.coefs_v[coef_idx..coef_idx + coef_per_uv_tx];
                    let eob = coefs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
                    if eob > 0 {
                        if header.lossless {
                            itxfm_add_lossless(&mut v[dst_offset..], uv_stride, coefs, bit_depth);
                        } else {
                            // UV always uses DCT_DCT (vp9recon.c line 280).
                            itxfm_add(
                                &mut v[dst_offset..],
                                uv_stride,
                                coefs,
                                block.uv_tx_size,
                                TxType::DctDct,
                                bit_depth,
                                eob,
                            );
                        }
                    }
                }

                coef_idx += coef_per_uv_tx;
            }
        }
    }

    Ok(())
}

/// Dispatch intra reconstruction to the sample container used by the frame.
///
/// FFmpeg instantiates `intra_recon` twice, once with `uint8_t` planes and
/// once with `uint16_t` planes. The generic body below is the same in both
/// cases; only the plane borrow changes.
pub fn reconstruct_intra_block(
    fb: &mut FrameBuffer,
    block: &BlockInfo,
    header: &FrameHeader,
    tile_col_start: usize,
) -> Result<()> {
    if fb.bit_depth == 8 {
        reconstruct_intra_block_impl::<u8>(fb, block, header, tile_col_start)
    } else {
        reconstruct_intra_block_impl::<u16>(fb, block, header, tile_col_start)
    }
}

// ---------------------------------------------------------------------------
// Inter block reconstruction
// ---------------------------------------------------------------------------

/// Reconstruct one inter-predicted block: motion compensation then add residual.
///
/// Translated from `inter_recon` in vp9recon.c. Inter blocks always use
/// `TxType::DctDct` for the inverse transform (no ADST for inter).
fn reconstruct_inter_block_impl<P: ItxfmSample + PlaneSample + mc::McSample>(
    fb: &mut FrameBuffer,
    block: &BlockInfo,
    header: &FrameHeader,
    ref_slots: &[Option<Arc<RefFrame>>; 8],
) -> Result<()> {
    let bit_depth = header.bit_depth;

    // 1. Motion compensation: write prediction into fb. The three mutable
    // plane borrows end at this call so the residual pass can re-borrow them.
    {
        let y = P::plane_slice_mut(&mut fb.y).expect("luma plane sample type matches bit depth");
        let u = P::plane_slice_mut(&mut fb.u).expect("U plane sample type matches bit depth");
        let v = P::plane_slice_mut(&mut fb.v).expect("V plane sample type matches bit depth");
        mc::inter_pred(
            y,
            u,
            v,
            fb.y_stride,
            fb.uv_stride,
            block,
            ref_slots,
            &header.ref_idx,
            header.subsampling_x,
            header.subsampling_y,
            bit_depth,
        );
    }

    let y = P::plane_slice_mut(&mut fb.y).expect("luma plane sample type matches bit depth");
    let u = P::plane_slice_mut(&mut fb.u).expect("U plane sample type matches bit depth");
    let v = P::plane_slice_mut(&mut fb.v).expect("V plane sample type matches bit depth");

    // 2. If skip, no residual to add.
    if block.skip {
        return Ok(());
    }

    let bs = block.bs as usize;

    // Luma residual.
    // FFmpeg vp9recon.c:611-614: dimensions from bwh_tab[1]<<1, clamped to frame edge.
    let cols_8x8 = (header.width as usize).div_ceil(8);
    let rows_8x8 = (header.height as usize).div_ceil(8);
    let w4 = (BWH_TAB[1][bs][0] as usize) * 2;
    let h4 = (BWH_TAB[1][bs][1] as usize) * 2;
    let end_x = w4.min(2 * cols_8x8.saturating_sub(block.col / 2));
    let end_y = h4.min(2 * rows_8x8.saturating_sub(block.row / 2));
    {
        let tx_step = 1usize << (block.tx_size as usize);
        let tx_px = tx_step * 4;
        let coef_per_tx = tx_px * tx_px;
        let mut coef_idx = 0usize;

        for ty in (0..end_y).step_by(tx_step) {
            for tx in (0..end_x).step_by(tx_step) {
                let px = (block.col + tx) * 4;
                let py = (block.row + ty) * 4;

                let coefs = &block.coefs_y[coef_idx..coef_idx + coef_per_tx];
                let eob = coefs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
                if eob > 0 {
                    let dst_offset = py * fb.y_stride + px;
                    if header.lossless {
                        itxfm_add_lossless(&mut y[dst_offset..], fb.y_stride, coefs, bit_depth);
                    } else {
                        itxfm_add(
                            &mut y[dst_offset..],
                            fb.y_stride,
                            coefs,
                            block.tx_size,
                            TxType::DctDct, // Inter always uses DCT_DCT.
                            bit_depth,
                            eob,
                        );
                    }
                }

                coef_idx += coef_per_tx;
            }
        }
    }

    // Chroma residual (U and V).
    let ss_h = if header.subsampling_x { 1usize } else { 0 };
    let ss_v = if header.subsampling_y { 1usize } else { 0 };
    // FFmpeg vp9recon.c:631-632 shifts the *luma* dimensions by ss_h/ss_v.
    // BWH_TAB[1] is the 4:2:0 table; using it unconditionally would halve
    // chroma residual coverage again in 4:2:2/4:4:4 frames.
    let uv_w4 = w4 >> ss_h;
    let uv_h4 = h4 >> ss_v;
    let uv_end_x = uv_w4.min(end_x >> ss_h);
    let uv_end_y = uv_h4.min(end_y >> ss_v);
    let uv_tx_step = 1usize << (block.uv_tx_size as usize);
    let uv_tx_px = uv_tx_step * 4;
    let coef_per_uv_tx = uv_tx_px * uv_tx_px;
    let uv_stride = fb.uv_stride;

    // U plane.
    {
        let mut coef_idx = 0usize;
        for ty in (0..uv_end_y).step_by(uv_tx_step) {
            for tx in (0..uv_end_x).step_by(uv_tx_step) {
                let px = ((block.col >> ss_h) + tx) * 4;
                let py = ((block.row >> ss_v) + ty) * 4;

                let coefs = &block.coefs_u[coef_idx..coef_idx + coef_per_uv_tx];
                let eob = coefs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
                if eob > 0 {
                    let dst_offset = py * uv_stride + px;
                    if header.lossless {
                        itxfm_add_lossless(&mut u[dst_offset..], uv_stride, coefs, bit_depth);
                    } else {
                        itxfm_add(
                            &mut u[dst_offset..],
                            uv_stride,
                            coefs,
                            block.uv_tx_size,
                            TxType::DctDct,
                            bit_depth,
                            eob,
                        );
                    }
                }

                coef_idx += coef_per_uv_tx;
            }
        }
    }

    // V plane.
    {
        let mut coef_idx = 0usize;
        for ty in (0..uv_end_y).step_by(uv_tx_step) {
            for tx in (0..uv_end_x).step_by(uv_tx_step) {
                let px = ((block.col >> ss_h) + tx) * 4;
                let py = ((block.row >> ss_v) + ty) * 4;

                let coefs = &block.coefs_v[coef_idx..coef_idx + coef_per_uv_tx];
                let eob = coefs.iter().rposition(|&c| c != 0).map_or(0, |p| p + 1);
                if eob > 0 {
                    let dst_offset = py * uv_stride + px;
                    if header.lossless {
                        itxfm_add_lossless(&mut v[dst_offset..], uv_stride, coefs, bit_depth);
                    } else {
                        itxfm_add(
                            &mut v[dst_offset..],
                            uv_stride,
                            coefs,
                            block.uv_tx_size,
                            TxType::DctDct,
                            bit_depth,
                            eob,
                        );
                    }
                }

                coef_idx += coef_per_uv_tx;
            }
        }
    }

    Ok(())
}

/// Dispatch inter reconstruction to the frame's sample container.
pub fn reconstruct_inter_block(
    fb: &mut FrameBuffer,
    block: &BlockInfo,
    header: &FrameHeader,
    ref_slots: &[Option<Arc<RefFrame>>; 8],
) -> Result<()> {
    if fb.bit_depth == 8 {
        reconstruct_inter_block_impl::<u8>(fb, block, header, ref_slots)
    } else {
        reconstruct_inter_block_impl::<u16>(fb, block, header, ref_slots)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u8_frame_layout_matches_legacy_byte_allocation() {
        let fb = FrameBuffer::new(64, 64, 8, true, true);
        assert_eq!(fb.bit_depth, 8);
        assert!(matches!(fb.y, Plane::U8(_)));
        assert!(matches!(fb.u, Plane::U8(_)));
        assert!(matches!(fb.v, Plane::U8(_)));
        assert_eq!(fb.y_stride, 64);
        assert_eq!(fb.uv_stride, 64);
        assert_eq!(fb.y.len_samples(), 64 * 64);
        assert_eq!(fb.u.len_samples(), 64 * 32);
        assert_eq!(fb.y.len_bytes(), 64 * 64);
        assert_eq!(fb.y_byte_stride(), fb.y_stride);
        assert_eq!(fb.uv_byte_stride(), fb.uv_stride);
        assert!(fb.y.as_u16().is_none());
    }

    #[test]
    fn high_depth_frames_allocate_u16_with_byte_strides() {
        for bit_depth in [10, 12] {
            let fb = FrameBuffer::new(64, 64, bit_depth, true, true);
            assert_eq!(fb.bit_depth, bit_depth);
            assert!(matches!(fb.y, Plane::U16(_)));
            assert!(matches!(fb.u, Plane::U16(_)));
            assert!(matches!(fb.v, Plane::U16(_)));
            // Strides stay in samples; the byte size doubles.
            assert_eq!(fb.y_stride, 64);
            assert_eq!(fb.uv_stride, 64);
            assert_eq!(fb.y_byte_stride(), 64 * 2);
            assert_eq!(fb.uv_byte_stride(), 64 * 2);
            assert_eq!(fb.y.len_samples(), 64 * 64);
            assert_eq!(fb.y.len_bytes(), 64 * 64 * 2);
            assert_eq!(fb.u.len_bytes(), 64 * 32 * 2);
            assert_eq!(fb.y.as_u16().unwrap().len(), 64 * 64);
        }
    }

    #[test]
    fn plane_byte_view_exposes_high_depth_samples() {
        let mut fb = FrameBuffer::new(4, 4, 10, true, true);
        let y = fb.y.as_u16_mut().expect("10-bit plane is u16");
        y[0] = 0x1234;
        y[1] = 0x00ff;
        let bytes = fb.y.as_u8();
        assert_eq!(&bytes[..2], &0x1234u16.to_ne_bytes());
        assert_eq!(&bytes[2..4], &0x00ffu16.to_ne_bytes());
        assert_eq!(bytes.len(), fb.y.len_bytes());
    }

    #[test]
    fn cloned_frame_buffer_keeps_plane_variant() {
        // RefFrame stores a clone of the working frame buffer, so the
        // reference must carry the same u8/u16 variant as the source.
        let fb = FrameBuffer::new(16, 16, 12, false, false);
        let copy = fb.clone();
        assert!(matches!(copy.y, Plane::U16(_)));
        assert_eq!(copy.bit_depth, 12);
        assert_eq!(copy.y_byte_stride(), fb.y_byte_stride());
        assert_eq!(copy.y.len_bytes(), fb.y.len_bytes());
    }

    #[test]
    fn plane_sample_dispatch_selects_the_matching_variant() {
        let mut fb = FrameBuffer::new(8, 8, 10, true, true);
        assert!(u8::plane_slice(&fb.y).is_none());
        assert_eq!(u16::plane_slice(&fb.y).unwrap().len(), 64 * 64);
        u16::plane_slice_mut(&mut fb.y).unwrap()[0] = 777;
        assert_eq!(u16::plane_slice(&fb.y).unwrap()[0], 777);

        let mut fb8 = FrameBuffer::new(8, 8, 8, true, true);
        u8::plane_slice_mut(&mut fb8.y).unwrap()[0] = 77;
        assert_eq!(u8::plane_slice(&fb8.y).unwrap()[0], 77);
        assert!(u16::plane_slice(&fb8.y).is_none());
    }

    #[test]
    fn intra_edge_defaults_scale_with_bit_depth() {
        // DIAG_DOWN_RIGHT keeps all three edge requirements at the top-left
        // corner, so the three FFmpeg defaults are observable.
        let mut above8 = vec![0u8; 9];
        let mut left8 = vec![0u8; 8];
        prepare_intra_edges(
            &[0u8; 64],
            8,
            0,
            0,
            8,
            8,
            0,
            8,
            &mut above8,
            &mut left8,
            IntraMode::DiagDownRightPred,
            true,
            8,
        );
        assert_eq!((above8[0], above8[1], left8[0]), (127, 127, 129));

        let mut above12 = vec![0u16; 9];
        let mut left12 = vec![0u16; 8];
        prepare_intra_edges(
            &[0u16; 64],
            8,
            0,
            0,
            8,
            8,
            0,
            8,
            &mut above12,
            &mut left12,
            IntraMode::DiagDownRightPred,
            true,
            12,
        );
        assert_eq!((above12[0], above12[1], left12[0]), (2047, 2047, 2049));
    }
}
