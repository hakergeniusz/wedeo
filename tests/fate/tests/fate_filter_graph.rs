//! Conformance / registration tests for `wedeo-filter-graph`.
//!
//! Verifies that the graph crate's built-in source, sink and identity filters
//! are visible through the shared `wedeo_filter` registry and that a graph
//! built entirely from registry factories transports frames end to end.

use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_filter_graph::{FilterLinkConfig, Graph, filters, find_filter_by_name};

const VIDEO_CFG: FilterLinkConfig = FilterLinkConfig {
    media_type: MediaType::Video,
    time_base: Rational::new(1, 25),
};

#[test]
fn graph_builtin_filters_registered() {
    for name in [
        "buffer",
        "abuffer",
        "buffersink",
        "abuffersink",
        "null",
        "anull",
    ] {
        let factory = find_filter_by_name(name)
            .unwrap_or_else(|| panic!("'{name}' filter was not registered by wedeo-filter-graph"));
        assert_eq!(factory.descriptor().name, name);
    }
}

#[test]
fn graph_filters_iterator_contains_builtins() {
    let names: Vec<&str> = filters().map(|f| f.descriptor().name).collect();
    for expected in ["buffer", "buffersink", "null"] {
        assert!(
            names.contains(&expected),
            "filters() missing registered filter '{expected}'"
        );
    }
}

#[test]
fn graph_processes_frame_through_registry_filters() {
    let mut graph = Graph::new();
    let src = graph.add_node_by_name("src", "buffer").unwrap();
    let null = graph.add_node_by_name("null", "null").unwrap();
    let sink = graph.add_node_by_name("sink", "buffersink").unwrap();
    graph.link(src, 0, null, 0, VIDEO_CFG).unwrap();
    graph.link(null, 0, sink, 0, VIDEO_CFG).unwrap();

    let mut frame = Frame::new_video(16, 16, PixelFormat::Yuv420p);
    frame.pts = 123;
    let out = graph.process(frame).unwrap();

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].pts, 123);
    let video = out[0].video().expect("expected a video frame");
    assert_eq!(video.width, 16);
    assert_eq!(video.height, 16);
    assert_eq!(video.format, PixelFormat::Yuv420p);
}
