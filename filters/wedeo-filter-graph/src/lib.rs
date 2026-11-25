#![forbid(unsafe_code)]
//! Functional filter graph for Wedeo.
//!
//! FFmpeg references: `libavfilter/avfilter.c`, `libavfilter/avfiltergraph.c`,
//! `libavfilter/buffersrc.c`, `libavfilter/buffersink.c`.
//!
//! This crate owns node creation from the filter registry, pad/link
//! validation, per-link frame queues and the `run` path that pushes frames
//! through a chain of `Box<dyn Filter>` via `filter_frame`. The frozen
//! `wedeo_filter::graph::FilterGraph` stub in the framework crate is not
//! modified.

mod builtin;
mod graph;

#[cfg(test)]
mod tests;

pub use builtin::PassThrough;
pub use graph::{Graph, Link, LinkId, Node, NodeId};
// Re-exported so the FATE harness can query the registry without adding a
// direct dependency on the framework crate.
pub use wedeo_filter::formats::{FilterFormat, FilterFormats, FormatsConfig, PadFormats};
pub use wedeo_filter::graph::FilterLinkConfig;
pub use wedeo_filter::registry::{FilterFactory, filters, find_filter_by_name};
pub use wedeo_filter::{
    Filter, FilterDescriptor, FilterFlags, FilterPadDescriptor, FilterPadDirection,
};

use inventory as _;
use wedeo_core as _;
