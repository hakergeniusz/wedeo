# `wedeo-codec-hevc-refs`

- Kind: codec library
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/hevc/refs.c`,
  `~/Projects/FFmpeg/libavcodec/hevc/mvs.c`,
  `~/Projects/FFmpeg/libavutil/refstruct.c`
- Owner: one subagent, this crate directory only
- Status: implemented

Pure-Rust reference-picture and motion-vector library for the HEVC decoder
work units. Depends on the merged `wedeo-codec-hevc-ps` crate for parsed
parameter sets, slice headers and RPS values.

## Modules / API

- `dpb`: `Dpb` - decoded picture buffer with short-term/long-term RPS
  application (`frame_rps`), reference marking, missing-reference generation,
  output ordering and `clear_refs` / `flush`.
- `rpl`: `RefPicList`, `RpsType`, `slice_rpl`, `init_slice_rpl`,
  `frame_nb_refs` - L0/L1 construction including list modification and
  collocated-reference selection.
- `mv`: `Mv`, `MvField`, `PredFlag`, `mv_scale`, `compare_mv_ref_idx`,
  `check_mvset`, `derive_temporal_colocated_mvs`.
- `prediction`: `temporal_luma_motion_vector`, spatial merge candidates,
  `luma_mv_merge_mode`, `luma_mv_mvp_mode` (AMVP) - sections 8.5.3.1.1/2/7/8.
- `frame`: `HevcFrame`, `FrameRef`, `FrameFlags`, `MvFieldTable`.
- `neighbors`: `set_neighbour_available`, `z_scan_block_avail`, `is_diff_mer`.
- `context`: `RefGeometry` (built from `Sps`/`Pps`), `PartMode`.
- `weights`: `PredWeightTable` accessors plus uni-/bi-directional weighted
  sample prediction (spec 8.4.3).

## Verification

- 19 crate unit tests (synthetic RPS, DPB, list building, MV prediction,
  weighted prediction).
- `tests/fate/tests/fate_codec_hevc_refs.rs` drives real FATE HEVC streams
  through RPS application and reference-list construction.
