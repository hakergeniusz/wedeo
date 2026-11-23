# `wedeo-format-mpegts-mux`

- Kind: format (muxer)
- FFmpeg reference: `~/Projects/FFmpeg/libavformat/mpegtsenc.c`
- Owner: one subagent, this crate directory only

Pure-Rust MPEG-2 Transport Stream muxer translated from FFmpeg's
`mpegtsenc.c`. Writes 188-byte packets, PAT/PMT/SDT sections with
CRC-32/MPEG-2, PES packets with PTS/DTS, stream IDs for H.264/AAC/MP3,
continuity counters and PCR on the first video PID.

Only the default single-program VBR configuration is implemented; MPEG-TS
options (`latm`, `system_b`, `m2ts`, `nit`, `resend_headers`, a dedicated PCR
PID, multiple services) are not exposed by the frozen `Muxer` trait.

Tests live in `src/lib.rs` (unit) and
`tests/fate/tests/fate_format_mpegts_mux.rs` (FATE).
