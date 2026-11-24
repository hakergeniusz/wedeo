//! Profile/Tier/Level parsing (`decode_profile_tier_level`, `parse_ptl` in
//! FFmpeg `ps.c`).

use wedeo_core::{Error, Result};

use crate::bitreader::BitReader;

/// FFmpeg's `PTLCommon`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileTierLevel {
    pub profile_space: u32,
    pub tier_flag: bool,
    pub profile_idc: u32,
    pub profile_compatibility_flag: [bool; 32],
    pub progressive_source_flag: bool,
    pub interlaced_source_flag: bool,
    pub non_packed_constraint_flag: bool,
    pub frame_only_constraint_flag: bool,
    pub max_12bit_constraint_flag: bool,
    pub max_10bit_constraint_flag: bool,
    pub max_8bit_constraint_flag: bool,
    pub max_422chroma_constraint_flag: bool,
    pub max_420chroma_constraint_flag: bool,
    pub max_monochrome_constraint_flag: bool,
    pub intra_constraint_flag: bool,
    pub one_picture_only_constraint_flag: bool,
    pub lower_bit_rate_constraint_flag: bool,
    pub max_14bit_constraint_flag: bool,
    pub inbld_flag: bool,
    pub level_idc: u32,
}

/// FFmpeg's `PTL` sub-layer presence flags and the eight possible sub-layer
/// PTLs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubLayerProfileTierLevel {
    pub sub_layer_profile_present_flag: [bool; 7],
    pub sub_layer_level_present_flag: [bool; 7],
    pub sub_layer_ptl: [ProfileTierLevel; 7],
}

/// `check_profile_idc(idc)` from `decode_profile_tier_level`.
fn check_profile_idc(ptl: &ProfileTierLevel, idc: usize) -> bool {
    ptl.profile_idc as usize == idc || ptl.profile_compatibility_flag[idc]
}

/// `decode_profile_tier_level()` from `ps.c`.
fn decode_profile_tier_level(br: &mut BitReader<'_>) -> Result<ProfileTierLevel> {
    if br.bits_left() < 2 + 1 + 5 + 32 + 4 + 43 + 1 {
        return Err(Error::InvalidData);
    }
    let mut ptl = ProfileTierLevel {
        profile_space: br.read_bits(2)?,
        tier_flag: br.read_flag()?,
        profile_idc: br.read_bits(5)?,
        ..Default::default()
    };

    for i in 0..32 {
        ptl.profile_compatibility_flag[i] = br.read_flag()?;
        if ptl.profile_idc == 0 && i > 0 && ptl.profile_compatibility_flag[i] {
            ptl.profile_idc = i as u32;
        }
    }
    ptl.progressive_source_flag = br.read_flag()?;
    ptl.interlaced_source_flag = br.read_flag()?;
    ptl.non_packed_constraint_flag = br.read_flag()?;
    ptl.frame_only_constraint_flag = br.read_flag()?;

    if (2..=10).any(|idc| check_profile_idc(&ptl, idc)) {
        ptl.max_12bit_constraint_flag = br.read_flag()?;
        ptl.max_10bit_constraint_flag = br.read_flag()?;
        ptl.max_8bit_constraint_flag = br.read_flag()?;
        ptl.max_422chroma_constraint_flag = br.read_flag()?;
        ptl.max_420chroma_constraint_flag = br.read_flag()?;
        ptl.max_monochrome_constraint_flag = br.read_flag()?;
        ptl.intra_constraint_flag = br.read_flag()?;
        ptl.one_picture_only_constraint_flag = br.read_flag()?;
        ptl.lower_bit_rate_constraint_flag = br.read_flag()?;

        if [5, 9, 10].iter().any(|&idc| check_profile_idc(&ptl, idc)) {
            ptl.max_14bit_constraint_flag = br.read_flag()?;
            br.skip_bits(33); // XXX_reserved_zero_33bits[0..32]
        } else {
            br.skip_bits(34); // XXX_reserved_zero_34bits[0..33]
        }
    } else if check_profile_idc(&ptl, 2) {
        br.skip_bits(7);
        ptl.one_picture_only_constraint_flag = br.read_flag()?;
        br.skip_bits(35); // XXX_reserved_zero_35bits[0..34]
    } else {
        br.skip_bits(43); // XXX_reserved_zero_43bits[0..42]
    }

    if [1, 2, 3, 4, 5, 9]
        .iter()
        .any(|&idc| check_profile_idc(&ptl, idc))
    {
        ptl.inbld_flag = br.read_flag()?;
    } else {
        br.skip_bits(1);
    }

    Ok(ptl)
}

/// `parse_ptl()` from `ps.c`.
///
/// `profile_present` selects whether the general profile is signalled (VPS,
/// SPS) or inferred to be zero (VPS extension). `max_num_sub_layers` is the
/// number of sub-layers (1 for a single-layer stream).
pub fn parse_ptl(
    br: &mut BitReader<'_>,
    profile_present: bool,
    max_num_sub_layers: usize,
) -> Result<(ProfileTierLevel, SubLayerProfileTierLevel)> {
    let general = if profile_present {
        decode_profile_tier_level(br)?
    } else {
        ProfileTierLevel::default()
    };

    if br.bits_left() < 8 + (8 * 2 * (max_num_sub_layers.saturating_sub(1))) as isize {
        return Err(Error::InvalidData);
    }

    let mut general = general;
    general.level_idc = br.read_bits(8)?;

    let mut sub = SubLayerProfileTierLevel::default();
    for i in 0..max_num_sub_layers.saturating_sub(1) {
        sub.sub_layer_profile_present_flag[i] = br.read_flag()?;
        sub.sub_layer_level_present_flag[i] = br.read_flag()?;
    }
    if max_num_sub_layers > 1 {
        for _ in (max_num_sub_layers - 1)..8 {
            br.skip_bits(2); // reserved_zero_2bits[i]
        }
    }
    for i in 0..max_num_sub_layers.saturating_sub(1) {
        if sub.sub_layer_profile_present_flag[i] {
            sub.sub_layer_ptl[i] = decode_profile_tier_level(br)?;
        }
        if sub.sub_layer_level_present_flag[i] {
            if br.bits_left() < 8 {
                return Err(Error::InvalidData);
            }
            sub.sub_layer_ptl[i].level_idc = br.read_bits(8)?;
        }
    }

    Ok((general, sub))
}
