//! Registration conformance checks for `wedeo-filter-transpose`.
//!
//! The transpose filter is linked into this test binary by the workspace FATE
//! harness, so the scalar transpose behaviour is covered by the crate's own
//! unit tests (synthetic frames for all four directions). Here we verify that
//! the `inventory::submit!` registration is actually visible to the filter
//! registry and that the descriptor exposed to the graph matches FFmpeg's
//! `ff_vf_transpose`.

use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter_transpose as _;
use wedeo_filter_transpose::{
    FilterFormat, FormatsConfig, create_filter_with_options, find_filter_by_name,
};

#[test]
fn transpose_factory_is_registered() {
    assert!(
        wedeo_filter_transpose::is_registered(),
        "transpose filter factory was not submitted to the global inventory"
    );
}

#[test]
fn transpose_descriptor_matches_ffmpeg() {
    let descriptor = &wedeo_filter_transpose::TRANSPOSE_DESCRIPTOR;
    assert_eq!(descriptor.name, "transpose");
    assert_eq!(descriptor.description, "Transpose input video.");
    assert_eq!(descriptor.inputs.len(), 1);
    assert_eq!(descriptor.outputs.len(), 1);
    assert_eq!(descriptor.inputs[0].name, "default");
    assert_eq!(descriptor.outputs[0].name, "default");
}

#[test]
fn transpose_query_formats_matches_ffmpeg_filter() {
    let factory = find_filter_by_name("transpose").unwrap();
    let filter = factory.create().unwrap();
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();

    for accepted in [
        PixelFormat::Yuv420p,
        PixelFormat::Yuv444p,
        PixelFormat::Rgb24,
        PixelFormat::Gray16le,
    ] {
        assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(accepted)));
        assert!(cfg.outputs[0].accepts(FilterFormat::Pixel(accepted)));
    }
    // FFmpeg rejects palette, bitstream and packed 4:2:2 formats.
    for rejected in [
        PixelFormat::None,
        PixelFormat::Pal8,
        PixelFormat::MonoWhite,
        PixelFormat::Yuyv422,
        PixelFormat::Uyvy422,
    ] {
        assert!(!cfg.outputs[0].accepts(FilterFormat::Pixel(rejected)));
    }
}

#[test]
fn transpose_factory_options_reach_the_filter() {
    let mut clock = create_filter_with_options("transpose", &[("dir", "clock")]).unwrap();
    let mut numeric = create_filter_with_options("transpose", &[("dir", "1")]).unwrap();

    for filter in [&mut clock, &mut numeric] {
        let mut frame = wedeo_core::Frame::new_video(2, 3, PixelFormat::Gray8);
        frame
            .video_mut()
            .unwrap()
            .planes
            .push(wedeo_core::frame::FramePlane {
                buffer: wedeo_core::buffer::Buffer::from_slice(&[1u8, 2, 3, 4, 5, 6]),
                offset: 0,
                linesize: 2,
            });
        let out = filter.filter_frame(frame).unwrap().remove(0);
        assert_eq!(
            (out.video().unwrap().width, out.video().unwrap().height),
            (3, 2)
        );
    }

    assert!(create_filter_with_options("transpose", &[("dir", "4")]).is_err());
    assert!(create_filter_with_options("transpose", &[("bogus", "1")]).is_err());
}
