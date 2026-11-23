# `wedeo-codec-mpegvideo`

- Kind: reusable codec library (not a decoder registration)
- FFmpeg reference: `libavcodec/mpegvideo_dec.c`, `mpegvideo.c`, `mpegvideo.h`,
  `mpegvideo_motion.c`, `mpegvideo_unquantize.c`, `mpeg12data.c`
- Owner: one subagent, this crate directory only

Shared MPEG-1/2/4 building blocks for the Wedeo rewrite:

- quantizer matrices and zigzag/alternate scans (`tables`, `quant`, `scan`)
- canonical VLC decoding plus the raw MPEG-1/2 codebooks (`vlc`)
- sequence / extension / picture / GOP headers and bitstream parsers (`headers`)
- median and H.263/MPEG motion-vector prediction (`mvpred`)
- DC/AC block coefficient decoding helpers (`coeff`)
- block dequantization (MPEG-1/2/H.263) and 8x8 IDCT primitives
  (`unquant`, `idct`)
- shared picture and macroblock-context structs with public fields (`picture`)

`wedeo-codec-mpeg12` builds its decoder on top of this crate. Conformance tests
belong in `tests/fate/tests/fate_codec_mpegvideo.rs` (owned by the integrator).
