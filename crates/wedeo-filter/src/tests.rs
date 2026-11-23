//! Unit tests for the framework additions: format negotiation, EOF flush and
//! filter options.

use std::sync::Mutex;

use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;

use crate::formats::{FilterFormat, FilterFormats, FormatsConfig};
use crate::graph::{FilterGraph, FilterLinkConfig};
use crate::registry::{
    FilterFactory, create_filter_by_name, create_filter_with_options, find_filter_by_name,
};
use crate::{Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection};

static VIDEO_OUT: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "out",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];
static VIDEO_IN: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "in",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];
static AUDIO_OUT: &[FilterPadDescriptor] = &[FilterPadDescriptor {
    name: "audio",
    media_type: MediaType::Audio,
    direction: FilterPadDirection::Output,
}];
static NO_PADS: &[FilterPadDescriptor] = &[];

static SOURCE_DESC: FilterDescriptor = FilterDescriptor {
    name: "test-source",
    description: "test source",
    inputs: NO_PADS,
    outputs: VIDEO_OUT,
    flags: FilterFlags::empty(),
};
static SINK_DESC: FilterDescriptor = FilterDescriptor {
    name: "test-sink",
    description: "test sink",
    inputs: VIDEO_IN,
    outputs: NO_PADS,
    flags: FilterFlags::empty(),
};
static MIXED_DESC: FilterDescriptor = FilterDescriptor {
    name: "test-mixed",
    description: "test mixed filter",
    inputs: VIDEO_IN,
    outputs: AUDIO_OUT,
    flags: FilterFlags::empty(),
};
static OPTION_DESC: FilterDescriptor = FilterDescriptor {
    name: "test-option-filter",
    description: "test filter with options",
    inputs: NO_PADS,
    outputs: NO_PADS,
    flags: FilterFlags::empty(),
};

/// Filter used to exercise the new trait methods.
struct MockFilter {
    desc: &'static FilterDescriptor,
    formats: Option<FilterFormats>,
    tail: usize,
    flushed: bool,
}

impl MockFilter {
    fn new(desc: &'static FilterDescriptor, formats: Option<FilterFormats>, tail: usize) -> Self {
        Self {
            desc,
            formats,
            tail,
            flushed: false,
        }
    }
}

impl Filter for MockFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        self.desc
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        Ok(vec![frame])
    }

    fn query_formats(&self, cfg: &mut FormatsConfig) -> Result<()> {
        if let Some(formats) = &self.formats {
            cfg.set_common_formats(formats.clone());
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        if self.flushed {
            return Ok(Vec::new());
        }
        self.flushed = true;
        Ok((0..self.tail)
            .map(|i| Frame::new_video(i as u32 + 1, 1, PixelFormat::Yuv420p))
            .collect())
    }
}

/// Implements only the original required methods, to prove old filters build.
struct BareFilter;

impl Filter for BareFilter {
    fn descriptor(&self) -> &FilterDescriptor {
        &SOURCE_DESC
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        Ok(vec![frame])
    }
}

struct DefaultFactory;

impl FilterFactory for DefaultFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &SOURCE_DESC
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(BareFilter))
    }
}

static OPTIONS_SEEN: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

struct OptionFactory;

impl FilterFactory for OptionFactory {
    fn descriptor(&self) -> &FilterDescriptor {
        &OPTION_DESC
    }

    fn create(&self) -> Result<Box<dyn Filter>> {
        Ok(Box::new(BareFilter))
    }

    fn create_with_options(&self, options: &[(&str, &str)]) -> Result<Box<dyn Filter>> {
        let mut seen = OPTIONS_SEEN.lock().unwrap_or_else(|e| e.into_inner());
        seen.clear();
        seen.extend(
            options
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        );
        Ok(Box::new(BareFilter))
    }
}

inventory::submit!(&OptionFactory as &dyn FilterFactory);

fn pixels(formats: &[PixelFormat]) -> FilterFormats {
    FilterFormats::from_pixels(formats.iter().copied())
}

#[test]
fn filter_format_raw_conversion() {
    assert_eq!(
        FilterFormat::Pixel(PixelFormat::Nv12).as_raw(),
        PixelFormat::Nv12 as i32
    );
    assert_eq!(
        FilterFormat::from_raw(MediaType::Video, PixelFormat::Nv12 as i32),
        Some(FilterFormat::Pixel(PixelFormat::Nv12))
    );
    assert_eq!(
        FilterFormat::from_raw(MediaType::Audio, SampleFormat::Fltp as i32),
        Some(FilterFormat::Sample(SampleFormat::Fltp))
    );
    assert_eq!(FilterFormat::from_raw(MediaType::Video, 9999), None);
    assert_eq!(FilterFormat::from_raw(MediaType::Data, 0), None);
}

#[test]
fn filter_formats_lists_and_intersection() {
    let formats = pixels(&[PixelFormat::Yuv420p, PixelFormat::Nv12]);
    assert_eq!(formats.len(), 2);
    assert!(formats.contains(FilterFormat::Pixel(PixelFormat::Nv12)));
    assert_eq!(
        formats.to_pixels(),
        vec![PixelFormat::Yuv420p, PixelFormat::Nv12]
    );
    assert!(formats.to_samples().is_empty());

    let other = pixels(&[PixelFormat::Nv12, PixelFormat::Rgb24]);
    assert_eq!(formats.intersect(&other), pixels(&[PixelFormat::Nv12]));
    assert_eq!(FilterFormats::Any.intersect(&other), other);
    assert_eq!(formats.intersect(&FilterFormats::Any), formats);
    assert!(formats.intersect(&pixels(&[PixelFormat::Rgb24])).is_empty());
    assert!(FilterFormats::Any.is_any());
    assert!(FilterFormats::List(Vec::new()).is_empty());

    assert_eq!(
        FilterFormats::from_raw_list(MediaType::Audio, &[SampleFormat::S16 as i32]).unwrap(),
        FilterFormats::from_samples([SampleFormat::S16])
    );
    assert_eq!(
        FilterFormats::from_raw_list(MediaType::Video, &[9999]),
        Err(Error::Other(
            "unsupported video format number 9999".to_owned()
        ))
    );
}

#[test]
fn formats_config_from_descriptor_and_common_formats() {
    let mut cfg = FormatsConfig::from_descriptor(&MIXED_DESC);
    assert_eq!(cfg.inputs.len(), 1);
    assert_eq!(cfg.outputs.len(), 1);
    assert!(cfg.inputs[0].formats.is_any());
    assert!(cfg.outputs[0].formats.is_any());
    assert_eq!(cfg.inputs[0].name, "in");
    assert_eq!(cfg.outputs[0].direction, FilterPadDirection::Output);

    cfg.set_common_pixel_formats(&[PixelFormat::Yuv420p, PixelFormat::Nv12]);
    assert_eq!(
        cfg.inputs[0].formats,
        pixels(&[PixelFormat::Yuv420p, PixelFormat::Nv12])
    );
    assert!(
        cfg.outputs[0].formats.is_any(),
        "audio pad must be untouched"
    );

    cfg.set_common_sample_formats(&[SampleFormat::Fltp]);
    assert_eq!(
        cfg.outputs[0].formats,
        FilterFormats::from_samples([SampleFormat::Fltp])
    );
    assert_eq!(cfg.pad(FilterPadDirection::Input, 0).unwrap().name, "in");
    assert!(cfg.pad(FilterPadDirection::Input, 1).is_err());
    cfg.pad_mut(FilterPadDirection::Output, 0).unwrap().formats = FilterFormats::Any;
    assert!(cfg.pad_mut(FilterPadDirection::Output, 1).is_err());
    assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(PixelFormat::Nv12)));
}

#[test]
fn formats_config_negotiate() {
    let mut cfg = FormatsConfig::from_descriptor(&SOURCE_DESC);
    cfg.outputs[0].formats = pixels(&[PixelFormat::Yuv420p, PixelFormat::Nv12]);
    cfg.inputs.push(VIDEO_IN[0].default_formats());
    cfg.inputs[0].formats = pixels(&[PixelFormat::Nv12, PixelFormat::Rgb24]);
    assert_eq!(cfg.negotiate(0, 0).unwrap(), pixels(&[PixelFormat::Nv12]));

    cfg.inputs[0].formats = pixels(&[PixelFormat::Rgb24]);
    assert!(cfg.negotiate(0, 0).is_err());

    cfg.inputs[0].media_type = MediaType::Audio;
    assert!(matches!(cfg.negotiate(0, 0), Err(Error::InvalidArgument)));
    assert!(cfg.negotiate(3, 0).is_err());
}

#[test]
fn default_filter_methods_are_noops() {
    let mut filter = BareFilter;
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();
    assert!(cfg.inputs.is_empty());
    assert!(cfg.outputs[0].formats.is_any());
    assert!(filter.flush().unwrap().is_empty());

    let frame = Frame::new_video(2, 2, PixelFormat::Yuv420p);
    assert_eq!(filter.filter_frame(frame).unwrap().len(), 1);
    filter.activate().unwrap();
}

fn link_config() -> FilterLinkConfig {
    FilterLinkConfig {
        media_type: MediaType::Video,
        time_base: Rational::new(1, 25),
    }
}

#[test]
fn graph_query_formats_negotiates_across_links() {
    let mut graph = FilterGraph::new();
    graph.add_node(
        "source".to_owned(),
        Box::new(MockFilter::new(
            &SOURCE_DESC,
            Some(pixels(&[PixelFormat::Yuv420p, PixelFormat::Nv12])),
            0,
        )),
    );
    graph.add_node(
        "sink".to_owned(),
        Box::new(MockFilter::new(
            &SINK_DESC,
            Some(pixels(&[PixelFormat::Nv12, PixelFormat::Rgb24])),
            0,
        )),
    );
    let link = graph.add_link(0, 0, 1, 0, link_config());

    graph.query_formats().unwrap();
    assert_eq!(graph.nb_nodes(), 2);
    assert_eq!(graph.nb_links(), 1);
    assert_eq!(
        graph.link_formats(link),
        Some(&pixels(&[PixelFormat::Nv12]))
    );
    assert_eq!(
        graph.node_formats(0).unwrap().outputs[0].formats,
        pixels(&[PixelFormat::Nv12])
    );
    assert_eq!(
        graph.node_formats(1).unwrap().inputs[0].formats,
        pixels(&[PixelFormat::Nv12])
    );
    assert!(graph.node_formats(2).is_none());
    assert!(graph.link_formats(3).is_none());
}

#[test]
fn graph_query_formats_reports_incompatible_links() {
    let mut graph = FilterGraph::new();
    graph.add_node(
        "a".to_owned(),
        Box::new(MockFilter::new(
            &SOURCE_DESC,
            Some(pixels(&[PixelFormat::Yuv420p])),
            0,
        )),
    );
    graph.add_node(
        "b".to_owned(),
        Box::new(MockFilter::new(
            &SINK_DESC,
            Some(pixels(&[PixelFormat::Nv12])),
            0,
        )),
    );
    graph.add_link(0, 0, 1, 0, link_config());

    let err = graph.query_formats().unwrap_err();
    assert!(
        err.to_string().contains("a:0 -> b:0"),
        "unexpected error: {err}"
    );
    assert!(err.to_string().contains("no common format"));
}

#[test]
fn graph_flush_collects_node_tail_frames() {
    let mut graph = FilterGraph::new();
    graph.add_node(
        "first".to_owned(),
        Box::new(MockFilter::new(&SOURCE_DESC, None, 2)),
    );
    graph.add_node(
        "second".to_owned(),
        Box::new(MockFilter::new(&SINK_DESC, None, 0)),
    );
    graph.add_node(
        "third".to_owned(),
        Box::new(MockFilter::new(&SINK_DESC, None, 3)),
    );

    let frames = graph.flush().unwrap();
    assert_eq!(frames.len(), 5);
    assert_eq!(frames[0].video().unwrap().width, 1);
    assert_eq!(frames[4].video().unwrap().width, 3);
    assert!(graph.flush().unwrap().is_empty());
    assert!(graph.flush_node(0).unwrap().is_empty());
    assert!(graph.flush_node(9).is_err());
}

#[test]
fn filter_factory_options() {
    let factory = DefaultFactory;
    assert!(factory.create_with_options(&[("ignored", "yes")]).is_ok());
    assert!(factory.create().is_ok());

    assert!(create_filter_by_name("no-such-filter").is_err());
    assert!(create_filter_with_options("no-such-filter", &[]).is_err());
    assert!(find_filter_by_name("test-option-filter").is_some());

    create_filter_with_options("test-option-filter", &[("first", "1"), ("second", "2")]).unwrap();
    assert_eq!(
        *OPTIONS_SEEN.lock().unwrap_or_else(|e| e.into_inner()),
        vec![
            ("first".to_owned(), "1".to_owned()),
            ("second".to_owned(), "2".to_owned()),
        ]
    );
}

#[test]
fn graph_add_node_with_options() {
    let mut graph = FilterGraph::new();
    let id = graph
        .add_node_with_options(
            "configured".to_owned(),
            "test-option-filter",
            &[("key", "value")],
        )
        .unwrap();
    assert_eq!(id, 0);
    assert_eq!(graph.nb_nodes(), 1);
    assert_eq!(
        *OPTIONS_SEEN.lock().unwrap_or_else(|e| e.into_inner()),
        vec![("key".to_owned(), "value".to_owned())]
    );

    assert!(
        graph
            .add_node_by_name("dupe".to_owned(), "test-option-filter")
            .is_ok()
    );
    assert!(
        graph
            .add_node_by_name("missing".to_owned(), "no-such-filter")
            .is_err()
    );
}
