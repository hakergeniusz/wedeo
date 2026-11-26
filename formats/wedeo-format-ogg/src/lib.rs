#![forbid(unsafe_code)]
//! Ogg container demuxer and muxer for wedeo.
//!
//! FFmpeg reference: `libavformat/ogg/demuxer.c`, `libavformat/oggenc.c` and
//! the codec mappings in `libavformat/ogg/mapping/{vorbis,opus,theora,flac,ogm}.c`.
//!
//! Implements page parsing (capture pattern, CRC validation, segment tables),
//! packet assembly across pages, stream identification, codec header parsing
//! (Vorbis, Opus, Theora, FLAC, Ogm) and granule-to-timestamp conversion.
//!
//! The muxer writes Vorbis and Opus streams: Xiph headers from extradata,
//! BOS/EOS page flags, per-stream serials and sequence numbers, sample-based
//! granule positions and validated page CRCs.

pub mod crc;
mod demuxer;
mod mapping;
mod muxer;
#[cfg(test)]
mod muxer_tests;
mod page;
#[cfg(test)]
mod tests;
mod vorbis_parser;

pub(crate) use vorbis_parser::BitReader;

use wedeo_format::registry::{DemuxerFactory, MuxerFactory};

inventory::submit!(&crate::demuxer::OggDemuxerFactory as &dyn DemuxerFactory);
inventory::submit!(&crate::muxer::OggMuxerFactory as &dyn MuxerFactory);
