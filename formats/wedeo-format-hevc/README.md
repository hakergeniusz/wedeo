# `wedeo-format-hevc`

- Kind: format (raw demuxer)
- FFmpeg reference: `libavformat/hevc.c`, `libavformat/rawdec.c`,
  `libavcodec/hevc/parser.c`, `libavformat/mpegts.c`
- Owner: one subagent, this crate directory only

Pure-Rust raw HEVC Annex B demuxer. Splits Annex B NAL units, parses
VPS/SPS/PPS for stream metadata, groups NALs into access units following
`hevc_find_frame_end()` (first_slice_segment_in_pic_flag plus AUD/VPS/SPS/PPS
boundaries), derives pts/dts using the picture order count, and registers the
`hevc` input format via `inventory` at priority 100. Also provides
`annexb_to_hvcc()` for `hvcC` (HEVCDecoderConfigurationRecord) generation.

Conformance tests live in `tests/fate/tests/fate_format_hevc.rs`.
