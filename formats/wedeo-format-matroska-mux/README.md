# `wedeo-format-matroska-mux`

- Kind: format
- FFmpeg reference: `~/Projects/FFmpeg/libavformat/matroskaenc.c`
  (EBML IDs from `libavformat/matroska.h`, codec tags from `matroska.c`)
- Owner: one subagent, this crate directory only
- Status: implemented (Matroska + WebM muxer, registered as `matroska` and `webm`)

Implements a complete EBML/Matroska writer: EBML header, Segment with a
patched size, SeekHead, Info (TimecodeScale / MuxingApp / WritingApp /
Duration), Tracks (CodecID strings for H264, VP9, AV1, AAC, PCM and more),
Tags (per-track DURATION), Clusters with SimpleBlocks, and Cues. Level-one
elements carry CRC-32 in Matroska mode, matching FFmpeg's `write_crc32`
default; WebM mode disables CRC and writes DocType `webm`.

Verification: unit tests cover EBML primitives and element layout; the FATE
test includes a byte-for-byte comparison with `ffmpeg -bitexact -f matroska`
for a single PCM frame (858 bytes, zero differing bytes).

Do not put this work unit in another crate. Conformance tests belong in
`tests/fate/tests/fate_format_matroska_mux.rs`.
