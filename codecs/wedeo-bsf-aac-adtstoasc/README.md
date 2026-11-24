# `wedeo-bsf-aac-adtstoasc`

- Kind: bsf
- FFmpeg reference: `~/Projects/FFmpeg/libavcodec/bsf/aac_adtstoasc.c`
- Owner: one subagent, this crate directory only
- Status: implemented

Converts MPEG-2/4 AAC ADTS packets to raw AAC packets and attaches the derived
MPEG-4 AudioSpecificConfig as `AV_PKT_DATA_NEW_EXTRADATA` on the first output
packet. Conformance tests belong in
`tests/fate/tests/fate_bsf_aac_adtstoasc.rs`.
