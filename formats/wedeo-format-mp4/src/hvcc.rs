// HEVC decoder configuration record (hvcC) construction.
//
// Reference: ISO/IEC 14496-15, ISO/IEC 23008-2. Ported from FFmpeg's
// `ff_isom_write_hvcc()` in `libavformat/hevc.c`: the muxer accepts either an
// already-built hvcC record (pass-through) or Annex B VPS/SPS/PPS NAL units,
// which are parsed and serialized into an hvcC record.

use wedeo_core::error::{Error, Result};

/// Minimum size of a syntactically valid hvcC record (FFmpeg's `ff_hevc_is_hvcc`).
const HVCC_MIN_SIZE: usize = 23;

/// Build an hvcC record from muxer extradata.
///
/// If `data` is already an hvcC record (`configurationVersion == 1`), it is
/// returned unchanged, matching FFmpeg's pass-through behaviour. Otherwise the
/// Annex B NAL units (VPS/SPS/PPS) are parsed into a configuration record.
pub fn build_hvcc(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() >= HVCC_MIN_SIZE && data[0] == 1 {
        return Ok(data.to_vec());
    }

    let nals = split_annex_b(data)?;
    if nals.is_empty() {
        return Err(Error::InvalidData);
    }

    // Parameter set NAL unit types (ISO/IEC 23008-2).
    let vps_nals: Vec<&[u8]> = nals.iter().copied().filter(|n| nal_type(n) == 32).collect();
    let sps_nals: Vec<&[u8]> = nals.iter().copied().filter(|n| nal_type(n) == 33).collect();
    let pps_nals: Vec<&[u8]> = nals.iter().copied().filter(|n| nal_type(n) == 34).collect();

    if sps_nals.is_empty() {
        return Err(Error::InvalidData);
    }

    let info = parse_sps(sps_nals[0]).ok_or(Error::InvalidData)?;

    let mut out = Vec::with_capacity(data.len() + 32);
    out.push(1); // configurationVersion
    out.push((info.profile_space << 6) | (info.tier_flag << 5) | (info.profile_idc & 0x1f));
    out.extend_from_slice(&info.compat_flags.to_be_bytes());
    // general_constraint_indicator_flags is 48 bits (6 bytes).
    out.extend_from_slice(&info.constraint_flags.to_be_bytes()[2..]);
    out.push(info.level_idc);
    out.extend_from_slice(&[0xf0, 0x00]); // reserved=1111, min_spatial_segmentation_idc=0
    out.push(0xfc); // reserved=111111, parallelismType=0
    out.push(0xfc | (info.chroma_format & 0x03));
    out.push(0xf8 | (info.bit_depth_luma_minus8 & 0x07));
    out.push(0xf8 | (info.bit_depth_chroma_minus8 & 0x07));
    out.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate
    out.push((info.num_temporal_layers << 3) | ((info.temporal_id_nested as u8) << 2) | 3);

    let arrays: [(u8, &[&[u8]]); 3] = [(32, &vps_nals), (33, &sps_nals), (34, &pps_nals)];
    let num_arrays = arrays.iter().filter(|(_, nals)| !nals.is_empty()).count();
    out.push(num_arrays as u8);
    for (nal_type, nals) in arrays {
        if nals.is_empty() {
            continue;
        }
        // array_completeness=1 (hvc1), reserved=0, NAL_unit_type.
        out.push(0x80 | nal_type);
        out.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for nal in nals {
            out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            out.extend_from_slice(nal);
        }
    }

    Ok(out)
}

/// Profile/tier/level and related fields parsed from the first SPS.
struct SpsInfo {
    profile_space: u8,
    tier_flag: u8,
    profile_idc: u8,
    compat_flags: u32,
    constraint_flags: u64,
    level_idc: u8,
    chroma_format: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_temporal_layers: u8,
    temporal_id_nested: bool,
}

/// Minimal bit reader for HEVC RBSP parsing.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        if self.bit >= self.data.len() * 8 {
            return None;
        }
        let byte = self.data[self.bit / 8];
        let bit = ((byte >> (7 - (self.bit % 8))) & 1) as u32;
        self.bit += 1;
        Some(bit)
    }

    fn read_bits(&mut self, n: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()?;
        }
        Some(value)
    }

    fn read_bits64(&mut self, n: usize) -> Option<u64> {
        let mut value = 0u64;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()? as u64;
        }
        Some(value)
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        for _ in 0..n {
            self.read_bit()?;
        }
        Some(())
    }

    /// Unsigned Exp-Golomb code (HEVC `u(v)` syntax elements).
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u32;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 32 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros as usize)?;
        Some((1u32 << leading_zeros) - 1 + suffix)
    }
}

/// Parse the general profile/tier/level from an SPS NAL unit (including its
/// 2-byte NAL header).
fn parse_sps(nal: &[u8]) -> Option<SpsInfo> {
    if nal.len() < 3 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[2..]);
    let mut br = BitReader::new(&rbsp);

    br.read_bits(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = br.read_bits(3)? as usize;
    let temporal_id_nested = br.read_bit()? != 0;

    // profile_tier_level(1, sps_max_sub_layers_minus1): general part.
    let profile_space = br.read_bits(2)? as u8;
    let tier_flag = br.read_bit()? as u8;
    let profile_idc = br.read_bits(5)? as u8;
    let compat_flags = br.read_bits(32)?;
    let constraint_flags = br.read_bits64(48)?;
    let level_idc = br.read_bits(8)? as u8;

    // Sub-layer PTL flags and payloads (skipped; only the general PTL is used).
    let mut sub_layer_profile_present = [false; 8];
    let mut sub_layer_level_present = [false; 8];
    for i in 0..max_sub_layers_minus1.min(8) {
        sub_layer_profile_present[i] = br.read_bit()? != 0;
        sub_layer_level_present[i] = br.read_bit()? != 0;
    }
    if max_sub_layers_minus1 > 0 {
        br.skip(2 * (8 - max_sub_layers_minus1))?; // reserved_zero_2bits
    }
    for i in 0..max_sub_layers_minus1 {
        if sub_layer_profile_present[i] {
            br.skip(88)?; // sub-layer profile info
        }
        if sub_layer_level_present[i] {
            br.skip(8)?;
        }
    }

    br.read_ue()?; // sps_seq_parameter_set_id
    let chroma_format = br.read_ue()? as u8;
    if chroma_format == 3 {
        br.read_bit()?; // separate_colour_plane_flag
    }
    br.read_ue()?; // pic_width_in_luma_samples
    br.read_ue()?; // pic_height_in_luma_samples
    if br.read_bit()? != 0 {
        // conformance_window_flag
        for _ in 0..4 {
            br.read_ue()?;
        }
    }
    let bit_depth_luma_minus8 = br.read_ue()? as u8;
    let bit_depth_chroma_minus8 = br.read_ue()? as u8;

    Some(SpsInfo {
        profile_space,
        tier_flag,
        profile_idc,
        compat_flags,
        constraint_flags,
        level_idc,
        chroma_format,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        num_temporal_layers: (max_sub_layers_minus1 + 1) as u8,
        temporal_id_nested,
    })
}

/// NAL unit type from a 2-byte NAL header.
fn nal_type(nal: &[u8]) -> u8 {
    if nal.len() < 2 {
        0
    } else {
        (nal[0] >> 1) & 0x3f
    }
}

/// Remove Annex B emulation prevention bytes (0x03 after 0x00 0x00).
fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u8;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        out.push(b);
        if b == 0 {
            zeros = (zeros + 1).min(2);
        } else {
            zeros = 0;
        }
    }
    out
}

/// Split Annex B byte stream into NAL units (each including its 2-byte header).
fn split_annex_b(data: &[u8]) -> Result<Vec<&[u8]>> {
    // Collect the offsets just after each `00 00 01` start code.
    let mut start_codes = Vec::new();
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            start_codes.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if start_codes.is_empty() {
        return Err(Error::InvalidData);
    }

    let mut nals = Vec::new();
    for (idx, &start) in start_codes.iter().enumerate() {
        let end = if idx + 1 < start_codes.len() {
            // The next NAL begins after its start code. Back up over the `01`
            // and the preceding zero bytes to find the start of that code.
            let mut e = start_codes[idx + 1] - 1; // position of the `01`
            while e > start && data[e - 1] == 0 {
                e -= 1;
            }
            e
        } else {
            let mut e = data.len();
            while e > start && data[e - 1] == 0 {
                e -= 1;
            }
            e
        };
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    Ok(nals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hvcc_passthrough() {
        let mut data = vec![1u8; HVCC_MIN_SIZE];
        data[0] = 1;
        assert_eq!(build_hvcc(&data).unwrap(), data);
    }

    #[test]
    fn split_and_nal_type() {
        let data = [0, 0, 0, 1, 0x40, 0x01, 0xff, 0, 0, 1, 0x42, 0x01, 0x02];
        let nals = split_annex_b(&data).unwrap();
        assert_eq!(nals.len(), 2);
        assert_eq!(nal_type(nals[0]), 32); // VPS
        assert_eq!(nal_type(nals[1]), 33); // SPS
    }

    /// Bit writer with an explicit bit cursor (MSB first).
    struct BitWriter {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bits: 0,
            }
        }

        fn push(&mut self, value: u64, n: usize) {
            for i in (0..n).rev() {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((value >> i) & 1) as u8;
                let last = self.bytes.last_mut().unwrap();
                *last |= bit << (7 - (self.bits % 8));
                self.bits += 1;
            }
        }

        fn push_ue(&mut self, value: u32) {
            let code = value + 1;
            let bits = 32 - code.leading_zeros() as usize;
            self.push(0, bits - 1);
            self.push(code as u64, bits);
        }

        fn finish(mut self) -> Vec<u8> {
            if !self.bits.is_multiple_of(8) {
                // rbsp_stop_one_bit plus alignment zeros.
                self.push(1, 1);
                while !self.bits.is_multiple_of(8) {
                    self.push(0, 1);
                }
            }
            self.bytes
        }
    }

    #[test]
    fn annex_b_to_hvcc() {
        // Build a valid SPS RBSP: Main profile (idc=1), level 30, 4:2:0, 8-bit.
        let mut w = BitWriter::new();
        w.push(0, 4); // sps_video_parameter_set_id
        w.push(0, 3); // sps_max_sub_layers_minus1
        w.push(1, 1); // sps_temporal_id_nesting_flag
        // general profile_tier_level
        w.push(0, 2); // general_profile_space
        w.push(0, 1); // general_tier_flag
        w.push(1, 5); // general_profile_idc = 1 (Main)
        w.push(0x6000_0000, 32); // compat flags
        w.push(0, 48); // constraint flags
        w.push(30, 8); // general_level_idc = 30
        w.push_ue(0); // sps_seq_parameter_set_id
        w.push_ue(1); // chroma_format_idc = 1 (4:2:0)
        w.push_ue(320); // pic_width_in_luma_samples
        w.push_ue(240); // pic_height_in_luma_samples
        w.push(0, 1); // conformance_window_flag
        w.push_ue(0); // bit_depth_luma_minus8
        w.push_ue(0); // bit_depth_chroma_minus8
        let sps_rbsp = w.finish();

        let mut sps = vec![0x42, 0x01]; // NAL header (type 33)
        sps.extend_from_slice(&sps_rbsp);
        let vps = [0x40, 0x01, 0xaa, 0xbb]; // type 32
        let pps = [0x44, 0x01, 0xcc]; // type 34

        let mut annex_b = Vec::new();
        for nal in [&vps[..], &sps[..], &pps[..]] {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(nal);
        }
        let hvcc = build_hvcc(&annex_b).unwrap();
        assert_eq!(hvcc[0], 1);
        assert!(hvcc.len() >= HVCC_MIN_SIZE);
        assert_eq!(hvcc[1] & 0x1f, 1); // profile_idc = Main
        assert_eq!(hvcc[12], 30); // level_idc
        assert_eq!(hvcc[21] & 0x03, 3); // lengthSizeMinusOne = 3 (4-byte NAL lengths)
        assert_eq!(hvcc[22], 3); // three arrays: VPS, SPS, PPS
        assert!(hvcc.windows(vps.len()).any(|w| w == vps));
        assert!(hvcc.windows(sps.len()).any(|w| w == sps));
    }
}
