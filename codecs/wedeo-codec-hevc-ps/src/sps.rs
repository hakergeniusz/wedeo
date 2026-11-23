//! SPS RBSP parsing (`ff_hevc_parse_sps` in FFmpeg `ps.c`).
//!
//! The single-layer syntax is implemented in full. The multi-layer SPS
//! extension (`nuh_layer_id > 0 && sps_max_sub_layers_minus1 == 7`) is reported
//! as `PatchwelcomeNotImplemented`, matching the limited scope of this crate.

use wedeo_core::{Error, PixelFormat, Result};

use crate::bitreader::BitReader;
use crate::ptl::{ProfileTierLevel, SubLayerProfileTierLevel, parse_ptl};
use crate::rps::{ShortTermRps, parse_short_term_rps};
use crate::scaling::{ScalingList, parse_scaling_list_data, scaling_list_defaults};
use crate::vui::{Vui, parse_vui};
use crate::{
    MAX_DPB_SIZE, MAX_HEIGHT, MAX_LOG2_CTB_SIZE, MAX_LONG_TERM_REF_PICS,
    MAX_SHORT_TERM_REF_PIC_SETS, MAX_SUB_LAYERS, MAX_WIDTH, MIN_LOG2_CTB_SIZE, SUB_HEIGHT_C,
    SUB_WIDTH_C, Window, read_window,
};

/// Per-temporal-layer DPB parameters (FFmpeg `ShortTermRPS`'s sibling in
/// `HEVCSPS::temporal_layer`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TemporalLayerInfo {
    pub max_dec_pic_buffering: u32,
    pub num_reorder_pics: u32,
    pub max_latency_increase: u32,
}

/// PCM configuration from the SPS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PcmConfig {
    pub bit_depth: u32,
    pub bit_depth_chroma: u32,
    pub log2_min_pcm_cb_size: u32,
    pub log2_max_pcm_cb_size: u32,
    pub pcm_loop_filter_disabled: bool,
}

/// Range extension flags from the SPS extension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RangeExtension {
    pub transform_skip_rotation_enabled: bool,
    pub transform_skip_context_enabled: bool,
    pub implicit_rdpcm_enabled: bool,
    pub explicit_rdpcm_enabled: bool,
    pub extended_precision_processing: bool,
    pub intra_smoothing_disabled: bool,
    pub high_precision_offsets_enabled: bool,
    pub persistent_rice_adaptation_enabled: bool,
    pub cabac_bypass_alignment_enabled: bool,
}

/// Screen content coding extension flags from the SPS extension.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpsSccExtension {
    pub curr_pic_ref_enabled: bool,
    pub palette_mode_enabled: bool,
    pub palette_max_size: u32,
    pub delta_palette_max_predictor_size: u32,
    pub palette_predictor_initializers_present: bool,
    pub sps_num_palette_predictor_initializers: u32,
    pub motion_vector_resolution_control_idc: u32,
    pub intra_boundary_filtering_disabled: bool,
}

/// SPS syntax (FFmpeg's `HEVCSPS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub vps_id: u8,
    pub sps_id: u32,
    pub max_sub_layers: usize,
    pub temporal_id_nesting: bool,
    pub ptl: ProfileTierLevel,
    pub sub_layer_ptl: SubLayerProfileTierLevel,

    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    pub conformance_window: bool,
    pub pic_conf_win: Window,
    pub output_window: Window,

    pub bit_depth: u32,
    pub bit_depth_chroma: u32,
    pub pixel_shift: u32,
    /// Best-effort mapping to wedeo-core's `PixelFormat`; `None` when the
    /// combination is not representable there.
    pub pix_fmt: Option<PixelFormat>,

    pub log2_max_poc_lsb: u32,
    pub sublayer_ordering_info: bool,
    pub temporal_layer: [TemporalLayerInfo; MAX_SUB_LAYERS],

    pub log2_min_cb_size: u32,
    pub log2_diff_max_min_coding_block_size: u32,
    pub log2_min_tb_size: u32,
    pub log2_diff_max_min_transform_block_size: u32,
    pub log2_max_trafo_size: u32,
    pub max_transform_hierarchy_depth_inter: u32,
    pub max_transform_hierarchy_depth_intra: u32,

    pub scaling_list_enabled: bool,
    pub scaling_list: ScalingList,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm_enabled: bool,
    pub pcm: PcmConfig,

    pub nb_st_rps: usize,
    pub st_rps: Vec<ShortTermRps>,
    pub long_term_ref_pics_present: bool,
    pub num_long_term_ref_pics_sps: usize,
    pub lt_ref_pic_poc_lsb_sps: Vec<u32>,
    pub used_by_curr_pic_lt: u32,

    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing_enabled: bool,
    pub vui_present: bool,
    pub vui: Vui,

    pub extension_present: bool,
    pub range_extension: bool,
    pub multilayer_extension: bool,
    pub sps_3d_extension: bool,
    pub scc_extension: bool,
    pub range_ext: RangeExtension,
    pub scc: SpsSccExtension,

    // Inferred parameters (FFmpeg's derived SPS fields).
    pub log2_ctb_size: u32,
    pub log2_min_pu_size: u32,
    pub ctb_width: u32,
    pub ctb_height: u32,
    pub ctb_size: u32,
    pub min_cb_width: u32,
    pub min_cb_height: u32,
    pub min_tb_width: u32,
    pub min_tb_height: u32,
    pub min_pu_width: u32,
    pub min_pu_height: u32,
    pub tb_mask: u32,
    pub qp_bd_offset: u32,
    pub hshift: [u32; 3],
    pub vshift: [u32; 3],
}

impl Default for Sps {
    fn default() -> Self {
        Self {
            vps_id: 0,
            sps_id: 0,
            max_sub_layers: 1,
            temporal_id_nesting: false,
            ptl: ProfileTierLevel::default(),
            sub_layer_ptl: SubLayerProfileTierLevel::default(),
            chroma_format_idc: 1,
            separate_colour_plane: false,
            width: 0,
            height: 0,
            conformance_window: false,
            pic_conf_win: Window::default(),
            output_window: Window::default(),
            bit_depth: 8,
            bit_depth_chroma: 8,
            pixel_shift: 0,
            pix_fmt: Some(PixelFormat::Yuv420p),
            log2_max_poc_lsb: 4,
            sublayer_ordering_info: false,
            temporal_layer: [TemporalLayerInfo::default(); MAX_SUB_LAYERS],
            log2_min_cb_size: 3,
            log2_diff_max_min_coding_block_size: 0,
            log2_min_tb_size: 2,
            log2_diff_max_min_transform_block_size: 0,
            log2_max_trafo_size: 2,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            scaling_list_enabled: false,
            scaling_list: scaling_list_defaults(),
            amp_enabled: false,
            sao_enabled: false,
            pcm_enabled: false,
            pcm: PcmConfig::default(),
            nb_st_rps: 0,
            st_rps: Vec::new(),
            long_term_ref_pics_present: false,
            num_long_term_ref_pics_sps: 0,
            lt_ref_pic_poc_lsb_sps: Vec::new(),
            used_by_curr_pic_lt: 0,
            temporal_mvp_enabled: false,
            strong_intra_smoothing_enabled: false,
            vui_present: false,
            vui: Vui::default(),
            extension_present: false,
            range_extension: false,
            multilayer_extension: false,
            sps_3d_extension: false,
            scc_extension: false,
            range_ext: RangeExtension::default(),
            scc: SpsSccExtension::default(),
            log2_ctb_size: 3,
            log2_min_pu_size: 2,
            ctb_width: 0,
            ctb_height: 0,
            ctb_size: 0,
            min_cb_width: 0,
            min_cb_height: 0,
            min_tb_width: 0,
            min_tb_height: 0,
            min_pu_width: 0,
            min_pu_height: 0,
            tb_mask: 0,
            qp_bd_offset: 0,
            hshift: [0, 1, 1],
            vshift: [0, 1, 1],
        }
    }
}

impl Sps {
    /// Number of luma samples in a CTB: `1 << log2_ctb_size`.
    pub fn ctb_size_samples(&self) -> u32 {
        1 << self.log2_ctb_size
    }

    /// Chroma sub-sampling width factor (FFmpeg `sub_width_c`).
    pub fn sub_width_c(&self) -> u32 {
        SUB_WIDTH_C[(self.chroma_format_idc as usize).min(3)]
    }

    /// Chroma sub-sampling height factor (FFmpeg `sub_height_c`).
    pub fn sub_height_c(&self) -> u32 {
        SUB_HEIGHT_C[(self.chroma_format_idc as usize).min(3)]
    }

    /// Cropped display width (conformance window).
    pub fn cropped_width(&self) -> u32 {
        self.width - self.output_window.left_offset - self.output_window.right_offset
    }

    /// Cropped display height (conformance window).
    pub fn cropped_height(&self) -> u32 {
        self.height - self.output_window.top_offset - self.output_window.bottom_offset
    }
}

/// `map_pixel_format()` from `ps.c`, restricted to the pixel formats present
/// in wedeo-core. `chroma_format_idc == 0` is monochrome.
fn map_pixel_format(chroma_format_idc: u32, bit_depth: u32) -> Option<PixelFormat> {
    match (bit_depth, chroma_format_idc) {
        (8, 0) => Some(PixelFormat::Gray8),
        (8, 1) => Some(PixelFormat::Yuv420p),
        (8, 2) => Some(PixelFormat::Yuv422p),
        (8, 3) => Some(PixelFormat::Yuv444p),
        (10, 1) => Some(PixelFormat::Yuv420p10le),
        // Core does not yet expose 9/12-bit or 422/444 10-bit formats.
        _ => None,
    }
}

/// Parse an SPS RBSP.
///
/// `nuh_layer_id` is taken from the NAL header; a non-zero value combined
/// with `sps_max_sub_layers_minus1 == 7` signals the multi-layer extension,
/// which is not implemented here.
// Fields are assigned in bitstream order to mirror FFmpeg's sequential parser;
// a struct literal would obscure the syntax element ordering.
#[allow(clippy::field_reassign_with_default)]
pub fn parse_sps(rbsp: &[u8], nuh_layer_id: u8) -> Result<Sps> {
    let mut br = BitReader::new(rbsp);
    let mut sps = Sps::default();

    sps.vps_id = br.read_bits(4)? as u8;
    sps.max_sub_layers = br.read_bits(3)? as usize + 1;
    let multi_layer_ext = nuh_layer_id > 0 && sps.max_sub_layers == MAX_SUB_LAYERS + 1;
    if multi_layer_ext {
        return Err(Error::PatchwelcomeNotImplemented);
    }
    if sps.max_sub_layers > MAX_SUB_LAYERS {
        return Err(Error::InvalidData);
    }

    sps.temporal_id_nesting = br.read_flag()?;
    let (ptl, sub) = parse_ptl(&mut br, true, sps.max_sub_layers)?;
    sps.ptl = ptl;
    sps.sub_layer_ptl = sub;

    sps.sps_id = br.read_ue()?;
    if sps.sps_id as usize >= 16 {
        return Err(Error::InvalidData);
    }

    sps.chroma_format_idc = br.read_ue()?;
    if sps.chroma_format_idc > 3 {
        return Err(Error::InvalidData);
    }
    if sps.chroma_format_idc == 3 {
        sps.separate_colour_plane = br.read_flag()?;
    }
    if sps.separate_colour_plane {
        sps.chroma_format_idc = 0;
    }

    sps.width = br.read_ue()?;
    sps.height = br.read_ue()?;
    if sps.width == 0 || sps.height == 0 || sps.width > MAX_WIDTH || sps.height > MAX_HEIGHT {
        return Err(Error::InvalidData);
    }

    sps.conformance_window = br.read_flag()?;
    if sps.conformance_window {
        sps.pic_conf_win = read_window(&mut br, sps.chroma_format_idc, sps.width, sps.height)?;
    }

    sps.bit_depth = br.read_ue31()? + 8;
    let bit_depth_chroma = br.read_ue31()? + 8;
    if sps.bit_depth > 16 || bit_depth_chroma > 16 {
        return Err(Error::InvalidData);
    }
    if sps.chroma_format_idc != 0 && bit_depth_chroma != sps.bit_depth {
        return Err(Error::InvalidData);
    }
    sps.bit_depth_chroma = bit_depth_chroma;
    sps.output_window = sps.pic_conf_win;
    sps.pix_fmt = map_pixel_format(sps.chroma_format_idc, sps.bit_depth);

    sps.log2_max_poc_lsb = br.read_ue()? + 4;
    if sps.log2_max_poc_lsb > 16 {
        return Err(Error::InvalidData);
    }

    sps.sublayer_ordering_info = br.read_flag()?;
    let start = if sps.sublayer_ordering_info {
        0
    } else {
        sps.max_sub_layers - 1
    };
    for i in start..sps.max_sub_layers {
        sps.temporal_layer[i].max_dec_pic_buffering = br.read_ue()? + 1;
        sps.temporal_layer[i].num_reorder_pics = br.read_ue()?;
        sps.temporal_layer[i].max_latency_increase = br.read_ue()?.wrapping_sub(1);
        if sps.temporal_layer[i].max_dec_pic_buffering as usize > MAX_DPB_SIZE {
            return Err(Error::InvalidData);
        }
        if sps.temporal_layer[i].num_reorder_pics > sps.temporal_layer[i].max_dec_pic_buffering - 1
        {
            return Err(Error::InvalidData);
        }
    }
    if !sps.sublayer_ordering_info {
        for i in 0..start {
            sps.temporal_layer[i] = sps.temporal_layer[start];
        }
    }

    sps.log2_min_cb_size = br.read_ue()? + 3;
    sps.log2_diff_max_min_coding_block_size = br.read_ue()?;
    sps.log2_min_tb_size = br.read_ue()? + 2;
    sps.log2_diff_max_min_transform_block_size = br.read_ue()?;
    sps.log2_max_trafo_size = sps.log2_diff_max_min_transform_block_size + sps.log2_min_tb_size;

    // FFmpeg: `log2_min_cb_size < 3 || log2_min_cb_size > 30` (the
    // HEVC_MIN_LOG2_CTB_SIZE bound applies to log2_ctb_size, not this field).
    if !(3..=30).contains(&sps.log2_min_cb_size)
        || sps.log2_diff_max_min_coding_block_size > 30
        || sps.log2_min_tb_size >= sps.log2_min_cb_size
        || sps.log2_min_tb_size < 2
        || sps.log2_diff_max_min_transform_block_size > 30
    {
        return Err(Error::InvalidData);
    }

    sps.max_transform_hierarchy_depth_inter = br.read_ue()?;
    sps.max_transform_hierarchy_depth_intra = br.read_ue()?;

    sps.scaling_list_enabled = br.read_flag()?;
    if sps.scaling_list_enabled {
        sps.scaling_list = scaling_list_defaults();
        if br.read_flag()? {
            parse_scaling_list_data(&mut br, &mut sps.scaling_list, sps.chroma_format_idc)?;
        }
    }

    sps.amp_enabled = br.read_flag()?;
    sps.sao_enabled = br.read_flag()?;
    sps.pcm_enabled = br.read_flag()?;
    if sps.pcm_enabled {
        sps.pcm.bit_depth = br.read_bits(4)? + 1;
        sps.pcm.bit_depth_chroma = br.read_bits(4)? + 1;
        sps.pcm.log2_min_pcm_cb_size = br.read_ue()? + 3;
        sps.pcm.log2_max_pcm_cb_size = sps.pcm.log2_min_pcm_cb_size + br.read_ue()?;
        if sps.pcm.bit_depth.max(sps.pcm.bit_depth_chroma) > sps.bit_depth {
            return Err(Error::InvalidData);
        }
        sps.pcm.pcm_loop_filter_disabled = br.read_flag()?;
    }

    sps.nb_st_rps = br.read_ue()? as usize;
    if sps.nb_st_rps > MAX_SHORT_TERM_REF_PIC_SETS {
        return Err(Error::InvalidData);
    }
    for i in 0..sps.nb_st_rps {
        let rps = parse_short_term_rps(&mut br, &sps.st_rps, Some(i), false)?;
        sps.st_rps.push(rps);
    }

    sps.long_term_ref_pics_present = br.read_flag()?;
    if sps.long_term_ref_pics_present {
        sps.num_long_term_ref_pics_sps = br.read_ue()? as usize;
        if sps.num_long_term_ref_pics_sps > MAX_LONG_TERM_REF_PICS {
            return Err(Error::InvalidData);
        }
        for i in 0..sps.num_long_term_ref_pics_sps {
            sps.lt_ref_pic_poc_lsb_sps
                .push(br.read_bits(sps.log2_max_poc_lsb as usize)?);
            if br.read_flag()? {
                sps.used_by_curr_pic_lt |= 1 << i;
            }
        }
    }

    sps.temporal_mvp_enabled = br.read_flag()?;
    sps.strong_intra_smoothing_enabled = br.read_flag()?;
    sps.vui_present = br.read_flag()?;
    if sps.vui_present {
        sps.vui = parse_vui(
            &mut br,
            sps.width,
            sps.height,
            sps.chroma_format_idc,
            sps.max_sub_layers,
        )?;
    }

    sps.extension_present = br.read_flag()?;
    if sps.extension_present {
        sps.range_extension = br.read_flag()?;
        sps.multilayer_extension = br.read_flag()?;
        sps.sps_3d_extension = br.read_flag()?;
        sps.scc_extension = br.read_flag()?;
        br.skip_bits(4); // sps_extension_4bits

        if sps.range_extension {
            let re = &mut sps.range_ext;
            re.transform_skip_rotation_enabled = br.read_flag()?;
            re.transform_skip_context_enabled = br.read_flag()?;
            re.implicit_rdpcm_enabled = br.read_flag()?;
            re.explicit_rdpcm_enabled = br.read_flag()?;
            re.extended_precision_processing = br.read_flag()?;
            re.intra_smoothing_disabled = br.read_flag()?;
            re.high_precision_offsets_enabled = br.read_flag()?;
            re.persistent_rice_adaptation_enabled = br.read_flag()?;
            re.cabac_bypass_alignment_enabled = br.read_flag()?;
        }
        if sps.multilayer_extension {
            br.skip_bits(1); // inter_view_mv_vert_constraint_flag
        }
        if sps.sps_3d_extension {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        if sps.scc_extension {
            let scc = &mut sps.scc;
            scc.curr_pic_ref_enabled = br.read_flag()?;
            scc.palette_mode_enabled = br.read_flag()?;
            if scc.palette_mode_enabled {
                scc.palette_max_size = br.read_ue()?;
                scc.delta_palette_max_predictor_size = br.read_ue()?;
                scc.palette_predictor_initializers_present = br.read_flag()?;
                if scc.palette_predictor_initializers_present {
                    scc.sps_num_palette_predictor_initializers = br.read_ue()? + 1;
                    if scc.sps_num_palette_predictor_initializers as usize > 128 {
                        return Err(Error::InvalidData);
                    }
                    let num_comps = if sps.chroma_format_idc == 0 { 1 } else { 3 };
                    for comp in 0..num_comps {
                        let depth = if comp == 0 {
                            sps.bit_depth
                        } else {
                            sps.bit_depth_chroma
                        };
                        for _ in 0..scc.sps_num_palette_predictor_initializers {
                            let _ = br.read_bits(depth as usize)?;
                        }
                    }
                }
            }
            scc.motion_vector_resolution_control_idc = br.read_bits(2)?;
            scc.intra_boundary_filtering_disabled = br.read_flag()?;
        }
    }

    // Inferred parameters (7.4.3.2.1).
    sps.log2_ctb_size = sps.log2_min_cb_size + sps.log2_diff_max_min_coding_block_size;
    sps.log2_min_pu_size = sps.log2_min_cb_size - 1;
    if sps.log2_ctb_size > MAX_LOG2_CTB_SIZE || sps.log2_ctb_size < MIN_LOG2_CTB_SIZE {
        return Err(Error::InvalidData);
    }
    let ctb_size = 1u32 << sps.log2_ctb_size;
    sps.ctb_width = (sps.width + ctb_size - 1) >> sps.log2_ctb_size;
    sps.ctb_height = (sps.height + ctb_size - 1) >> sps.log2_ctb_size;
    sps.ctb_size = sps.ctb_width * sps.ctb_height;
    sps.min_cb_width = sps.width >> sps.log2_min_cb_size;
    sps.min_cb_height = sps.height >> sps.log2_min_cb_size;
    sps.min_tb_width = sps.width >> sps.log2_min_tb_size;
    sps.min_tb_height = sps.height >> sps.log2_min_tb_size;
    sps.min_pu_width = sps.width >> sps.log2_min_pu_size;
    sps.min_pu_height = sps.height >> sps.log2_min_pu_size;
    sps.tb_mask = (1u32 << (sps.log2_ctb_size - sps.log2_min_tb_size)) - 1;
    sps.qp_bd_offset = 6 * (sps.bit_depth - 8);
    sps.pixel_shift = u32::from(sps.bit_depth > 8);

    // Chroma shift (FFmpeg's map_pixel_format → desc->log2_chroma_w/h).
    let (cw, ch) = match sps.chroma_format_idc {
        1 => (1, 1), // 4:2:0
        2 => (1, 0), // 4:2:2
        3 => (0, 0), // 4:4:4
        _ => (0, 0), // monochrome
    };
    sps.hshift = [0, cw, cw];
    sps.vshift = [0, ch, ch];

    if sps.width % (1 << sps.log2_min_cb_size) != 0 || sps.height % (1 << sps.log2_min_cb_size) != 0
    {
        return Err(Error::InvalidData);
    }
    let max_depth = sps.log2_ctb_size - sps.log2_min_tb_size;
    if sps.max_transform_hierarchy_depth_inter > max_depth
        || sps.max_transform_hierarchy_depth_intra > max_depth
        || sps.log2_max_trafo_size > sps.log2_ctb_size.min(5)
    {
        return Err(Error::InvalidData);
    }

    if sps.output_window.left_offset >= sps.width - sps.output_window.right_offset
        || sps.output_window.top_offset >= sps.height - sps.output_window.bottom_offset
    {
        sps.output_window = Window::default();
        sps.pic_conf_win = Window::default();
    }

    Ok(sps)
}
