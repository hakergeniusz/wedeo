# `wedeo-codec-hevc-dsp`

- Kind: codec (reusable DSP library for the HEVC decoder)
- FFmpeg references: `libavcodec/hevc/pred_template.c`,
  `libavcodec/h26x/h2656_inter_template.c`,
  `libavcodec/h26x/h2656_sao_template.c`,
  `libavcodec/h26x/h2656_deblock_template.c`,
  `libavcodec/hevc/dsp_template.c`, `libavcodec/hevc/filter.c`
- Owner: one subagent, this crate directory only
- Status: implemented

All routines are pure Rust and operate on 8/9/10/12-bit samples stored in `u16`
(converted to the 14-bit domain where FFmpeg does so). No FFI, no bindgen.
Reference vectors in `tests/reference_vectors.rs` were generated from the C
source with `tools/gen_vectors.py`.
