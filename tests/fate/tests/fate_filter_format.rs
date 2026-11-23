//! Negotiation and passthrough conformance tests for `wedeo-filter-format`.
//!
//! FFmpeg's `format` filter is metadata-only: it has no `filter_frame()`, its
//! entire behavior is `query_formats()` narrowing the pad formats to the
//! configured lists. These tests verify registration, that options reach the
//! factory, that both pads are restricted (and intersect correctly in a
//! graph), and that frames pass through untouched.

// Link the inventory registration under test.
use wedeo_filter_format as _;

use wedeo_core::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_filter_format::{
    Filter, FilterFlags, FilterFormat, FilterFormats, FilterGraph, FilterLinkConfig,
    FilterPadDirection, FormatFilter, FormatsConfig, create_filter_with_options,
    find_filter_by_name,
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
fn format_filter_passthrough_preserves_frame() {
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
        out[0].metadata.len(),
        1,
        "no wedeo.format.* metadata workaround"
    );

    let video = out[0].video().expect("video frame");
    assert_eq!(video.width, 32);
    assert_eq!(video.height, 16);
    // FFmpeg's format filter does not convert pixels; the incoming format is
    // already the negotiated one.
    assert_eq!(video.format, PixelFormat::Yuv420p);
}

#[test]
fn format_filter_restricts_both_video_pads() {
    let filter = FormatFilter::new([PixelFormat::Yuv420p, PixelFormat::Rgb24]);
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();

    let expected = FilterFormats::from_pixels([PixelFormat::Yuv420p, PixelFormat::Rgb24]);
    assert_eq!(cfg.inputs.len(), 1);
    assert_eq!(cfg.outputs.len(), 1);
    assert_eq!(cfg.inputs[0].formats, expected);
    assert_eq!(cfg.outputs[0].formats, expected);
    assert_eq!(cfg.inputs[0].direction, FilterPadDirection::Input);
    assert_eq!(cfg.outputs[0].direction, FilterPadDirection::Output);
    assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(PixelFormat::Rgb24)));
    assert!(!cfg.inputs[0].accepts(FilterFormat::Pixel(PixelFormat::Nv12)));
}

#[test]
fn format_filter_without_options_leaves_pads_unrestricted() {
    let filter = find_filter_by_name("format").unwrap().create().unwrap();
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();
    assert!(cfg.inputs[0].formats.is_any());
    assert!(cfg.outputs[0].formats.is_any());
}

#[test]
fn format_options_reach_the_registered_factory() {
    let filter = create_filter_with_options("format", &[("pix_fmts", "yuv420p|rgb24")]).unwrap();
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();
    assert_eq!(
        cfg.outputs[0].formats,
        FilterFormats::from_pixels([PixelFormat::Yuv420p, PixelFormat::Rgb24])
    );

    // Unknown options are rejected like FFmpeg's av_opt_set().
    assert!(create_filter_with_options("format", &[("bogus", "yuv420p")]).is_err());
}

#[test]
fn format_filter_negotiates_across_graph_links() {
    let mut graph = FilterGraph::new();
    let first = graph
        .add_node_with_options(
            "fmt1".to_string(),
            "format",
            &[("pix_fmts", "yuv420p|rgb24")],
        )
        .unwrap();
    let second = graph
        .add_node_with_options("fmt2".to_string(), "format", &[("pix_fmts", "rgb24")])
        .unwrap();
    graph.add_link(
        first,
        0,
        second,
        0,
        FilterLinkConfig {
            media_type: MediaType::Video,
            time_base: Rational::new(1, 25),
        },
    );

    graph.query_formats().unwrap();
    // The graph narrows both ends of the link to the intersection of the two
    // query_formats() lists. The unlinked input pad keeps its own configured
    // list (both are video pads, so query_formats() restricted them too).
    assert_eq!(
        graph.node_formats(first).unwrap().outputs[0].formats,
        FilterFormats::from_pixels([PixelFormat::Rgb24])
    );
    assert_eq!(
        graph.node_formats(first).unwrap().inputs[0].formats,
        FilterFormats::from_pixels([PixelFormat::Yuv420p, PixelFormat::Rgb24])
    );
    assert_eq!(
        graph.node_formats(second).unwrap().inputs[0].formats,
        FilterFormats::from_pixels([PixelFormat::Rgb24])
    );
    assert_eq!(
        graph.link_formats(0).unwrap(),
        &FilterFormats::from_pixels([PixelFormat::Rgb24])
    );

    // An incompatible pair fails negotiation rather than silently passing
    // frames with the wrong pixel format.
    let mut bad = FilterGraph::new();
    let a = bad
        .add_node_with_options("a".to_string(), "format", &[("pix_fmts", "yuv420p")])
        .unwrap();
    let b = bad
        .add_node_with_options("b".to_string(), "format", &[("pix_fmts", "nv12")])
        .unwrap();
    bad.add_link(
        a,
        0,
        b,
        0,
        FilterLinkConfig {
            media_type: MediaType::Video,
            time_base: Rational::new(1, 25),
        },
    );
    assert!(bad.query_formats().is_err());
}
