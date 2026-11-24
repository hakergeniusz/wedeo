//! Conformance / registration tests for `wedeo-filter-graph`.
//!
//! Verifies that the graph crate's built-in source, sink and identity filters
//! are visible through the shared `wedeo_filter` registry and that a graph
//! built entirely from registry factories transports frames end to end.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use wedeo_core::error::Result;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_filter_graph::{
    Filter, FilterDescriptor, FilterFlags, FilterFormat, FilterFormats, FilterLinkConfig,
    FilterPadDescriptor, FilterPadDirection, FormatsConfig, Graph, filters, find_filter_by_name,
};

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

// ---------------------------------------------------------------------------
// Format negotiation and EOF flush driven by the graph
// ---------------------------------------------------------------------------

static TAIL_INPUTS: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "in",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];
static NO_OUTPUTS: [FilterPadDescriptor; 0] = [];
static TAIL_DESCRIPTOR: FilterDescriptor = FilterDescriptor {
    name: "tail_sink",
    description: "Test sink with a delayed tail frame.",
    inputs: &TAIL_INPUTS,
    outputs: &NO_OUTPUTS,
    flags: FilterFlags::empty(),
};

/// Test-only sink that restricts its input format and emits one delayed
/// frame from `Filter::flush()` at end of stream.
struct TailSink {
    queries: Arc<AtomicUsize>,
    flushes: Arc<AtomicUsize>,
}

impl Filter for TailSink {
    fn descriptor(&self) -> &FilterDescriptor {
        &TAIL_DESCRIPTOR
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn query_formats(&self, cfg: &mut FormatsConfig) -> Result<()> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        cfg.set_common_pixel_formats(&[PixelFormat::Yuv420p]);
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        Ok(vec![frame])
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        let call = self.flushes.fetch_add(1, Ordering::SeqCst) + 1;
        if call > 1 {
            return Ok(Vec::new());
        }
        let mut frame = Frame::new_video(16, 16, PixelFormat::Yuv420p);
        frame.pts = 999;
        Ok(vec![frame])
    }
}

#[test]
fn graph_config_runs_query_formats_and_narrows_links() {
    let mut graph = Graph::new();
    let src = graph.add_node_by_name("src", "buffer").unwrap();
    let null = graph.add_node_by_name("null", "null").unwrap();
    let sink = graph.add_node_by_name("sink", "buffersink").unwrap();
    let first = graph.link(src, 0, null, 0, VIDEO_CFG).unwrap();
    graph.link(null, 0, sink, 0, VIDEO_CFG).unwrap();

    // node configs exist as soon as nodes are added; negotiations do not.
    assert!(graph.node_formats(null).is_some());
    assert!(graph.link_formats(first).is_none());

    graph.config().unwrap();
    assert!(graph.is_active());
    assert!(graph.node_formats(null).unwrap().inputs[0].formats.is_any());
    assert!(graph.link_formats(first).unwrap().is_any());
    assert!(graph.node_formats(999).is_none());
    assert!(graph.link_formats(999).is_none());
    assert!(
        graph
            .link_formats(first)
            .unwrap()
            .contains(FilterFormat::Pixel(PixelFormat::Yuv420p))
    );
}

#[test]
fn graph_config_negotiates_restricted_formats() {
    let mut graph = Graph::new();
    let src = graph.add_node_by_name("src", "buffer").unwrap();
    let queries = Arc::new(AtomicUsize::new(0));
    let flushes = Arc::new(AtomicUsize::new(0));
    let sink = graph
        .add_node(
            "sink",
            Box::new(TailSink {
                queries: queries.clone(),
                flushes: flushes.clone(),
            }),
        )
        .unwrap();
    let link = graph.link(src, 0, sink, 0, VIDEO_CFG).unwrap();

    graph.config().unwrap();
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    assert_eq!(
        graph.link_formats(link),
        Some(&FilterFormats::from_pixels([PixelFormat::Yuv420p]))
    );

    let mut frame = Frame::new_video(16, 16, PixelFormat::Yuv420p);
    frame.pts = 42;
    graph.push(src, frame).unwrap();
    graph.close().unwrap();
    let pts: Vec<i64> = graph.drain().iter().map(|frame| frame.pts).collect();
    assert_eq!(pts, vec![42, 999], "close() must drain the filter tail");
    assert_eq!(flushes.load(Ordering::SeqCst), 1);
}
