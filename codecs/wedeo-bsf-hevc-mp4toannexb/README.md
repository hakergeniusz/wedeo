# `wedeo-bsf-hevc-mp4toannexb`

- Kind: bsf
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/bsf/hevc_mp4toannexb.c`
- Owner: one subagent, this crate directory only
- Status: implemented

Converts length-prefixed (hvcC / ISO BMFF) HEVC packets into Annex B byte
stream format. The hvcC extradata is parsed into VPS/SPS/PPS and prefix/suffix
SEI NAL arrays, which are injected before IRAP access units when not already
present in-band. In-band parameter sets are matched against the extradata
copies by FNV-1a hash to avoid duplicate emission. Handles
`AV_PKT_DATA_NEW_EXTRADATA` side data and Annex-B pass-through.

Conformance tests live in `tests/fate/tests/fate_bsf_hevc_mp4toannexb.rs`.
The FATE test includes a real hvcC record plus its length-prefixed packet and
the byte-exact output of `ffmpeg -bsf:v hevc_mp4toannexb` as a conformance vector.
