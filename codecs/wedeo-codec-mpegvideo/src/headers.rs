//! MPEG-1/2 sequence, extension and picture header structures.
//!
//! The parsers consume bits **after** the relevant start code (and, for
//! extensions, after the 4-bit `extension_start_code_identifier`). This keeps
//! start-code scanning the responsibility of the calling decoder.
//!
//! C references: `mpeg12dec.c` (`mpeg_decode_sequence_header`,
//! `mpeg_decode_sequence_extension`, `mpeg_decode_picture_coding_extension`,
//! `mpeg_decode_gop`) and `mpegvideo.h`.

use wedeo_codec::bitstream::{BitRead, BitReadBE};
use wedeo_core::Result;

use crate::tables::{MPEG2_ASPECT, MPEG12_FRAME_RATE};

/// Picture coding type (FFmpeg `AV_PICTURE_TYPE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PictureType {
    /// Intra picture.
    I = 1,
    /// Predictive picture.
    P = 2,
    /// Bidirectionally predictive picture.
    B = 3,
    /// D picture (MPEG-1 only).
    D = 4,
    #[default]
    /// Unknown / not yet decoded.
    Unknown = 0,
}

impl From<u32> for PictureType {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::I,
            2 => Self::P,
            3 => Self::B,
            4 => Self::D,
            _ => Self::Unknown,
        }
    }
}

/// Picture structure (FFmpeg `PICT_*` from `mpegutils.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PictureStructure {
    /// Frame picture (value 3 in the bitstream).
    #[default]
    Frame = 3,
    /// Top field (value 1).
    TopField = 1,
    /// Bottom field (value 2).
    BottomField = 2,
}

impl From<u32> for PictureStructure {
    fn from(v: u32) -> Self {
        match v {
            1 => Self::TopField,
            2 => Self::BottomField,
            _ => Self::Frame,
        }
    }
}

/// Chroma subsampling format (FFmpeg `CHROMA_420/422/444`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChromaFormat {
    #[default]
    Chroma420 = 1,
    Chroma422 = 2,
    Chroma444 = 3,
}

impl From<u32> for ChromaFormat {
    fn from(v: u32) -> Self {
        match v {
            2 => Self::Chroma422,
            3 => Self::Chroma444,
            _ => Self::Chroma420,
        }
    }
}

/// Sequence header (FFmpeg `mpeg_decode_sequence_header`).
#[derive(Debug, Clone)]
pub struct SequenceHeader {
    /// Coded horizontal size (12 bits, possibly extended to 14 by the
    /// sequence extension).
    pub width: u32,
    /// Coded vertical size.
    pub height: u32,
    /// Aspect ratio information code (0..15).
    pub aspect_ratio_info: u32,
    /// Frame rate code (1..8 for MPEG-1, up to 62 for MPEG-2).
    pub frame_rate_code: u32,
    /// Bit rate in units of 400 bits/s; `0x3ffff` means unspecified.
    pub bit_rate: u32,
    /// Marker bit (must be 1).
    pub marker: bool,
    /// Video buffer verifier size in units of 16 kbit (10 bits).
    pub vbv_buffer_size: u32,
    /// Constrained parameters flag.
    pub constrained_parameters: bool,
    /// Frame rate as `(numerator, denominator)`, derived from
    /// `frame_rate_code` when in range.
    pub frame_rate: (u32, u32),
}

impl Default for SequenceHeader {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            aspect_ratio_info: 1,
            frame_rate_code: 1,
            bit_rate: 0,
            marker: true,
            vbv_buffer_size: 0,
            constrained_parameters: false,
            frame_rate: (1, 1),
        }
    }
}

/// Parse an MPEG-1 sequence header following the `sequence_header_code`.
pub fn parse_sequence_header(br: &mut BitReadBE<'_>) -> Result<SequenceHeader> {
    let width = br.get_bits_32(12);
    let height = br.get_bits_32(12);
    let aspect_ratio_info = br.get_bits_32(4);
    let frame_rate_code = br.get_bits_32(4);
    let bit_rate = br.get_bits_32(18);
    let marker = br.get_bit();
    let vbv_buffer_size = br.get_bits_32(10);
    let constrained_parameters = br.get_bit();

    let mut frame_rate = (1u32, 1u32);
    if (frame_rate_code as usize) < MPEG12_FRAME_RATE.len() {
        let (num, den) = MPEG12_FRAME_RATE[frame_rate_code as usize];
        if num != 0 {
            frame_rate = (num as u32, den as u32);
        }
    }
    Ok(SequenceHeader {
        width,
        height,
        aspect_ratio_info,
        frame_rate_code,
        bit_rate,
        marker,
        vbv_buffer_size,
        constrained_parameters,
        frame_rate,
    })
}

/// MPEG-2 sequence extension (FFmpeg `mpeg_decode_sequence_extension`).
#[derive(Debug, Clone)]
pub struct SequenceExtension {
    pub profile_and_level_escape: bool,
    pub profile: u32,
    pub level: u32,
    pub progressive_sequence: bool,
    pub chroma_format: ChromaFormat,
    pub horiz_size_ext: u32,
    pub vert_size_ext: u32,
    pub bit_rate_ext: u32,
    pub marker: bool,
    pub vbv_buffer_size_ext: u32,
    pub low_delay: bool,
    /// Frame rate extension numerator (added to the base numerator).
    pub frame_rate_ext_num: u32,
    /// Frame rate extension denominator (added to the base denominator).
    pub frame_rate_ext_den: u32,
}

/// Parse a sequence extension following the 4-bit identifier `0x1`.
///
/// The caller must apply the size/bitrate extensions to the
/// [`SequenceHeader`] as FFmpeg does (`width |= horiz_size_ext << 12`, etc.).
pub fn parse_sequence_extension(br: &mut BitReadBE<'_>) -> Result<SequenceExtension> {
    let esc = br.get_bit();
    let profile = br.get_bits_32(3);
    let level = br.get_bits_32(4);
    let progressive_sequence = br.get_bit();
    // A zero chroma_format field is invalid; FFmpeg logs a warning and forces
    // 4:2:0. `From<u32>` already maps 0 to Chroma420.
    let chroma_format = ChromaFormat::from(br.get_bits_32(2));

    let horiz_size_ext = br.get_bits_32(2);
    let vert_size_ext = br.get_bits_32(2);
    let bit_rate_ext = br.get_bits_32(12);
    let marker = br.get_bit();
    let vbv_buffer_size_ext = br.get_bits_32(8);
    let low_delay = br.get_bit();
    let frame_rate_ext_num = br.get_bits_32(2) + 1;
    let frame_rate_ext_den = br.get_bits_32(5) + 1;

    Ok(SequenceExtension {
        profile_and_level_escape: esc,
        profile,
        level,
        progressive_sequence,
        chroma_format,
        horiz_size_ext,
        vert_size_ext,
        bit_rate_ext,
        marker,
        vbv_buffer_size_ext,
        low_delay,
        frame_rate_ext_num,
        frame_rate_ext_den,
    })
}

/// Picture coding extension (FFmpeg `mpeg_decode_picture_coding_extension`).
#[derive(Debug, Clone)]
pub struct PictureCodingExtension {
    /// `f_code[direction][0/1]`: direction 0 = forward, 1 = backward.
    pub f_code: [[u32; 2]; 2],
    pub intra_dc_precision: u32,
    pub picture_structure: PictureStructure,
    pub top_field_first: bool,
    pub frame_pred_frame_dct: bool,
    pub concealment_motion_vectors: bool,
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    pub repeat_first_field: bool,
    pub chroma_420_type: bool,
    pub progressive_frame: bool,
}

impl Default for PictureCodingExtension {
    fn default() -> Self {
        Self {
            f_code: [[1, 1], [1, 1]],
            intra_dc_precision: 0,
            picture_structure: PictureStructure::Frame,
            top_field_first: false,
            frame_pred_frame_dct: true,
            concealment_motion_vectors: false,
            q_scale_type: false,
            intra_vlc_format: false,
            alternate_scan: false,
            repeat_first_field: false,
            chroma_420_type: false,
            progressive_frame: false,
        }
    }
}

/// Parse a picture coding extension following the 4-bit identifier `0x8`.
pub fn parse_picture_coding_extension(br: &mut BitReadBE<'_>) -> Result<PictureCodingExtension> {
    let mut e = PictureCodingExtension::default();
    for dir in 0..2 {
        for xy in 0..2 {
            let mut f = br.get_bits_32(4);
            // FFmpeg: a zero f_code is promoted to 1.
            if f == 0 {
                f = 1;
            }
            e.f_code[dir][xy] = f;
        }
    }
    e.intra_dc_precision = br.get_bits_32(2);
    e.picture_structure = PictureStructure::from(br.get_bits_32(2));
    e.top_field_first = br.get_bit();
    e.frame_pred_frame_dct = br.get_bit();
    e.concealment_motion_vectors = br.get_bit();
    e.q_scale_type = br.get_bit();
    e.intra_vlc_format = br.get_bit();
    e.alternate_scan = br.get_bit();
    e.repeat_first_field = br.get_bit();
    e.chroma_420_type = br.get_bit();
    e.progressive_frame = br.get_bit();
    Ok(e)
}

/// Sequence display extension (FFmpeg `mpeg_decode_sequence_display_extension`).
#[derive(Debug, Clone, Default)]
pub struct SequenceDisplayExtension {
    pub video_format: u32,
    pub color_description: bool,
    pub color_primaries: u32,
    pub color_trc: u32,
    pub colorspace: u32,
    /// Display horizontal size in pixels (16 * the 14-bit field).
    pub display_width: u32,
    pub display_height: u32,
}

/// Parse a sequence display extension following the 4-bit identifier `0x2`.
pub fn parse_sequence_display_extension(
    br: &mut BitReadBE<'_>,
) -> Result<SequenceDisplayExtension> {
    let video_format = br.get_bits_32(3);
    let color_description = br.get_bit();
    let (color_primaries, color_trc, colorspace) = if color_description {
        (br.get_bits_32(8), br.get_bits_32(8), br.get_bits_32(8))
    } else {
        (0, 0, 0)
    };
    let w = br.get_bits_32(14);
    let _marker = br.get_bit();
    let h = br.get_bits_32(14);
    Ok(SequenceDisplayExtension {
        video_format,
        color_description,
        color_primaries,
        color_trc,
        colorspace,
        display_width: w * 16,
        display_height: h * 16,
    })
}

/// Group-of-pictures header (FFmpeg `mpeg_decode_gop`).
#[derive(Debug, Clone, Default)]
pub struct GopHeader {
    pub time_code_hours: u32,
    pub time_code_minutes: u32,
    pub marker: bool,
    pub time_code_seconds: u32,
    pub time_code_pictures: u32,
    pub closed_gop: bool,
    pub broken_link: bool,
}

/// Parse a GOP header following the `group_start_code` (0x000001B8).
pub fn parse_gop(br: &mut BitReadBE<'_>) -> Result<GopHeader> {
    Ok(GopHeader {
        time_code_hours: br.get_bits_32(5),
        time_code_minutes: br.get_bits_32(6),
        marker: br.get_bit(),
        time_code_seconds: br.get_bits_32(6),
        time_code_pictures: br.get_bits_32(6),
        closed_gop: br.get_bit(),
        broken_link: br.get_bit(),
    })
}

/// Return the (numerator, denominator) aspect ratio for a sequence header.
///
/// MPEG-1 stores a float approximation; it is quantized to a sufficiently
/// precise fraction (FFmpeg uses `av_d2q(..., 255)`).
pub fn aspect_ratio(h: &SequenceHeader, mpeg2: bool) -> (u32, u32) {
    let idx = h.aspect_ratio_info as usize;
    if mpeg2 {
        if idx < MPEG2_ASPECT.len() {
            let (n, d) = MPEG2_ASPECT[idx];
            (n as u32, d as u32)
        } else {
            (0, 1)
        }
    } else {
        // FFmpeg: av_d2q(ff_mpeg1_aspect[idx], 255). Provide the common cases
        // exactly and a 255-denominator approximation for the rest.
        match idx {
            1 => (1, 1),
            2 => (15, 11), // 0.6735 ~ 15/11 within 255
            3 => (16, 9),  // 0.7031
            4 => (11, 8),  // 0.7615 ~ 1.375 / 1.8 approximation
            _ => (0, 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        v.resize(16, 0);
        v
    }

    #[test]
    fn parse_sequence_header_fields() {
        // 352x288, aspect=1, frame_rate_code=3 (24/1), bit_rate=0x1234,
        // marker=1, vbv=0x2aa, constrained=0.
        let mut bits: u64 = 0;
        bits |= 352 << 52;
        bits |= 288 << 40;
        bits |= 1 << 36;
        bits |= 3 << 32;
        bits |= 0x1234 << 14;
        bits |= 1 << 13;
        bits |= 0x2aa << 3;
        bits |= 0 << 2;
        let data = [
            (bits >> 56) as u8,
            (bits >> 48) as u8,
            (bits >> 40) as u8,
            (bits >> 32) as u8,
            (bits >> 24) as u8,
            (bits >> 16) as u8,
            (bits >> 8) as u8,
            bits as u8,
        ];
        let buf = reader(&data);
        let mut br = BitReadBE::new(&buf);
        let h = parse_sequence_header(&mut br).unwrap();
        assert_eq!(h.width, 352);
        assert_eq!(h.height, 288);
        assert_eq!(h.aspect_ratio_info, 1);
        assert_eq!(h.frame_rate_code, 3);
        assert_eq!(h.frame_rate, (25, 1));
        assert_eq!(h.bit_rate, 0x1234);
        assert!(h.marker);
        assert_eq!(h.vbv_buffer_size, 0x2aa);
        assert!(!h.constrained_parameters);
    }

    #[test]
    fn parse_picture_coding_extension_defaults() {
        // All f_codes = 1 (15 in the bitstream), precision=0, structure=3,
        // then a mix of flags. Build 4*4 + 2 + 2 + 9 = 29 bits.
        let data = [0xFF, 0xFF, 0xFF, 0xE0, 0, 0, 0, 0];
        let buf = reader(&data);
        let mut br = BitReadBE::new(&buf);
        let e = parse_picture_coding_extension(&mut br).unwrap();
        // 15 is a legal raw f_code value; only 0 is promoted to 1.
        assert_eq!(e.f_code, [[15, 15], [15, 15]]);
        assert_eq!(e.intra_dc_precision, 3);
        assert_eq!(e.picture_structure, PictureStructure::Frame);
    }
}
