//! PPS RBSP parsing (`ff_hevc_decode_nal_pps` and `setup_pps` in FFmpeg
//! `ps.c`), including the derived tile / CTB address tables.

use wedeo_core::{Error, Result};

use crate::bitreader::BitReader;
use crate::scaling::{ScalingList, parse_scaling_list_data, scaling_list_defaults};
use crate::sps::Sps;
use crate::{MAX_PPS_COUNT, MAX_REFS, MAX_SPS_COUNT, PROFILE_HEVC_REXT};

/// Tile geometry derived in `setup_pps()` (FFmpeg's `col_bd`, `row_bd`,
/// `ctb_addr_rs_to_ts`, `ctb_addr_ts_to_rs`, `tile_id`, `tile_pos_rs`,
/// `min_tb_addr_zs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileInfo {
    pub col_bd: Vec<i32>,
    pub row_bd: Vec<i32>,
    pub col_idx_x: Vec<i32>,
    pub ctb_addr_rs_to_ts: Vec<i32>,
    pub ctb_addr_ts_to_rs: Vec<i32>,
    pub tile_id: Vec<i32>,
    pub tile_pos_rs: Vec<i32>,
    /// `min_tb_addr_zs` points into a table of size `(tb_mask+2)^2`; here it
    /// is stored as a flat owned vector including the -1 border.
    pub min_tb_addr_zs_tab: Vec<i32>,
    /// Index into `min_tb_addr_zs_tab` of `min_tb_addr_zs[0][0]`.
    pub min_tb_addr_zs_offset: usize,
}

/// PPS syntax (FFmpeg's `HEVCPPS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u32,
    pub sps_id: u32,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u32,
    pub sign_data_hiding_flag: bool,
    pub cabac_init_present_flag: bool,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub pic_init_qp_minus26: i32,
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub pic_slice_level_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enable_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,

    pub num_tile_columns: usize,
    pub num_tile_rows: usize,
    pub uniform_spacing_flag: bool,
    pub column_width: Vec<u32>,
    pub row_height: Vec<u32>,
    pub loop_filter_across_tiles_enabled_flag: bool,
    pub seq_loop_filter_across_slices_enabled_flag: bool,

    pub deblocking_filter_control_present_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub disable_dbf: bool,
    pub beta_offset: i32,
    pub tc_offset: i32,

    pub scaling_list_data_present_flag: bool,
    pub scaling_list: ScalingList,
    pub lists_modification_present_flag: bool,
    pub log2_parallel_merge_level: u32,
    pub slice_header_extension_present_flag: bool,
    pub pps_extension_present_flag: bool,
    pub pps_range_extensions_flag: bool,
    pub pps_multilayer_extension_flag: bool,
    pub pps_3d_extension_flag: bool,
    pub pps_scc_extension_flag: bool,

    // PPS range extensions.
    pub log2_max_transform_skip_block_size: u32,
    pub cross_component_prediction_enabled_flag: bool,
    pub chroma_qp_offset_list_enabled_flag: bool,
    pub diff_cu_chroma_qp_offset_depth: u32,
    pub chroma_qp_offset_list_len_minus1: u32,
    pub cb_qp_offset_list: Vec<i32>,
    pub cr_qp_offset_list: Vec<i32>,
    pub log2_sao_offset_scale_luma: u32,
    pub log2_sao_offset_scale_chroma: u32,

    // PPS SCC extension.
    pub pps_curr_pic_ref_enabled_flag: bool,
    pub residual_adaptive_colour_transform_enabled_flag: bool,
    pub pps_slice_act_qp_offsets_present_flag: bool,
    pub pps_act_y_qp_offset: i32,
    pub pps_act_cb_qp_offset: i32,
    pub pps_act_cr_qp_offset: i32,
    pub pps_palette_predictor_initializers_present_flag: bool,
    pub pps_num_palette_predictor_initializers: u32,

    /// Tables derived by `setup_pps()`.
    pub tile: TileInfo,
}

impl Default for Pps {
    fn default() -> Self {
        Self {
            pps_id: 0,
            sps_id: 0,
            dependent_slice_segments_enabled_flag: false,
            output_flag_present_flag: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding_flag: false,
            cabac_init_present_flag: false,
            num_ref_idx_l0_default_active: 1,
            num_ref_idx_l1_default_active: 1,
            pic_init_qp_minus26: 0,
            constrained_intra_pred_flag: false,
            transform_skip_enabled_flag: false,
            cu_qp_delta_enabled_flag: false,
            diff_cu_qp_delta_depth: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            pic_slice_level_chroma_qp_offsets_present_flag: false,
            weighted_pred_flag: false,
            weighted_bipred_flag: false,
            transquant_bypass_enable_flag: false,
            tiles_enabled_flag: false,
            entropy_coding_sync_enabled_flag: false,
            num_tile_columns: 1,
            num_tile_rows: 1,
            uniform_spacing_flag: true,
            column_width: Vec::new(),
            row_height: Vec::new(),
            loop_filter_across_tiles_enabled_flag: true,
            seq_loop_filter_across_slices_enabled_flag: false,
            deblocking_filter_control_present_flag: false,
            deblocking_filter_override_enabled_flag: false,
            disable_dbf: false,
            beta_offset: 0,
            tc_offset: 0,
            scaling_list_data_present_flag: false,
            scaling_list: scaling_list_defaults(),
            lists_modification_present_flag: false,
            log2_parallel_merge_level: 2,
            slice_header_extension_present_flag: false,
            pps_extension_present_flag: false,
            pps_range_extensions_flag: false,
            pps_multilayer_extension_flag: false,
            pps_3d_extension_flag: false,
            pps_scc_extension_flag: false,
            log2_max_transform_skip_block_size: 2,
            cross_component_prediction_enabled_flag: false,
            chroma_qp_offset_list_enabled_flag: false,
            diff_cu_chroma_qp_offset_depth: 0,
            chroma_qp_offset_list_len_minus1: 0,
            cb_qp_offset_list: Vec::new(),
            cr_qp_offset_list: Vec::new(),
            log2_sao_offset_scale_luma: 0,
            log2_sao_offset_scale_chroma: 0,
            pps_curr_pic_ref_enabled_flag: false,
            residual_adaptive_colour_transform_enabled_flag: false,
            pps_slice_act_qp_offsets_present_flag: false,
            pps_act_y_qp_offset: 0,
            pps_act_cb_qp_offset: 0,
            pps_act_cr_qp_offset: 0,
            pps_palette_predictor_initializers_present_flag: false,
            pps_num_palette_predictor_initializers: 0,
            tile: TileInfo::default(),
        }
    }
}

/// PPS range extensions (FFmpeg `pps_range_extensions`).
fn pps_range_extensions(br: &mut BitReader<'_>, pps: &mut Pps, sps: &Sps) -> Result<()> {
    if pps.transform_skip_enabled_flag {
        pps.log2_max_transform_skip_block_size = br.read_ue31()? + 2;
    }
    pps.cross_component_prediction_enabled_flag = br.read_flag()?;
    pps.chroma_qp_offset_list_enabled_flag = br.read_flag()?;
    if pps.chroma_qp_offset_list_enabled_flag {
        pps.diff_cu_chroma_qp_offset_depth = br.read_ue31()?;
        pps.chroma_qp_offset_list_len_minus1 = br.read_ue31()?;
        if pps.chroma_qp_offset_list_len_minus1 > 5 {
            return Err(Error::InvalidData);
        }
        for _ in 0..=pps.chroma_qp_offset_list_len_minus1 {
            let cb = br.read_se()?;
            let cr = br.read_se()?;
            pps.cb_qp_offset_list.push(cb);
            pps.cr_qp_offset_list.push(cr);
        }
    }
    pps.log2_sao_offset_scale_luma = br.read_ue31()?;
    pps.log2_sao_offset_scale_chroma = br.read_ue31()?;
    if pps.log2_sao_offset_scale_luma > sps.bit_depth.saturating_sub(10)
        || pps.log2_sao_offset_scale_chroma > sps.bit_depth_chroma.saturating_sub(10)
    {
        return Err(Error::InvalidData);
    }
    Ok(())
}

/// PPS screen-content-coding extension (FFmpeg `pps_scc_extension`), limited
/// to the scalar fields needed by slice headers (palette initializers are
/// consumed but not stored).
fn pps_scc_extension(br: &mut BitReader<'_>, pps: &mut Pps, sps: &Sps) -> Result<()> {
    pps.pps_curr_pic_ref_enabled_flag = br.read_flag()?;
    pps.residual_adaptive_colour_transform_enabled_flag = br.read_flag()?;
    if pps.residual_adaptive_colour_transform_enabled_flag {
        pps.pps_slice_act_qp_offsets_present_flag = br.read_flag()?;
        pps.pps_act_y_qp_offset = br.read_se()?.wrapping_sub(5);
        pps.pps_act_cb_qp_offset = br.read_se()?.wrapping_sub(5);
        pps.pps_act_cr_qp_offset = br.read_se()?.wrapping_sub(3);
        for off in [
            pps.pps_act_y_qp_offset,
            pps.pps_act_cb_qp_offset,
            pps.pps_act_cr_qp_offset,
        ] {
            if !(-12..12).contains(&off) {
                return Err(Error::InvalidData);
            }
        }
    }
    pps.pps_palette_predictor_initializers_present_flag = br.read_flag()?;
    if pps.pps_palette_predictor_initializers_present_flag {
        pps.pps_num_palette_predictor_initializers = br.read_ue()?;
        if pps.pps_num_palette_predictor_initializers != 0 {
            if pps.pps_num_palette_predictor_initializers as usize > 128 {
                return Err(Error::InvalidData);
            }
            let monochrome = br.read_flag()?;
            let luma_depth = br.read_ue31()? + 8;
            if luma_depth != sps.bit_depth {
                return Err(Error::InvalidData);
            }
            let chroma_depth = if monochrome {
                luma_depth
            } else {
                let d = br.read_ue31()? + 8;
                if d != sps.bit_depth_chroma {
                    return Err(Error::InvalidData);
                }
                d
            };
            let num_comps = if monochrome { 1 } else { 3 };
            for comp in 0..num_comps {
                let depth = if comp == 0 { luma_depth } else { chroma_depth };
                for _ in 0..pps.pps_num_palette_predictor_initializers {
                    let _ = br.read_bits(depth as usize)?;
                }
            }
        }
    }
    Ok(())
}

/// `setup_pps()` from `ps.c`: derive tile boundaries and CTB address maps.
fn setup_pps(pps: &mut Pps, sps: &Sps) -> Result<()> {
    let ctb_width = sps.ctb_width as usize;
    let ctb_height = sps.ctb_height as usize;
    let cols = pps.num_tile_columns;
    let rows = pps.num_tile_rows;
    let mut tile = TileInfo::default();

    if pps.uniform_spacing_flag {
        pps.column_width = (0..cols)
            .map(|i| ((i + 1) * ctb_width / cols - i * ctb_width / cols) as u32)
            .collect();
        pps.row_height = (0..rows)
            .map(|i| ((i + 1) * ctb_height / rows - i * ctb_height / rows) as u32)
            .collect();
    }

    tile.col_bd = vec![0; cols + 1];
    for i in 0..cols {
        tile.col_bd[i + 1] = tile.col_bd[i] + pps.column_width[i] as i32;
    }
    tile.row_bd = vec![0; rows + 1];
    for i in 0..rows {
        tile.row_bd[i + 1] = tile.row_bd[i] + pps.row_height[i] as i32;
    }

    tile.col_idx_x = vec![0; ctb_width];
    let mut j = 0usize;
    for i in 0..ctb_width {
        if i > tile.col_bd[j] as usize {
            j += 1;
        }
        tile.col_idx_x[i] = j as i32;
    }

    let pic_area_in_ctbs = ctb_width * ctb_height;
    tile.ctb_addr_rs_to_ts = vec![0; pic_area_in_ctbs];
    tile.ctb_addr_ts_to_rs = vec![0; pic_area_in_ctbs];
    tile.tile_id = vec![0; pic_area_in_ctbs];

    for ctb_addr_rs in 0..pic_area_in_ctbs {
        let tb_x = ctb_addr_rs % ctb_width;
        let tb_y = ctb_addr_rs / ctb_width;
        let mut tile_x = 0usize;
        let mut tile_y = 0usize;
        for i in 0..cols {
            if tb_x < tile.col_bd[i + 1] as usize {
                tile_x = i;
                break;
            }
        }
        for i in 0..rows {
            if tb_y < tile.row_bd[i + 1] as usize {
                tile_y = i;
                break;
            }
        }
        let mut val = 0usize;
        for i in 0..tile_x {
            val += pps.row_height[tile_y] as usize * pps.column_width[i] as usize;
        }
        for i in 0..tile_y {
            val += ctb_width * pps.row_height[i] as usize;
        }
        val += (tb_y - tile.row_bd[tile_y] as usize) * pps.column_width[tile_x] as usize + tb_x
            - tile.col_bd[tile_x] as usize;

        tile.ctb_addr_rs_to_ts[ctb_addr_rs] = val as i32;
        tile.ctb_addr_ts_to_rs[val] = ctb_addr_rs as i32;
    }

    let mut tile_id = 0i32;
    for j in 0..rows {
        for i in 0..cols {
            for y in tile.row_bd[j]..tile.row_bd[j + 1] {
                for x in tile.col_bd[i]..tile.col_bd[i + 1] {
                    let rs = y as usize * ctb_width + x as usize;
                    let ts = tile.ctb_addr_rs_to_ts[rs] as usize;
                    tile.tile_id[ts] = tile_id;
                }
            }
            tile_id += 1;
        }
    }

    tile.tile_pos_rs = vec![0; rows * cols];
    for j in 0..rows {
        for i in 0..cols {
            tile.tile_pos_rs[j * cols + i] = tile.row_bd[j] * ctb_width as i32 + tile.col_bd[i];
        }
    }

    let log2_diff = sps.log2_ctb_size - sps.log2_min_tb_size;
    let side = sps.tb_mask as usize + 2;
    let mut tab = vec![0i32; side * side];
    for y in 0..side {
        tab[y * side] = -1;
        tab[y] = -1;
    }
    for y in 0..sps.tb_mask as usize + 1 {
        for x in 0..sps.tb_mask as usize + 1 {
            let tb_x = x >> log2_diff;
            let tb_y = y >> log2_diff;
            let rs = sps.ctb_width as usize * tb_y + tb_x;
            let mut val = tile.ctb_addr_rs_to_ts[rs] << (log2_diff * 2);
            for i in 0..log2_diff {
                let m = 1u32 << i;
                if m & x as u32 != 0 {
                    val += (m * m) as i32;
                }
                if m & y as u32 != 0 {
                    val += (2 * m * m) as i32;
                }
            }
            tab[y * side + x] = val;
        }
    }
    tile.min_tb_addr_zs_offset = side + 1;
    tile.min_tb_addr_zs_tab = tab;

    pps.tile = tile;
    Ok(())
}

/// Parse a PPS RBSP. The referenced `sps` provides the derived CTB geometry
/// used for validation and `setup_pps()`.
// Fields are assigned in bitstream order to mirror FFmpeg's sequential parser;
// a struct literal would obscure the syntax element ordering.
#[allow(clippy::field_reassign_with_default)]
pub fn parse_pps(rbsp: &[u8], sps: &Sps) -> Result<Pps> {
    let mut br = BitReader::new(rbsp);
    let mut pps = Pps::default();

    pps.pps_id = br.read_ue()?;
    if pps.pps_id as usize >= MAX_PPS_COUNT {
        return Err(Error::InvalidData);
    }
    pps.sps_id = br.read_ue()?;
    if pps.sps_id as usize >= MAX_SPS_COUNT || pps.sps_id != sps.sps_id {
        return Err(Error::InvalidData);
    }

    pps.dependent_slice_segments_enabled_flag = br.read_flag()?;
    pps.output_flag_present_flag = br.read_flag()?;
    pps.num_extra_slice_header_bits = br.read_bits(3)?;
    pps.sign_data_hiding_flag = br.read_flag()?;
    pps.cabac_init_present_flag = br.read_flag()?;

    pps.num_ref_idx_l0_default_active = br.read_ue31()? + 1;
    pps.num_ref_idx_l1_default_active = br.read_ue31()? + 1;
    if pps.num_ref_idx_l0_default_active as usize >= MAX_REFS
        || pps.num_ref_idx_l1_default_active as usize >= MAX_REFS
    {
        return Err(Error::InvalidData);
    }

    pps.pic_init_qp_minus26 = br.read_se()?;
    pps.constrained_intra_pred_flag = br.read_flag()?;
    pps.transform_skip_enabled_flag = br.read_flag()?;
    pps.cu_qp_delta_enabled_flag = br.read_flag()?;
    if pps.cu_qp_delta_enabled_flag {
        pps.diff_cu_qp_delta_depth = br.read_ue()?;
    }
    if pps.diff_cu_qp_delta_depth > sps.log2_diff_max_min_coding_block_size {
        return Err(Error::InvalidData);
    }

    pps.cb_qp_offset = br.read_se()?;
    pps.cr_qp_offset = br.read_se()?;
    if !(-12..=12).contains(&pps.cb_qp_offset) || !(-12..=12).contains(&pps.cr_qp_offset) {
        return Err(Error::InvalidData);
    }
    pps.pic_slice_level_chroma_qp_offsets_present_flag = br.read_flag()?;
    pps.weighted_pred_flag = br.read_flag()?;
    pps.weighted_bipred_flag = br.read_flag()?;
    pps.transquant_bypass_enable_flag = br.read_flag()?;
    pps.tiles_enabled_flag = br.read_flag()?;
    pps.entropy_coding_sync_enabled_flag = br.read_flag()?;

    if pps.tiles_enabled_flag {
        let num_tile_columns_minus1 = br.read_ue()?;
        let num_tile_rows_minus1 = br.read_ue()?;
        if num_tile_columns_minus1 >= sps.ctb_width
            || num_tile_rows_minus1 >= sps.ctb_height
            || num_tile_columns_minus1 as usize >= 20
            || num_tile_rows_minus1 as usize >= 22
        {
            return Err(Error::InvalidData);
        }
        pps.num_tile_columns = num_tile_columns_minus1 as usize + 1;
        pps.num_tile_rows = num_tile_rows_minus1 as usize + 1;
        pps.uniform_spacing_flag = br.read_flag()?;
        if !pps.uniform_spacing_flag {
            let mut sum = 0u64;
            pps.column_width = vec![0; pps.num_tile_columns];
            for w in pps.column_width.iter_mut().take(pps.num_tile_columns - 1) {
                *w = br.read_ue()? + 1;
                sum += *w as u64;
            }
            if sum >= sps.ctb_width as u64 {
                return Err(Error::InvalidData);
            }
            *pps.column_width.last_mut().unwrap() = sps.ctb_width - sum as u32;

            sum = 0;
            pps.row_height = vec![0; pps.num_tile_rows];
            for h in pps.row_height.iter_mut().take(pps.num_tile_rows - 1) {
                *h = br.read_ue()? + 1;
                sum += *h as u64;
            }
            if sum >= sps.ctb_height as u64 {
                return Err(Error::InvalidData);
            }
            *pps.row_height.last_mut().unwrap() = sps.ctb_height - sum as u32;
        }
        pps.loop_filter_across_tiles_enabled_flag = br.read_flag()?;
    }

    pps.seq_loop_filter_across_slices_enabled_flag = br.read_flag()?;
    pps.deblocking_filter_control_present_flag = br.read_flag()?;
    if pps.deblocking_filter_control_present_flag {
        pps.deblocking_filter_override_enabled_flag = br.read_flag()?;
        pps.disable_dbf = br.read_flag()?;
        if !pps.disable_dbf {
            let beta = br.read_se()?;
            let tc = br.read_se()?;
            if !(-6..=6).contains(&beta) || !(-6..=6).contains(&tc) {
                return Err(Error::InvalidData);
            }
            pps.beta_offset = 2 * beta;
            pps.tc_offset = 2 * tc;
        }
    }

    pps.scaling_list_data_present_flag = br.read_flag()?;
    if pps.scaling_list_data_present_flag {
        pps.scaling_list = scaling_list_defaults();
        parse_scaling_list_data(&mut br, &mut pps.scaling_list, sps.chroma_format_idc)?;
    }
    pps.lists_modification_present_flag = br.read_flag()?;
    let log2_parallel_merge_level_minus2 = br.read_ue()?;
    if log2_parallel_merge_level_minus2 > sps.log2_ctb_size {
        return Err(Error::InvalidData);
    }
    pps.log2_parallel_merge_level = log2_parallel_merge_level_minus2 + 2;
    pps.slice_header_extension_present_flag = br.read_flag()?;

    pps.pps_extension_present_flag = br.read_flag()?;
    if pps.pps_extension_present_flag {
        pps.pps_range_extensions_flag = br.read_flag()?;
        pps.pps_multilayer_extension_flag = br.read_flag()?;
        pps.pps_3d_extension_flag = br.read_flag()?;
        pps.pps_scc_extension_flag = br.read_flag()?;
        br.skip_bits(4); // pps_extension_4bits

        if sps.ptl.profile_idc >= PROFILE_HEVC_REXT as u32 && pps.pps_range_extensions_flag {
            pps_range_extensions(&mut br, &mut pps, sps)?;
        }
        if pps.pps_multilayer_extension_flag || pps.pps_3d_extension_flag {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        if pps.pps_scc_extension_flag {
            pps_scc_extension(&mut br, &mut pps, sps)?;
        }
    }

    setup_pps(&mut pps, sps)?;
    Ok(pps)
}
