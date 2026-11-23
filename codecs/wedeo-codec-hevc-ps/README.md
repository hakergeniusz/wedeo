# `wedeo-codec-hevc-ps`

- Kind: codec (pure parameter-set / header parsing library)
- FFmpeg references: `libavcodec/hevc/ps.c`, `parse.c`, `sei.c`, `hevc.h`
- Owner: one subagent, this crate directory only
- Status: implemented

## What it does

Pure-Rust parsing of HEVC NAL units, VPS/SPS/PPS RBSPs, slice segment
headers, VUI/HRD, scaling lists, short-term RPS, and SEI message framing. It
is a library for the downstream HEVC work units (`hevc-cabac`, `hevc-dsp`,
`hevc-refs`, `hevc`) and contains no full decoder.

## Public API

- NAL: `parse_nal_header`, `parse_nal_unit`, `ebsp_to_rbsp`, `annex_b_split`,
  `NalUnitHeader`, `NalUnitType`.
- Parameter sets: `parse_vps` -> `Vps`, `parse_sps` -> `Sps`,
  `parse_pps(rbsp, &sps)` -> `Pps`, plus `ParamSets` (storage by id with
  `decode_nal` / `decode_nal_units` / `decode_extradata` /
  `slice_context` / `parse_slice_header`).
- Slice header: `parse_slice_header(rbsp, &SliceContext)` -> `SliceHeader`,
  `SliceType`, `compute_poc`, `L0` / `L1`.
- Supporting: `ProfileTierLevel`, `ShortTermRps`, `ScalingList` (with the
  diagonal scan tables), `Vui` / `HrdParams`, `SeiMessage`, `Window` and
  the FFmpeg limit constants.

All fields are public so downstream crates can use them directly. Errors are
`wedeo_core::Result`; unsupported multi-layer / 3D extensions return
`Error::PatchwelcomeNotImplemented`.

## Verification

- 20 in-crate unit tests build synthetic VPS/SPS/PPS/slice bitstreams from
  FFmpeg field semantics and parse them back.
- `tests/fate/tests/fate_codec_hevc_ps.rs` parses real FATE HEVC vectors and
  checks coded/cropped dimensions, chroma format, bit depth and profile
  against FFmpeg's `Parsed SPS` debug output, plus the first IRAP slice header.
