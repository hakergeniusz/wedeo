# `wedeo-format-mpegts-demux`

- Kind: format (demuxer)
- FFmpeg reference: `~/Projects/FFmpeg/libavformat/mpegts.c` (with `mpegts.h`)
- Owner: one subagent, this crate directory only

Pure-Rust MPEG-2 Transport Stream demuxer:

- 188/192/204-byte packet synchronisation and `analyze()`-based packet-size detection
- PSI section reassembly with CRC-32/MPEG-2 validation, PAT and PMT parsing
- PID-to-stream mapping with stream-type to `CodecId` conversion (H264, HEVC,
  MPEG-2 video, MP3, AAC/AC3, AV1/Opus via registration descriptors)
- PES assembly with PTS/DTS extraction, PES length handling and padding skip
- `Demuxer` trait (`read_header`/`read_packet`/`seek`), `DemuxerFactory` and
  `inventory::submit!` registration under the name `mpegts`

Conformance tests live in `tests/fate/tests/fate_format_mpegts_demux.rs`.
