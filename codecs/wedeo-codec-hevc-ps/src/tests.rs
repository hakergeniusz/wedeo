//! Synthetic parameter-set bitstreams built from FFmpeg field semantics and
//! parsed back through the public API. No external test vectors are needed.

use wedeo_core::PixelFormat;

use crate::nal::{NalUnitType, annex_b_split, parse_nal_header};
use crate::ps::{ParamSets, decode_extradata, is_hvcc};
use crate::slice::{SliceContext, SliceType, parse_slice_header};
use crate::{SUB_HEIGHT_C, SUB_WIDTH_C, parse_pps, parse_sps, parse_vps};

/// Minimal MSB-first bit writer for building synthetic RBSPs.
#[derive(Default)]
struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    fn new() -> Self {
        Self::default()
    }

    fn bit(&mut self, b: bool) {
        self.bits.push(b);
    }

    fn flag(&mut self, b: bool) {
        self.bit(b);
    }

    fn bits(&mut self, v: u64, n: usize) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 != 0);
        }
    }

    fn ue(&mut self, v: u32) {
        let code = v + 1;
        let zeros = 31 - code.leading_zeros();
        for _ in 0..zeros {
            self.bit(false);
        }
        self.bits(code as u64, zeros as usize + 1);
    }

    fn se(&mut self, v: i32) {
        let code = if v <= 0 {
            (-2i64 * v as i64) as u32
        } else {
            (2 * v - 1) as u32
        };
        self.ue(code);
    }

    fn trailing(&mut self) {
        self.bit(true);
        while !self.bits.len().is_multiple_of(8) {
            self.bit(false);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.trailing();
        let mut out = vec![0u8; self.bits.len() / 8];
        for (i, &b) in self.bits.iter().enumerate() {
            if b {
                out[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        out
    }
}

/// `decode_profile_tier_level` for Main profile (profile_idc = 1).
fn write_ptl(w: &mut BitWriter) {
    w.bits(0, 2); // profile_space
    w.flag(false); // tier_flag
    w.bits(1, 5); // profile_idc = Main
    for i in 0..32 {
        w.flag(i == 1); // profile_compatibility_flag[1] = 1
    }
    w.flag(true); // progressive_source_flag
    w.flag(false); // interlaced_source_flag
    w.flag(false); // non_packed_constraint_flag
    w.flag(false); // frame_only_constraint_flag
    // Neither 4..10 nor 2 is signalled: 43 reserved bits.
    w.bits(0, 43);
    w.flag(false); // inbld_flag (present for profile 1)
    w.bits(120, 8); // general_level_idc
}

fn make_vps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(0, 4); // vps_video_parameter_set_id
    w.flag(true); // vps_base_layer_internal_flag
    w.flag(true); // vps_base_layer_available_flag
    w.bits(0, 6); // vps_max_layers_minus1
    w.bits(0, 3); // vps_max_sub_layers_minus1
    w.flag(true); // vps_temporal_id_nesting_flag
    w.bits(0xffff, 16); // vps_reserved_ffff_16bits
    write_ptl(&mut w);
    w.flag(true); // vps_sub_layer_ordering_info_present_flag
    w.ue(0); // vps_max_dec_pic_buffering_minus1[0] -> 1
    w.ue(0); // vps_num_reorder_pics[0]
    w.ue(0); // vps_max_latency_increase_plus1[0]
    w.bits(0, 6); // vps_max_layer_id
    w.ue(0); // vps_num_layer_sets_minus1 -> 1
    w.flag(false); // vps_timing_info_present_flag
    w.finish()
}

/// Build an SPS. `vui` selects a minimal VUI block; `scaling` selects a
/// default (all-copy) scaling list block.
fn make_sps(vui: bool, scaling: bool, rps: bool) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.bits(0, 4); // sps_video_parameter_set_id
    w.bits(0, 3); // sps_max_sub_layers_minus1
    w.flag(true); // sps_temporal_id_nesting_flag
    write_ptl(&mut w);
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(1); // chroma_format_idc = 4:2:0
    w.ue(64); // pic_width_in_luma_samples
    w.ue(64); // pic_height_in_luma_samples
    w.flag(false); // conformance_window_flag
    w.ue(0); // bit_depth_luma_minus8
    w.ue(0); // bit_depth_chroma_minus8
    w.ue(0); // log2_max_pic_order_cnt_lsb_minus4 -> 4
    w.flag(true); // sps_sub_layer_ordering_info_present_flag
    w.ue(0);
    w.ue(0);
    w.ue(0); // DPB params
    w.ue(0); // log2_min_luma_coding_block_size_minus3 -> 3
    w.ue(3); // log2_diff_max_min_luma_coding_block_size -> log2_ctb_size 6
    w.ue(0); // log2_min_luma_transform_block_size_minus2 -> 2
    w.ue(2); // log2_diff_max_min_luma_transform_block_size -> max trafo 4
    w.ue(0); // max_transform_hierarchy_depth_inter
    w.ue(0); // max_transform_hierarchy_depth_intra
    w.flag(scaling); // scaling_list_enabled_flag
    if scaling {
        w.flag(true); // sps_scaling_list_data_present_flag
        // 20 lists: 6 + 6 + 6 + 2. All use pred mode 0 with delta 0.
        for size_id in 0..4 {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut m = 0;
            while m < 6 {
                w.flag(false); // scaling_list_pred_mode_flag
                w.ue(0); // scaling_list_pred_matrix_id_delta
                m += step;
            }
        }
    }
    w.flag(false); // amp_enabled_flag
    w.flag(true); // sample_adaptive_offset_enabled_flag
    w.flag(false); // pcm_enabled_flag
    if rps {
        w.ue(1); // num_short_term_ref_pic_sets
        // RPS[0]: inter_ref_pic_set_prediction_flag absent for index 0.
        w.ue(1); // num_negative_pics
        w.ue(1); // num_positive_pics
        w.ue(1); // delta_poc_s0_minus1 = 1 -> -2
        w.flag(true); // used_by_curr_pic_s0_flag
        w.ue(2); // delta_poc_s1_minus1 = 2 -> 3
        w.flag(false); // used_by_curr_pic_s1_flag
    } else {
        w.ue(0); // num_short_term_ref_pic_sets
    }
    w.flag(false); // long_term_ref_pics_present_flag
    w.flag(true); // sps_temporal_mvp_enabled_flag
    w.flag(false); // strong_intra_smoothing_enabled_flag
    w.flag(vui); // vui_parameters_present_flag
    if vui {
        w.flag(true); // aspect_ratio_info_present_flag
        w.bits(1, 8); // aspect_ratio_idc = 1 (1:1)
        w.flag(false); // overscan_info_present_flag
        w.flag(false); // video_signal_type_present_flag
        w.flag(false); // chroma_loc_info_present_flag
        w.flag(false); // neutral_chroma_indication_flag
        w.flag(false); // field_seq_flag
        w.flag(false); // frame_field_info_present_flag
        w.flag(false); // default_display_window_flag
        w.flag(false); // vui_timing_info_present_flag
        w.flag(false); // bitstream_restriction_flag
    }
    w.flag(false); // sps_extension_present_flag
    w.finish()
}

fn make_pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pps_pic_parameter_set_id
    w.ue(0); // pps_seq_parameter_set_id
    w.flag(false); // dependent_slice_segments_enabled_flag
    w.flag(true); // output_flag_present_flag
    w.bits(0, 3); // num_extra_slice_header_bits
    w.flag(false); // sign_data_hiding_enabled_flag
    w.flag(true); // cabac_init_present_flag
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.se(0); // init_qp_minus26
    w.flag(false); // constrained_intra_pred_flag
    w.flag(false); // transform_skip_enabled_flag
    w.flag(false); // cu_qp_delta_enabled_flag
    w.se(0); // pps_cb_qp_offset
    w.se(0); // pps_cr_qp_offset
    w.flag(false); // pps_slice_chroma_qp_offsets_present_flag
    w.flag(false); // weighted_pred_flag
    w.flag(false); // weighted_bipred_flag
    w.flag(false); // transquant_bypass_enabled_flag
    w.flag(false); // tiles_enabled_flag
    w.flag(false); // entropy_coding_sync_enabled_flag
    w.flag(true); // pps_loop_filter_across_slices_enabled_flag
    w.flag(true); // deblocking_filter_control_present_flag
    w.flag(true); // pps_deblocking_filter_override_enabled_flag
    w.flag(false); // pps_deblocking_filter_disabled_flag
    w.se(0); // pps_beta_offset_div2
    w.se(0); // pps_tc_offset_div2
    w.flag(false); // pps_scaling_list_data_present_flag
    w.flag(false); // lists_modification_present_flag
    w.ue(0); // log2_parallel_merge_level_minus2
    w.flag(false); // slice_segment_header_extension_present_flag
    w.flag(false); // pps_extension_present_flag
    w.finish()
}

/// I-slice header for an IDR_N_LP NAL, PPS 0.
fn make_slice_header() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.flag(true); // first_slice_segment_in_pic_flag
    w.flag(false); // no_output_of_prior_pics_flag (IRAP)
    w.ue(0); // slice_pic_parameter_set_id
    w.ue(2); // slice_type = I
    w.flag(true); // pic_output_flag
    // IDR: no POC LSB, no RPS, no LTRPS, no temporal MVP
    w.flag(true); // slice_sao_luma_flag
    w.flag(true); // slice_sao_chroma_flag
    w.se(0); // slice_qp_delta
    w.flag(false); // slice_deblocking_filter_override_flag
    w.flag(true); // slice_loop_filter_across_slices_enabled_flag
    w.flag(true); // alignment_bit_equal_to_one
    w.bits(0, 7); // alignment zeros
    w.bits(0xAA, 8); // first slice data byte
    w.finish()
}

fn nal_bytes(nal_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut nal = vec![(nal_type << 1) & 0x7f, 0x01]; // layer 0, tid_plus1 = 1
    nal.extend_from_slice(payload);
    nal
}

#[test]
fn vps_roundtrip() {
    let rbsp = make_vps();
    let vps = parse_vps(&rbsp).unwrap();
    assert_eq!(vps.vps_id, 0);
    assert_eq!(vps.vps_max_layers, 1);
    assert_eq!(vps.vps_max_sub_layers, 1);
    assert!(vps.vps_temporal_id_nesting_flag);
    assert_eq!(vps.ptl.profile_idc, 1);
    assert_eq!(vps.ptl.level_idc, 120);
    assert_eq!(vps.vps_max_dec_pic_buffering[0], 1);
    assert_eq!(vps.vps_num_reorder_pics[0], 0);
    assert_eq!(vps.vps_max_layer_id, 0);
    assert_eq!(vps.vps_num_layer_sets, 1);
    assert!(vps.extension.is_none());
}

#[test]
fn sps_roundtrip() {
    let rbsp = make_sps(false, false, false);
    let sps = parse_sps(&rbsp, 0).unwrap();
    assert_eq!(sps.sps_id, 0);
    assert_eq!(sps.vps_id, 0);
    assert_eq!(sps.chroma_format_idc, 1);
    assert_eq!(sps.width, 64);
    assert_eq!(sps.height, 64);
    assert_eq!(sps.bit_depth, 8);
    assert_eq!(sps.bit_depth_chroma, 8);
    assert_eq!(sps.pix_fmt, Some(PixelFormat::Yuv420p));
    assert_eq!(sps.log2_max_poc_lsb, 4);
    assert_eq!(sps.log2_min_cb_size, 3);
    assert_eq!(sps.log2_ctb_size, 6);
    assert_eq!(sps.log2_min_tb_size, 2);
    assert_eq!(sps.log2_max_trafo_size, 4);
    assert_eq!(sps.ctb_width, 1);
    assert_eq!(sps.ctb_height, 1);
    assert_eq!(sps.ctb_size, 1);
    assert_eq!(sps.tb_mask, 15);
    assert_eq!(sps.qp_bd_offset, 0);
    assert_eq!(sps.hshift, [0, 1, 1]);
    assert_eq!(sps.vshift, [0, 1, 1]);
    assert!(sps.sao_enabled);
    assert!(sps.temporal_mvp_enabled);
    assert_eq!(SUB_WIDTH_C[sps.chroma_format_idc as usize], 2);
    assert_eq!(SUB_HEIGHT_C[sps.chroma_format_idc as usize], 2);
}

#[test]
fn sps_with_vui_and_scaling() {
    let sps = parse_sps(&make_sps(true, true, false), 0).unwrap();
    assert!(sps.vui_present);
    assert_eq!(sps.vui.common.aspect_ratio_idc, 1);
    assert_eq!(sps.vui.common.sar.num, 1);
    assert_eq!(sps.vui.common.sar.den, 1);
    assert!(sps.scaling_list_enabled);
    // All lists copied from defaults: 4x4 coefficient 0 is 16, 8x8 is 16.
    assert_eq!(sps.scaling_list.sl[0][0][0], 16);
    assert_eq!(sps.scaling_list.sl[1][0][0], 16);
    assert_eq!(sps.scaling_list.sl_dc, [[16; 6], [16; 6]]);
}

#[test]
fn sps_with_short_term_rps() {
    let sps = parse_sps(&make_sps(false, false, true), 0).unwrap();
    assert_eq!(sps.nb_st_rps, 1);
    let rps = &sps.st_rps[0];
    assert_eq!(rps.num_negative_pics, 1);
    assert_eq!(rps.num_delta_pocs, 2);
    assert_eq!(rps.delta_poc[0], -2);
    assert_eq!(rps.delta_poc[1], 3);
    assert_eq!(rps.used, 1);
    assert!(!rps.rps_predict);
}

#[test]
fn pps_roundtrip_and_tile_tables() {
    let sps = parse_sps(&make_sps(false, false, false), 0).unwrap();
    let pps = parse_pps(&make_pps(), &sps).unwrap();
    assert_eq!(pps.pps_id, 0);
    assert_eq!(pps.sps_id, 0);
    assert_eq!(pps.num_ref_idx_l0_default_active, 1);
    assert!(pps.cabac_init_present_flag);
    assert!(pps.output_flag_present_flag);
    assert_eq!(pps.num_tile_columns, 1);
    assert_eq!(pps.num_tile_rows, 1);
    assert_eq!(pps.tile.col_bd, vec![0, 1]);
    assert_eq!(pps.tile.row_bd, vec![0, 1]);
    assert_eq!(pps.tile.ctb_addr_rs_to_ts, vec![0]);
    assert_eq!(pps.tile.tile_id, vec![0]);
}

#[test]
fn slice_header_roundtrip() {
    let sps = parse_sps(&make_sps(false, false, false), 0).unwrap();
    let pps = parse_pps(&make_pps(), &sps).unwrap();
    let vps = parse_vps(&make_vps()).unwrap();
    let ctx = SliceContext {
        nal_unit_type: NalUnitType::IdrNLp,
        nuh_layer_id: 0,
        poc_tid0: 0,
        last_eos: false,
        slice_initialized: false,
        vps: &vps,
        sps: &sps,
        pps: &pps,
    };
    let sh = parse_slice_header(&make_slice_header(), &ctx).unwrap();
    assert!(sh.first_slice_in_pic_flag);
    assert_eq!(sh.pps_id, 0);
    assert_eq!(sh.slice_type, SliceType::I);
    assert!(sh.pic_output_flag);
    assert!(sh.slice_sample_adaptive_offset_flag[0]);
    assert!(sh.slice_sample_adaptive_offset_flag[1]);
    assert_eq!(sh.slice_qp, 26);
    assert_eq!(sh.data_offset, 2); // header byte + alignment byte
}

#[test]
fn nal_and_annex_b_pipeline() {
    let vps_rbsp = make_vps();
    let sps_rbsp = make_sps(false, false, true);
    let pps_rbsp = make_pps();
    let mut stream = Vec::new();
    for (t, payload) in [
        (32u8, vps_rbsp.as_slice()),
        (33, sps_rbsp.as_slice()),
        (34, pps_rbsp.as_slice()),
    ] {
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&nal_bytes(t, payload));
    }
    let nals = annex_b_split(&stream);
    assert_eq!(nals.len(), 3);
    let (h, _) = parse_nal_header(nals[0]).unwrap();
    assert_eq!(h.nal_unit_type, NalUnitType::Vps);
    assert_eq!(h.nuh_temporal_id_plus1, 1);

    let ps = decode_extradata(&stream).unwrap();
    assert!(ps.vps_list[0].is_some());
    assert!(ps.sps_list[0].is_some());
    assert!(ps.pps_list[0].is_some());

    let ctx = ps
        .slice_context(
            0,
            parse_nal_header(&nal_bytes(20, &[])).unwrap().0,
            0,
            false,
            false,
        )
        .unwrap();
    let sh = parse_slice_header(&make_slice_header(), &ctx).unwrap();
    assert_eq!(sh.slice_type, SliceType::I);
}

#[test]
fn hvcc_detection() {
    let mut hvcc = vec![1u8; 24];
    hvcc[21] = 3; // lengthSizeMinusOne = 4 bytes
    hvcc[22] = 0; // no arrays
    assert!(is_hvcc(&hvcc));
    let mut sets = ParamSets::new();
    assert!(sets.decode_extradata(&hvcc).unwrap().is_empty());
    assert!(!is_hvcc(&[0, 0, 0, 1, 0x40]));
}

#[test]
fn invalid_inputs_rejected() {
    assert!(parse_vps(&[]).is_err());
    assert!(parse_sps(&[], 0).is_err());
    let sps = parse_sps(&make_sps(false, false, false), 0).unwrap();
    assert!(parse_pps(&[], &sps).is_err());
    assert!(parse_sps(&make_sps(false, false, false), 1).is_ok());
}
