//! VPS RBSP parsing (`ff_hevc_decode_nal_vps` and `decode_vps_ext` in FFmpeg
//! `ps.c`). The extension follows FFmpeg's simplifying assumptions for
//! stereoscopic MV-HEVC / alpha layers (two layers, two layer sets).

use wedeo_core::{Error, Result};

use crate::bitreader::BitReader;
use crate::ptl::{ProfileTierLevel, SubLayerProfileTierLevel, parse_ptl};
use crate::vui::{HrdParams, decode_hrd, read_window};
use crate::{MAX_DPB_SIZE, MAX_LAYER_SETS, MAX_SUB_LAYERS, Window};

/// A VPS representation format (from the VPS extension), FFmpeg's
/// `RepFormat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepFormat {
    pub pic_width_in_luma_samples: u32,
    pub pic_height_in_luma_samples: u32,
    pub chroma_format_idc: u32,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub conf_win: Window,
}

/// DPB sizing parameters derived from the VPS extension (FFmpeg's
/// `repeated_vps_dpb_size`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VpsDpbSize {
    pub max_dec_pic_buffering: u32,
    pub max_num_reorder_pics: u32,
    pub max_latency_increase: u32,
}

/// Parameters parsed from the VPS extension (multi-layer streams).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VpsExtension {
    pub nb_layers: usize,
    pub layer_idx: [i8; 64],
    pub layer_id_in_nuh: [u8; 2],
    pub scalability_mask_flag: u16,
    pub num_direct_ref_layers: [u32; 2],
    pub default_ref_layers_active: bool,
    pub max_one_active_ref_layer: bool,
    pub poc_lsb_aligned: bool,
    pub poc_lsb_not_present: u32,
    pub num_output_layer_sets: u32,
    pub ols: [u64; 2],
    pub rep_format: Option<RepFormat>,
    pub dpb_size: VpsDpbSize,
}

impl Default for VpsExtension {
    fn default() -> Self {
        let mut layer_idx = [-1i8; 64];
        layer_idx[0] = 0;
        Self {
            nb_layers: 1,
            layer_idx,
            layer_id_in_nuh: [0, 0],
            scalability_mask_flag: 0,
            num_direct_ref_layers: [0; 2],
            default_ref_layers_active: false,
            max_one_active_ref_layer: false,
            poc_lsb_aligned: false,
            poc_lsb_not_present: 0,
            num_output_layer_sets: 0,
            ols: [0; 2],
            rep_format: None,
            dpb_size: VpsDpbSize::default(),
        }
    }
}

/// VPS syntax (FFmpeg's `HEVCVPS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vps {
    pub vps_id: u8,
    pub vps_max_layers: usize,
    pub vps_max_sub_layers: usize,
    pub vps_temporal_id_nesting_flag: bool,
    pub ptl: ProfileTierLevel,
    pub sub_layer_ptl: SubLayerProfileTierLevel,
    pub vps_sub_layer_ordering_info_present_flag: bool,
    pub vps_max_dec_pic_buffering: [u32; MAX_SUB_LAYERS],
    pub vps_num_reorder_pics: [u32; MAX_SUB_LAYERS],
    pub vps_max_latency_increase: [u32; MAX_SUB_LAYERS],
    pub vps_max_layer_id: u8,
    pub vps_num_layer_sets: usize,
    /// `layer_id_included_flag[0]`, used by the VPS extension.
    pub layer1_id_included: u64,
    pub vps_timing_info_present_flag: bool,
    pub vps_num_units_in_tick: u32,
    pub vps_time_scale: u32,
    pub vps_poc_proportional_to_timing_flag: bool,
    pub vps_num_ticks_poc_diff_one: u32,
    pub vps_num_hrd_parameters: usize,
    pub hdr: Vec<HrdParams>,
    pub num_output_layer_sets: usize,
    pub ols: [u64; 2],
    /// Populated only when `vps_extension_flag` is set and the extension is
    /// supported.
    pub extension: Option<VpsExtension>,
}

impl Default for Vps {
    fn default() -> Self {
        Self {
            vps_id: 0,
            vps_max_layers: 1,
            vps_max_sub_layers: 1,
            vps_temporal_id_nesting_flag: false,
            ptl: ProfileTierLevel::default(),
            sub_layer_ptl: SubLayerProfileTierLevel::default(),
            vps_sub_layer_ordering_info_present_flag: false,
            vps_max_dec_pic_buffering: [0; MAX_SUB_LAYERS],
            vps_num_reorder_pics: [0; MAX_SUB_LAYERS],
            vps_max_latency_increase: [0; MAX_SUB_LAYERS],
            vps_max_layer_id: 0,
            vps_num_layer_sets: 1,
            layer1_id_included: 0,
            vps_timing_info_present_flag: false,
            vps_num_units_in_tick: 0,
            vps_time_scale: 0,
            vps_poc_proportional_to_timing_flag: false,
            vps_num_ticks_poc_diff_one: 0,
            vps_num_hrd_parameters: 0,
            hdr: Vec::new(),
            num_output_layer_sets: 1,
            ols: [1, 0],
            extension: None,
        }
    }
}

/// `decode_vps_ext()` from `ps.c`, limited to the two-layer MV-HEVC /
/// auxiliary configurations FFmpeg supports.
fn decode_vps_ext(
    br: &mut BitReader<'_>,
    vps: &mut Vps,
    layer1_id_included: u64,
) -> Result<VpsExtension> {
    if vps.vps_max_layers == 1 {
        // "Ignoring VPS extensions with a single layer"
        return Ok(VpsExtension::default());
    }
    if vps.vps_max_layers > 2 || vps.vps_num_layer_sets > 2 {
        return Err(Error::PatchwelcomeNotImplemented);
    }

    br.align();

    // The PTL at the start of the extension is parsed and discarded
    // (`ptl_dummy` in FFmpeg).
    let _ = parse_ptl(br, false, vps.vps_max_sub_layers)?;

    let splitting_flag = br.read_flag()?;
    let scalability_mask_flag = br.read_bits(16)? as u16;
    let num_scalability_types = scalability_mask_flag.count_ones() as usize;
    if num_scalability_types == 0 {
        return Err(Error::InvalidData);
    }
    // MULTIVIEW (bit 14) or AUXILIARY (bit 12).
    if scalability_mask_flag & 0x4040 == 0 {
        return Err(Error::PatchwelcomeNotImplemented);
    }

    let mut dimension_id_len = [0u32; 16];
    let mut n = 0u32;
    for len in dimension_id_len
        .iter_mut()
        .take(num_scalability_types.saturating_sub(splitting_flag as usize))
    {
        *len = br.read_bits(3)? + 1;
        n += *len;
    }
    if splitting_flag {
        dimension_id_len[num_scalability_types - 1] = 5 - n;
    }

    let mut ext = VpsExtension {
        nb_layers: 2,
        layer_idx: {
            let mut l = [-1i8; 64];
            l[0] = 0;
            l
        },
        scalability_mask_flag,
        ols: vps.ols,
        ..Default::default()
    };

    if br.read_flag()? {
        let layer_id = br.read_bits(6)? as usize;
        if layer_id >= ext.layer_idx.len() {
            return Err(Error::InvalidData);
        }
        ext.layer_idx[layer_id] = 1;
        ext.layer_id_in_nuh[1] = layer_id as u8;
    } else {
        ext.layer_idx[1] = 1;
        ext.layer_id_in_nuh[1] = 1;
    }

    if !splitting_flag {
        let mut dimension_id = [0u32; 16];
        for i in 0..num_scalability_types {
            dimension_id[i] = br.read_bits(dimension_id_len[i] as usize)?;
        }
        if scalability_mask_flag & 0x4000 != 0 {
            // MULTIVIEW: one extra dimension index.
            // (index is only used for the auxiliary check below)
        }
        let _ = dimension_id;
    }

    let view_id_len = br.read_bits(4)? as usize;
    if view_id_len != 0 {
        let count = if scalability_mask_flag & 0x4000 != 0 {
            2
        } else {
            1
        };
        for _ in 0..count {
            let _ = br.read_bits(view_id_len)?; // view_id[i]
        }
    }

    ext.num_direct_ref_layers[1] = u32::from(br.read_flag()?);
    if ext.num_direct_ref_layers[1] == 0 {
        let num_add_layer_sets = br.read_ue()?;
        if num_add_layer_sets > 1 {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        if num_add_layer_sets != 0 && !br.read_flag()? {
            return Err(Error::PatchwelcomeNotImplemented);
        }
    }
    let num_output_layer_sets = vps.vps_num_layer_sets as u32 + ext.num_output_layer_sets;
    // `num_add_layer_sets` is folded into num_output_layer_sets above; FFmpeg
    // stores it separately, so recompute from the parsed value instead.
    let _ = num_output_layer_sets;

    let sub_layers_max_present = br.read_flag()?;
    if sub_layers_max_present {
        for _ in 0..vps.vps_max_layers {
            let _ = br.read_bits(3)?; // layer_max_sub_layers_minus1
        }
    }
    if br.read_flag()? {
        br.skip_bits(3); // max_tid_il_ref_pics_plus1
    }
    ext.default_ref_layers_active = br.read_flag()?;

    let nb_ptl = br.read_ue()? + 1;
    for _ in 2..nb_ptl {
        let profile_present = br.read_flag()?;
        let _ = parse_ptl(br, profile_present, vps.vps_max_sub_layers)?;
    }

    if br.read_ue()? != 0 {
        return Err(Error::PatchwelcomeNotImplemented); // num_add_olss
    }
    let default_output_layer_idc = br.read_bits(2)?;
    if default_output_layer_idc != 0 {
        return Err(Error::PatchwelcomeNotImplemented);
    }

    // FFmpeg's `layer_id_in_nuh[0]` is always 0 (the base layer).
    if layer1_id_included != 0 && layer1_id_included != (1u64 | 1u64 << ext.layer_id_in_nuh[1]) {
        return Err(Error::PatchwelcomeNotImplemented);
    }
    ext.ols[1] = if layer1_id_included == 0 { 2 } else { 3 };
    vps.num_output_layer_sets = 2;

    if vps.vps_num_layer_sets == 1 || default_output_layer_idc == 2 {
        br.skip_bits(1);
    }
    if nb_ptl > 1 {
        let bits = (nb_ptl as f64).log2().ceil() as usize;
        for _ in 0..ext.ols[1].count_ones() {
            let _ = br.read_bits(bits)?; // ptl_idx
        }
    }

    if br.read_ue()? != 0 {
        return Err(Error::PatchwelcomeNotImplemented); // vps_num_rep_formats_minus1
    }
    let width = br.read_bits(16)?;
    let height = br.read_bits(16)?;
    if !br.read_flag()? {
        return Err(Error::InvalidData); // chroma_and_bit_depth_vps_present_flag
    }
    let chroma_format_idc = br.read_bits(2)?;
    let separate_colour_plane_flag = if chroma_format_idc == 3 {
        br.read_flag()?
    } else {
        false
    };
    let bit_depth_luma = br.read_bits(4)? as u8 + 8;
    let bit_depth_chroma = br.read_bits(4)? as u8 + 8;
    if bit_depth_luma > 16 || bit_depth_chroma > 16 || bit_depth_luma != bit_depth_chroma {
        return Err(Error::PatchwelcomeNotImplemented);
    }
    let conf_win = if br.read_flag()? {
        read_window(br, chroma_format_idc, width, height)?
    } else {
        Window::default()
    };
    ext.rep_format = Some(RepFormat {
        pic_width_in_luma_samples: width,
        pic_height_in_luma_samples: height,
        chroma_format_idc,
        separate_colour_plane_flag,
        bit_depth_luma,
        bit_depth_chroma,
        conf_win,
    });

    ext.max_one_active_ref_layer = br.read_flag()?;
    ext.poc_lsb_aligned = br.read_flag()?;
    if ext.num_direct_ref_layers[1] == 0 {
        ext.poc_lsb_not_present = u32::from(br.read_flag()?) << 1;
    }

    let sub_layer_flag_info_present_flag = br.read_flag()?;
    for j in 0..vps.vps_max_sub_layers {
        let mut present = true;
        if j > 0 && sub_layer_flag_info_present_flag {
            present = br.read_flag()?;
        }
        if present {
            for _ in 0..ext.ols[1].count_ones() {
                ext.dpb_size.max_dec_pic_buffering = br.read_ue()? + 1;
            }
            ext.dpb_size.max_num_reorder_pics = br.read_ue()?;
            ext.dpb_size.max_latency_increase = br.read_ue()?.wrapping_sub(1);
        }
    }

    let direct_dep_type_len = br.read_ue()? + 2;
    if direct_dep_type_len > 32 {
        return Err(Error::InvalidData);
    }
    if br.read_flag()? {
        let direct_dep_type = br.read_bits(direct_dep_type_len as usize)?;
        if direct_dep_type > 2 {
            return Err(Error::PatchwelcomeNotImplemented);
        }
    }

    let non_vui_extension_length = br.read_ue()?;
    if non_vui_extension_length > 4096 {
        return Err(Error::InvalidData);
    }
    br.skip_bits(non_vui_extension_length as usize * 8);
    if br.read_flag()? {
        // VPS VUI is not supported by FFmpeg either.
    }

    Ok(ext)
}

/// Parse a VPS RBSP (payload after the NAL header, with emulation prevention
/// bytes already removed).
// Fields are assigned in bitstream order to mirror FFmpeg's sequential parser;
// a struct literal would obscure the syntax element ordering.
#[allow(clippy::field_reassign_with_default)]
pub fn parse_vps(rbsp: &[u8]) -> Result<Vps> {
    let mut br = BitReader::new(rbsp);
    let mut vps = Vps::default();

    vps.vps_id = br.read_bits(4)? as u8;

    let base_internal = br.read_flag()?;
    let base_available = br.read_flag()?;
    if !base_internal || !base_available {
        return Err(Error::PatchwelcomeNotImplemented);
    }

    vps.vps_max_layers = br.read_bits(6)? as usize + 1;
    vps.vps_max_sub_layers = br.read_bits(3)? as usize + 1;
    vps.vps_temporal_id_nesting_flag = br.read_flag()?;
    if br.read_bits(16)? != 0xffff {
        return Err(Error::InvalidData);
    }
    if vps.vps_max_sub_layers > MAX_SUB_LAYERS {
        return Err(Error::InvalidData);
    }

    let (ptl, sub) = parse_ptl(&mut br, true, vps.vps_max_sub_layers)?;
    vps.ptl = ptl;
    vps.sub_layer_ptl = sub;

    vps.vps_sub_layer_ordering_info_present_flag = br.read_flag()?;
    let start = if vps.vps_sub_layer_ordering_info_present_flag {
        0
    } else {
        vps.vps_max_sub_layers - 1
    };
    for i in start..vps.vps_max_sub_layers {
        vps.vps_max_dec_pic_buffering[i] = br.read_ue()? + 1;
        vps.vps_num_reorder_pics[i] = br.read_ue()?;
        vps.vps_max_latency_increase[i] = br.read_ue()?.wrapping_sub(1);
        if vps.vps_max_dec_pic_buffering[i] == 0
            || vps.vps_max_dec_pic_buffering[i] as usize > MAX_DPB_SIZE
        {
            return Err(Error::InvalidData);
        }
        if vps.vps_num_reorder_pics[i] > vps.vps_max_dec_pic_buffering[i] - 1 {
            return Err(Error::InvalidData);
        }
    }

    vps.vps_max_layer_id = br.read_bits(6)? as u8;
    vps.vps_num_layer_sets = br.read_ue()? as usize + 1;
    if vps.vps_num_layer_sets < 1 || vps.vps_num_layer_sets > MAX_LAYER_SETS {
        return Err(Error::InvalidData);
    }
    let layer_count =
        vps.vps_num_layer_sets.saturating_sub(1) * (vps.vps_max_layer_id as usize + 1);
    if layer_count > br.bits_left().max(0) as usize {
        return Err(Error::InvalidData);
    }

    vps.layer1_id_included = 0;
    let mut layer1_id_included = 0u64;
    if vps.vps_num_layer_sets > 1 {
        layer1_id_included = br.read_bits64(vps.vps_max_layer_id as usize + 1)?;
        vps.layer1_id_included = layer1_id_included;
    }
    if vps.vps_num_layer_sets > 2 {
        br.skip_bits((vps.vps_num_layer_sets - 2) * (vps.vps_max_layer_id as usize + 1));
    }
    vps.num_output_layer_sets = 1;
    vps.ols[0] = 1;

    vps.vps_timing_info_present_flag = br.read_flag()?;
    if vps.vps_timing_info_present_flag {
        vps.vps_num_units_in_tick = br.read_bits(32)?;
        vps.vps_time_scale = br.read_bits(32)?;
        vps.vps_poc_proportional_to_timing_flag = br.read_flag()?;
        if vps.vps_poc_proportional_to_timing_flag {
            vps.vps_num_ticks_poc_diff_one = br.read_ue()? + 1;
        }
        vps.vps_num_hrd_parameters = br.read_ue()? as usize;
        if vps.vps_num_hrd_parameters > vps.vps_num_layer_sets {
            return Err(Error::InvalidData);
        }
        for i in 0..vps.vps_num_hrd_parameters {
            let _ = br.read_ue()?; // hrd_layer_set_idx
            let common_inf_present = i == 0 || br.read_flag()?;
            vps.hdr.push(decode_hrd(
                &mut br,
                common_inf_present,
                vps.vps_max_sub_layers,
            )?);
        }
    }

    vps.extension = None;
    if vps.vps_max_layers > 1 && br.read_flag()? {
        match decode_vps_ext(&mut br, &mut vps, layer1_id_included) {
            Ok(ext) if ext.nb_layers >= 1 => {
                vps.extension = Some(ext);
            }
            Ok(_) => {}
            Err(Error::PatchwelcomeNotImplemented) => {
                // FFmpeg ignores unsupported extensions with a warning.
                vps.extension = None;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(vps)
}
