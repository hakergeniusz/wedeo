//! Conformance / registration tests for `wedeo-filter-copy`.
//!
//! Mirrors FFmpeg's `vf_copy` filter: the registered `copy` filter must copy
//! the input video unchanged to the output, preserving all metadata.

// Ensure the inventory registration from `wedeo-filter-copy` is linked in.
use wedeo_filter_copy as _;

use wedeo_core::frame::Frame;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter_copy::framework::registry::find_filter_by_name;

#[test]
fn copy_filter_is_registered() {
    let factory = find_filter_by_name("copy").expect("copy filter was not registered");
    let d = factory.descriptor();
    assert_eq!(d.name, "copy");
    assert_eq!(
        d.description,
        "Copy the input video unchanged to the output."
    );
    assert_eq!(d.inputs.len(), 1);
    assert_eq!(d.outputs.len(), 1);
}

#[test]
fn copy_filter_passes_frame_through_unchanged() {
    let factory = find_filter_by_name("copy").expect("copy filter was not registered");
    let mut filter = factory.create().unwrap();
    filter.activate().unwrap();

    let mut frame = Frame::new_video(16, 16, PixelFormat::Yuv420p);
    frame.pts = 42;
    frame.duration = 100;
    frame.metadata.set("title", "copy me");
    frame.metadata.set("LANGUAGE", "eng");
    let expected = frame.clone();

    let out = filter.filter_frame(frame).unwrap();
    assert_eq!(out.len(), 1, "copy must emit exactly one frame per input");

    assert_eq!(out[0].pts, expected.pts);
    assert_eq!(out[0].duration, expected.duration);
    assert_eq!(
        out[0].metadata.iter().collect::<Vec<_>>(),
        expected.metadata.iter().collect::<Vec<_>>()
    );
    assert_eq!(out[0].video().unwrap().width, 16);
    assert_eq!(out[0].video().unwrap().height, 16);
    assert_eq!(out[0].video().unwrap().format, PixelFormat::Yuv420p);
}
