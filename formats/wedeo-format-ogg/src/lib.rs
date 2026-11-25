#![forbid(unsafe_code)]
//! Ogg container demuxer for wedeo.
//!
//! FFmpeg reference: `libavformat/ogg/demuxer.c` and the codec mappings in
//! `libavformat/ogg/mapping/{vorbis,opus,theora,flac,ogm}.c`.
//!
//! Implements page parsing (capture pattern, CRC validation, segment tables),
//! packet assembly across pages, stream identification, codec header parsing
//! (Vorbis, Opus, Theora, FLAC, Ogm) and granule-to-timestamp conversion.

pub mod crc;
mod demuxer;
mod mapping;
#[cfg(test)]
mod tests;
mod vorbis_parser;

pub(crate) use vorbis_parser::BitReader;

use wedeo_format::registry::DemuxerFactory;

inventory::submit!(&crate::demuxer::OggDemuxerFactory as &dyn DemuxerFactory);
