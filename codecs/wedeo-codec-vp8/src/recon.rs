// Reconstructed frame buffers and reference-frame storage.
// LGPL-2.1-or-later, same as FFmpeg.

use wedeo_core::buffer::Buffer;
use wedeo_core::frame::{Frame, FrameData, FrameFlags, FramePlane, PictureType};
use wedeo_core::pixel_format::PixelFormat;

/// Margin (pixels) on each side of a reconstructed plane. Provides room for
/// prediction / filter neighbour reads without out-of-bounds access.
pub const MARGIN: usize = 32;

/// A reconstructed VP8 frame with padded Y/U/V planes.
#[derive(Clone)]
pub struct FrameBuffer {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub stride_y: usize,
    pub stride_uv: usize,
    pub width: usize,
    pub height: usize,
    pub is_keyframe: bool,
    /// Per-macroblock segmentation map (carried across frames).
    pub seg_map: Vec<u8>,
}

impl FrameBuffer {
    /// Allocate a zeroed frame buffer for `width` x `height` (padded to
    /// multiples of 16 for MB operations).
    pub fn new(width: usize, height: usize) -> Self {
        let w = (width + 15) & !15;
        let h = (height + 15) & !15;
        let stride_y = align(w + 2 * MARGIN, 64);
        let stride_uv = align(w / 2 + 2 * MARGIN, 64);
        let size_y = (h + 2 * MARGIN) * stride_y;
        let size_uv = (h / 2 + 2 * MARGIN) * stride_uv;
        Self {
            y: vec![0u8; size_y],
            u: vec![0u8; size_uv],
            v: vec![0u8; size_uv],
            stride_y,
            stride_uv,
            width,
            height,
            is_keyframe: false,
            seg_map: Vec::new(),
        }
    }

    /// Select the U (0) or V (1) chroma plane.
    #[inline]
    pub fn u_or_v(&mut self, ch: usize) -> &mut Vec<u8> {
        if ch == 0 { &mut self.u } else { &mut self.v }
    }

    /// Offset of the visible pixel (0,0) in each plane.
    #[inline]
    pub fn base_y(&self) -> usize {
        MARGIN * self.stride_y + MARGIN
    }
    #[inline]
    pub fn base_uv(&self) -> usize {
        MARGIN * self.stride_uv + MARGIN
    }

    /// Convert to a wedeo `Frame` with tightly packed visible planes.
    pub fn to_frame(&self, pts: i64, time_base: wedeo_core::rational::Rational) -> Frame {
        let mut frame =
            Frame::new_video(self.width as u32, self.height as u32, PixelFormat::Yuv420p);
        frame.pts = pts;
        frame.best_effort_timestamp = pts;
        frame.time_base = time_base;
        frame.flags = if self.is_keyframe {
            FrameFlags::KEY
        } else {
            FrameFlags::empty()
        };

        let extract = |src: &[u8], stride: usize, base: usize, w: usize, h: usize| -> Buffer {
            let mut out = vec![0u8; w * h];
            for y in 0..h {
                let s = base + y * stride;
                out[y * w..(y + 1) * w].copy_from_slice(&src[s..s + w]);
            }
            Buffer::from_slice(&out)
        };

        let w = self.width;
        let h = self.height;
        // FFmpeg's yuv420p chroma planes use AV_CEIL_RSHIFT(dim, 1), not a
        // truncating /2. For odd dimensions (e.g. 175x143) the chroma planes
        // are 88x72, not 87x71; the reconstructed buffer is padded to the MB
        // grid (176x144) so the extra row/column is present and valid.
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        let planes = vec![
            FramePlane {
                buffer: extract(&self.y, self.stride_y, self.base_y(), w, h),
                offset: 0,
                linesize: w,
            },
            FramePlane {
                buffer: extract(&self.u, self.stride_uv, self.base_uv(), cw, ch),
                offset: 0,
                linesize: cw,
            },
            FramePlane {
                buffer: extract(&self.v, self.stride_uv, self.base_uv(), cw, ch),
                offset: 0,
                linesize: cw,
            },
        ];

        if let FrameData::Video(ref mut video) = frame.data {
            video.planes = planes;
            video.picture_type = if self.is_keyframe {
                PictureType::I
            } else {
                PictureType::P
            };
        }
        frame
    }
}

#[inline]
fn align(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// The four VP8 reference frame slots plus the "current" slot.
#[derive(Default)]
pub struct References {
    pub slots: [Option<FrameBuffer>; 4],
}

impl References {
    pub fn new() -> Self {
        Self {
            slots: [None, None, None, None],
        }
    }

    pub fn clear(&mut self) {
        self.slots.fill(None);
    }

    pub fn get(&self, index: usize) -> Option<&FrameBuffer> {
        self.slots[index].as_ref()
    }

    pub fn put(&mut self, index: usize, frame: FrameBuffer) {
        self.slots[index] = Some(frame);
    }
}
