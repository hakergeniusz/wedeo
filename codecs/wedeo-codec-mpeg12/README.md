# `wedeo-codec-mpeg12`

- Kind: codec
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/mpeg12dec.c` (with
  `mpegvideo_dec.c`, `mpegvideo_motion.c`, `mpegvideo_unquantize.c`,
  `mpeg12data.c` and `simple_idct_template.c`)
- Owner: one subagent, this crate directory only
- Status: implemented. Frame pictures (MPEG-1/2), B frames, complementary
  field pictures and 4:2:2 chroma are bitexact. The `mpeg2_field_encoding.ts`
  FATE sample still shows small residual differences on inter macroblocks,
  but its field pairs are all emitted (frame count matches FFmpeg).

Conformance tests belong in `tests/fate/tests/fate_codec_mpeg12.rs`.
