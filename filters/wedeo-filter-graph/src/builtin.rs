//! Built-in source/sink/identity filters registered by the graph crate.
//!
//! FFmpeg defines its frame-injection and frame-collection filters in
//! `libavfilter/buffersrc.c` (`buffer` / `abuffer`) and
//! `libavfilter/buffersink.c` (`buffersink` / `abuffersink`). Wedeo's
//! `Filter` trait has no pull/request mechanism, so these filters are
//! pass-through nodes: the graph feeds source nodes at [`crate::Graph::push`]
//! and collects the frames returned by sink nodes at [`crate::Graph::pull`].
//!
//! `null` / `anull` are the standard pass-through filters used to build
//! chains in tests and as placeholders in graphs.

use wedeo_core::error::Result;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_filter::registry::FilterFactory;
use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection,
};

static VIDEO_IN: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "in",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Input,
}];
static VIDEO_OUT: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "out",
    media_type: MediaType::Video,
    direction: FilterPadDirection::Output,
}];
static AUDIO_IN: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "in",
    media_type: MediaType::Audio,
    direction: FilterPadDirection::Input,
}];
static AUDIO_OUT: [FilterPadDescriptor; 1] = [FilterPadDescriptor {
    name: "out",
    media_type: MediaType::Audio,
    direction: FilterPadDirection::Output,
}];
static NO_PADS: [FilterPadDescriptor; 0] = [];

static BUFFER_DESC: FilterDescriptor = FilterDescriptor {
    name: "buffer",
    description: "Buffer video frames, and make them available to a filter graph.",
    inputs: &NO_PADS,
    outputs: &VIDEO_OUT,
    flags: FilterFlags::empty(),
};
static ABUFFER_DESC: FilterDescriptor = FilterDescriptor {
    name: "abuffer",
    description: "Buffer audio frames, and make them available to a filter graph.",
    inputs: &NO_PADS,
    outputs: &AUDIO_OUT,
    flags: FilterFlags::empty(),
};
static BUFFERSINK_DESC: FilterDescriptor = FilterDescriptor {
    name: "buffersink",
    description: "Buffer video frames, and make them available to an application.",
    inputs: &VIDEO_IN,
    outputs: &NO_PADS,
    flags: FilterFlags::empty(),
};
static ABUFFERSINK_DESC: FilterDescriptor = FilterDescriptor {
    name: "abuffersink",
    description: "Buffer audio frames, and make them available to an application.",
    inputs: &AUDIO_IN,
    outputs: &NO_PADS,
    flags: FilterFlags::empty(),
};
static NULL_DESC: FilterDescriptor = FilterDescriptor {
    name: "null",
    description: "Pass the source unchanged to the output.",
    inputs: &VIDEO_IN,
    outputs: &VIDEO_OUT,
    flags: FilterFlags::empty(),
};
static ANULL_DESC: FilterDescriptor = FilterDescriptor {
    name: "anull",
    description: "Pass the source unchanged to the output.",
    inputs: &AUDIO_IN,
    outputs: &AUDIO_OUT,
    flags: FilterFlags::empty(),
};

/// A pass-through filter used for all six built-in registrations.
#[derive(Debug)]
pub struct PassThrough {
    desc: &'static FilterDescriptor,
    /// Number of frames seen by this instance (useful for tracing/tests).
    pub frames_forwarded: usize,
}

impl PassThrough {
    fn new(desc: &'static FilterDescriptor) -> Self {
        Self {
            desc,
            frames_forwarded: 0,
        }
    }
}

impl Filter for PassThrough {
    fn descriptor(&self) -> &FilterDescriptor {
        self.desc
    }

    fn activate(&mut self) -> Result<()> {
        Ok(())
    }

    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        self.frames_forwarded += 1;
        Ok(vec![frame])
    }
}

macro_rules! builtin_filter {
    ($factory:ident, $desc:ident) => {
        struct $factory;

        impl FilterFactory for $factory {
            fn descriptor(&self) -> &FilterDescriptor {
                &$desc
            }

            fn create(&self) -> Result<Box<dyn Filter>> {
                Ok(Box::new(PassThrough::new(&$desc)))
            }
        }

        inventory::submit!(&$factory as &dyn FilterFactory);
    };
}

builtin_filter!(BufferFactory, BUFFER_DESC);
builtin_filter!(AudioBufferFactory, ABUFFER_DESC);
builtin_filter!(BufferSinkFactory, BUFFERSINK_DESC);
builtin_filter!(AudioBufferSinkFactory, ABUFFERSINK_DESC);
builtin_filter!(NullFactory, NULL_DESC);
builtin_filter!(AudioNullFactory, ANULL_DESC);
