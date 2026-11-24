# `wedeo-codec-hevc`

- Kind: codec (native HEVC / H.265 decoder)
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/hevc/hevcdec.c`
  (plus `filter.c`, `cabac.c`, `mvs.c`, `refs.c`, `ps.c`, `pred_template.c`)
- Owner: one subagent, this crate directory only
- Status: implemented

## What it does

Complete pure-Rust HEVC decoder. It wires together the four HEVC library
crates (parameter sets, CABAC, DSP and references) and implements the
picture/slice/CTU control flow of FFmpeg's `hevcdec.c`:

- Annex B / length-prefixed NAL extraction, VPS/SPS/PPS resolution
- Slice header parsing with POC computation
- CTU/CU/PU/TU quadtree parsing with CABAC
- Intra (35 modes) and inter (merge/AMVP, uni/bi prediction) reconstruction
- Residual scaling-list + QP dequantisation, inverse transforms, RDPCM
- Deblocking and SAO filtering
- DPB reordering / output

The decoder is registered with `inventory::submit!` at priority 100 for
`CodecId::Hevc`. Conformance tests belong in
`tests/fate/tests/fate_codec_hevc.rs`.
