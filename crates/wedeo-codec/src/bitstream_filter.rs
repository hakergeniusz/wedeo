//! Bitstream filter interface, matching FFmpeg's `AVBSFContext` and
//! `FFBitstreamFilter`.
//!
//! A bitstream filter consumes one encoded packet and produces zero or more
//! encoded packets, without decoding. Implementations register a factory via
//! `inventory::submit!` in the same way as decoders and muxers.

use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Result;
use wedeo_core::packet::Packet;

/// A bitstream filter instance, matching FFmpeg's `av_bsf_send_packet` /
/// `av_bsf_receive_packet` pair collapsed into a single call.
pub trait BitstreamFilter: Send {
    /// Transform one input packet into zero or more output packets.
    fn filter(&mut self, packet: &Packet) -> Result<Vec<Packet>>;

    /// Flush any buffered packets at end of stream.
    fn flush(&mut self) -> Result<Vec<Packet>>;
}

/// Registry metadata for a bitstream filter.
#[derive(Debug, Clone, Copy)]
pub struct BitstreamFilterDescriptor {
    pub name: &'static str,
    pub long_name: &'static str,
    pub codec_id: CodecId,
    pub priority: i32,
}

/// Factory for creating bitstream filter instances.
pub trait BitstreamFilterFactory: Send + Sync {
    fn descriptor(&self) -> &BitstreamFilterDescriptor;
    fn create(&self) -> Result<Box<dyn BitstreamFilter>>;
}

inventory::collect!(&'static dyn BitstreamFilterFactory);

/// Find a bitstream filter by name.
pub fn find_bitstream_filter(name: &str) -> Option<&'static dyn BitstreamFilterFactory> {
    inventory::iter::<&'static dyn BitstreamFilterFactory>()
        .find(|f| f.descriptor().name == name)
        .copied()
}

/// Iterate over all registered bitstream filters.
pub fn bitstream_filters() -> impl Iterator<Item = &'static dyn BitstreamFilterFactory> {
    inventory::iter::<&'static dyn BitstreamFilterFactory>().copied()
}
