// Codec tag (fourcc) to CodecId mapping for AVI.
//
// FFmpeg uses `ff_codec_get_id(ff_codec_bmp_tags, tag1)` for video streams and
// `ff_wav_codec_get_id(id, bits_per_coded_sample)` for audio streams. See
// libavformat/riff.c and libavformat/riffdec.c. Only tags relevant to the
// codec IDs Wedeo knows are listed; unknown tags map to CodecId::None exactly
// as FFmpeg leaves the codec ID unset.

use wedeo_core::codec_id::CodecId;

/// Map an AVI BITMAPINFOHEADER compression fourcc (little-endian u32) to a
/// `CodecId`. Mirrors `ff_codec_get_id(ff_codec_bmp_tags, tag)`.
pub fn bmp_tag_to_codec_id(tag: u32) -> CodecId {
    let t = tag.to_le_bytes();
    match &t {
        // H.264 (ff_codec_bmp_tags)
        b"H264" | b"h264" | b"X264" | b"x264" | b"avc1" | b"DAVC" | b"SMV2" | b"VSSH" | b"Q264"
        | b"V264" | b"GAVC" | b"UMSV" | b"tshd" | b"INMC" => CodecId::H264,
        // MPEG-4 Visual
        b"FMP4"
        | b"DIVX"
        | b"DX50"
        | b"XVID"
        | b"MP4S"
        | b"M4S2"
        | b"ZMP4"
        | b"DIV1"
        | b"BLZ0"
        | b"mp4v"
        | b"UMP4"
        | b"WV1F"
        | b"SEDG"
        | b"RMP4"
        | b"3IV2"
        | b"WAWV"
        | b"FFDS"
        | b"FVFW"
        | b"DCOD"
        | b"MVXM"
        | b"PM4V"
        | b"SMP4"
        | b"DXGM"
        | b"VIDM"
        | b"M4T3"
        | b"GEOX"
        | b"G264"
        | b"HDX4"
        | b"DM4V"
        | b"DMK2"
        | b"DYM4"
        | b"DIGI"
        | b"EPHV"
        | b"EM4A"
        | b"M4CC"
        | b"SN40"
        | b"VSPX"
        | b"ULDX"
        | b"GEOV"
        | b"SIPP"
        | b"SM4V"
        | b"XVIX"
        | b"DreX"
        | b"QMP4"
        | b"PLV1"
        | b"GLV4"
        | b"GMP4"
        | b"MNM4"
        | b"GTM4"
        | [4, 0, 0, 0]
        | [4, 0, 0, 0x10] => CodecId::Mpeg4,
        // MPEG-1 / MPEG-2 video
        b"mpg1" | b"PIM1" | b"BW10" | b"XMPG" | [1, 0, 0, 0x10] => CodecId::Mpeg1video,
        b"mpg2"
        | b"MPEG"
        | b"PIM2"
        | b"VCR2"
        | b"DVR "
        | b"MMES"
        | b"LMP2"
        | b"slif"
        | b"EM2V"
        | b"M701"
        | b"M702"
        | b"M703"
        | b"M704"
        | b"M705"
        | b"mpgv"
        | [2, 0, 0, 0x10] => CodecId::Mpeg2video,
        // H.263 / H.261
        b"H263" | b"X263" | b"T263" | b"L263" | b"VX1K" | b"ZyGo" | b"M263" | b"lsvm" | b"U263"
        | b"VSM4" => CodecId::H263,
        b"H261" => CodecId::H261,
        // Motion JPEG
        b"MJPG" | b"msc2" | b"LJPG" | b"MJPA" | b"JFIF" | b"jpeg" | b"JPEG" => CodecId::Mjpeg,
        // VP8 / VP9 / AV1 / HEVC
        b"VP80" => CodecId::Vp8,
        b"VP90" => CodecId::Vp9,
        b"AV01" => CodecId::Av1,
        b"H265" | b"HEVC" | b"hvc1" | b"x265" => CodecId::Hevc,
        // RealVideo
        b"RV10" | b"rv10" => CodecId::Rv10,
        b"RV20" | b"rv20" => CodecId::Rv20,
        // Uncompressed / raw
        b"I420" | b"YUY2" | b"YV12" | b"IYUV" | b"NV12" | b"NV21" | b"UYVY" | b"GREY" | b"Y8  "
        | b"RGB2" | b"RV24" | b"RV32" | b"DIB " => CodecId::Rawvideo,
        _ => CodecId::None,
    }
}

/// Map a WAVEFORMAT `wFormatTag` to a `CodecId`, using the bits-per-sample
/// refinement of `ff_wav_codec_get_id()`.
pub fn wav_tag_to_codec_id(tag: u16, bits: u16) -> CodecId {
    match tag {
        // WAVE_FORMAT_PCM: ff_get_pcm_codec_id(bps, 0, 0, ~1)
        0x0001 => match bits {
            8 => CodecId::PcmU8,
            16 => CodecId::PcmS16le,
            24 => CodecId::PcmS24le,
            32 => CodecId::PcmS32le,
            _ => CodecId::None,
        },
        // WAVE_FORMAT_IEEE_FLOAT: ff_get_pcm_codec_id(bps, 1, 0, 0)
        0x0003 => match bits {
            32 => CodecId::PcmF32le,
            64 => CodecId::PcmF64le,
            _ => CodecId::None,
        },
        0x0006 => CodecId::PcmAlaw,  // WAVE_FORMAT_ALAW
        0x0007 => CodecId::PcmMulaw, // WAVE_FORMAT_MULAW
        0x0002 => CodecId::AdpcmMs,  // WAVE_FORMAT_ADPCM
        0x0011 => CodecId::AdpcmImaWav,
        0x0055 => CodecId::Mp3, // WAVE_FORMAT_MPEGLAYER3
        0x0050 => CodecId::Mp2, // WAVE_FORMAT_MPEG
        0x2000 => CodecId::Ac3,
        0x00FF | 0x1600 => CodecId::Aac,
        0xF1AC => CodecId::Flac,
        0x5756 => CodecId::Vorbis,
        0x7FFF => CodecId::Opus,
        _ => CodecId::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*s)
    }

    #[test]
    fn maps_common_video_tags() {
        assert_eq!(bmp_tag_to_codec_id(tag(b"SMV2")), CodecId::H264);
        assert_eq!(bmp_tag_to_codec_id(tag(b"avc1")), CodecId::H264);
        assert_eq!(bmp_tag_to_codec_id(tag(b"XVID")), CodecId::Mpeg4);
        assert_eq!(bmp_tag_to_codec_id(tag(b"VCR2")), CodecId::Mpeg2video);
        assert_eq!(bmp_tag_to_codec_id(tag(b"MJPG")), CodecId::Mjpeg);
        assert_eq!(bmp_tag_to_codec_id(tag(b"VP90")), CodecId::Vp9);
        assert_eq!(bmp_tag_to_codec_id(tag(b"ZZZZ")), CodecId::None);
    }

    #[test]
    fn maps_wav_tags_with_bits_refinement() {
        assert_eq!(wav_tag_to_codec_id(0x0001, 8), CodecId::PcmU8);
        assert_eq!(wav_tag_to_codec_id(0x0001, 16), CodecId::PcmS16le);
        assert_eq!(wav_tag_to_codec_id(0x0003, 32), CodecId::PcmF32le);
        assert_eq!(wav_tag_to_codec_id(0x0055, 16), CodecId::Mp3);
        assert_eq!(wav_tag_to_codec_id(0x9999, 16), CodecId::None);
    }
}
