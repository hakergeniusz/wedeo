# `wedeo-format-ogg`

- Kind: format
- FFmpeg reference: `~/Projects/FFmpeg/libavformat/ogg/demuxer.c` plus the codec
  mappings in `libavformat/ogg/mapping/{vorbis,opus,theora,flac,ogm,vp8}.c`
- Owner: one subagent, this crate directory only
- Status: implemented

Implements the Ogg demuxer in pure Rust:

- page parsing (capture pattern, CRC-32 validation, segment tables),
- packet assembly across pages including continuation packets,
- stream identification by codec magic,
- codec headers and extradata for Vorbis, Opus, Theora, FLAC, Ogm and VP8,
- granule-to-timestamp conversion and per-packet duration parsing
  (including the Vorbis and FLAC frame parsers),
- Vorbis comment metadata extraction,
- chained-stream replacement and `inventory` registration as `ogg`.

Conformance tests live in `tests/fate/tests/fate_format_ogg.rs`.
