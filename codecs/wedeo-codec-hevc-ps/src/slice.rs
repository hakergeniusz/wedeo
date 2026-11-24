//! Slice segment header parsing (`hls_slice_header` in FFmpeg `hevcdec.c`).
//!
//! This crate does not decode slices, but exposes a typed [`SliceHeader`] and
//! a parser that consumes exactly the same syntax as FFmpeg, so later HEVC
//! work units can start at `slice_header.data_offset` with CABAC / prediction.

use wedeo_core::{Error, Result};

use crate::MAX_REFS;
use crate::bitreader::BitReader;
use crate::nal::NalUnitType;
use crate::pps::Pps;
use crate::rps::{ShortTermRps, parse_short_term_rps};
use crate::sps::Sps;
use crate::vps::Vps;

/// Reference list indices (`L0` / `L1` in FFmpeg).
pub const L0: usize = 0;
pub const L1: usize = 1;

/// `enum HEVCSliceType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SliceType {
    B = 0,
    P = 1,
    I = 2,
}

impl SliceType {
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::B),
            1 => Some(Self::P),
            2 => Some(Self::I),
            _ => None,
        }
    }
}

/// Long-term RPS parsed from a slice header (`LongTermRPS` in FFmpeg).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LongTermRps {
    pub nb_refs: usize,
    pub poc: [i32; 16],
    pub used: [bool; 16],
    pub poc_msb_present: [bool; 16],
}

/// Pred weight table state (`pred_weight_table` in FFmpeg `hevcdec.c`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PredWeightTable {
    pub luma_log2_weight_denom: u32,
    pub chroma_log2_weight_denom: u32,
    pub luma_weight_l0: [i32; MAX_REFS],
    pub luma_offset_l0: [i32; MAX_REFS],
    pub luma_weight_l1: [i32; MAX_REFS],
    pub luma_offset_l1: [i32; MAX_REFS],
    pub chroma_weight_l0: [[i32; 2]; MAX_REFS],
    pub chroma_offset_l0: [[i32; 2]; MAX_REFS],
    pub chroma_weight_l1: [[i32; 2]; MAX_REFS],
    pub chroma_offset_l1: [[i32; 2]; MAX_REFS],
}

/// Inputs that come from outside the slice header bitstream.
#[derive(Debug, Clone, Copy)]
pub struct SliceContext<'a> {
    pub nal_unit_type: NalUnitType,
    pub nuh_layer_id: u8,
    /// POC of the previous picture with TemporalId == 0.
    pub poc_tid0: i32,
    pub last_eos: bool,
    /// Whether an independent slice has already been initialized in the picture.
    pub slice_initialized: bool,
    pub vps: &'a Vps,
    pub sps: &'a Sps,
    pub pps: &'a Pps,
}

/// Typed slice segment header (FFmpeg's `SliceHeader`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_slice_in_pic_flag: bool,
    pub no_output_of_prior_pics_flag: bool,
    pub pps_id: u32,
    pub dependent_slice_segment_flag: bool,
    pub slice_segment_addr: u32,
    pub slice_addr: u32,
    pub slice_ctb_addr_rs: u32,
    pub slice_type: SliceType,
    pub pic_output_flag: bool,
    pub colour_plane_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub poc: i32,
    pub short_term_ref_pic_set_sps_flag: bool,
    pub short_term_rps: Option<ShortTermRps>,
    pub short_term_ref_pic_set_size: usize,
    pub long_term_rps: LongTermRps,
    pub long_term_ref_pic_set_size: usize,
    pub slice_temporal_mvp_enabled_flag: bool,
    pub inter_layer_pred: bool,
    pub slice_sample_adaptive_offset_flag: [bool; 3],
    pub nb_refs: [u32; 2],
    pub rpl_modification_flag: [bool; 2],
    pub list_entry_lx: [[u32; MAX_REFS]; 2],
    pub mvd_l1_zero_flag: bool,
    pub cabac_init_flag: bool,
    /// 0 = L0, 1 = L1 (FFmpeg's `collocated_list`).
    pub collocated_list: usize,
    pub collocated_ref_idx: u32,
    pub max_num_merge_cand: u32,
    pub use_integer_mv_flag: bool,
    pub pred_weight_table: Option<PredWeightTable>,
    pub slice_qp_delta: i32,
    pub slice_cb_qp_offset: i32,
    pub slice_cr_qp_offset: i32,
    pub slice_act_y_qp_offset: i32,
    pub slice_act_cb_qp_offset: i32,
    pub slice_act_cr_qp_offset: i32,
    pub cu_chroma_qp_offset_enabled_flag: bool,
    pub disable_deblocking_filter_flag: bool,
    pub beta_offset: i32,
    pub tc_offset: i32,
    pub slice_loop_filter_across_slices_enabled_flag: bool,
    pub num_entry_point_offsets: usize,
    pub entry_point_offset: Vec<u32>,
    pub slice_header_extension_length: u32,
    /// Bit position (from the start of the slice RBSP) at which slice data
    /// begins, after the `alignment_bit_equal_to_one`.
    pub data_offset: usize,
    pub slice_qp: i32,
}

impl Default for SliceHeader {
    fn default() -> Self {
        Self {
            first_slice_in_pic_flag: false,
            no_output_of_prior_pics_flag: false,
            pps_id: 0,
            dependent_slice_segment_flag: false,
            slice_segment_addr: 0,
            slice_addr: 0,
            slice_ctb_addr_rs: 0,
            slice_type: SliceType::I,
            pic_output_flag: true,
            colour_plane_id: 0,
            pic_order_cnt_lsb: 0,
            poc: 0,
            short_term_ref_pic_set_sps_flag: false,
            short_term_rps: None,
            short_term_ref_pic_set_size: 0,
            long_term_rps: LongTermRps::default(),
            long_term_ref_pic_set_size: 0,
            slice_temporal_mvp_enabled_flag: false,
            inter_layer_pred: false,
            slice_sample_adaptive_offset_flag: [false; 3],
            nb_refs: [0; 2],
            rpl_modification_flag: [false; 2],
            list_entry_lx: [[0; MAX_REFS]; 2],
            mvd_l1_zero_flag: false,
            cabac_init_flag: false,
            collocated_list: L0,
            collocated_ref_idx: 0,
            max_num_merge_cand: 5,
            use_integer_mv_flag: false,
            pred_weight_table: None,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            slice_act_y_qp_offset: 0,
            slice_act_cb_qp_offset: 0,
            slice_act_cr_qp_offset: 0,
            cu_chroma_qp_offset_enabled_flag: false,
            disable_deblocking_filter_flag: false,
            beta_offset: 0,
            tc_offset: 0,
            slice_loop_filter_across_slices_enabled_flag: false,
            num_entry_point_offsets: 0,
            entry_point_offset: Vec::new(),
            slice_header_extension_length: 0,
            data_offset: 0,
            slice_qp: 26,
        }
    }
}

/// `ff_hevc_compute_poc2()` from `ps.c`.
pub fn compute_poc(
    log2_max_poc_lsb: u32,
    poc_tid0: i32,
    poc_lsb: u32,
    nal_unit_type: NalUnitType,
) -> i32 {
    let max_poc_lsb = 1i32 << log2_max_poc_lsb;
    let prev_poc_lsb = poc_tid0.rem_euclid(max_poc_lsb);
    let prev_poc_msb = poc_tid0 - prev_poc_lsb;
    let poc_lsb = poc_lsb as i32;
    let poc_msb = if poc_lsb < prev_poc_lsb && prev_poc_lsb - poc_lsb >= max_poc_lsb / 2 {
        prev_poc_msb + max_poc_lsb
    } else if poc_lsb > prev_poc_lsb && poc_lsb - prev_poc_lsb > max_poc_lsb / 2 {
        prev_poc_msb - max_poc_lsb
    } else {
        prev_poc_msb
    };
    // For BLA picture types, POCmsb is set to 0.
    if matches!(
        nal_unit_type,
        NalUnitType::BlaWLp | NalUnitType::BlaWRadl | NalUnitType::BlaNLp
    ) {
        0
    } else {
        poc_msb + poc_lsb
    }
}

fn decode_lt_rps(
    br: &mut BitReader<'_>,
    sps: &Sps,
    cur_poc: i32,
    poc_lsb: u32,
) -> Result<LongTermRps> {
    let mut rps = LongTermRps::default();
    if !sps.long_term_ref_pics_present {
        return Ok(rps);
    }
    let max_poc_lsb = 1i32 << sps.log2_max_poc_lsb;
    let mut prev_delta_msb = 0i64;
    let nb_sps = if sps.num_long_term_ref_pics_sps > 0 {
        br.read_ue()?
    } else {
        0
    };
    let nb_sh = br.read_ue()?;
    if nb_sps as usize > sps.num_long_term_ref_pics_sps
        || nb_sh as usize + nb_sps as usize > rps.poc.len()
    {
        return Err(Error::InvalidData);
    }
    rps.nb_refs = nb_sh as usize + nb_sps as usize;
    for i in 0..rps.nb_refs {
        if i < nb_sps as usize {
            let idx = if sps.num_long_term_ref_pics_sps > 1 {
                let bits = (sps.num_long_term_ref_pics_sps as f64).log2().ceil() as usize;
                br.read_bits(bits)? as usize
            } else {
                0
            };
            rps.poc[i] = sps.lt_ref_pic_poc_lsb_sps[idx] as i32;
            rps.used[i] = sps.used_by_curr_pic_lt & (1 << idx) != 0;
        } else {
            rps.poc[i] = br.read_bits(sps.log2_max_poc_lsb as usize)? as i32;
            rps.used[i] = br.read_flag()?;
        }
        rps.poc_msb_present[i] = br.read_flag()?;
        if rps.poc_msb_present[i] {
            let mut delta = br.read_ue()? as i64;
            if i != 0 && i != nb_sps as usize {
                delta += prev_delta_msb;
            }
            let poc =
                rps.poc[i] as i64 + cur_poc as i64 - delta * max_poc_lsb as i64 - poc_lsb as i64;
            // FFmpeg: `poc != (int32_t)poc`.
            if !(i32::MIN as i64..=i32::MAX as i64).contains(&poc) {
                return Err(Error::InvalidData);
            }
            rps.poc[i] = poc as i32;
            prev_delta_msb = delta;
        }
    }
    Ok(rps)
}

fn pred_weight_table(br: &mut BitReader<'_>, sh: &mut SliceHeader, sps: &Sps) -> Result<()> {
    let mut pwt = PredWeightTable::default();
    let luma_denom = br.read_ue()?;
    if luma_denom > 7 {
        return Err(Error::InvalidData);
    }
    pwt.luma_log2_weight_denom = luma_denom;
    if sps.chroma_format_idc != 0 {
        let chroma_delta = br.read_se()? as i64;
        let chroma_denom = luma_denom as i64 + chroma_delta;
        if !(0..=7).contains(&chroma_denom) {
            return Err(Error::InvalidData);
        }
        pwt.chroma_log2_weight_denom = chroma_denom as u32;
    }
    let l0 = sh.nb_refs[L0] as usize;
    let l1 = sh.nb_refs[L1] as usize;
    let luma_flags0 = br.read_bits(l0)?;
    let chroma_flags0 = if sps.chroma_format_idc != 0 {
        br.read_bits(l0)?
    } else {
        0
    };
    for i in 0..l0 {
        let bit = 1u32 << (l0 - 1 - i);
        if luma_flags0 & bit != 0 {
            let delta = br.read_se()?;
            if delta < i8::MIN as i32 || delta > i8::MAX as i32 {
                return Err(Error::InvalidData);
            }
            pwt.luma_weight_l0[i] = (1 << pwt.luma_log2_weight_denom) + delta;
            pwt.luma_offset_l0[i] = br.read_se()?;
        } else {
            pwt.luma_weight_l0[i] = 1 << pwt.luma_log2_weight_denom;
        }
        if chroma_flags0 & bit != 0 {
            for j in 0..2 {
                let dw = br.read_se()?;
                let dow = br.read_se()?;
                if dw < i8::MIN as i32
                    || dw > i8::MAX as i32
                    || !(-(1 << 17)..=1 << 17).contains(&dow)
                {
                    return Err(Error::InvalidData);
                }
                pwt.chroma_weight_l0[i][j] = (1 << pwt.chroma_log2_weight_denom) + dw;
                let val = dow
                    - ((128 * pwt.chroma_weight_l0[i][j]) >> pwt.chroma_log2_weight_denom)
                    + 128;
                pwt.chroma_offset_l0[i][j] = val.clamp(-128, 127);
            }
        } else {
            for j in 0..2 {
                pwt.chroma_weight_l0[i][j] = 1 << pwt.chroma_log2_weight_denom;
            }
        }
    }
    if sh.slice_type == SliceType::B {
        let luma_flags1 = br.read_bits(l1)?;
        let chroma_flags1 = if sps.chroma_format_idc != 0 {
            br.read_bits(l1)?
        } else {
            0
        };
        for i in 0..l1 {
            let bit = 1u32 << (l1 - 1 - i);
            if luma_flags1 & bit != 0 {
                let delta = br.read_se()?;
                if delta < i8::MIN as i32 || delta > i8::MAX as i32 {
                    return Err(Error::InvalidData);
                }
                pwt.luma_weight_l1[i] = (1 << pwt.luma_log2_weight_denom) + delta;
                pwt.luma_offset_l1[i] = br.read_se()?;
            } else {
                pwt.luma_weight_l1[i] = 1 << pwt.luma_log2_weight_denom;
            }
            if chroma_flags1 & bit != 0 {
                for j in 0..2 {
                    let dw = br.read_se()?;
                    let dow = br.read_se()?;
                    if dw < i8::MIN as i32
                        || dw > i8::MAX as i32
                        || !(-(1 << 17)..=1 << 17).contains(&dow)
                    {
                        return Err(Error::InvalidData);
                    }
                    pwt.chroma_weight_l1[i][j] = (1 << pwt.chroma_log2_weight_denom) + dw;
                    let val = dow
                        - ((128 * pwt.chroma_weight_l1[i][j]) >> pwt.chroma_log2_weight_denom)
                        + 128;
                    pwt.chroma_offset_l1[i][j] = val.clamp(-128, 127);
                }
            } else {
                for j in 0..2 {
                    pwt.chroma_weight_l1[i][j] = 1 << pwt.chroma_log2_weight_denom;
                }
            }
        }
    }
    sh.pred_weight_table = Some(pwt);
    Ok(())
}

/// `ff_hevc_frame_nb_refs()` (from `hevcdec.c`): number of distinct refs.
fn frame_nb_refs(sh: &SliceHeader) -> usize {
    let mut ref_count = 0usize;
    for i in 0..sh.nb_refs[L0] as usize {
        let is_lt = i < sh.long_term_rps.nb_refs && sh.long_term_rps.used[i];
        if !is_lt {
            ref_count += 1;
        }
    }
    if sh.slice_type == SliceType::B {
        for i in 0..sh.nb_refs[L1] as usize {
            let is_lt = i < sh.long_term_rps.nb_refs && sh.long_term_rps.used[i];
            if !is_lt {
                ref_count += 1;
            }
        }
    }
    ref_count
}

/// Parse a slice segment header from a slice NAL's RBSP.
pub fn parse_slice_header(rbsp: &[u8], ctx: &SliceContext<'_>) -> Result<SliceHeader> {
    let mut br = BitReader::new(rbsp);
    let mut sh = SliceHeader::default();
    let pps = ctx.pps;
    let sps = ctx.sps;

    sh.first_slice_in_pic_flag = br.read_flag()?;
    if ctx.nal_unit_type.is_irap() {
        sh.no_output_of_prior_pics_flag = br.read_flag()?;
    }
    if ctx.nal_unit_type == NalUnitType::CraNut && ctx.last_eos {
        sh.no_output_of_prior_pics_flag = true;
    }

    sh.pps_id = br.read_ue()?;
    if sh.pps_id as usize >= 64 || sh.pps_id != pps.pps_id {
        return Err(Error::InvalidData);
    }
    if !sh.first_slice_in_pic_flag && sh.pps_id != pps.pps_id {
        return Err(Error::InvalidData);
    }

    if !sh.first_slice_in_pic_flag {
        let slice_address_length = (sps.ctb_size as f64).log2().ceil() as usize;
        if pps.dependent_slice_segments_enabled_flag {
            sh.dependent_slice_segment_flag = br.read_flag()?;
        }
        if sh.dependent_slice_segment_flag && !ctx.slice_initialized {
            return Err(Error::InvalidData);
        }
        sh.slice_segment_addr = br.read_bits(slice_address_length)?;
        if sh.slice_segment_addr >= sps.ctb_size {
            return Err(Error::InvalidData);
        }
        if !sh.dependent_slice_segment_flag {
            sh.slice_addr = sh.slice_segment_addr;
        }
    }

    if !sh.dependent_slice_segment_flag {
        br.skip_bits(pps.num_extra_slice_header_bits as usize);
        let slice_type = br.read_ue()?;
        sh.slice_type = SliceType::from_code(slice_type).ok_or(Error::InvalidData)?;
        if ctx.nal_unit_type.is_irap()
            && sh.slice_type != SliceType::I
            && !pps.pps_curr_pic_ref_enabled_flag
            && ctx.nuh_layer_id == 0
        {
            return Err(Error::InvalidData);
        }

        sh.pic_output_flag = true;
        if pps.output_flag_present_flag {
            sh.pic_output_flag = br.read_flag()?;
        }
        if sps.separate_colour_plane {
            sh.colour_plane_id = br.read_bits(2)?;
        }

        if !ctx.nal_unit_type.is_idr() {
            sh.pic_order_cnt_lsb = br.read_bits(sps.log2_max_poc_lsb as usize)?;
            sh.poc = compute_poc(
                sps.log2_max_poc_lsb,
                ctx.poc_tid0,
                sh.pic_order_cnt_lsb,
                ctx.nal_unit_type,
            );
        }

        if !ctx.nal_unit_type.is_idr() {
            sh.short_term_ref_pic_set_sps_flag = br.read_flag()?;
            let pos = br.bits_left();
            if !sh.short_term_ref_pic_set_sps_flag {
                let rps = parse_short_term_rps(&mut br, &sps.st_rps, None, true)?;
                sh.short_term_rps = Some(rps);
            } else {
                if sps.st_rps.is_empty() {
                    return Err(Error::InvalidData);
                }
                let bits = (sps.st_rps.len() as f64).log2().ceil() as usize;
                let idx = if bits > 0 {
                    br.read_bits(bits)? as usize
                } else {
                    0
                };
                sh.short_term_rps = Some(sps.st_rps[idx].clone());
            }
            sh.short_term_ref_pic_set_size = (pos - br.bits_left()) as usize;

            let pos = br.bits_left();
            sh.long_term_rps = decode_lt_rps(&mut br, sps, sh.poc, sh.pic_order_cnt_lsb)?;
            sh.long_term_ref_pic_set_size = (pos - br.bits_left()) as usize;

            sh.slice_temporal_mvp_enabled_flag = if sps.temporal_mvp_enabled {
                br.read_flag()?
            } else {
                false
            };
        }

        if ctx.nuh_layer_id > 0 {
            let num_direct_ref_layers = ctx
                .vps
                .extension
                .as_ref()
                .map_or(0, |e| e.num_direct_ref_layers.get(1).copied().unwrap_or(0));
            let default_active = ctx
                .vps
                .extension
                .as_ref()
                .is_some_and(|e| e.default_ref_layers_active);
            if default_active {
                sh.inter_layer_pred = num_direct_ref_layers != 0;
            } else if num_direct_ref_layers != 0 {
                sh.inter_layer_pred = br.read_flag()?;
            }
        }

        if sps.sao_enabled {
            sh.slice_sample_adaptive_offset_flag[0] = br.read_flag()?;
            if sps.chroma_format_idc != 0 {
                let chroma = br.read_flag()?;
                sh.slice_sample_adaptive_offset_flag[1] = chroma;
                sh.slice_sample_adaptive_offset_flag[2] = chroma;
            }
        }

        if sh.slice_type == SliceType::P || sh.slice_type == SliceType::B {
            sh.nb_refs[L0] = pps.num_ref_idx_l0_default_active;
            if sh.slice_type == SliceType::B {
                sh.nb_refs[L1] = pps.num_ref_idx_l1_default_active;
            }
            if br.read_flag()? {
                sh.nb_refs[L0] = br.read_ue31()? + 1;
                if sh.slice_type == SliceType::B {
                    sh.nb_refs[L1] = br.read_ue31()? + 1;
                }
            }
            if sh.nb_refs[L0] as usize >= MAX_REFS || sh.nb_refs[L1] as usize >= MAX_REFS {
                return Err(Error::InvalidData);
            }

            let nb_refs = frame_nb_refs(&sh);
            if nb_refs == 0 {
                return Err(Error::InvalidData);
            }
            if pps.lists_modification_present_flag && nb_refs > 1 {
                sh.rpl_modification_flag[L0] = br.read_flag()?;
                if sh.rpl_modification_flag[L0] {
                    let bits = (nb_refs as f64).log2().ceil() as usize;
                    for i in 0..sh.nb_refs[L0] as usize {
                        sh.list_entry_lx[L0][i] = br.read_bits(bits)?;
                    }
                }
                if sh.slice_type == SliceType::B {
                    sh.rpl_modification_flag[L1] = br.read_flag()?;
                    if sh.rpl_modification_flag[L1] {
                        let bits = (nb_refs as f64).log2().ceil() as usize;
                        for i in 0..sh.nb_refs[L1] as usize {
                            sh.list_entry_lx[L1][i] = br.read_bits(bits)?;
                        }
                    }
                }
            }

            if sh.slice_type == SliceType::B {
                sh.mvd_l1_zero_flag = br.read_flag()?;
            }
            sh.cabac_init_flag = if pps.cabac_init_present_flag {
                br.read_flag()?
            } else {
                false
            };

            if sh.slice_temporal_mvp_enabled_flag {
                sh.collocated_list = L0;
                if sh.slice_type == SliceType::B {
                    sh.collocated_list = usize::from(!br.read_flag()?);
                }
                if sh.nb_refs[sh.collocated_list] > 1 {
                    sh.collocated_ref_idx = br.read_ue()?;
                    if sh.collocated_ref_idx >= sh.nb_refs[sh.collocated_list] {
                        return Err(Error::InvalidData);
                    }
                }
            }

            if (pps.weighted_pred_flag && sh.slice_type == SliceType::P)
                || (pps.weighted_bipred_flag && sh.slice_type == SliceType::B)
            {
                pred_weight_table(&mut br, &mut sh, sps)?;
            }

            let max_merge = 5 - br.read_ue()?;
            if !(1..=5).contains(&max_merge) {
                return Err(Error::InvalidData);
            }
            sh.max_num_merge_cand = max_merge;
            sh.use_integer_mv_flag = if sps.scc.motion_vector_resolution_control_idc == 2 {
                br.read_flag()?
            } else {
                sps.scc.motion_vector_resolution_control_idc != 0
            };
        }

        sh.slice_qp_delta = br.read_se()?;
        if pps.pic_slice_level_chroma_qp_offsets_present_flag {
            sh.slice_cb_qp_offset = br.read_se()?;
            sh.slice_cr_qp_offset = br.read_se()?;
            if !(-12..=12).contains(&sh.slice_cb_qp_offset)
                || !(-12..=12).contains(&sh.slice_cr_qp_offset)
            {
                return Err(Error::InvalidData);
            }
        }
        if pps.pps_slice_act_qp_offsets_present_flag {
            sh.slice_act_y_qp_offset = br.read_se()?;
            sh.slice_act_cb_qp_offset = br.read_se()?;
            sh.slice_act_cr_qp_offset = br.read_se()?;
        }
        sh.cu_chroma_qp_offset_enabled_flag = if pps.chroma_qp_offset_list_enabled_flag {
            br.read_flag()?
        } else {
            false
        };

        if pps.deblocking_filter_control_present_flag {
            let mut override_flag = false;
            if pps.deblocking_filter_override_enabled_flag {
                override_flag = br.read_flag()?;
            }
            if override_flag {
                sh.disable_deblocking_filter_flag = br.read_flag()?;
                if !sh.disable_deblocking_filter_flag {
                    let beta = br.read_se()?;
                    let tc = br.read_se()?;
                    if !(-6..=6).contains(&beta) || !(-6..=6).contains(&tc) {
                        return Err(Error::InvalidData);
                    }
                    sh.beta_offset = 2 * beta;
                    sh.tc_offset = 2 * tc;
                }
            } else {
                sh.disable_deblocking_filter_flag = pps.disable_dbf;
                sh.beta_offset = pps.beta_offset;
                sh.tc_offset = pps.tc_offset;
            }
        }

        if pps.seq_loop_filter_across_slices_enabled_flag
            && (sh.slice_sample_adaptive_offset_flag[0]
                || sh.slice_sample_adaptive_offset_flag[1]
                || !sh.disable_deblocking_filter_flag)
        {
            sh.slice_loop_filter_across_slices_enabled_flag = br.read_flag()?;
        } else {
            sh.slice_loop_filter_across_slices_enabled_flag =
                pps.seq_loop_filter_across_slices_enabled_flag;
        }
    }

    if pps.tiles_enabled_flag || pps.entropy_coding_sync_enabled_flag {
        let num = br.read_ue()?;
        if num > br.bits_left().max(0) as u32 || num > u16::MAX as u32 {
            return Err(Error::InvalidData);
        }
        sh.num_entry_point_offsets = num as usize;
        if num > 0 {
            let offset_len = br.read_ue()? + 1;
            if !(1..=32).contains(&offset_len) {
                return Err(Error::InvalidData);
            }
            for _ in 0..num {
                let val = br.read_bits(offset_len as usize)?;
                sh.entry_point_offset.push(val + 1);
            }
        }
    }

    if pps.slice_header_extension_present_flag {
        let length = br.read_ue()?;
        if length as usize * 8 > br.bits_left().max(0) as usize {
            return Err(Error::InvalidData);
        }
        sh.slice_header_extension_length = length;
        br.skip_bits(length as usize * 8);
    }

    if !br.read_flag()? {
        return Err(Error::InvalidData);
    }
    sh.data_offset = br.bit_position().div_ceil(8);

    sh.slice_qp = 26 + pps.pic_init_qp_minus26 + sh.slice_qp_delta;
    if sh.slice_qp > 51 || sh.slice_qp < -(sps.qp_bd_offset as i32) {
        return Err(Error::InvalidData);
    }
    sh.slice_ctb_addr_rs = sh.slice_segment_addr;
    if sh.dependent_slice_segment_flag
        && (sh.slice_ctb_addr_rs == 0
            || pps
                .tile
                .ctb_addr_rs_to_ts
                .get(sh.slice_ctb_addr_rs as usize)
                .copied()
                .unwrap_or(0)
                == 0)
    {
        return Err(Error::InvalidData);
    }

    Ok(sh)
}
