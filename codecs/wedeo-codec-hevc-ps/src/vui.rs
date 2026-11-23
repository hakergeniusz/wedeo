//! VUI and HRD parsing (`decode_vui`, `decode_hrd`,
//! `ff_h2645_decode_common_vui_params` in FFmpeg `ps.c` / `h2645_vui.c`).
//!
//! FFmpeg contains several "alternate syntax" retries for malformed VUI
//! bitstreams; this parser implements the spec syntax only, which is
//! bit-exact for well-formed streams. See `DIVERGENCES` notes in the report.

use wedeo_core::{Error, Rational, Result};

use crate::bitreader::BitReader;
use crate::{SUB_HEIGHT_C, SUB_WIDTH_C, Window};

/// `ff_h2645_pixel_aspect[]` from `libavcodec/h2645data.c`.
pub const PIXEL_ASPECT: [Rational; 17] = [
    Rational::new(0, 1),
    Rational::new(1, 1),
    Rational::new(12, 11),
    Rational::new(10, 11),
    Rational::new(16, 11),
    Rational::new(40, 33),
    Rational::new(24, 11),
    Rational::new(20, 11),
    Rational::new(32, 11),
    Rational::new(80, 33),
    Rational::new(18, 11),
    Rational::new(15, 11),
    Rational::new(64, 33),
    Rational::new(160, 99),
    Rational::new(4, 3),
    Rational::new(3, 2),
    Rational::new(2, 1),
];

const EXTENDED_SAR: u8 = 255;

/// The common H.264/HEVC VUI parameters (`H2645VUI`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonVui {
    pub aspect_ratio_info_present_flag: bool,
    pub aspect_ratio_idc: u8,
    /// Sample aspect ratio, `0/1` when unspecified.
    pub sar: Rational,
    pub overscan_info_present_flag: bool,
    pub overscan_appropriate_flag: bool,
    pub video_signal_type_present_flag: bool,
    pub video_format: u32,
    pub video_full_range_flag: bool,
    pub colour_description_present_flag: bool,
    pub colour_primaries: u32,
    pub transfer_characteristics: u32,
    pub matrix_coeffs: u32,
    pub chroma_loc_info_present_flag: bool,
    pub chroma_sample_loc_type_top_field: u32,
    pub chroma_sample_loc_type_bottom_field: u32,
    pub chroma_location: u32,
}

impl Default for CommonVui {
    fn default() -> Self {
        Self {
            aspect_ratio_info_present_flag: false,
            aspect_ratio_idc: 0,
            sar: Rational::new(0, 1),
            overscan_info_present_flag: false,
            overscan_appropriate_flag: false,
            video_signal_type_present_flag: false,
            video_format: 0,
            video_full_range_flag: false,
            colour_description_present_flag: false,
            colour_primaries: 2,         // AVCOL_PRI_UNSPECIFIED
            transfer_characteristics: 2, // AVCOL_TRC_UNSPECIFIED
            matrix_coeffs: 2,            // AVCOL_SPC_UNSPECIFIED
            chroma_loc_info_present_flag: false,
            chroma_sample_loc_type_top_field: 0,
            chroma_sample_loc_type_bottom_field: 0,
            chroma_location: 1, // AVCHROMA_LOC_LEFT
        }
    }
}

/// `HEVCSublayerHdrParams`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubLayerHrdParams {
    pub bit_rate_value_minus1: Vec<u32>,
    pub cpb_size_value_minus1: Vec<u32>,
    pub cpb_size_du_value_minus1: Vec<u32>,
    pub bit_rate_du_value_minus1: Vec<u32>,
    pub cbr_flag: u32,
}

/// `HEVCHdrParams`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HrdParams {
    pub nal_hrd_parameters_present_flag: bool,
    pub vcl_hrd_parameters_present_flag: bool,
    pub sub_pic_hrd_params_present_flag: bool,
    pub tick_divisor_minus2: u32,
    pub du_cpb_removal_delay_increment_length_minus1: u32,
    pub sub_pic_cpb_params_in_pic_timing_sei_flag: bool,
    pub dpb_output_delay_du_length_minus1: u32,
    pub bit_rate_scale: u32,
    pub cpb_size_scale: u32,
    pub cpb_size_du_scale: u32,
    pub initial_cpb_removal_delay_length_minus1: u32,
    pub au_cpb_removal_delay_length_minus1: u32,
    pub dpb_output_delay_length_minus1: u32,
    pub fixed_pic_rate_general_flag: u32,
    pub fixed_pic_rate_within_cvs_flag: u32,
    pub low_delay_hrd_flag: u32,
    pub elemental_duration_in_tc_minus1: Vec<u32>,
    pub cpb_cnt_minus1: Vec<u32>,
    pub nal_params: Vec<SubLayerHrdParams>,
    pub vcl_params: Vec<SubLayerHrdParams>,
}

/// HEVC-specific VUI (`VUI` in `ps.c`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vui {
    pub common: CommonVui,
    pub neutra_chroma_indication_flag: bool,
    pub field_seq_flag: bool,
    pub frame_field_info_present_flag: bool,
    pub default_display_window_flag: bool,
    pub def_disp_win: Window,
    pub vui_timing_info_present_flag: bool,
    pub vui_num_units_in_tick: u32,
    pub vui_time_scale: u32,
    pub vui_poc_proportional_to_timing_flag: bool,
    pub vui_num_ticks_poc_diff_one_minus1: u32,
    pub vui_hrd_parameters_present_flag: bool,
    pub hrd: HrdParams,
    pub bitstream_restriction_flag: bool,
    pub tiles_fixed_structure_flag: bool,
    pub motion_vectors_over_pic_boundaries_flag: bool,
    pub restricted_ref_pic_lists_flag: bool,
    pub min_spatial_segmentation_idc: u32,
    pub max_bytes_per_pic_denom: u32,
    pub max_bits_per_min_cu_denom: u32,
    pub log2_max_mv_length_horizontal: u32,
    pub log2_max_mv_length_vertical: u32,
}

/// `read_window()` from `ps.c`. Offsets are multiplied by the chroma
/// sub-sampling factors and validated against the picture size.
pub fn read_window(
    br: &mut BitReader<'_>,
    chroma_format_idc: u32,
    width: u32,
    height: u32,
) -> Result<Window> {
    let chroma = (chroma_format_idc as usize).min(3);
    let vert_mult = SUB_HEIGHT_C[chroma] as i64;
    let horiz_mult = SUB_WIDTH_C[chroma] as i64;
    let left = br.read_ue()? as i64 * horiz_mult;
    let right = br.read_ue()? as i64 * horiz_mult;
    let top = br.read_ue()? as i64 * vert_mult;
    let bottom = br.read_ue()? as i64 * vert_mult;

    if left < 0
        || right < 0
        || top < 0
        || bottom < 0
        || width as i64 <= left + right
        || height as i64 <= top + bottom
    {
        return Err(Error::InvalidData);
    }

    Ok(Window {
        left_offset: left as u32,
        right_offset: right as u32,
        top_offset: top as u32,
        bottom_offset: bottom as u32,
    })
}

/// `ff_h2645_decode_common_vui_params()` from `h2645_vui.c`.
// Fields are assigned in bitstream order to mirror FFmpeg's sequential parser;
// a struct literal would obscure the syntax element ordering.
#[allow(clippy::field_reassign_with_default)]
pub fn parse_common_vui(br: &mut BitReader<'_>) -> Result<CommonVui> {
    let mut vui = CommonVui::default();
    vui.aspect_ratio_info_present_flag = br.read_flag()?;
    if vui.aspect_ratio_info_present_flag {
        vui.aspect_ratio_idc = br.read_bits(8)? as u8;
        if (vui.aspect_ratio_idc as usize) < PIXEL_ASPECT.len() {
            vui.sar = PIXEL_ASPECT[vui.aspect_ratio_idc as usize];
        } else if vui.aspect_ratio_idc == EXTENDED_SAR {
            let num = br.read_bits(16)? as i32;
            let den = br.read_bits(16)? as i32;
            if num != 0 && den != 0 {
                vui.sar = Rational::new(num, den);
            }
        }
    }
    vui.overscan_info_present_flag = br.read_flag()?;
    if vui.overscan_info_present_flag {
        vui.overscan_appropriate_flag = br.read_flag()?;
    }
    vui.video_signal_type_present_flag = br.read_flag()?;
    if vui.video_signal_type_present_flag {
        vui.video_format = br.read_bits(3)?;
        vui.video_full_range_flag = br.read_flag()?;
        vui.colour_description_present_flag = br.read_flag()?;
        if vui.colour_description_present_flag {
            vui.colour_primaries = br.read_bits(8)?;
            vui.transfer_characteristics = br.read_bits(8)?;
            vui.matrix_coeffs = br.read_bits(8)?;
        }
    }
    vui.chroma_loc_info_present_flag = br.read_flag()?;
    if vui.chroma_loc_info_present_flag {
        vui.chroma_sample_loc_type_top_field = br.read_ue31()?;
        vui.chroma_sample_loc_type_bottom_field = br.read_ue31()?;
        if vui.chroma_sample_loc_type_top_field <= 5 {
            vui.chroma_location = vui.chroma_sample_loc_type_top_field + 1;
        }
    }
    Ok(vui)
}

fn decode_sublayer_hrd(
    br: &mut BitReader<'_>,
    nb_cpb: usize,
    subpic_params_present: bool,
) -> Result<SubLayerHrdParams> {
    let mut par = SubLayerHrdParams {
        bit_rate_value_minus1: vec![0; nb_cpb],
        cpb_size_value_minus1: vec![0; nb_cpb],
        cpb_size_du_value_minus1: vec![0; nb_cpb],
        bit_rate_du_value_minus1: vec![0; nb_cpb],
        cbr_flag: 0,
    };
    for i in 0..nb_cpb {
        par.bit_rate_value_minus1[i] = br.read_ue()?;
        par.cpb_size_value_minus1[i] = br.read_ue()?;
        if subpic_params_present {
            par.cpb_size_du_value_minus1[i] = br.read_ue()?;
            par.bit_rate_du_value_minus1[i] = br.read_ue()?;
        }
        if br.read_flag()? {
            par.cbr_flag |= 1 << i;
        }
    }
    Ok(par)
}

/// `decode_hrd()` from `ps.c`.
pub fn decode_hrd(
    br: &mut BitReader<'_>,
    common_inf_present: bool,
    max_sublayers: usize,
) -> Result<HrdParams> {
    let mut hdr = HrdParams::default();
    if common_inf_present {
        hdr.nal_hrd_parameters_present_flag = br.read_flag()?;
        hdr.vcl_hrd_parameters_present_flag = br.read_flag()?;
        if hdr.nal_hrd_parameters_present_flag || hdr.vcl_hrd_parameters_present_flag {
            hdr.sub_pic_hrd_params_present_flag = br.read_flag()?;
            if hdr.sub_pic_hrd_params_present_flag {
                hdr.tick_divisor_minus2 = br.read_bits(8)?;
                hdr.du_cpb_removal_delay_increment_length_minus1 = br.read_bits(5)?;
                hdr.sub_pic_cpb_params_in_pic_timing_sei_flag = br.read_flag()?;
                hdr.dpb_output_delay_du_length_minus1 = br.read_bits(5)?;
            }
            hdr.bit_rate_scale = br.read_bits(4)?;
            hdr.cpb_size_scale = br.read_bits(4)?;
            if hdr.sub_pic_hrd_params_present_flag {
                hdr.cpb_size_du_scale = br.read_bits(4)?;
            }
            hdr.initial_cpb_removal_delay_length_minus1 = br.read_bits(5)?;
            hdr.au_cpb_removal_delay_length_minus1 = br.read_bits(5)?;
            hdr.dpb_output_delay_length_minus1 = br.read_bits(5)?;
        }
    }

    hdr.elemental_duration_in_tc_minus1 = vec![0; max_sublayers];
    hdr.cpb_cnt_minus1 = vec![0; max_sublayers];
    for i in 0..max_sublayers {
        let fixed_pic_rate_general_flag = br.read_flag()?;
        hdr.fixed_pic_rate_general_flag |= u32::from(fixed_pic_rate_general_flag) << i;
        let mut fixed_pic_rate_within_cvs_flag = false;
        if !fixed_pic_rate_general_flag {
            fixed_pic_rate_within_cvs_flag = br.read_flag()?;
        }
        hdr.fixed_pic_rate_within_cvs_flag |= u32::from(fixed_pic_rate_within_cvs_flag) << i;

        let mut low_delay_hrd_flag = false;
        if fixed_pic_rate_within_cvs_flag || fixed_pic_rate_general_flag {
            hdr.elemental_duration_in_tc_minus1[i] = br.read_ue()?;
        } else {
            low_delay_hrd_flag = br.read_flag()?;
        }
        hdr.low_delay_hrd_flag |= u32::from(low_delay_hrd_flag) << i;

        if !low_delay_hrd_flag {
            let cpb_cnt_minus1 = br.read_ue()?;
            if cpb_cnt_minus1 > 31 {
                return Err(Error::InvalidData);
            }
            hdr.cpb_cnt_minus1[i] = cpb_cnt_minus1;
        }

        if hdr.nal_hrd_parameters_present_flag {
            hdr.nal_params.push(decode_sublayer_hrd(
                br,
                hdr.cpb_cnt_minus1[i] as usize + 1,
                hdr.sub_pic_hrd_params_present_flag,
            )?);
        }
        if hdr.vcl_hrd_parameters_present_flag {
            hdr.vcl_params.push(decode_sublayer_hrd(
                br,
                hdr.cpb_cnt_minus1[i] as usize + 1,
                hdr.sub_pic_hrd_params_present_flag,
            )?);
        }
    }

    Ok(hdr)
}

/// `decode_vui()` from `ps.c` (spec syntax only, no malformed-stream retries).
pub fn parse_vui(
    br: &mut BitReader<'_>,
    width: u32,
    height: u32,
    chroma_format_idc: u32,
    max_sub_layers: usize,
) -> Result<Vui> {
    let mut vui = Vui {
        common: parse_common_vui(br)?,
        ..Default::default()
    };
    vui.neutra_chroma_indication_flag = br.read_flag()?;
    vui.field_seq_flag = br.read_flag()?;
    vui.frame_field_info_present_flag = br.read_flag()?;

    vui.default_display_window_flag = br.read_flag()?;
    if vui.default_display_window_flag {
        vui.def_disp_win = read_window(br, chroma_format_idc, width, height)?;
    }

    vui.vui_timing_info_present_flag = br.read_flag()?;
    if vui.vui_timing_info_present_flag {
        vui.vui_num_units_in_tick = br.read_bits(32)?;
        vui.vui_time_scale = br.read_bits(32)?;
        vui.vui_poc_proportional_to_timing_flag = br.read_flag()?;
        if vui.vui_poc_proportional_to_timing_flag {
            vui.vui_num_ticks_poc_diff_one_minus1 = br.read_ue()?;
        }
        vui.vui_hrd_parameters_present_flag = br.read_flag()?;
        if vui.vui_hrd_parameters_present_flag {
            vui.hrd = decode_hrd(br, true, max_sub_layers)?;
        }
    }

    vui.bitstream_restriction_flag = br.read_flag()?;
    if vui.bitstream_restriction_flag {
        vui.tiles_fixed_structure_flag = br.read_flag()?;
        vui.motion_vectors_over_pic_boundaries_flag = br.read_flag()?;
        vui.restricted_ref_pic_lists_flag = br.read_flag()?;
        vui.min_spatial_segmentation_idc = br.read_ue()?;
        vui.max_bytes_per_pic_denom = br.read_ue()?;
        vui.max_bits_per_min_cu_denom = br.read_ue()?;
        vui.log2_max_mv_length_horizontal = br.read_ue()?;
        vui.log2_max_mv_length_vertical = br.read_ue()?;
    }

    Ok(vui)
}
