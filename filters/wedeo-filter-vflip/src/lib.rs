/*
 * Copyright (c) 2007 Bobby Bingham
 *
 * This file is part of FFmpeg.
 *
 * FFmpeg is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Lesser General Public
 * License as published by the Free Software Foundation; either
 * version 2.1 of the License, or (at your option) any later version.
 *
 * FFmpeg is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Lesser General Public License for more details.
 *
 * You should have received a copy of the GNU Lesser General Public
 * License along with FFmpeg; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA 02110-1301 USA
 */

//! Video vertical flip filter.
//!
//! FFmpeg reference: `libavfilter/vf_vflip.c`.
//!
//! FFmpeg implements the flip as a zero-copy trick: it points each plane at
//! its last row and negates the stride. Wedeo stores strides as `usize`, so
//! the equivalent result is produced here by reversing the order of the rows
//! inside each plane's buffer. The visible pixel data is identical.

use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter::registry::FilterFactory;
// Re-exported so the FATE conformance test can drive the filter even though
// the wedeo-fate harness has no direct dependency on wedeo-filter.
pub use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection, registry,
};

/// Amount to shift the luma height right to find the chroma height, matching
/// FFmpeg's `desc->log2_chroma_h` for the formats supported by wedeo-core.
fn log2_chroma_h(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Yuv420p
        | PixelFormat::Yuvj420p
        | PixelFormat::Nv12
        | PixelFormat::Nv21
        | PixelFormat::Yuv420p16le
        | PixelFormat::Yuv420p16be
        | PixelFormat::Yuv420p10le
        | PixelFormat::Yuv420p10be => 1,
        PixelFormat::Yuv410p => 2,
        _ => 0,
    }
}

/// `!!(desc->flags & AV_PIX_FMT_FLAG_BAYER)`.
///
/// wedeo-core's `PixelFormat` subset currently contains no Bayer formats, so
/// this is always false today. The Bayer code path below is kept so the
/// filter stays correct if such formats are added later.
fn is_bayer(_format: PixelFormat) -> bool {
    false
}

/// `AV_CEIL_RSHIFT(h, vsub)`.
fn ceil_rshift(value: usize, shift: u32) -> usize {
    if shift == 0 {
        value
    } else {
        value.wrapping_add((1usize << shift) - 1) >> shift
    }
}

/// Swap two equal-length, non-overlapping regions of `data` in place.
fn swap_range(data: &mut [u8], a: usize, b: usize, len: usize) {
    if a == b || len == 0 {
        return;
    }
    if a + len > data.len() || b + len > data.len() {
        return;
    }
    if a < b {
        if a + len > b {
            // Overlapping ranges: fall back to element-wise swapping.
            for k in 0..len {
                data.swap(a + k, b + k);
            }
        } else {
            let (lo, hi) = data.split_at_mut(b);
            lo[a..a + len].swap_with_slice(&mut hi[..len]);
        }
    } else {
        swap_range(data, b, a, len);
    }
}

/// Reverse the order of `rows` rows in a plane, respecting the plane stride.
fn flip_plane(plane: &mut FramePlane, rows: usize) {
    if rows < 2 || plane.linesize == 0 {
        return;
    }

    // Frames may share their backing buffer (copy-on-write).
    plane.buffer.make_writable();

    let off = plane.offset;
    let available = plane.buffer.data().len().saturating_sub(off);
    // Normally `available == rows * linesize`. Falling back to the packed row
    // size also handles buffers that store only the active pixels per row.
    let row_bytes = plane.linesize.min(available / rows);
    if row_bytes == 0 {
        return;
    }

    let data = plane.buffer.data_mut();
    for row in 0..rows / 2 {
        let a = off + row * row_bytes;
        let b = off + (rows - 1 - row) * row_bytes;
        swap_range(data, a, b, row_bytes);
    }
}

/// `flip_bayer()` from vf_vflip.c: reverse the order of row *pairs* so the
/// Bayer CFA pattern (which alternates between rows) stays intact.
fn flip_bayer(frame: &mut Frame, height: usize) -> Result<()> {
    if height & 1 != 0 {
        // "Bayer vertical flip needs even height" -> AVERROR_INVALIDDATA
        return Err(Error::InvalidData);
    }

    let video = frame.video_mut().ok_or(Error::Bug)?;
    let Some(plane) = video.planes.first_mut() else {
        return Ok(());
    };

    if plane.linesize == 0 {
        return Ok(());
    }
    plane.buffer.make_writable();

    let off = plane.offset;
    let available = plane.buffer.data().len().saturating_sub(off);
    let row_bytes = plane.linesize.min(available / height.max(1));
    if row_bytes == 0 {
        return Ok(());
    }

    let pairs = height / 2;
    let data = plane.buffer.data_mut();
    for j in 0..pairs / 2 {
        let src = 2 * j;
        let dst = 2 * (pairs - 1 - j);

        let a = off + src * row_bytes;
        let b = off + dst * row_bytes;
        swap_range(data, a, b, row_bytes);

        let a_next = a + row_bytes;
        let b_next = b + row_bytes;
        swap_range(data, a_next, b_next, row_bytes);
    }

    Ok(())
}

static INPUT_PADS: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];
static OUTPUT_PADS: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

static DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "vflip",
    description: "Flip the input video vertically.",
    inputs: &INPUT_PADS,
    outputs: &OUTPUT_PADS,
    flags: FilterFlags::SUPPORT_TIMELINE_GENERIC,
};

/// The `vflip` filter instance. All state is derived from the frame format
/// inside `filter_frame` (the Wedeo `Filter` trait has no config_props step).
pub struct VflipFilter;

impl VflipFilter {
    fn new() -> Self {
        Self
    }
}

impl Filter for VflipFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let mut frame = frame;

        let (height, format) = {
            let video = frame.video().ok_or(Error::InvalidData)?;
            (video.height, video.format)
        };

        if is_bayer(format) {
            flip_bayer(&mut frame, height as usize)?;
        } else {
            let vsub = log2_chroma_h(format);
            let height = height as usize;
            let video = frame.video_mut().ok_or(Error::Bug)?;
            for (i, plane) in video.planes.iter_mut().enumerate() {
                let vsub = if i == 1 || i == 2 { vsub } else { 0 };
                let plane_height = ceil_rshift(height, vsub);
                flip_plane(plane, plane_height);
            }
        }

        // The frame (including pts, flags, metadata and side data) is carried
        // through unchanged, matching FFmpeg's av_frame_copy_props semantics.
        Ok(vec![frame])
    }
}

struct VflipFilterFactory;

impl FilterFactory for VflipFilterFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(VflipFilter::new()))
    }
}

inventory::submit!(&VflipFilterFactory as &dyn FilterFactory);

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_core::buffer::Buffer;
    use wedeo_core::frame::FrameFlags;
    use wedeo_core::rational::Rational;

    fn plane(data: &[u8], linesize: usize) -> FramePlane {
        FramePlane {
            buffer: Buffer::from_slice(data),
            offset: 0,
            linesize,
        }
    }

    fn make_frame(width: u32, height: u32, format: PixelFormat, planes: Vec<FramePlane>) -> Frame {
        let mut frame = Frame::new_video(width, height, format);
        if let Some(video) = frame.video_mut() {
            video.planes = planes;
        }
        frame
    }

    fn plane_bytes(frame: &Frame, index: usize) -> Vec<u8> {
        frame.video().unwrap().planes[index].buffer.data().to_vec()
    }

    fn flip(frame: Frame) -> Frame {
        let mut filter = VflipFilter::new();
        filter.activate().unwrap();
        let mut out = filter.filter_frame(frame).unwrap();
        assert_eq!(out.len(), 1);
        out.remove(0)
    }

    #[test]
    fn test_descriptor() {
        let filter = VflipFilter::new();
        let desc = filter.descriptor();
        assert_eq!(desc.name, "vflip");
        assert_eq!(desc.description, "Flip the input video vertically.");
        assert_eq!(desc.inputs.len(), 1);
        assert_eq!(desc.inputs[0].name, "default");
        assert_eq!(desc.inputs[0].media_type, MediaType::Video);
        assert_eq!(desc.inputs[0].direction, FilterPadDirection::Input);
        assert_eq!(desc.outputs.len(), 1);
        assert_eq!(desc.outputs[0].direction, FilterPadDirection::Output);
        assert!(desc.flags.contains(FilterFlags::SUPPORT_TIMELINE_GENERIC));
    }

    #[test]
    fn test_registration() {
        let factory = wedeo_filter::registry::find_filter_by_name("vflip")
            .expect("vflip filter must be registered");
        assert_eq!(factory.descriptor().name, "vflip");
        let filter = factory.create().unwrap();
        assert_eq!(filter.descriptor().name, "vflip");
    }

    #[test]
    fn test_flip_rgb24() {
        // 2x2 RGB24 image: rows [A B] and [C D].
        let data = vec![
            10, 11, 12, 20, 21, 22, // row 0
            30, 31, 32, 40, 41, 42, // row 1
        ];
        let frame = make_frame(2, 2, PixelFormat::Rgb24, vec![plane(&data, 6)]);
        let out = flip(frame);
        assert_eq!(
            plane_bytes(&out, 0),
            vec![30, 31, 32, 40, 41, 42, 10, 11, 12, 20, 21, 22]
        );
    }

    #[test]
    fn test_flip_yuv420p_with_chroma_subsampling() {
        // 4x4 luma, 2x2 chroma planes.
        let y: Vec<u8> = (1..=16).collect();
        let u = vec![1, 2, 3, 4];
        let v = vec![5, 6, 7, 8];
        let frame = make_frame(
            4,
            4,
            PixelFormat::Yuv420p,
            vec![plane(&y, 4), plane(&u, 2), plane(&v, 2)],
        );
        let out = flip(frame);
        assert_eq!(
            plane_bytes(&out, 0),
            vec![
                13, 14, 15, 16, // old row 3
                9, 10, 11, 12, // old row 2
                5, 6, 7, 8, // old row 1
                1, 2, 3, 4, // old row 0
            ]
        );
        assert_eq!(plane_bytes(&out, 1), vec![3, 4, 1, 2]);
        assert_eq!(plane_bytes(&out, 2), vec![7, 8, 5, 6]);
    }

    #[test]
    fn test_flip_respects_linesize_padding() {
        // 2x3 GRAY8 with a 4-byte stride (2 bytes of padding per row).
        let data = vec![
            1, 2, 0xaa, 0xaa, // row 0
            3, 4, 0xaa, 0xaa, // row 1
            5, 6, 0xaa, 0xaa, // row 2
        ];
        let frame = make_frame(2, 3, PixelFormat::Gray8, vec![plane(&data, 4)]);
        let out = flip(frame);
        assert_eq!(
            plane_bytes(&out, 0),
            vec![5, 6, 0xaa, 0xaa, 3, 4, 0xaa, 0xaa, 1, 2, 0xaa, 0xaa]
        );
    }

    #[test]
    fn test_flip_single_row_is_noop() {
        let data = vec![1, 2, 3, 4];
        let frame = make_frame(4, 1, PixelFormat::Gray8, vec![plane(&data, 4)]);
        let out = flip(frame);
        assert_eq!(plane_bytes(&out, 0), data);
    }

    #[test]
    fn test_flip_no_planes_is_noop() {
        let frame = make_frame(4, 4, PixelFormat::Yuv420p, vec![]);
        let out = flip(frame);
        assert!(out.video().unwrap().planes.is_empty());
    }

    #[test]
    fn test_flip_preserves_metadata_and_properties() {
        let mut frame = make_frame(2, 2, PixelFormat::Gray8, vec![plane(&[1, 2, 3, 4], 2)]);
        frame.pts = 42;
        frame.pkt_dts = 41;
        frame.duration = 7;
        frame.time_base = Rational::new(1, 25);
        frame.repeat_pict = 2;
        frame.metadata.set("title", "flipped");
        frame.metadata.set("LANGUAGE", "pl");
        frame.flags |= FrameFlags::KEY;

        let out = flip(frame);
        assert_eq!(out.pts, 42);
        assert_eq!(out.pkt_dts, 41);
        assert_eq!(out.duration, 7);
        assert_eq!(out.time_base, Rational::new(1, 25));
        assert_eq!(out.repeat_pict, 2);
        assert_eq!(out.metadata.get("title"), Some("flipped"));
        assert_eq!(out.metadata.get("language"), Some("pl"));
        assert!(out.flags.contains(FrameFlags::KEY));
        assert_eq!(out.video().unwrap().height, 2);
    }

    #[test]
    fn test_flip_twice_is_identity() {
        let original: Vec<u8> = (1..=16).collect();
        let frame = make_frame(4, 4, PixelFormat::Gray8, vec![plane(&original, 4)]);
        let out = flip(flip(frame));
        assert_eq!(plane_bytes(&out, 0), original);
    }

    #[test]
    fn test_flip_cow_buffer() {
        // A shared buffer must be copied before the rows are reversed.
        let shared = Buffer::from_slice(&[1, 2, 3, 4, 5, 6]);
        let other = shared.clone();
        let frame = make_frame(
            2,
            3,
            PixelFormat::Gray8,
            vec![FramePlane {
                buffer: shared,
                offset: 0,
                linesize: 2,
            }],
        );
        let out = flip(frame);
        assert_eq!(plane_bytes(&out, 0), vec![5, 6, 3, 4, 1, 2]);
        assert_eq!(other.data(), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn test_flip_rejects_audio_frame() {
        use wedeo_core::channel_layout::ChannelLayout;
        use wedeo_core::sample_format::SampleFormat;
        let frame = Frame::new_audio(128, SampleFormat::Flt, 48000, ChannelLayout::stereo());
        let mut filter = VflipFilter::new();
        assert_eq!(filter.filter_frame(frame).err(), Some(Error::InvalidData));
    }

    #[test]
    fn test_bayer_requires_even_height() {
        let mut frame = make_frame(2, 3, PixelFormat::Gray8, vec![plane(&[0; 6], 2)]);
        assert_eq!(flip_bayer(&mut frame, 3), Err(Error::InvalidData));
    }

    #[test]
    fn test_bayer_flips_row_pairs() {
        let mut frame = make_frame(
            2,
            4,
            PixelFormat::Gray8,
            vec![plane(&[1, 1, 2, 2, 3, 3, 4, 4], 2)],
        );
        flip_bayer(&mut frame, 4).unwrap();
        assert_eq!(plane_bytes(&frame, 0), vec![3, 3, 4, 4, 1, 1, 2, 2]);
    }
}
