//! HEVCDecoderConfigurationRecord (`hvcC`) generation from Annex B NAL units.
//!
//! Ported from FFmpeg `libavformat/hevc.c`:
//! `hvcc_init()`, `hvcc_update_ptl()`, `hvcc_array_add_nal_unit()`,
//! `hvcc_add_nal_unit()`, `hvcc_write()` and `ff_isom_write_hvcc()`.
//!
//! The raw Annex B demuxer keeps its packets in Annex B form, so `hvcC` is not
//! written into `CodecParameters::extradata` here. This module is a reusable
//! capability (used by MP4 muxers / the `hevc_mp4toannexb` bitstream filter)
//! and is exercised by unit tests in this crate.

use wedeo_codec_hevc_ps::{
    NalUnitType, ProfileTierLevel, Sps, Vps, ebsp_to_rbsp, parse_nal_header, parse_sps, parse_vps,
};
use wedeo_core::error::{Error, Result};

/// Maximum number of NAL units in one `hvcC` array (`UINT16_MAX`).
const MAX_NALS_PER_ARRAY: usize = u16::MAX as usize;

/// Indices of the five arrays FFmpeg keeps in an `hvcC`
/// (`libavformat/hevc.c:34-40`).
const VPS_INDEX: usize = 0;
const SPS_INDEX: usize = 1;
const PPS_INDEX: usize = 2;
const SEI_PREFIX_INDEX: usize = 3;
const SEI_SUFFIX_INDEX: usize = 4;
const NB_ARRAYS: usize = 5;

/// One parameter-set array in the configuration record.
#[derive(Debug, Clone, Default)]
struct HvccArray {
    array_completeness: bool,
    nal_unit_type: u8,
    nals: Vec<(Vec<u8>, u8)>, // (nal bytes including 2-byte header, nuh_layer_id)
}

/// Mutable state mirroring FFmpeg's `HEVCDecoderConfigurationRecord`.
#[derive(Clone)]
struct Hvcc {
    general_profile_space: u8,
    general_tier_flag: bool,
    general_profile_idc: u8,
    general_profile_compatibility_flags: u32,
    general_constraint_indicator_flags: u64, // 48 bits
    general_level_idc: u8,
    min_spatial_segmentation_idc: u16,
    parallelism_type: u8,
    chroma_format: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_temporal_layers: u8,
    temporal_id_nested: bool,
    arrays: [HvccArray; NB_ARRAYS],
}

impl Hvcc {
    /// `hvcc_init()` from `libavformat/hevc.c:926-944`.
    fn new() -> Self {
        Self {
            general_profile_space: 0,
            general_tier_flag: false,
            general_profile_idc: 0,
            general_profile_compatibility_flags: 0xffff_ffff,
            general_constraint_indicator_flags: 0x0000_ffff_ffff_ffff,
            general_level_idc: 0,
            // Invalid value: reset to 0 (unspecified) when writing.
            min_spatial_segmentation_idc: MAX_SPATIAL_SEGMENTATION + 1,
            parallelism_type: 0,
            chroma_format: 0,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            num_temporal_layers: 0,
            temporal_id_nested: false,
            arrays: std::array::from_fn(|_| HvccArray::default()),
        }
    }

    /// `hvcc_update_ptl()` from `libavformat/hevc.c:89-141`.
    fn update_ptl(&mut self, ptl: &ProfileTierLevel) {
        self.general_profile_space = ptl.profile_space as u8;

        // general_level_idc must be equal to or greater than the highest
        // level indicated for the highest tier in all the parameter sets.
        if !self.general_tier_flag && ptl.tier_flag {
            self.general_level_idc = ptl.level_idc as u8;
        } else {
            self.general_level_idc = self.general_level_idc.max(ptl.level_idc as u8);
        }
        self.general_tier_flag |= ptl.tier_flag;
        self.general_profile_idc = self.general_profile_idc.max(ptl.profile_idc as u8);

        // Each bit may only be set if all the parameter sets set that bit.
        let mut compat = 0u32;
        for (i, &set) in ptl.profile_compatibility_flag.iter().enumerate() {
            if set {
                compat |= 1u32 << (31 - i);
            }
        }
        self.general_profile_compatibility_flags &= compat;
        self.general_constraint_indicator_flags &= constraint_indicator_flags(ptl);
    }

    /// `hvcc_array_add_nal_unit()` / `hvcc_add_nal_unit()` from
    /// `libavformat/hevc.c:831-924`.
    fn add_nal(&mut self, nal: &[u8], array_idx: usize, completeness: bool) -> Result<()> {
        let (header, _) = parse_nal_header(nal)?;
        if header.nuh_layer_id > 0 {
            return Ok(()); // only base-layer NALs in a plain hvcC
        }
        let array = &mut self.arrays[array_idx];
        if array.nals.len() >= MAX_NALS_PER_ARRAY {
            return Err(Error::InvalidData);
        }
        if array.nals.is_empty() {
            array.nal_unit_type = nal_type_code(header.nal_unit_type);
            // Default and mandatory value is 1 for arrays of parameter sets.
            array.array_completeness = completeness
                && matches!(
                    header.nal_unit_type,
                    NalUnitType::Vps | NalUnitType::Sps | NalUnitType::Pps
                );
        }
        array.nals.push((nal.to_vec(), header.nuh_layer_id));
        Ok(())
    }

    /// `hvcc_write()` from `libavformat/hevc.c:955-1193`.
    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut num_nals = [0u16; NB_ARRAYS];
        let mut num_arrays = 0usize;
        for i in 0..NB_ARRAYS {
            num_nals[i] = self.arrays[i].nals.len() as u16;
            if num_nals[i] > 0 {
                num_arrays += 1;
            }
        }

        // We need at least one of each: VPS, SPS and PPS when completeness
        // was requested (hvc1 sample entry).
        if num_nals[VPS_INDEX] == 0
            || num_nals[VPS_INDEX] > 16
            || num_nals[SPS_INDEX] == 0
            || num_nals[SPS_INDEX] > 16
            || num_nals[PPS_INDEX] == 0
            || num_nals[PPS_INDEX] > 64
        {
            return Err(Error::InvalidData);
        }

        // unsigned int(8) configurationVersion = 1;
        out.push(1);

        // unsigned int(2) general_profile_space;
        // unsigned int(1) general_tier_flag;
        // unsigned int(5) general_profile_idc;
        out.push(
            self.general_profile_space << 6
                | (u8::from(self.general_tier_flag) << 5)
                | self.general_profile_idc,
        );

        // unsigned int(32) general_profile_compatibility_flags;
        out.extend_from_slice(&self.general_profile_compatibility_flags.to_be_bytes());

        // unsigned int(48) general_constraint_indicator_flags;
        let cif = self.general_constraint_indicator_flags;
        out.extend_from_slice(&[(cif >> 40) as u8, (cif >> 32) as u8, (cif >> 24) as u8]);
        out.extend_from_slice(&[(cif >> 16) as u8, (cif >> 8) as u8, cif as u8]);

        // unsigned int(8) general_level_idc;
        out.push(self.general_level_idc);

        // bit(4) reserved = '1111'b; unsigned int(12) min_spatial_segmentation_idc;
        let mss = if self.min_spatial_segmentation_idc > MAX_SPATIAL_SEGMENTATION {
            0
        } else {
            self.min_spatial_segmentation_idc
        };
        out.extend_from_slice(&(mss | 0xf000).to_be_bytes());

        // bit(6) reserved = '111111'b; unsigned int(2) parallelismType;
        out.push(self.parallelism_type | 0xfc);

        // bit(6) reserved = '111111'b; unsigned int(2) chromaFormat;
        out.push(self.chroma_format | 0xfc);
        // bit(5) reserved = '11111'b; unsigned int(3) bitDepthLumaMinus8;
        out.push(self.bit_depth_luma_minus8 | 0xf8);
        // bit(5) reserved = '11111'b; unsigned int(3) bitDepthChromaMinus8;
        out.push(self.bit_depth_chroma_minus8 | 0xf8);

        // bit(16) avgFrameRate; (always zero: "unspecified" in FFmpeg)
        out.extend_from_slice(&0u16.to_be_bytes());

        // bit(2) constantFrameRate = 0;
        // bit(3) numTemporalLayers;
        // bit(1) temporalIdNested;
        // unsigned int(2) lengthSizeMinusOne = 3 (4-byte NAL lengths).
        out.push((self.num_temporal_layers << 3) | (u8::from(self.temporal_id_nested) << 2) | 3);

        // unsigned int(8) numOfArrays;
        out.push(num_arrays as u8);

        for i in 0..NB_ARRAYS {
            let array = &self.arrays[i];
            if num_nals[i] == 0 {
                continue;
            }
            // bit(1) array_completeness; unsigned int(1) reserved = 0;
            // unsigned int(6) NAL_unit_type;
            out.push((u8::from(array.array_completeness) << 7) | (array.nal_unit_type & 0x3f));
            out.extend_from_slice(&num_nals[i].to_be_bytes());
            for (nal, _) in &array.nals {
                let len = u16::try_from(nal.len()).map_err(|_| Error::InvalidData)?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
        Ok(())
    }
}

const MAX_SPATIAL_SEGMENTATION: u16 = 4095;

fn nal_type_code(t: NalUnitType) -> u8 {
    t.code()
}

/// Rebuild the 48-bit `general_constraint_indicator_flags` from the parsed
/// profile/tier/level structure. Bits are laid out exactly as FFmpeg reads
/// them with `get_bits64(gb, 48)` (see `hvcc_parse_ptl`, hevc.c:158).
fn constraint_indicator_flags(ptl: &ProfileTierLevel) -> u64 {
    let mut v = 0u64;
    // First four flags occupy the most significant four bits.
    v |= u64::from(ptl.progressive_source_flag) << 47;
    v |= u64::from(ptl.interlaced_source_flag) << 46;
    v |= u64::from(ptl.non_packed_constraint_flag) << 45;
    v |= u64::from(ptl.frame_only_constraint_flag) << 44;

    // The remaining 44 bits are profile-dependent. The PS crate keeps the
    // flags it parsed; reconstruct their positions in the bitstream.
    let compat =
        |idc: usize| ptl.profile_idc as usize == idc || ptl.profile_compatibility_flag[idc];
    let rext = (2..=10).any(compat);
    if rext {
        v |= u64::from(ptl.max_12bit_constraint_flag) << 43;
        v |= u64::from(ptl.max_10bit_constraint_flag) << 42;
        v |= u64::from(ptl.max_8bit_constraint_flag) << 41;
        v |= u64::from(ptl.max_422chroma_constraint_flag) << 40;
        v |= u64::from(ptl.max_420chroma_constraint_flag) << 39;
        v |= u64::from(ptl.max_monochrome_constraint_flag) << 38;
        v |= u64::from(ptl.intra_constraint_flag) << 37;
        v |= u64::from(ptl.one_picture_only_constraint_flag) << 36;
        v |= u64::from(ptl.lower_bit_rate_constraint_flag) << 35;
        if [5, 9, 10].iter().any(|&idc| compat(idc)) {
            v |= u64::from(ptl.max_14bit_constraint_flag) << 34;
        }
    } else if compat(2) {
        // `XXX_reserved_zero_43bits` then one_picture_only at bit 7 of this
        // group (matches `ps.c:89`): 7 skipped, flag, 35 skipped.
        v |= u64::from(ptl.one_picture_only_constraint_flag) << 36;
    }
    if [1, 2, 3, 4, 5, 9].iter().any(|&idc| compat(idc)) {
        v |= u64::from(ptl.inbld_flag);
    }
    v
}

/// Build an `hvcC` configuration record from Annex B NAL units.
///
/// `nals` are complete NAL units including their two-byte headers (no start
/// codes), as produced by `annex_b_split()` or this crate's NAL scanner.
/// VPS/SPS/PPS NALs are stored in the record; PTL fields are taken from the
/// SPS (and VPS) so the record describes the stream's profile/tier/level.
///
/// This is the equivalent of FFmpeg's `ff_isom_write_hvcc()` with
/// `ps_array_completeness = 1` (`hvc1` sample entry).
pub fn annexb_to_hvcc(nals: &[&[u8]]) -> Result<Vec<u8>> {
    let mut hvcc = Hvcc::new();
    let mut sps: Option<Sps> = None;
    let mut vps: Option<Vps> = None;

    for &nal in nals {
        let (header, _) = parse_nal_header(nal)?;
        if header.nuh_layer_id > 0 {
            continue;
        }
        let array_idx = match header.nal_unit_type {
            NalUnitType::Vps => {
                let (_, ebsp) = parse_nal_header(nal)?;
                if let Ok(parsed) = parse_vps(&ebsp_to_rbsp(ebsp)) {
                    vps = Some(parsed);
                }
                VPS_INDEX
            }
            NalUnitType::Sps => {
                let (_, ebsp) = parse_nal_header(nal)?;
                if let Ok(parsed) = parse_sps(&ebsp_to_rbsp(ebsp), header.nuh_layer_id) {
                    sps = Some(parsed);
                }
                SPS_INDEX
            }
            NalUnitType::Pps => PPS_INDEX,
            NalUnitType::SeiPrefix => SEI_PREFIX_INDEX,
            NalUnitType::SeiSuffix => SEI_SUFFIX_INDEX,
            _ => continue,
        };
        hvcc.add_nal(nal, array_idx, true)?;
    }

    // Derive the configuration PTL from the SPS (preferred) or VPS.
    if let Some(ref s) = sps {
        hvcc.update_ptl(&s.ptl);
        hvcc.chroma_format = s.chroma_format_idc as u8;
        hvcc.bit_depth_luma_minus8 = s.bit_depth.saturating_sub(8) as u8;
        hvcc.bit_depth_chroma_minus8 = s.bit_depth_chroma.saturating_sub(8) as u8;
        hvcc.num_temporal_layers = (s.max_sub_layers.min(7) as u8).max(1);
        hvcc.temporal_id_nested = s.temporal_id_nesting;
        if s.vui_present && s.vui.bitstream_restriction_flag {
            hvcc.min_spatial_segmentation_idc = s.vui.min_spatial_segmentation_idc as u16;
        }
    } else if let Some(ref v) = vps {
        hvcc.update_ptl(&v.ptl);
        hvcc.num_temporal_layers = (v.vps_max_sub_layers.min(7) as u8).max(1);
        hvcc.temporal_id_nested = v.vps_temporal_id_nesting_flag;
    }

    let mut out = Vec::new();
    hvcc.write(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_codec_hevc_ps::annex_b_split;

    #[test]
    fn hvcc_init_defaults_match_ffmpeg() {
        let h = Hvcc::new();
        assert_eq!(h.general_profile_compatibility_flags, 0xffff_ffff);
        assert_eq!(h.general_constraint_indicator_flags, 0x0000_ffff_ffff_ffff);
        assert_eq!(h.min_spatial_segmentation_idc, MAX_SPATIAL_SEGMENTATION + 1);
    }

    #[test]
    fn hvcc_write_rejects_missing_parameter_sets() {
        let h = Hvcc::new();
        let mut out = Vec::new();
        assert!(h.write(&mut out).is_err());
    }

    #[test]
    fn annexb_to_hvcc_rejects_non_nal_data() {
        assert!(annexb_to_hvcc(&[&[0x80, 0x00]]).is_err());
        // No parameter sets at all.
        assert!(annexb_to_hvcc(&[]).is_err());
    }

    #[test]
    fn annexb_to_hvcc_from_fate_vector() {
        let suite = match std::env::var("FATE_SUITE") {
            Ok(s) => s,
            Err(_) => return,
        };
        let path = std::path::Path::new(&suite).join("hevc/ipcm_A_NEC_2.bit");
        let Ok(data) = std::fs::read(&path) else {
            return;
        };
        let nals = annex_b_split(&data);
        let ps_nals: Vec<&[u8]> = nals
            .into_iter()
            .filter(|n| {
                parse_nal_header(n).is_ok_and(|(h, _)| {
                    matches!(
                        h.nal_unit_type,
                        NalUnitType::Vps | NalUnitType::Sps | NalUnitType::Pps
                    )
                })
            })
            .collect();
        let hvcc = annexb_to_hvcc(&ps_nals).expect("generate hvcC");
        // configurationVersion, lengthSizeMinusOne = 3, numOfArrays >= 3.
        assert_eq!(hvcc[0], 1);
        assert_eq!(hvcc[21] & 3, 3);
        assert!(hvcc[22] >= 3);

        // The generated record must be parseable by the shared HEVC PS crate
        // (the same entry point the native decoder uses for extradata).
        let decoded = wedeo_codec_hevc_ps::decode_extradata(&hvcc).expect("parse generated hvcC");
        assert!(decoded.sps_list.iter().any(Option::is_some));
        assert!(decoded.pps_list.iter().any(Option::is_some));
    }
}
