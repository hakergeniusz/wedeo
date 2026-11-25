#![forbid(unsafe_code)]
//! Work unit: `wedeo-filter-copy`.
//!
//! FFmpeg reference: `libavfilter/vf_copy.c`.
//!
//! A video filter that copies the input video unchanged to the output.

/*
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

/// Re-export of the filter framework so downstream harnesses that do not depend
/// on `wedeo-filter` directly can query the registry in registration tests.
pub use wedeo_filter as framework;

use wedeo_core::error::Result;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_filter::registry::FilterFactory;
use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection,
};

/// The single video input pad, matching FFmpeg's `avfilter_vf_copy_inputs`.
static INPUT_PADS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];

/// The single video output pad, matching FFmpeg's `ff_video_default_filterpad`.
static OUTPUT_PADS: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "default",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];

static DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "copy",
    description: "Copy the input video unchanged to the output.",
    inputs: INPUT_PADS,
    outputs: OUTPUT_PADS,
    flags: FilterFlags::METADATA_ONLY,
};

/// The `copy` video filter.
///
/// FFmpeg allocates a fresh output frame with `ff_get_video_buffer`, copies all
/// frame properties with `av_frame_copy_props`, and copies the plane data with
/// `av_frame_copy`, then frees the input. In this API frames are owned values,
/// so forwarding the frame itself is observationally identical: every byte of
/// plane data and every piece of metadata reaches the output untouched.
struct CopyFilter;

impl Filter for CopyFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    /// Pass the frame through unchanged.
    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        Ok(vec![frame])
    }
}

/// Factory for the `copy` filter, registered via `inventory`.
struct CopyFilterFactory;

impl FilterFactory for CopyFilterFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &DESCRIPTOR
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(CopyFilter))
    }
}

inventory::submit!(&CopyFilterFactory as &dyn FilterFactory);

#[cfg(test)]
mod tests {
    use wedeo_core::buffer::Buffer;
    use wedeo_core::color_primaries::ColorPrimaries;
    use wedeo_core::color_trc::ColorTransferCharacteristic;
    use wedeo_core::frame::{FrameData, FrameFlags, FramePlane, PictureType, VIDEO_MAX_PLANES};
    use wedeo_core::frame_side_data::{FrameSideData, FrameSideDataType};
    use wedeo_core::media_type::MediaType;
    use wedeo_core::pixel_format::PixelFormat;
    use wedeo_core::rational::Rational;

    use super::*;

    /// Build a video frame with distinctive values in every field so that any
    /// mutation by the filter would be detected.
    fn rich_frame() -> Frame {
        let mut frame = Frame::new_video(4, 4, PixelFormat::Yuv420p);

        // Distinct plane bytes and strides.
        let planes: Vec<FramePlane> = (0..3)
            .map(|p| FramePlane {
                buffer: Buffer::from_slice(&[p as u8; 16]),
                offset: p as usize * 2,
                linesize: 8 + p as usize,
            })
            .collect();
        if let FrameData::Video(v) = &mut frame.data {
            v.planes = planes;
            v.picture_type = PictureType::I;
            v.color_range = wedeo_core::frame::ColorRange::Jpeg;
            v.color_space = wedeo_core::frame::ColorSpace::Bt709;
            v.color_primaries = ColorPrimaries::Bt709;
            v.color_trc = ColorTransferCharacteristic::Bt709;
            v.chroma_location = wedeo_core::chroma_location::ChromaLocation::Left;
            v.sample_aspect_ratio = Rational::new(4, 3);
            v.crop_top = 1;
            v.crop_bottom = 2;
            v.crop_left = 3;
            v.crop_right = 4;
        }

        frame.pts = 12345;
        frame.pkt_dts = 999;
        frame.best_effort_timestamp = 12345;
        frame.duration = 512;
        frame.time_base = Rational::new(1, 90000);
        frame.flags = FrameFlags::KEY | FrameFlags::INTERLACED | FrameFlags::TOP_FIRST;
        frame.repeat_pict = 2;
        frame.metadata.set("title", "Copy test");
        frame.metadata.set("Language", "eng");
        frame.add_side_data(FrameSideData {
            data_type: FrameSideDataType::DisplayMatrix,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
        });
        frame.add_side_data(FrameSideData {
            data_type: FrameSideDataType::A53Cc,
            data: vec![0xAA, 0xBB],
        });
        frame
    }

    /// Assert that two frames are byte-for-byte and metadata-for-metadata equal.
    fn assert_frames_identical(expected: &Frame, actual: &Frame) {
        assert_eq!(actual.pts, expected.pts);
        assert_eq!(actual.pkt_dts, expected.pkt_dts);
        assert_eq!(actual.best_effort_timestamp, expected.best_effort_timestamp);
        assert_eq!(actual.duration, expected.duration);
        assert_eq!(actual.time_base, expected.time_base);
        assert_eq!(actual.flags, expected.flags);
        assert_eq!(actual.repeat_pict, expected.repeat_pict);
        assert_eq!(
            actual.metadata.iter().collect::<Vec<_>>(),
            expected.metadata.iter().collect::<Vec<_>>()
        );
        assert_eq!(actual.side_data.len(), expected.side_data.len());
        for (a, e) in actual.side_data.iter().zip(expected.side_data.iter()) {
            assert_eq!(a.data_type, e.data_type);
            assert_eq!(a.data, e.data);
        }

        match (&expected.data, &actual.data) {
            (FrameData::Video(e), FrameData::Video(a)) => {
                assert_eq!(a.width, e.width);
                assert_eq!(a.height, e.height);
                assert_eq!(a.format, e.format);
                assert_eq!(a.picture_type, e.picture_type);
                assert_eq!(a.color_range, e.color_range);
                assert_eq!(a.color_space, e.color_space);
                assert_eq!(a.color_primaries, e.color_primaries);
                assert_eq!(a.color_trc, e.color_trc);
                assert_eq!(a.chroma_location, e.chroma_location);
                assert_eq!(a.sample_aspect_ratio, e.sample_aspect_ratio);
                assert_eq!(a.crop_top, e.crop_top);
                assert_eq!(a.crop_bottom, e.crop_bottom);
                assert_eq!(a.crop_left, e.crop_left);
                assert_eq!(a.crop_right, e.crop_right);
                assert_eq!(a.planes.len(), e.planes.len());
                assert!(a.planes.len() <= VIDEO_MAX_PLANES);
                for (ap, ep) in a.planes.iter().zip(e.planes.iter()) {
                    assert_eq!(ap.offset, ep.offset);
                    assert_eq!(ap.linesize, ep.linesize);
                    assert_eq!(ap.buffer.size(), ep.buffer.size());
                    assert_eq!(ap.buffer.data(), ep.buffer.data());
                }
            }
            _ => panic!("frame data kind changed"),
        }
    }

    #[test]
    fn descriptor_matches_ffmpeg_vf_copy() {
        let filter = CopyFilter;
        let d = filter.descriptor();
        assert_eq!(d.name, "copy");
        assert_eq!(
            d.description,
            "Copy the input video unchanged to the output."
        );
        assert!(d.flags.contains(FilterFlags::METADATA_ONLY));
        assert_eq!(d.inputs.len(), 1);
        assert_eq!(d.inputs[0].name, "default");
        assert_eq!(d.inputs[0].media_type, MediaType::Video);
        assert_eq!(d.inputs[0].direction, FilterPadDirection::Input);
        assert_eq!(d.outputs.len(), 1);
        assert_eq!(d.outputs[0].name, "default");
        assert_eq!(d.outputs[0].media_type, MediaType::Video);
        assert_eq!(d.outputs[0].direction, FilterPadDirection::Output);
    }

    #[test]
    fn activate_succeeds() {
        let mut filter = CopyFilter;
        filter.activate().unwrap();
    }

    #[test]
    fn filter_frame_preserves_bytes_and_metadata() {
        let frame = rich_frame();
        let expected = frame.clone();

        let mut filter = CopyFilter;
        let out = filter.filter_frame(frame).unwrap();

        assert_eq!(out.len(), 1, "copy must emit exactly one frame per input");
        assert_frames_identical(&expected, &out[0]);
    }

    #[test]
    fn filter_frame_is_stateless_across_frames() {
        let mut filter = CopyFilter;
        for n in 0..4 {
            let mut frame = rich_frame();
            frame.pts = n;
            frame.metadata.set("seq", n.to_string());
            let expected = frame.clone();
            let out = filter.filter_frame(frame).unwrap();
            assert_eq!(out.len(), 1);
            assert_frames_identical(&expected, &out[0]);
        }
    }

    #[test]
    fn factory_creates_working_filter() {
        let factory = CopyFilterFactory;
        let d = factory.descriptor();
        assert_eq!(d.name, "copy");
        let mut filter = factory.create().unwrap();
        assert_eq!(filter.descriptor().name, "copy");
        filter.activate().unwrap();
        let frame = rich_frame();
        let expected = frame.clone();
        let out = filter.filter_frame(frame).unwrap();
        assert_eq!(out.len(), 1);
        assert_frames_identical(&expected, &out[0]);
    }

    #[test]
    fn registration_finds_copy_filter() {
        let factory = wedeo_filter::registry::find_filter_by_name("copy")
            .expect("copy filter was not registered via inventory");
        assert_eq!(factory.descriptor().name, "copy");
        let mut filter = factory.create().unwrap();
        let frame = rich_frame();
        let expected = frame.clone();
        let out = filter.filter_frame(frame).unwrap();
        assert_eq!(out.len(), 1);
        assert_frames_identical(&expected, &out[0]);
    }
}
