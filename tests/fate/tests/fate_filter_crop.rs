//! Registration conformance test for `wedeo-filter-crop`.
//!
//! Verifies that the `crop` filter is submitted to the frozen filter registry
//! and that a factory-created instance implements the whole `Filter` contract
//! (descriptor, activate, filter_frame) on a synthetic video frame.

use wedeo_core::PixelFormat;
use wedeo_core::buffer::Buffer;
use wedeo_core::frame::{Frame, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_filter_crop::registry::{create_filter_with_options, filters, find_filter_by_name};
use wedeo_filter_crop::{FilterFormat, FormatsConfig};

#[test]
fn crop_filter_is_registered() {
    let factory = find_filter_by_name("crop").expect("crop filter was not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "crop");
    assert_eq!(desc.description, "Crop the input video.");
    assert_eq!(desc.inputs.len(), 1);
    assert_eq!(desc.inputs[0].name, "default");
    assert_eq!(desc.inputs[0].media_type, MediaType::Video);
    assert_eq!(desc.outputs.len(), 1);
    assert_eq!(desc.outputs[0].media_type, MediaType::Video);

    assert!(filters().any(|f| f.descriptor().name == "crop"));
}

#[test]
fn registered_factory_produces_a_working_crop_filter() {
    let factory = find_filter_by_name("crop").expect("crop filter was not registered");
    let mut filter = factory.create().expect("failed to create crop filter");
    assert_eq!(filter.descriptor().name, "crop");

    filter.activate().expect("activate failed");

    // Default filter crops to the full input, centered: a 4x4 gray frame
    // passes through with identical geometry.
    let frame = Frame::new_video(4, 4, PixelFormat::Gray8);
    let out = filter.filter_frame(frame).expect("filter_frame failed");
    assert_eq!(out.len(), 1);
    let video = out[0].video().expect("expected a video frame");
    assert_eq!(video.width, 4);
    assert_eq!(video.height, 4);
    assert_eq!(video.format, PixelFormat::Gray8);
}

#[test]
fn crop_query_formats_mirrors_ff_formats_pixdesc_filter() {
    let filter = find_filter_by_name("crop").unwrap().create().unwrap();
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();

    assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(PixelFormat::Yuv420p)));
    assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(PixelFormat::Nv12)));
    assert!(cfg.outputs[0].accepts(FilterFormat::Pixel(PixelFormat::Pal8)));
    // BITSTREAM and SW_FLAT_SUB formats are rejected.
    for rejected in [
        PixelFormat::MonoWhite,
        PixelFormat::MonoBlack,
        PixelFormat::Yuyv422,
        PixelFormat::Uyvy422,
        PixelFormat::None,
    ] {
        assert!(!cfg.outputs[0].accepts(FilterFormat::Pixel(rejected)));
    }
    assert_eq!(cfg.inputs[0].formats, cfg.outputs[0].formats);
    // All 30 representable formats except NONE, monow, monoblack, yuyv422 and
    // uyvy422 survive FFmpeg's crop filter.
    assert_eq!(cfg.outputs[0].formats.len(), 30);
}

#[test]
fn crop_factory_options_reach_the_filter() {
    let mut filter = create_filter_with_options(
        "crop",
        &[("out_w", "4"), ("out_h", "2"), ("x", "2"), ("y", "1")],
    )
    .unwrap();
    filter.activate().unwrap();

    let mut frame = Frame::new_video(8, 8, PixelFormat::Gray8);
    frame.video_mut().unwrap().planes.push(FramePlane {
        buffer: Buffer::from_slice(&vec![0u8; 8 * 8]),
        offset: 0,
        linesize: 8,
    });
    let out = filter.filter_frame(frame).unwrap().remove(0);
    let video = out.video().unwrap();
    assert_eq!((video.width, video.height), (4, 2));
    assert_eq!(video.planes[0].offset, 1 * 8 + 2);

    assert!(create_filter_with_options("crop", &[("bogus", "1")]).is_err());
}
