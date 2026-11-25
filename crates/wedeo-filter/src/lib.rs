#![forbid(unsafe_code)]
pub mod formats;
pub mod graph;
pub mod registry;

#[cfg(test)]
mod tests;

use bitflags::bitflags;

use wedeo_core::error::Result;
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;

pub use formats::{FilterFormat, FilterFormats, FormatsConfig, PadFormats};

bitflags! {
    /// Filter flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FilterFlags: u32 {
        const DYNAMIC_INPUTS       = 1 << 0;
        const DYNAMIC_OUTPUTS      = 1 << 1;
        const SLICE_THREADS        = 1 << 2;
        const METADATA_ONLY        = 1 << 3;
        const SUPPORT_TIMELINE_GENERIC  = 1 << 16;
        const SUPPORT_TIMELINE_INTERNAL = 1 << 17;
    }
}

/// Filter pad direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPadDirection {
    Input,
    Output,
}

/// Descriptor for a filter pad (input or output).
#[derive(Debug, Clone)]
pub struct FilterPadDescriptor {
    pub name: &'static str,
    pub media_type: MediaType,
    pub direction: FilterPadDirection,
}

/// Descriptor for a filter type.
#[derive(Debug, Clone)]
pub struct FilterDescriptor {
    pub name: &'static str,
    pub description: &'static str,
    pub inputs: &'static [FilterPadDescriptor],
    pub outputs: &'static [FilterPadDescriptor],
    pub flags: FilterFlags,
}

/// Filter trait — the main abstraction for filter implementations.
pub trait Filter: Send {
    fn descriptor(&self) -> &FilterDescriptor;
    fn activate(&mut self) -> Result<()>;

    /// Transform one input frame into zero or more output frames.
    ///
    /// Filters that drop frames return an empty vec; filters that duplicate
    /// frames (e.g. fps) return more than one. Metadata is carried on `Frame`.
    fn filter_frame(&mut self, frame: Frame) -> Result<Vec<Frame>>;

    /// Query formats supported by the filter on its inputs and outputs.
    ///
    /// FFmpeg's `query_formats()` callback (libavfilter/filters.h) is called
    /// after the filter is initialized (so the inputs and outputs are fixed),
    /// shortly before the format negotiation. This callback may be called
    /// more than once.
    ///
    /// This callback must set the format list on every input pad and on every
    /// output pad to a list of pixel/sample formats that the filter supports
    /// on that pad. FFmpeg's `query_func2()` writes the results into provided
    /// arrays (`cfg_in` and `cfg_out`); here the equivalent arrays are
    /// [`FormatsConfig::inputs`] and [`FormatsConfig::outputs`], parallel to
    /// the filter's pad descriptors.
    ///
    /// The default implementation leaves every pad unrestricted, matching
    /// FFmpeg's default `FF_FILTER_FORMATS_PASSTHROUGH` state. Filters that
    /// constrain formats (e.g. `format`, `fps`) override this method.
    fn query_formats(&self, _cfg: &mut FormatsConfig) -> Result<()> {
        Ok(())
    }

    /// Drain frames buffered inside the filter at end of stream.
    ///
    /// FFmpeg's `activate()` drives the graph and may push tail frames via
    /// `ff_filter_frame()` when a source signals EOF; our `activate()` cannot
    /// return frames, so filters that delay frames (e.g. `fps`) emit their
    /// tail here. A filter may be flushed more than once; after the tail has
    /// been produced it should return an empty vec on subsequent calls.
    ///
    /// The default implementation emits no frames, so existing filters need
    /// no change.
    fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }
}
