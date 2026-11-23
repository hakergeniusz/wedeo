//! NAL unit header parsing and Annex B EBSP -> RBSP conversion.
//!
//! FFmpeg reference: `libavcodec/hevc/hevc.h` (Table 7-1) and
//! `libavcodec/h2645_parse.c` (emulation prevention byte removal).

use wedeo_core::{Error, Result};

/// Table 7-1 NAL unit type codes (from `enum HEVCNALUnitType` in `hevc.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum NalUnitType {
    TrailN = 0,
    TrailR = 1,
    TsaN = 2,
    TsaR = 3,
    StsaN = 4,
    StsaR = 5,
    RadlN = 6,
    RadlR = 7,
    RaslN = 8,
    RaslR = 9,
    VclN10 = 10,
    VclR11 = 11,
    VclN12 = 12,
    VclR13 = 13,
    VclN14 = 14,
    VclR15 = 15,
    BlaWLp = 16,
    BlaWRadl = 17,
    BlaNLp = 18,
    IdrWRadl = 19,
    IdrNLp = 20,
    CraNut = 21,
    RsvIrapVcl22 = 22,
    RsvIrapVcl23 = 23,
    Vps = 32,
    Sps = 33,
    Pps = 34,
    Aud = 35,
    EosNut = 36,
    EobNut = 37,
    FdNut = 38,
    SeiPrefix = 39,
    SeiSuffix = 40,
    /// A reserved or unspecified NAL type not named above.
    Other(u8),
}

impl NalUnitType {
    /// Construct a NAL unit type from its 6-bit code.
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => Self::TrailN,
            1 => Self::TrailR,
            2 => Self::TsaN,
            3 => Self::TsaR,
            4 => Self::StsaN,
            5 => Self::StsaR,
            6 => Self::RadlN,
            7 => Self::RadlR,
            8 => Self::RaslN,
            9 => Self::RaslR,
            10 => Self::VclN10,
            11 => Self::VclR11,
            12 => Self::VclN12,
            13 => Self::VclR13,
            14 => Self::VclN14,
            15 => Self::VclR15,
            16 => Self::BlaWLp,
            17 => Self::BlaWRadl,
            18 => Self::BlaNLp,
            19 => Self::IdrWRadl,
            20 => Self::IdrNLp,
            21 => Self::CraNut,
            22 => Self::RsvIrapVcl22,
            23 => Self::RsvIrapVcl23,
            32 => Self::Vps,
            33 => Self::Sps,
            34 => Self::Pps,
            35 => Self::Aud,
            36 => Self::EosNut,
            37 => Self::EobNut,
            38 => Self::FdNut,
            39 => Self::SeiPrefix,
            40 => Self::SeiSuffix,
            other => Self::Other(other),
        }
    }

    /// The numeric 6-bit code.
    pub fn code(self) -> u8 {
        match self {
            Self::TrailN => 0,
            Self::TrailR => 1,
            Self::TsaN => 2,
            Self::TsaR => 3,
            Self::StsaN => 4,
            Self::StsaR => 5,
            Self::RadlN => 6,
            Self::RadlR => 7,
            Self::RaslN => 8,
            Self::RaslR => 9,
            Self::VclN10 => 10,
            Self::VclR11 => 11,
            Self::VclN12 => 12,
            Self::VclR13 => 13,
            Self::VclN14 => 14,
            Self::VclR15 => 15,
            Self::BlaWLp => 16,
            Self::BlaWRadl => 17,
            Self::BlaNLp => 18,
            Self::IdrWRadl => 19,
            Self::IdrNLp => 20,
            Self::CraNut => 21,
            Self::RsvIrapVcl22 => 22,
            Self::RsvIrapVcl23 => 23,
            Self::Vps => 32,
            Self::Sps => 33,
            Self::Pps => 34,
            Self::Aud => 35,
            Self::EosNut => 36,
            Self::EobNut => 37,
            Self::FdNut => 38,
            Self::SeiPrefix => 39,
            Self::SeiSuffix => 40,
            Self::Other(c) => c,
        }
    }

    /// Whether this is a VCL NAL unit (type 0..=31).
    pub fn is_vcl(self) -> bool {
        self.code() <= 31
    }

    /// `IS_IRAP`: BLA (16..=18), IDR (19..=20), CRA (21) and reserved IRAP
    /// types (22..=23).
    pub fn is_irap(self) -> bool {
        (16..=23).contains(&self.code())
    }

    /// `IS_IDR`: IDR_W_RADL (19) and IDR_N_LP (20).
    pub fn is_idr(self) -> bool {
        matches!(self, Self::IdrWRadl | Self::IdrNLp)
    }

    /// Whether this NAL carries a parameter set (VPS/SPS/PPS).
    pub fn is_param_set(self) -> bool {
        matches!(self, Self::Vps | Self::Sps | Self::Pps)
    }
}

/// The two-byte HEVC NAL unit header (section 7.3.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalUnitHeader {
    /// Shall be zero.
    pub forbidden_zero_bit: bool,
    pub nal_unit_type: NalUnitType,
    pub nuh_layer_id: u8,
    pub nuh_temporal_id_plus1: u8,
}

/// Parse a NAL unit header and return it together with the EBSP payload.
///
/// `nal` includes the two-byte header. The returned payload slice has
/// emulation-prevention bytes still present; call [`ebsp_to_rbsp`] before
/// parsing parameter-set syntax.
pub fn parse_nal_header(nal: &[u8]) -> Result<(NalUnitHeader, &[u8])> {
    if nal.len() < 2 {
        return Err(Error::InvalidData);
    }
    let b0 = nal[0];
    let b1 = nal[1];
    let header = NalUnitHeader {
        forbidden_zero_bit: (b0 >> 7) != 0,
        nal_unit_type: NalUnitType::from_code((b0 >> 1) & 0x3f),
        nuh_layer_id: ((b0 & 1) << 5) | (b1 >> 3),
        nuh_temporal_id_plus1: b1 & 0x07,
    };
    if header.forbidden_zero_bit || header.nuh_temporal_id_plus1 == 0 {
        return Err(Error::InvalidData);
    }
    Ok((header, &nal[2..]))
}

/// A NAL unit after header parsing and emulation-prevention removal.
#[derive(Debug, Clone)]
pub struct ParsedNalUnit<'a> {
    pub header: NalUnitHeader,
    /// The EBSP payload (before emulation prevention removal).
    pub ebsp: &'a [u8],
    /// The RBSP payload (after emulation prevention removal). Certain NALs
    /// (SEI) are parsed from the EBSP directly, so both are retained.
    pub rbsp: Vec<u8>,
}

/// Parse a full NAL unit, removing emulation prevention bytes.
pub fn parse_nal_unit(nal: &[u8]) -> Result<ParsedNalUnit<'_>> {
    let (header, ebsp) = parse_nal_header(nal)?;
    let rbsp = ebsp_to_rbsp(ebsp);
    Ok(ParsedNalUnit { header, ebsp, rbsp })
}

/// Remove emulation prevention bytes from an EBSP to obtain the RBSP.
///
/// Whenever `0x00 0x00` is followed by `0x03`, the `0x03` is dropped (as long
/// as the following byte, if present, is `<= 0x03`), matching
/// `ff_h2645_packet_split`'s RBSP conversion.
pub fn ebsp_to_rbsp(ebsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ebsp.len());
    let mut zeros = 0u32;
    for &b in ebsp {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

/// Split an Annex B byte stream into NAL units (without start codes).
///
/// Recognizes 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes.
/// Trailing zero bytes are ignored.
pub fn annex_b_split(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).map_or(data.len(), |&next| {
            next.saturating_sub(if next >= 3 { 3 } else { 0 })
        });
        // A 4-byte start code leaves one extra zero in the preceding NAL.
        let mut end = end;
        if end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    nals
}

#[cfg(test)]
mod nal_tests {
    use super::*;

    #[test]
    fn parses_nal_header() {
        // forbidden=0, type=32 (VPS), layer_id=0, tid_plus1=1
        let data = [0x40, 0x01, 0xAA];
        let (h, payload) = parse_nal_header(&data).unwrap();
        assert_eq!(h.nal_unit_type, NalUnitType::Vps);
        assert_eq!(h.nuh_layer_id, 0);
        assert_eq!(h.nuh_temporal_id_plus1, 1);
        assert_eq!(payload, &[0xAA]);
        assert!(h.nal_unit_type.is_param_set());
        assert!(!h.nal_unit_type.is_vcl());
    }

    #[test]
    fn parses_layer_and_type() {
        // type=1, layer_id=3, tid_plus1=2: 0b0_000001_0 = 0x02, then (3<<3)|2 = 0x1A
        let data = [0x02, 0x1A];
        let (h, _) = parse_nal_header(&data).unwrap();
        assert_eq!(h.nal_unit_type, NalUnitType::TrailR);
        assert_eq!(h.nuh_layer_id, 3);
        assert_eq!(h.nuh_temporal_id_plus1, 2);
        assert!(h.nal_unit_type.is_vcl());
    }

    #[test]
    fn emulation_prevention() {
        let ebsp = [0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x03];
        assert_eq!(ebsp_to_rbsp(&ebsp), vec![0, 0, 1, 0, 0, 3]);
    }

    #[test]
    fn annex_b() {
        let data = [0, 0, 0, 1, 0x40, 0x01, 0, 0, 1, 0x42, 0x01];
        let nals = annex_b_split(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x40, 0x01]);
        assert_eq!(nals[1], &[0x42, 0x01]);
    }
}
