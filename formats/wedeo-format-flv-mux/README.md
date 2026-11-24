# `wedeo-format-flv-mux`

- Kind: format
- FFmpeg reference: `~/Projects/FFmpeg/libavformat/flvenc.c` (+ `flv.h`)
- Owner: one subagent, this crate directory only
- Status: implemented

Pure-Rust FLV muxer registered as `flv` (extensions `flv`, MIME `video/x-flv`).
Writes the FLV header, the `onMetaData` script-data tag (with duration/filesize
patched in the trailer), AVC/AAC/MP3/etc. sequence headers and audio/video tags,
plus the AVC end-of-sequence tag. H.264 AVCC and Annex-B extradata are both
handled. Enhanced-RTMP ex-header layouts are used for HEVC/VP9/AV1/Opus/FLAC/AC-3.

Conformance tests live in `tests/fate/tests/fate_format_flv_mux.rs`; two of them
re-mux embedded `ffmpeg -f flv` reference files and require byte-for-byte
equality.
