//! Unit tests for the functional graph, using a local dummy `Filter`
//! implementation (concurrent filter crates are not visible from this
//! worktree).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use wedeo_core::error::Error;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;
use wedeo_filter::formats::{FilterFormat, FilterFormats, FormatsConfig};
use wedeo_filter::graph::FilterLinkConfig;

use crate::{Graph, filters, find_filter_by_name};

// ---------------------------------------------------------------------------
// Local dummy filter
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DummyState {
    activated: AtomicBool,
    calls: AtomicUsize,
    query_formats_calls: AtomicUsize,
    flush_calls: AtomicUsize,
    tail_emitted: AtomicBool,
}

#[derive(Clone)]
struct DummyFilter {
    desc: &'static wedeo_filter::FilterDescriptor,
    tag: &'static str,
    state: Arc<DummyState>,
    multiply: usize,
    drop_frames: bool,
    formats: Option<&'static [PixelFormat]>,
    tail: Option<i64>,
}

impl DummyFilter {
    fn new(desc: &'static wedeo_filter::FilterDescriptor, tag: &'static str) -> Self {
        Self {
            desc,
            tag,
            state: Arc::new(DummyState::default()),
            multiply: 1,
            drop_frames: false,
            formats: None,
            tail: None,
        }
    }

    fn with_multiply(mut self, n: usize) -> Self {
        self.multiply = n;
        self
    }

    fn dropping(mut self) -> Self {
        self.drop_frames = true;
        self
    }

    /// Restrict the formats advertised by `query_formats()`.
    fn with_formats(mut self, formats: &'static [PixelFormat]) -> Self {
        self.formats = Some(formats);
        self
    }

    /// Emit one tail frame at EOS from `flush()`.
    fn with_tail(mut self, pts: i64) -> Self {
        self.tail = Some(pts);
        self
    }
}

impl wedeo_filter::Filter for DummyFilter {
    fn descriptor(&self) -> &wedeo_filter::FilterDescriptor {
        self.desc
    }

    fn activate(&mut self) -> wedeo_core::error::Result<()> {
        self.state.activated.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn query_formats(&self, cfg: &mut FormatsConfig) -> wedeo_core::error::Result<()> {
        self.state
            .query_formats_calls
            .fetch_add(1, Ordering::SeqCst);
        if let Some(formats) = self.formats {
            cfg.set_common_pixel_formats(formats);
        }
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> wedeo_core::error::Result<Vec<Frame>> {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        if self.drop_frames {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for _ in 0..self.multiply {
            let mut frame = frame.clone();
            let prev = frame.metadata.get("chain").unwrap_or("").to_owned();
            frame.metadata.set("chain", format!("{prev}{}", self.tag));
            out.push(frame);
        }
        Ok(out)
    }

    fn flush(&mut self) -> wedeo_core::error::Result<Vec<Frame>> {
        self.state.flush_calls.fetch_add(1, Ordering::SeqCst);
        let Some(pts) = self.tail else {
            return Ok(Vec::new());
        };
        if self.state.tail_emitted.swap(true, Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let mut frame = video_frame(pts);
        frame.metadata.set("chain", self.tag);
        Ok(vec![frame])
    }
}

use wedeo_filter::{FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection};

const fn pad(
    name: &'static str,
    media_type: MediaType,
    direction: FilterPadDirection,
) -> FilterPadDescriptor {
    FilterPadDescriptor {
        name,
        media_type,
        direction,
    }
}

static DUMMY_V1O1: FilterDescriptor = FilterDescriptor {
    name: "dummy",
    description: "One video input, one video output",
    inputs: &[pad("in", MediaType::Video, FilterPadDirection::Input)],
    outputs: &[pad("out", MediaType::Video, FilterPadDirection::Output)],
    flags: FilterFlags::empty(),
};
static DUMMY_V0O1: FilterDescriptor = FilterDescriptor {
    name: "dummy_source",
    description: "Video source",
    inputs: &[],
    outputs: &[pad("out", MediaType::Video, FilterPadDirection::Output)],
    flags: FilterFlags::empty(),
};
static DUMMY_V1O0: FilterDescriptor = FilterDescriptor {
    name: "dummy_sink",
    description: "Video sink",
    inputs: &[pad("in", MediaType::Video, FilterPadDirection::Input)],
    outputs: &[],
    flags: FilterFlags::empty(),
};
static DUMMY_V1O2: FilterDescriptor = FilterDescriptor {
    name: "dummy_fanout",
    description: "One video input, two video outputs",
    inputs: &[pad("in", MediaType::Video, FilterPadDirection::Input)],
    outputs: &[
        pad("out0", MediaType::Video, FilterPadDirection::Output),
        pad("out1", MediaType::Video, FilterPadDirection::Output),
    ],
    flags: FilterFlags::empty(),
};
const VIDEO_CFG: FilterLinkConfig = FilterLinkConfig {
    media_type: MediaType::Video,
    time_base: Rational::new(1, 25),
};
const AUDIO_CFG: FilterLinkConfig = FilterLinkConfig {
    media_type: MediaType::Audio,
    time_base: Rational::new(1, 48000),
};

fn video_frame(pts: i64) -> Frame {
    let mut frame = Frame::new_video(16, 16, PixelFormat::Yuv420p);
    frame.pts = pts;
    frame
}

fn audio_frame(pts: i64) -> Frame {
    let mut frame = Frame::new_audio(
        1024,
        SampleFormat::S16,
        48000,
        wedeo_core::channel_layout::ChannelLayout::mono(),
    );
    frame.pts = pts;
    frame
}

fn chain_config() -> Graph {
    let mut g = Graph::new();
    let src = g
        .add_node("src", Box::new(DummyFilter::new(&DUMMY_V0O1, "S")))
        .unwrap();
    let a = g
        .add_node("a", Box::new(DummyFilter::new(&DUMMY_V1O1, "A")))
        .unwrap();
    let b = g
        .add_node("b", Box::new(DummyFilter::new(&DUMMY_V1O1, "B")))
        .unwrap();
    let sink = g
        .add_node("sink", Box::new(DummyFilter::new(&DUMMY_V1O0, "K")))
        .unwrap();
    g.link(src, 0, a, 0, VIDEO_CFG).unwrap();
    g.link(a, 0, b, 0, VIDEO_CFG).unwrap();
    g.link(b, 0, sink, 0, VIDEO_CFG).unwrap();
    g
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn builtin_filters_are_registered_by_name() {
    for name in [
        "buffer",
        "abuffer",
        "buffersink",
        "abuffersink",
        "null",
        "anull",
    ] {
        let factory = find_filter_by_name(name)
            .unwrap_or_else(|| panic!("'{name}' filter was not registered"));
        assert_eq!(factory.descriptor().name, name);
    }
    assert!(find_filter_by_name("no_such_filter").is_none());
}

#[test]
fn filters_iterator_exposes_builtins() {
    let names: Vec<&str> = filters().map(|f| f.descriptor().name).collect();
    for name in [
        "buffer",
        "abuffer",
        "buffersink",
        "abuffersink",
        "null",
        "anull",
    ] {
        assert!(names.contains(&name), "'{name}' missing from filters()");
    }
    assert!(Graph::nb_registered_filters() >= names.len());
}

#[test]
fn factory_creates_filter_with_expected_pads() {
    let buffer = find_filter_by_name("buffer").unwrap();
    let d = buffer.descriptor();
    assert!(d.inputs.is_empty());
    assert_eq!(d.outputs.len(), 1);
    assert_eq!(d.outputs[0].media_type, MediaType::Video);

    let sink = find_filter_by_name("abuffersink").unwrap();
    let d = sink.descriptor();
    assert_eq!(d.inputs.len(), 1);
    assert_eq!(d.inputs[0].media_type, MediaType::Audio);
    assert!(d.outputs.is_empty());

    let instance = buffer.create().unwrap();
    assert_eq!(instance.descriptor().name, "buffer");
}

// ---------------------------------------------------------------------------
// Node registry and linking / validation
// ---------------------------------------------------------------------------

#[test]
fn add_node_by_name_uses_registry_and_rejects_duplicates() {
    let mut g = Graph::new();
    let id = g.add_node_by_name("in", "buffer").unwrap();
    assert_eq!(g.nb_nodes(), 1);
    assert_eq!(g.nodes()[id].filter_name, "buffer");
    assert_eq!(g.node_id_by_name("in"), Some(id));
    assert_eq!(g.node_id_by_name("missing"), None);

    let err = g.add_node_by_name("in", "buffer").unwrap_err();
    assert!(matches!(err, Error::Other(_)));

    let err = g.add_node_by_name("x", "does_not_exist").unwrap_err();
    assert!(matches!(err, Error::Other(_)));
}

#[test]
fn link_rejects_out_of_range_pads() {
    let mut g = Graph::new();
    let src = g.add_node_by_name("in", "buffer").unwrap();
    let sink = g.add_node_by_name("out", "buffersink").unwrap();
    assert!(matches!(
        g.link(src, 1, sink, 0, VIDEO_CFG),
        Err(Error::Other(_))
    ));
    assert!(matches!(
        g.link(src, 0, sink, 1, VIDEO_CFG),
        Err(Error::Other(_))
    ));
    assert!(matches!(
        g.link(99, 0, sink, 0, VIDEO_CFG),
        Err(Error::Other(_))
    ));
}

#[test]
fn link_rejects_media_type_mismatch() {
    let mut g = Graph::new();
    let src = g.add_node_by_name("in", "buffer").unwrap();
    let sink = g.add_node_by_name("out", "abuffersink").unwrap();
    let err = g.link(src, 0, sink, 0, VIDEO_CFG).unwrap_err();
    assert!(err.to_string().contains("media type mismatch"));

    let err = g
        .link(
            src,
            0,
            sink,
            0,
            FilterLinkConfig {
                media_type: MediaType::Audio,
                time_base: VIDEO_CFG.time_base,
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("media type mismatch"));
}

#[test]
fn link_rejects_reused_pads() {
    let mut g = Graph::new();
    let src = g.add_node_by_name("in", "null").unwrap();
    let a = g.add_node_by_name("a", "null").unwrap();
    let b = g.add_node_by_name("b", "null").unwrap();
    g.link(src, 0, a, 0, VIDEO_CFG).unwrap();
    assert!(g.link(src, 0, b, 0, VIDEO_CFG).is_err());
    assert!(g.link(src, 0, a, 0, VIDEO_CFG).is_err());
}

#[test]
fn link_rejects_cycles() {
    let mut g = Graph::new();
    let a = g.add_node_by_name("a", "null").unwrap();
    let b = g.add_node_by_name("b", "null").unwrap();
    g.link(a, 0, b, 0, VIDEO_CFG).unwrap();
    assert!(matches!(
        g.link(b, 0, a, 0, VIDEO_CFG),
        Err(Error::Other(_))
    ));
    let c = g.add_node_by_name("c", "null").unwrap();
    assert!(matches!(
        g.link(c, 0, c, 0, VIDEO_CFG),
        Err(Error::Other(_))
    ));
}

#[test]
fn link_by_names_works() {
    let mut g = Graph::new();
    g.add_node_by_name("in", "buffer").unwrap();
    g.add_node_by_name("out", "buffersink").unwrap();
    let id = g.link_by_names("in", 0, "out", 0, VIDEO_CFG).unwrap();
    assert_eq!(g.nb_links(), 1);
    assert_eq!(g.links()[id].src_node, 0);
    assert_eq!(g.links()[id].dst_node, 1);
}

#[test]
fn validate_detects_unconnected_pads() {
    let mut g = Graph::new();
    let a = g.add_node_by_name("a", "null").unwrap();
    assert!(g.validate().is_err());
    let b = g.add_node_by_name("b", "null").unwrap();
    g.link(a, 0, b, 0, VIDEO_CFG).unwrap();
    // a's input and b's output are still unconnected.
    let err = g.validate().unwrap_err();
    assert!(err.to_string().contains("not connected"));

    let src = g.add_node_by_name("src", "buffer").unwrap();
    g.link(src, 0, a, 0, VIDEO_CFG).unwrap();
    let sink = g.add_node_by_name("sink", "buffersink").unwrap();
    g.link(b, 0, sink, 0, VIDEO_CFG).unwrap();
    assert!(g.validate().is_ok());
    assert!(g.config().is_ok());
}

// ---------------------------------------------------------------------------
// Activation and processing
// ---------------------------------------------------------------------------

#[test]
fn config_activates_every_filter() {
    let mut g = Graph::new();
    let mut states = Vec::new();
    for (name, desc, tag) in [
        ("src", &DUMMY_V0O1, "S"),
        ("a", &DUMMY_V1O1, "A"),
        ("sink", &DUMMY_V1O0, "K"),
    ] {
        let filter = DummyFilter::new(desc, tag);
        states.push(filter.state.clone());
        g.add_node(name, Box::new(filter)).unwrap();
    }
    let src = g.node_id_by_name("src").unwrap();
    let a = g.node_id_by_name("a").unwrap();
    let sink = g.node_id_by_name("sink").unwrap();
    g.link(src, 0, a, 0, VIDEO_CFG).unwrap();
    g.link(a, 0, sink, 0, VIDEO_CFG).unwrap();

    assert!(!g.is_active());
    assert!(states.iter().all(|s| !s.activated.load(Ordering::SeqCst)));
    g.config().unwrap();
    assert!(g.is_active());
    assert!(states.iter().all(|s| s.activated.load(Ordering::SeqCst)));
}

#[test]
fn push_run_pull_passes_frames_through_registry_filters() {
    let mut g = Graph::new();
    let src = g.add_node_by_name("in", "buffer").unwrap();
    let nul = g.add_node_by_name("nul", "null").unwrap();
    let sink = g.add_node_by_name("out", "buffersink").unwrap();
    g.link(src, 0, nul, 0, VIDEO_CFG).unwrap();
    g.link(nul, 0, sink, 0, VIDEO_CFG).unwrap();

    g.push(src, video_frame(42)).unwrap();
    assert_eq!(g.output_len(), 0);
    // source -> null -> sink: three filter_frame calls.
    assert_eq!(g.run().unwrap(), 3);
    assert_eq!(g.output_len(), 1);
    assert_eq!(g.pull().unwrap().pts, 42);
    assert_eq!(g.pull().unwrap_err(), Error::Again);
}

#[test]
fn process_applies_dummy_filters_in_chain_order() {
    let mut g = chain_config();
    let frames = g.process(video_frame(7)).unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pts, 7);
    assert_eq!(frames[0].metadata.get("chain"), Some("SABK"));
}

#[test]
fn fifo_order_is_preserved() {
    let mut g = chain_config();
    let src = g.node_id_by_name("src").unwrap();
    for pts in [10, 20, 30] {
        g.push(src, video_frame(pts)).unwrap();
    }
    g.run().unwrap();
    let pts: Vec<i64> = g.drain().iter().map(|f| f.pts).collect();
    assert_eq!(pts, vec![10, 20, 30]);
}

#[test]
fn dropping_filter_removes_frames() {
    let mut g = Graph::new();
    let src = g
        .add_node("src", Box::new(DummyFilter::new(&DUMMY_V0O1, "S")))
        .unwrap();
    let drop = g
        .add_node(
            "drop",
            Box::new(DummyFilter::new(&DUMMY_V1O1, "D").dropping()),
        )
        .unwrap();
    let sink = g
        .add_node("sink", Box::new(DummyFilter::new(&DUMMY_V1O0, "K")))
        .unwrap();
    g.link(src, 0, drop, 0, VIDEO_CFG).unwrap();
    g.link(drop, 0, sink, 0, VIDEO_CFG).unwrap();
    let frames = g.process(video_frame(1)).unwrap();
    assert!(frames.is_empty());
}

#[test]
fn duplicating_filter_produces_multiple_frames() {
    let mut g = Graph::new();
    let src = g
        .add_node("src", Box::new(DummyFilter::new(&DUMMY_V0O1, "S")))
        .unwrap();
    let dup = g
        .add_node(
            "dup",
            Box::new(DummyFilter::new(&DUMMY_V1O1, "D").with_multiply(3)),
        )
        .unwrap();
    let sink = g
        .add_node("sink", Box::new(DummyFilter::new(&DUMMY_V1O0, "K")))
        .unwrap();
    g.link(src, 0, dup, 0, VIDEO_CFG).unwrap();
    g.link(dup, 0, sink, 0, VIDEO_CFG).unwrap();
    let frames = g.process(video_frame(5)).unwrap();
    assert_eq!(frames.len(), 3);
    assert!(frames.iter().all(|f| f.pts == 5));
}

#[test]
fn fanout_routes_frames_to_multiple_sinks() {
    let mut g = Graph::new();
    let src = g
        .add_node("src", Box::new(DummyFilter::new(&DUMMY_V0O1, "S")))
        .unwrap();
    let fan = g
        .add_node(
            "fan",
            Box::new(DummyFilter::new(&DUMMY_V1O2, "F").with_multiply(2)),
        )
        .unwrap();
    let sink_a = g
        .add_node("a", Box::new(DummyFilter::new(&DUMMY_V1O0, "A")))
        .unwrap();
    let sink_b = g
        .add_node("b", Box::new(DummyFilter::new(&DUMMY_V1O0, "B")))
        .unwrap();
    g.link(src, 0, fan, 0, VIDEO_CFG).unwrap();
    g.link(fan, 0, sink_a, 0, VIDEO_CFG).unwrap();
    g.link(fan, 1, sink_b, 0, VIDEO_CFG).unwrap();
    g.push(src, video_frame(11)).unwrap();
    g.run().unwrap();
    let frames = g.drain();
    assert_eq!(frames.len(), 2);
    let chains: Vec<&str> = frames
        .iter()
        .map(|f| f.metadata.get("chain").unwrap_or(""))
        .collect();
    assert!(chains.contains(&"SFA"));
    assert!(chains.contains(&"SFB"));
}

#[test]
fn push_rejects_non_source_and_wrong_media_type() {
    let mut g = chain_config();
    let a = g.node_id_by_name("a").unwrap();
    assert!(g.push(a, video_frame(0)).is_err());

    let err = g
        .push(g.node_id_by_name("src").unwrap(), audio_frame(0))
        .unwrap_err();
    assert!(err.to_string().contains("media type mismatch"));
}

#[test]
fn audio_graph_processes_audio_frames() {
    let mut g = Graph::new();
    let src = g.add_node_by_name("in", "abuffer").unwrap();
    let nul = g.add_node_by_name("nul", "anull").unwrap();
    let sink = g.add_node_by_name("out", "abuffersink").unwrap();
    g.link(src, 0, nul, 0, AUDIO_CFG).unwrap();
    g.link(nul, 0, sink, 0, AUDIO_CFG).unwrap();
    let frames = g.process(audio_frame(99)).unwrap();
    assert_eq!(frames.len(), 1);
    assert!(frames[0].is_audio());
    assert_eq!(frames[0].pts, 99);
}

#[test]
fn close_turns_empty_pull_into_eof() {
    let mut g = chain_config();
    let src = g.node_id_by_name("src").unwrap();
    g.push(src, video_frame(1)).unwrap();
    g.close().unwrap();
    assert_eq!(g.pull().unwrap().pts, 1);
    assert_eq!(g.pull().unwrap_err(), Error::Eof);
    assert!(g.is_eof());
}

#[test]
fn process_requires_exactly_one_source() {
    let mut g = Graph::new();
    assert!(g.process(video_frame(0)).is_err());
    let a = g
        .add_node("a", Box::new(DummyFilter::new(&DUMMY_V0O1, "A")))
        .unwrap();
    let b = g
        .add_node("b", Box::new(DummyFilter::new(&DUMMY_V0O1, "B")))
        .unwrap();
    let sink = g
        .add_node("sink", Box::new(DummyFilter::new(&DUMMY_V1O0, "K")))
        .unwrap();
    g.link(a, 0, sink, 0, VIDEO_CFG).unwrap();
    let _ = b;
    assert!(g.process(video_frame(0)).is_err());
}

// ---------------------------------------------------------------------------
// Format negotiation and EOF flush
// ---------------------------------------------------------------------------

#[test]
fn config_drives_query_formats_on_every_node() {
    let mut g = Graph::new();
    let mut states = Vec::new();
    for (name, desc, tag) in [
        ("src", &DUMMY_V0O1, "S"),
        ("a", &DUMMY_V1O1, "A"),
        ("sink", &DUMMY_V1O0, "K"),
    ] {
        let filter = DummyFilter::new(desc, tag);
        states.push(filter.state.clone());
        g.add_node(name, Box::new(filter)).unwrap();
    }
    let src = g.node_id_by_name("src").unwrap();
    let a = g.node_id_by_name("a").unwrap();
    let sink = g.node_id_by_name("sink").unwrap();
    let first = g.link(src, 0, a, 0, VIDEO_CFG).unwrap();
    let second = g.link(a, 0, sink, 0, VIDEO_CFG).unwrap();

    assert!(
        states
            .iter()
            .all(|s| s.query_formats_calls.load(Ordering::SeqCst) == 0)
    );
    g.config().unwrap();
    assert!(
        states
            .iter()
            .all(|s| s.query_formats_calls.load(Ordering::SeqCst) == 1)
    );

    // Default query_formats() leaves every pad unrestricted; the intersection
    // of two unrestricted lists is still unrestricted.
    assert!(g.node_formats(a).unwrap().inputs[0].formats.is_any());
    assert!(g.link_formats(first).unwrap().is_any());
    assert!(g.link_formats(second).unwrap().is_any());
}

#[test]
fn config_narrows_link_formats_to_intersection() {
    let mut g = Graph::new();
    let src = g
        .add_node(
            "src",
            Box::new(
                DummyFilter::new(&DUMMY_V0O1, "S")
                    .with_formats(&[PixelFormat::Yuv420p, PixelFormat::Nv12]),
            ),
        )
        .unwrap();
    let mid = g
        .add_node(
            "mid",
            Box::new(DummyFilter::new(&DUMMY_V1O1, "M").with_formats(&[PixelFormat::Nv12])),
        )
        .unwrap();
    let sink = g
        .add_node("sink", Box::new(DummyFilter::new(&DUMMY_V1O0, "K")))
        .unwrap();
    let link = g.link(src, 0, mid, 0, VIDEO_CFG).unwrap();
    g.link(mid, 0, sink, 0, VIDEO_CFG).unwrap();

    g.config().unwrap();
    let nv12 = FilterFormats::from_pixels([PixelFormat::Nv12]);
    assert_eq!(g.link_formats(link), Some(&nv12));
    assert_eq!(g.node_formats(src).unwrap().outputs[0].formats, nv12);
    assert_eq!(g.node_formats(mid).unwrap().inputs[0].formats, nv12);
    // The unlinked input/output pads keep their own configured lists.
    assert_eq!(
        g.node_formats(src).unwrap().outputs[0].formats,
        FilterFormats::from_pixels([PixelFormat::Nv12])
    );
    assert_eq!(g.node_formats(sink).unwrap().inputs[0].formats, nv12);
    assert!(
        g.link_formats(link)
            .unwrap()
            .contains(FilterFormat::Pixel(PixelFormat::Nv12))
    );
    assert!(
        !g.link_formats(link)
            .unwrap()
            .contains(FilterFormat::Pixel(PixelFormat::Yuv420p))
    );
}

#[test]
fn config_fails_when_linked_pads_have_no_common_format() {
    let mut g = Graph::new();
    let src = g
        .add_node(
            "src",
            Box::new(DummyFilter::new(&DUMMY_V0O1, "S").with_formats(&[PixelFormat::Yuv420p])),
        )
        .unwrap();
    let sink = g
        .add_node(
            "sink",
            Box::new(DummyFilter::new(&DUMMY_V1O0, "K").with_formats(&[PixelFormat::Nv12])),
        )
        .unwrap();
    g.link(src, 0, sink, 0, VIDEO_CFG).unwrap();
    let err = g.config().unwrap_err();
    assert!(err.to_string().contains("no common format"), "{err}");
}

#[test]
fn close_flushes_filter_tails_to_the_graph_sink() {
    let mut g = Graph::new();
    let src = g
        .add_node("src", Box::new(DummyFilter::new(&DUMMY_V0O1, "S")))
        .unwrap();
    let sink_filter = DummyFilter::new(&DUMMY_V1O0, "K").with_tail(99);
    let sink_state = sink_filter.state.clone();
    let sink = g.add_node("sink", Box::new(sink_filter)).unwrap();
    g.link(src, 0, sink, 0, VIDEO_CFG).unwrap();
    g.config().unwrap();

    g.push(src, video_frame(1)).unwrap();
    g.close().unwrap();
    let frames = g.drain();
    assert_eq!(
        frames.iter().map(|f| f.pts).collect::<Vec<_>>(),
        vec![1, 99]
    );
    assert_eq!(sink_state.flush_calls.load(Ordering::SeqCst), 1);

    assert_eq!(g.flush().unwrap(), 0, "second flush emits no tail");
    assert_eq!(sink_state.flush_calls.load(Ordering::SeqCst), 2);
    assert!(g.drain().is_empty());
    let _ = sink;
}
