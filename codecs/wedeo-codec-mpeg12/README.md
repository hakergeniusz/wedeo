# `wedeo-codec-mpeg12`

- Kind: codec
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/mpeg12dec.c` (with
  `mpegvideo_dec.c`, `mpegvideo_motion.c`, `mpegvideo_unquantize.c`,
  `mpeg12data.c` and `simple_idct_template.c`)
- Owner: one subagent, this crate directory only
- Status: implemented. Frame pictures (MPEG-1/2) and 4:2:2 chroma are
  bitexact; complementary field pictures are paired into one output frame
  with field-based motion compensation. B-frame prediction still exhibits
  small (+/-1-3) sample differences versus FFmpeg.

Conformance tests belong in `tests/fate/tests/fate_codec_mpeg12.rs`.
