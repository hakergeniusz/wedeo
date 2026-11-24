# `wedeo-codec-mpeg12`

- Kind: codec
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/mpeg12dec.c` (with
  `mpegvideo_dec.c`, `mpegvideo_motion.c`, `mpegvideo_unquantize.c`,
  `mpeg12data.c` and `simple_idct_template.c`)
- Owner: one subagent, this crate directory only
- Status: implemented. Frame pictures (MPEG-1/2), B frames, complementary
  field pictures and 4:2:2 chroma are bitexact. The `mpeg2_field_encoding.ts`
  FATE sample is 31/31 bitexact: second-field DMV macroblocks perform both
  field predictions (the reference switches to the current frame's opposite
  field after the first one, per `mpegvideo_motion.c`), and field pictures
  start from FFmpeg's recycled reference buffer so missing slices from the
  truncated final access unit expose the same stale pixels as FFmpeg (FFmpeg
  disables error concealment for field pictures in `error_resilience.c`).

Conformance tests belong in `tests/fate/tests/fate_codec_mpeg12.rs`.
