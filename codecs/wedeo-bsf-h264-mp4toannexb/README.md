# `wedeo-bsf-h264-mp4toannexb`

- Kind: bsf
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/bsf/h264_mp4toannexb.c`
- Owner: one subagent, this crate directory only
- Status: implemented (AVCC to Annex B conversion, SPS/PPS injection, IDR detection)

Do not put this work unit in another crate. Conformance tests belong in
`tests/fate/tests/fate_bsf_h264_mp4toannexb.rs`.
