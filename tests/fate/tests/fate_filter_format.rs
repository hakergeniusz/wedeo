//! Registration and passthrough conformance tests for `wedeo-filter-format`.
//!
//! FFmpeg's `format` filter is metadata-only: it never modifies frame data.
//! These tests verify registration, that no pixel conversion happens, and
//! that the requested format lists are recorded on the frame (the maximal
//! behavior allowed by the frozen Wedeo `Filter` interface).

// Link the inventory registration under test.
use wedeo_filter_format as _;

use wedeo_core::Frame;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter_format::{
    Filter, FilterFlags, FormatFilter, METADATA_PIX_FMTS, find_filter_by_name,
};

#[test]
fn format_filter_is_registered() {
    let factory = find_filter_by_name("format").expect("format filter must be registered");
    assert_eq!(factory.descriptor().name, "format");
    assert_eq!(
        factory.descriptor().description,
        "Convert the input video to one of the specified pixel formats."
    );
    assert!(
        factory
            .descriptor()
            .flags
            .contains(FilterFlags::METADATA_ONLY)
    );

    let filter = factory.create().expect("create format filter");
    assert_eq!(filter.descriptor().name, "format");
}

#[test]
fn format_filter_passthrough_preserves_frame_and_records_request() {
    let mut filter = FormatFilter::new([PixelFormat::Yuv420p, PixelFormat::Rgb24]);
    filter.activate().unwrap();

    let mut frame = Frame::new_video(32, 16, PixelFormat::Yuv420p);
    frame.pts = 99;
    frame.metadata.set("title", "original");

    let out = filter.filter_frame(frame).unwrap();
    assert_eq!(out.len(), 1, "format is a 1-in/1-out passthrough filter");
    assert_eq!(out[0].pts, 99);
    assert_eq!(out[0].metadata.get("title"), Some("original"));
    assert_eq!(
        out[0].metadata.get(METADATA_PIX_FMTS),
        Some("yuv420p|rgb24")
    );

    let video = out[0].video().expect("video frame");
    assert_eq!(video.width, 32);
    assert_eq!(video.height, 16);
    // FFmpeg's format filter does not convert pixels; the incoming format is
    // already the negotiated one. Wedeo cannot negotiate, so leave it alone.
    assert_eq!(video.format, PixelFormat::Yuv420p);
}
