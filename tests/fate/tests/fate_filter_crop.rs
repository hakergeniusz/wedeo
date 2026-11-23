//! Registration conformance test for `wedeo-filter-crop`.
//!
//! Verifies that the `crop` filter is submitted to the frozen filter registry
//! and that a factory-created instance implements the whole `Filter` contract
//! (descriptor, activate, filter_frame) on a synthetic video frame.

use wedeo_core::PixelFormat;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_filter_crop::registry::{filters, find_filter_by_name};

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
