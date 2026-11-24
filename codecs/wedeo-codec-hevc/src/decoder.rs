//! Native HEVC decoder: NAL processing, slice/CTU control flow, DPB output.
//!
//! FFmpeg reference: `libavcodec/hevc/hevcdec.c`
//! (`decode_nal_units`, `decode_slice`, `decode_slice_data`,
//! `hls_decode_entry`, `hls_coding_quadtree`, `hls_coding_unit`,
//! `hls_prediction_unit`, `hls_transform_tree`, `hls_transform_unit`).

use std::collections::VecDeque;

use wedeo_codec::decoder::{CodecParameters, Decoder, DecoderDescriptor};
use wedeo_codec::descriptor::{CodecCapabilities, CodecDescriptor, CodecProperties};
use wedeo_codec::registry::DecoderFactory;
use wedeo_codec_hevc_cabac::{Cabac, InterPredIdc, PartMode, ScanType};
use wedeo_codec_hevc_dsp as dsp;
use wedeo_codec_hevc_dsp::sao::{SaoParams, SaoType};
use wedeo_codec_hevc_ps::{
    BitReader, L0, L1, NalUnitType, ParamSets, Pps, SliceHeader, SliceType, Sps, annex_b_split,
    ebsp_to_rbsp, parse_nal_header,
};
use wedeo_codec_hevc_refs::{
    Dpb, FrameRef, FrameRpsParams, Mv, MvField, MvFieldTable, MvpParams, NeighbourAvailability,
    NeighbourCtbFlags, NewRefParams, PredFlag, RefGeometry, RefPicList, SliceRplParams,
    TemporalCtx, luma_mv_merge_mode, luma_mv_mvp_mode, slice_rpl,
};
use wedeo_core::buffer::Buffer;
use wedeo_core::frame::{Frame, FramePlane, PictureType};
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::{CodecId, Error, MediaType, Result};

use crate::filter::{boundary_strengths, filter_picture};
use crate::intra::{IntraState, intra_defaults, intra_predict_tu, intra_prediction_unit};
use crate::picture::{Picture, PictureStore};

/// Snapshot of CABAC context models shared between WPP rows (FFmpeg's
/// `common_cabac_state`).
type CabacStates = (
    [u8; wedeo_codec_hevc_cabac::HEVC_CONTEXTS],
    [u8; wedeo_codec_hevc_cabac::HEVC_STAT_COEFFS],
);

/// The native HEVC decoder.
pub struct HevcDecoder {
    params: CodecParameters,
    descriptor: CodecDescriptor,
    ps: ParamSets,
    dpb: Dpb,
    pictures: PictureStore,
    output_queue: VecDeque<FrameRef>,
    draining: bool,
    nal_length_size: usize,
    poc_tid0: i32,
    last_eos: bool,
    eos: bool,
    slice_initialized: bool,
    cur_ref: Option<FrameRef>,
    pts: i64,
    /// Set when an SPS changes geometry; forces a new sequence.
    last_sps_id: Option<u32>,
    /// Filter parameters from the last decoded slice of the current picture.
    filter_pps: Option<Pps>,
    filter_sps: Option<Sps>,
    filter_disable_deblock: bool,
}

impl HevcDecoder {
    /// Construct a decoder from stream parameters.
    pub fn new(params: CodecParameters) -> Result<Self> {
        let mut ps = ParamSets::new();
        let mut nal_length_size = 0usize;
        if !params.extradata.is_empty() {
            ps.decode_extradata(&params.extradata)?;
            // `ff_hevc_is_hvcc()` (hevc.h), inlined: the PS crate does not
            // re-export its local helper.
            let d = &params.extradata;
            let is_hvcc = d.len() >= 23 && (d[0] == 1 || (d[0] == 0 && (d[1] != 0 || d[2] > 1)));
            if is_hvcc {
                nal_length_size = ((d[21] & 3) as usize) + 1;
            }
        }
        Ok(Self {
            params,
            descriptor: CodecDescriptor {
                id: CodecId::Hevc,
                media_type: MediaType::Video,
                name: "hevc",
                long_name: "HEVC / H.265 (native)",
                properties: CodecProperties::LOSSY.union(CodecProperties::REORDER),
                profiles: &[],
            },
            ps,
            dpb: Dpb::default(),
            pictures: PictureStore::default(),
            output_queue: VecDeque::new(),
            draining: false,
            nal_length_size,
            poc_tid0: 0,
            last_eos: false,
            eos: false,
            slice_initialized: false,
            cur_ref: None,
            pts: 0,
            last_sps_id: None,
            filter_pps: None,
            filter_sps: None,
            filter_disable_deblock: false,
        })
    }

    /// Feed one packet (an access unit in HEVC) through the decoder.
    fn decode_packet(&mut self, data: &[u8], pts: i64) -> Result<()> {
        self.pts = pts;
        self.last_eos = self.eos;
        self.eos = false;
        self.slice_initialized = false;

        let nals = self.split_nals(data)?;
        // EOS/EOB flags within this packet.
        let mut eos_at_start = true;
        for nal in &nals {
            if let Ok((h, _)) = parse_nal_header(nal) {
                if matches!(h.nal_unit_type, NalUnitType::EosNut | NalUnitType::EobNut) {
                    if eos_at_start {
                        self.last_eos = true;
                    } else {
                        self.eos = true;
                    }
                } else {
                    eos_at_start = false;
                }
            }
        }

        for nal in nals {
            let (header, ebsp) = parse_nal_header(nal)?;
            let rbsp = ebsp_to_rbsp(ebsp);
            match header.nal_unit_type {
                NalUnitType::Vps | NalUnitType::Sps | NalUnitType::Pps => {
                    self.ps.decode_nal(nal)?;
                }
                t if t.is_vcl() && header.nuh_layer_id == 0 => {
                    self.decode_slice(&rbsp, header.nal_unit_type, header.nuh_temporal_id_plus1)?;
                }
                _ => {}
            }
        }
        self.finish_frame()?;
        Ok(())
    }

    fn split_nals<'a>(&self, data: &'a [u8]) -> Result<Vec<&'a [u8]>> {
        if self.nal_length_size == 0 {
            Ok(annex_b_split(data))
        } else {
            let n = self.nal_length_size;
            if n > 4 {
                return Err(Error::InvalidData);
            }
            let mut out = Vec::new();
            let mut pos = 0usize;
            while pos + n <= data.len() {
                let mut len = 0usize;
                for &b in &data[pos..pos + n] {
                    len = (len << 8) | b as usize;
                }
                pos += n;
                if len == 0 || pos + len > data.len() {
                    return Err(Error::InvalidData);
                }
                out.push(&data[pos..pos + len]);
                pos += len;
            }
            Ok(out)
        }
    }

    /// Parse and decode one VCL NAL.
    fn decode_slice(&mut self, rbsp: &[u8], nal_type: NalUnitType, tid_plus1: u8) -> Result<()> {
        // Peek the PPS id (first_slice flag, optional IRAP flag, then ue(v)).
        let mut br = BitReader::new(rbsp);
        let first_slice = br.read_flag()?;
        if nal_type.is_irap() {
            let _ = br.read_flag()?; // no_output_of_prior_pics_flag
        }
        let pps_id = br.read_ue()?;
        let pps = self
            .ps
            .pps_list
            .get(pps_id as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::InvalidData)?
            .clone();
        let sps_id = pps.sps_id;
        let sps = self
            .ps
            .sps_list
            .get(sps_id as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::InvalidData)?
            .clone();
        let header = wedeo_codec_hevc_ps::NalUnitHeader {
            forbidden_zero_bit: false,
            nal_unit_type: nal_type,
            nuh_layer_id: 0,
            nuh_temporal_id_plus1: tid_plus1,
        };
        let sh = self.ps.parse_slice_header(
            rbsp,
            pps_id,
            header,
            self.poc_tid0,
            self.last_eos,
            self.slice_initialized,
        )?;

        if first_slice {
            self.finish_frame()?;
            self.start_frame(&sh, &sps, sps_id)?;
        } else if self.cur_ref.is_none() {
            return Err(Error::InvalidData);
        }

        self.decode_slice_data(rbsp, &sh, &sps, &pps)?;
        self.slice_initialized = true;

        // 8.3.1: prevTid0Pic update.
        let temporal_id = tid_plus1.saturating_sub(1);
        if temporal_id == 0
            && !matches!(
                nal_type,
                NalUnitType::TrailN
                    | NalUnitType::TsaN
                    | NalUnitType::StsaN
                    | NalUnitType::RadlN
                    | NalUnitType::RadlR
                    | NalUnitType::RaslN
                    | NalUnitType::RaslR
            )
        {
            self.poc_tid0 = sh.poc;
        }
        Ok(())
    }

    /// Begin a new picture (FFmpeg's `hevc_frame_start`).
    fn start_frame(&mut self, sh: &SliceHeader, sps: &Sps, sps_id: u32) -> Result<()> {
        let new_sequence = self.last_sps_id != Some(sps_id) || sh.no_output_of_prior_pics_flag;
        if new_sequence {
            self.flush_outputs(sh.no_output_of_prior_pics_flag)?;
            self.dpb.clear_refs();
            self.pictures.clear();
            self.last_sps_id = Some(sps_id);
        }
        if sh.no_output_of_prior_pics_flag {
            self.flush_outputs(true)?;
        }
        if matches!(sh.pic_order_cnt_lsb, _) {
            // IDR pictures clear all references.
        }
        // IDR is detected through the NAL type handed to the slice parser; the
        // SPS-independent check uses POC == 0 with no short-term RPS.
        // We conservatively clear references when there is no RPS at all.
        if sh.short_term_rps.is_none() && sh.long_term_rps.nb_refs == 0 && sh.poc == 0 {
            self.dpb.clear_refs();
            self.pictures.clear();
        }

        let min_cb_width = sps.min_cb_width as usize;
        let min_cb_height = sps.min_cb_height as usize;
        let mut empty_mvf = MvFieldTable::new(min_cb_width, min_cb_height);
        let slot = self.dpb.set_new_ref(NewRefParams {
            poc: sh.poc,
            pic_output_flag: sh.pic_output_flag,
            is_irap: true,
            is_recovering: false,
            recovery_poc: i32::MAX,
            output_corrupt: false,
            show_all: false,
            tab_mvf: std::mem::take(&mut empty_mvf),
        })?;
        let handle = {
            let f = self.dpb.frame(slot).ok_or(Error::Bug)?;
            FrameRef::new(slot, f.id, f.poc)
        };
        let picture = Picture::new(
            sh.poc,
            sps.width as usize,
            sps.height as usize,
            sps.bit_depth as u8,
            sps.chroma_format_idc,
            min_cb_width,
            min_cb_height,
            sps.min_tb_width as usize,
            sps.min_tb_height as usize,
            sps.min_pu_width as usize,
            sps.min_pu_height as usize,
            sps.ctb_size as usize,
        );
        self.pictures.insert(handle, picture);
        // Initialise deblock parameters for every CTB from the slice header.
        if let Some(frame) = self.dpb.frame_mut(slot) {
            frame.rpl_tab = vec![0; sps.ctb_size as usize];
            frame.rpl.resize(1, [RefPicList::default(); 2]);
        }
        if let Some(pic) = self.pictures.get_mut(handle) {
            for v in pic.deblock_tc.iter_mut() {
                *v = sh.tc_offset;
            }
            for v in pic.deblock_beta.iter_mut() {
                *v = sh.beta_offset;
            }
            for v in pic.filter_slice_edges.iter_mut() {
                *v = sh.slice_loop_filter_across_slices_enabled_flag;
            }
        }
        self.cur_ref = Some(handle);

        // Apply the reference picture set (FFmpeg's ff_hevc_frame_rps).
        let params = FrameRpsParams {
            sh,
            nal_unit_type: NalUnitType::TrailR, // used only for IRAP gating
            poc: sh.poc,
            log2_max_poc_lsb: sps.log2_max_poc_lsb,
            recovery_poc: i32::MAX,
            output_corrupt: false,
            show_all: false,
            inter_layer_poc: None,
        };
        let _ = self.dpb.frame_rps(&params);

        // Output frames in POC order.
        let max_output = sps
            .temporal_layer
            .get(sps.max_sub_layers.saturating_sub(1))
            .map_or(0, |t| t.num_reorder_pics as usize);
        let max_dpb = sps
            .temporal_layer
            .get(sps.max_sub_layers.saturating_sub(1))
            .map_or(16, |t| t.max_dec_pic_buffering as usize);
        self.emit_outputs(max_output, max_dpb)?;
        Ok(())
    }

    fn flush_outputs(&mut self, discard: bool) -> Result<()> {
        let handles = self.dpb.output_frames(0, 0, discard)?;
        self.output_queue.extend(handles);
        Ok(())
    }

    fn emit_outputs(&mut self, max_output: usize, max_dpb: usize) -> Result<()> {
        let handles = self.dpb.output_frames(max_output, max_dpb, false)?;
        self.output_queue.extend(handles);
        Ok(())
    }

    /// Finish the current picture: filters plus final DPB bookkeeping.
    fn finish_frame(&mut self) -> Result<()> {
        let Some(handle) = self.cur_ref else {
            return Ok(());
        };
        self.cur_ref = None;
        if let (Some(pps), Some(sps)) = (self.filter_pps.take(), self.filter_sps.take()) {
            let disable = self.filter_disable_deblock;
            if let Some(pic) = self.pictures.get_mut(handle) {
                filter_picture(pic, &sps, &pps, disable)?;
            }
        }
        Ok(())
    }

    /// Decode the slice data (CABAC + CTU loop), FFmpeg's `decode_slice_data`.
    fn decode_slice_data(
        &mut self,
        rbsp: &[u8],
        sh: &SliceHeader,
        sps: &Sps,
        pps: &Pps,
    ) -> Result<()> {
        let handle = self.cur_ref.ok_or(Error::Bug)?;
        let start_ctb = sh.slice_ctb_addr_rs as usize;

        // Build reference lists for inter slices (FFmpeg's ff_hevc_slice_rpl).
        let mut rpl = [RefPicList::default(), RefPicList::default()];
        let mut collocated = None;
        if sh.slice_type != SliceType::I {
            let params = SliceRplParams {
                sh,
                pps_curr_pic_ref_enabled_flag: pps.pps_curr_pic_ref_enabled_flag,
                log2_ctb_size: sps.log2_ctb_size,
                ctb_width: sps.ctb_width as usize,
                ctb_count: sps.ctb_size as usize,
                ctb_addr_rs_to_ts: &pps.tile.ctb_addr_rs_to_ts,
                slice_segment_addr: sh.slice_segment_addr,
                slice_idx: 0,
            };
            let slot = handle.slot;
            let rps = self.dpb.rps;
            if let Some(frame) = self.dpb.frame_mut(slot) {
                if frame.nb_rpl_elems() == 0 {
                    frame.rpl.resize(1, [RefPicList::default(); 2]);
                }
                collocated = slice_rpl(frame, &rps, &params)?;
                rpl = frame.rpl[0];
            }
        }

        let geo = RefGeometry::from_ps(sps, pps);
        let data_offset = sh.data_offset.min(rbsp.len());
        let slice_data = &rbsp[data_offset..];
        let sub_offsets = self.wpp_offsets(sh, slice_data, sps);
        let mut cabac = Cabac::new(slice_data)?;
        cabac.init_state_from_slice(sh);

        let intra_state = IntraState::new(sps.min_pu_width as usize, sps.min_pu_height as usize);
        // Temporarily remove the current picture from the store so it can be
        // borrowed mutably while the store is borrowed for reference frames.
        let mut picture = self.pictures.remove(handle).ok_or(Error::Bug)?;
        let decode_result: Result<()> = (|| {
            let mut decoder = CtuDecoder {
                sps,
                pps,
                sh,
                geo: &geo,
                pic: &mut picture,
                intra: intra_state,
                cabac: &mut cabac,
                slice_data,
                sub_offsets: &sub_offsets,
                wpp_states: None,
                dpb: &self.dpb,
                pictures: &self.pictures,
                rpl,
                collocated,
                qp_y: sh.slice_qp,
                q_py_pred: sh.slice_qp,
                first_qp_group: !sh.dependent_slice_segment_flag,
                cu_qp_delta: 0,
                is_cu_qp_delta_coded: false,
                is_cu_chroma_qp_offset_coded: false,
                cu_qp_offset_cb: 0,
                cu_qp_offset_cr: 0,
                slice_start_ts: 0,
                ctb_flags: NeighbourCtbFlags::default(),
                ct_depth: 0,
                pu_merge_flag: false,
                intra_split: false,
                part_mode: PartMode::Part2Nx2N,
                pred_mode_intra: sh.slice_type == SliceType::I,
                cu_transquant_bypass: false,
                max_trafo_depth: 0,
            };
            decoder.slice_start_ts = pps
                .tile
                .ctb_addr_rs_to_ts
                .get(start_ctb)
                .copied()
                .unwrap_or(0)
                .max(0) as usize;
            decoder.decode_ctbs(start_ctb)?;
            Ok(())
        })();
        self.pictures.insert(handle, picture);
        decode_result?;

        // Remember filter parameters for `finish_frame`.
        self.filter_pps = Some(pps.clone());
        self.filter_sps = Some(sps.clone());
        self.filter_disable_deblock = sh.disable_deblocking_filter_flag;

        Ok(())
    }

    fn wpp_offsets(&self, sh: &SliceHeader, _slice_data: &[u8], _sps: &Sps) -> Vec<usize> {
        if sh.num_entry_point_offsets == 0 {
            return Vec::new();
        }
        // FFmpeg's sh.offset[] values are relative to the whole NAL, but the
        // caller indexes `slice_data`, which already starts at data_offset.
        // Adding data_offset here shifts every substream by that amount.
        let mut offsets = Vec::with_capacity(sh.num_entry_point_offsets + 1);
        offsets.push(0);
        let mut sum = 0usize;
        for &v in sh
            .entry_point_offset
            .iter()
            .take(sh.num_entry_point_offsets)
        {
            sum = sum.saturating_add(v as usize);
            offsets.push(sum);
        }
        offsets
    }
}

/// Per-slice CTU decoder (mirrors FFmpeg's `HEVCLocalContext`).
struct CtuDecoder<'a, 'b> {
    sps: &'a Sps,
    pps: &'a Pps,
    sh: &'a SliceHeader,
    geo: &'a RefGeometry,
    pic: &'a mut Picture,
    intra: IntraState,
    cabac: &'a mut Cabac<'b>,
    slice_data: &'b [u8],
    sub_offsets: &'a [usize],
    /// Context models published by the second CTB of the previous WPP row.
    wpp_states: Option<CabacStates>,
    dpb: &'a Dpb,
    pictures: &'a PictureStore,
    rpl: [RefPicList; 2],
    collocated: Option<FrameRef>,
    qp_y: i32,
    q_py_pred: i32,
    first_qp_group: bool,
    cu_qp_delta: i32,
    is_cu_qp_delta_coded: bool,
    is_cu_chroma_qp_offset_coded: bool,
    cu_qp_offset_cb: i32,
    cu_qp_offset_cr: i32,
    slice_start_ts: usize,
    /// Current CTB neighbour flags, set before the quadtree walk.
    ctb_flags: NeighbourCtbFlags,
    /// Current CTB's TU-level state.
    ct_depth: u32,
    pu_merge_flag: bool,
    intra_split: bool,
    part_mode: PartMode,
    pred_mode_intra: bool,
    cu_transquant_bypass: bool,
    max_trafo_depth: u32,
}

impl CtuDecoder<'_, '_> {
    /// Decode CTBs in raster order (FFmpeg's `hls_decode_entry`).
    fn decode_ctbs(&mut self, start_ctb_rs: usize) -> Result<()> {
        let ctb_count = self.sps.ctb_size as usize;
        let ctb_width = self.sps.ctb_width as usize;
        let mut ts = self
            .pps
            .tile
            .ctb_addr_rs_to_ts
            .get(start_ctb_rs)
            .copied()
            .unwrap_or(0)
            .max(0) as usize;
        while ts < ctb_count {
            let rs = self
                .pps
                .tile
                .ctb_addr_ts_to_rs
                .get(ts)
                .copied()
                .unwrap_or(ts as i32)
                .max(0) as usize;
            let x = (rs % ctb_width) as i32 * (1 << self.sps.log2_ctb_size);
            let y = (rs / ctb_width) as i32 * (1 << self.sps.log2_ctb_size);

            if !self.sub_offsets.is_empty()
                && ts.is_multiple_of(ctb_width)
                && ts != self.slice_start_ts
            {
                let row = ts / ctb_width;
                if let Some(&off) = self.sub_offsets.get(row) {
                    // FFmpeg ends the previous row with a terminate bin, then
                    // opens this row's substream and inherits the context
                    // models saved after the previous row's second CTB
                    // (`ff_hevc_cabac_init` -> substream_continue + load_states).
                    let _ = self.cabac.get_cabac_terminate();
                    let data = &self.slice_data[off.min(self.slice_data.len())..];
                    // With a single CTB per row FFmpeg re-initialises the
                    // contexts instead of loading the neighbour row's states.
                    let restored = if ctb_width == 1 {
                        None
                    } else {
                        self.wpp_states.take()
                    };
                    let next = match restored {
                        Some((states, stat_coeff)) => {
                            Cabac::new_with_states(data, states, stat_coeff)?
                        }
                        None => {
                            let mut c = Cabac::new(data)?;
                            c.init_state_from_slice(self.sh);
                            c
                        }
                    };
                    *self.cabac = next;
                    self.first_qp_group = true;
                }
            }

            let more = self.decode_ctb(rs, x, y, ts)?;
            ts += 1;
            // FFmpeg's ff_hevc_save_states(): publish the contexts after the
            // second CTB of a row (or every CTB when the row is two wide) so
            // the next WPP row can inherit them.
            if ts % ctb_width == 2 || (ctb_width == 2 && ts.is_multiple_of(ctb_width)) {
                self.wpp_states = Some(self.cabac.save_states());
            }
            // FFmpeg's decode_slice_data stops at the end-of-slice flag. With
            // WPP substreams the flag also fires at row ends; in that case the
            // next row is decoded from the next substream by the loop above.
            if !more
                && !(self.sub_offsets.len() > 1 && ts.is_multiple_of(ctb_width) && ts < ctb_count)
            {
                break;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_ctb(&mut self, rs: usize, x_ctb: i32, y_ctb: i32, ctb_ts: usize) -> Result<bool> {
        let sps = self.sps;
        let ctb_size = 1i32 << sps.log2_ctb_size;
        let ctb_addr_in_slice = rs as i32 - self.sh.slice_addr as i32;
        let ctb_up_flag = y_ctb > 0 && ctb_addr_in_slice >= sps.ctb_width as i32;
        let ctb_left_flag = x_ctb > 0 && ctb_addr_in_slice > 0;
        let ctb_up_right_flag = y_ctb > 0
            && ctb_addr_in_slice + 1 >= sps.ctb_width as i32
            && rs + 1 < self.pic.tab_slice_address.len();
        let ctb_up_left_flag = x_ctb > 0 && y_ctb > 0 && ctb_addr_in_slice > sps.ctb_width as i32;
        self.pic.tab_slice_address[rs] = self.sh.slice_addr as i32;
        let flags = NeighbourCtbFlags {
            ctb_left_flag,
            ctb_up_flag,
            ctb_up_left_flag,
            ctb_up_right_flag,
            end_of_tiles_x: sps.width as i32,
            end_of_tiles_y: (y_ctb + ctb_size).min(sps.height as i32),
        };
        self.ctb_flags = flags;
        self.ct_depth = 0;
        let _ = ctb_ts;

        // SAO parameters (CABAC-coded, must be parsed here).
        self.hls_sao_param(rs, ctb_left_flag, ctb_up_flag)?;
        if let Some(v) = self.pic.deblock_tc.get_mut(rs) {
            *v = self.sh.tc_offset;
        }
        if let Some(v) = self.pic.deblock_beta.get_mut(rs) {
            *v = self.sh.beta_offset;
        }
        if let Some(v) = self.pic.filter_slice_edges.get_mut(rs) {
            *v = self.sh.slice_loop_filter_across_slices_enabled_flag;
        }

        let more = self.hls_coding_quadtree(x_ctb, y_ctb, sps.log2_ctb_size, 0, flags)?;
        Ok(more)
    }

    /// `hls_sao_param()` from hevcdec.c.
    fn hls_sao_param(&mut self, rs: usize, left: bool, up: bool) -> Result<()> {
        let rx = rs % self.sps.ctb_width as usize;
        let ry = rs / self.sps.ctb_width as usize;
        let mut merge_left = false;
        let mut merge_up = false;
        if self.sh.slice_sample_adaptive_offset_flag[0]
            || self.sh.slice_sample_adaptive_offset_flag[1]
        {
            if rx > 0 && left {
                merge_left = self.cabac.decode_sao_merge_flag() != 0;
            }
            if ry > 0 && !merge_left && up {
                merge_up = self.cabac.decode_sao_merge_flag() != 0;
            }
        }
        // FFmpeg keeps one `SAOParams` object per CTB (`SAOParams *sao =
        // &CTB(l->sao, rx, ry)`) and fills it component by component. Each
        // component must therefore see the values decoded for earlier
        // components: in particular `sao_type_idx[2]` is inferred from
        // `sao_type_idx[1]`, and `sao_eo_class[2]` from `sao_eo_class[1]`.
        // Re-creating the struct for every component (or publishing only the
        // last component) drops the chroma CB/CR state, skips the c_idx=2
        // residual bins and desynchronises the CABAC stream at the next CU.
        let mut sao = SaoParams::default();
        if merge_left && rx > 0 {
            sao = self.pic.sao[rs - 1].clone();
        } else if merge_up && ry > 0 {
            sao = self.pic.sao[rs - self.sps.ctb_width as usize].clone();
        }
        for c_idx in 0..if self.sps.chroma_format_idc != 0 {
            3
        } else {
            1
        } {
            if !self.sh.slice_sample_adaptive_offset_flag[c_idx] {
                sao.type_idx[c_idx] = SaoType::NotApplied;
                continue;
            }
            if c_idx == 2 {
                sao.type_idx[2] = sao.type_idx[1];
                sao.eo_class[2] = sao.eo_class[1];
            } else if !merge_left && !merge_up {
                sao.type_idx[c_idx] = match self.cabac.decode_sao_type_idx() {
                    wedeo_codec_hevc_cabac::SaoType::NotApplied => SaoType::NotApplied,
                    wedeo_codec_hevc_cabac::SaoType::Band => SaoType::Band,
                    wedeo_codec_hevc_cabac::SaoType::Edge => SaoType::Edge,
                };
            }
            if sao.type_idx[c_idx] == SaoType::NotApplied {
                continue;
            }
            if !merge_left && !merge_up {
                for i in 0..4 {
                    sao.offset_abs[c_idx][i] = self.cabac.decode_sao_offset_abs(self.sps.bit_depth);
                }
                if sao.type_idx[c_idx] == SaoType::Band {
                    for i in 0..4 {
                        sao.offset_sign[c_idx][i] = if sao.offset_abs[c_idx][i] != 0 {
                            self.cabac.decode_sao_offset_sign() as i32
                        } else {
                            0
                        };
                    }
                    sao.band_position[c_idx] = self.cabac.decode_sao_band_position() as u8;
                } else if c_idx != 2 {
                    sao.eo_class[c_idx] = self.cabac.decode_sao_eo_class();
                }
            }
            // Inferred offset values.
            let log2_scale = if c_idx == 0 {
                self.pps.log2_sao_offset_scale_luma
            } else {
                self.pps.log2_sao_offset_scale_chroma
            } as u8;
            sao.offset_val[c_idx][0] = 0;
            for i in 0..4 {
                let mut v = sao.offset_abs[c_idx][i];
                if sao.type_idx[c_idx] == SaoType::Edge {
                    if i > 1 {
                        v = -v;
                    }
                } else if sao.offset_sign[c_idx][i] != 0 {
                    v = -v;
                }
                sao.offset_val[c_idx][i + 1] = (v << log2_scale) as i16;
            }
        }
        self.pic.sao[rs] = sao;
        Ok(())
    }

    /// `hls_coding_quadtree()` from hevcdec.c.
    fn hls_coding_quadtree(
        &mut self,
        x0: i32,
        y0: i32,
        log2_cb_size: u32,
        cb_depth: u32,
        flags: NeighbourCtbFlags,
    ) -> Result<bool> {
        let cb_size = 1i32 << log2_cb_size;
        self.ct_depth = cb_depth;
        let split = if x0 + cb_size <= self.sps.width as i32
            && y0 + cb_size <= self.sps.height as i32
            && log2_cb_size > self.sps.log2_min_cb_size
        {
            // FFmpeg gates the tab_ct_depth lookups on CTB/boundary flags;
            // a slice boundary must not expose the previous slice's depths.
            let x0b = (x0 as u32) & ((1u32 << self.sps.log2_ctb_size) - 1);
            let y0b = (y0 as u32) & ((1u32 << self.sps.log2_ctb_size) - 1);
            let left_depth = if flags.ctb_left_flag || x0b != 0 {
                self.ct_depth_at(x0 - 1, y0)
            } else {
                0
            };
            let above_depth = if flags.ctb_up_flag || y0b != 0 {
                self.ct_depth_at(x0, y0 - 1)
            } else {
                0
            };
            self.cabac.decode_split_coding_unit_flag(
                left_depth > cb_depth as usize,
                above_depth > cb_depth as usize,
            ) != 0
        } else {
            log2_cb_size > self.sps.log2_min_cb_size
        };

        if self.pps.cu_qp_delta_enabled_flag
            && log2_cb_size >= self.sps.log2_ctb_size - self.pps.diff_cu_qp_delta_depth
        {
            self.is_cu_qp_delta_coded = false;
            self.cu_qp_delta = 0;
        }
        if self.sh.cu_chroma_qp_offset_enabled_flag
            && log2_cb_size >= self.sps.log2_ctb_size - self.pps.diff_cu_chroma_qp_offset_depth
        {
            self.is_cu_chroma_qp_offset_coded = false;
        }

        if split {
            let half = cb_size >> 1;
            let mut more = true;
            for (x, y) in [
                (x0, y0),
                (x0 + half, y0),
                (x0, y0 + half),
                (x0 + half, y0 + half),
            ] {
                if x >= self.sps.width as i32 || y >= self.sps.height as i32 {
                    continue;
                }
                more = self.hls_coding_quadtree(x, y, log2_cb_size - 1, cb_depth + 1, flags)?;
                if !more {
                    break;
                }
            }
            Ok(more)
        } else {
            self.hls_coding_unit(x0, y0, log2_cb_size, flags)?;
            let end = ((x0 + cb_size) % (1 << self.sps.log2_ctb_size) == 0
                || x0 + cb_size >= self.sps.width as i32)
                && ((y0 + cb_size) % (1 << self.sps.log2_ctb_size) == 0
                    || y0 + cb_size >= self.sps.height as i32);
            if end {
                Ok(!self.cabac.decode_end_of_slice_flag())
            } else {
                Ok(true)
            }
        }
    }

    fn ct_depth_at(&self, x: i32, y: i32) -> usize {
        if x < 0 || y < 0 {
            return 0;
        }
        let x_cb = (x as u32 >> self.sps.log2_min_cb_size) as usize;
        let y_cb = (y as u32 >> self.sps.log2_min_cb_size) as usize;
        self.pic
            .tab_ct_depth
            .get(y_cb * self.pic.min_cb_width + x_cb)
            .copied()
            .unwrap_or(0) as usize
    }

    fn set_ct_depth(&mut self, x0: i32, y0: i32, log2_cb_size: u32, depth: u32) {
        let length = (1i32 << log2_cb_size) >> self.sps.log2_min_cb_size;
        let x_cb = (x0 as u32 >> self.sps.log2_min_cb_size) as usize;
        let y_cb = (y0 as u32 >> self.sps.log2_min_cb_size) as usize;
        for y in 0..length as usize {
            for x in 0..length as usize {
                if let Some(v) = self
                    .pic
                    .tab_ct_depth
                    .get_mut((y_cb + y) * self.pic.min_cb_width + x_cb + x)
                {
                    *v = depth as u8;
                }
            }
        }
    }

    fn set_qp_tab(&mut self, x0: i32, y0: i32, log2_cb_size: u32) {
        let length = (1i32 << log2_cb_size) >> self.sps.log2_min_cb_size;
        let x_cb = (x0 as u32 >> self.sps.log2_min_cb_size) as usize;
        let y_cb = (y0 as u32 >> self.sps.log2_min_cb_size) as usize;
        for y in 0..length as usize {
            for x in 0..length as usize {
                if let Some(v) = self
                    .pic
                    .qp_y_tab
                    .get_mut((y_cb + y) * self.pic.min_cb_width + x_cb + x)
                {
                    *v = self.qp_y;
                }
            }
        }
    }

    /// `hls_coding_unit()` from hevcdec.c.
    fn hls_coding_unit(
        &mut self,
        x0: i32,
        y0: i32,
        log2_cb_size: u32,
        flags: NeighbourCtbFlags,
    ) -> Result<()> {
        let cb_size = 1i32 << log2_cb_size;
        let log2_min_cb = self.sps.log2_min_cb_size;
        let length = cb_size >> log2_min_cb;
        let x_cb = (x0 as u32 >> log2_min_cb) as usize;
        let y_cb = (y0 as u32 >> log2_min_cb) as usize;
        let min_cb_width = self.pic.min_cb_width;
        let x0b = x0 & ((1 << self.sps.log2_ctb_size) - 1);
        let y0b = y0 & ((1 << self.sps.log2_ctb_size) - 1);
        // FFmpeg's hls_coding_unit uses `lc->ct_depth`, set by the quadtree
        // caller, not the neighbour table (which is still zero at this point).
        let ct_depth = self.ct_depth;

        self.clear_skip(x_cb, y_cb, length as usize);
        self.intra.modes = [1; 4];
        self.intra.modes_c = [1; 4];

        let mut cu_transquant_bypass = false;
        if self.pps.transquant_bypass_enable_flag {
            cu_transquant_bypass = self.cabac.decode_cu_transquant_bypass_flag() != 0;
            if cu_transquant_bypass {
                self.set_pcm(x0, y0, log2_cb_size);
            }
        }

        let mut pred_mode_intra = self.sh.slice_type == SliceType::I;
        let mut skip = false;
        if self.sh.slice_type != SliceType::I {
            let left = (flags.ctb_left_flag || x0b != 0)
                && self.skip_flag_at(x_cb.saturating_sub(1), y_cb);
            let above =
                (flags.ctb_up_flag || y0b != 0) && self.skip_flag_at(x_cb, y_cb.saturating_sub(1));
            skip = self.cabac.decode_skip_flag(left, above) != 0;
            self.fill_skip(x_cb, y_cb, length as usize, u8::from(skip));
            if !skip {
                pred_mode_intra = self.cabac.decode_pred_mode_flag() != 0;
            }
        } else {
            self.fill_skip(x_cb, y_cb, length as usize, 0);
        }

        if skip {
            self.prediction_unit(x0, y0, cb_size, cb_size, log2_cb_size, 0, flags, true)?;
            intra_defaults(&mut self.intra, self.sps, x0, y0, log2_cb_size, self.pic);
            if !self.sh.disable_deblocking_filter_flag {
                boundary_strengths(self.pic, self.sps, x0, y0, log2_cb_size, true);
            }
            self.set_qp_tab(x0, y0, log2_cb_size);
            self.set_ct_depth(x0, y0, log2_cb_size, ct_depth);
            return Ok(());
        }

        let mut part_mode = PartMode::Part2Nx2N;
        if !pred_mode_intra || log2_cb_size == log2_min_cb {
            part_mode = self.cabac.decode_part_mode(
                log2_cb_size,
                log2_min_cb,
                pred_mode_intra,
                self.sps.amp_enabled,
            );
        }
        let intra_split = part_mode == PartMode::PartNxN && pred_mode_intra;

        let mut pcm_flag = false;
        if pred_mode_intra {
            if part_mode == PartMode::Part2Nx2N
                && self.sps.pcm_enabled
                && log2_cb_size >= self.sps.pcm.log2_min_pcm_cb_size
                && log2_cb_size <= self.sps.pcm.log2_max_pcm_cb_size
            {
                pcm_flag = self.cabac.decode_pcm_flag() != 0;
            }
            if pcm_flag {
                self.decode_pcm(x0, y0, log2_cb_size)?;
                intra_defaults(&mut self.intra, self.sps, x0, y0, log2_cb_size, self.pic);
            } else {
                intra_prediction_unit(
                    self.cabac,
                    self.sps,
                    self.pps,
                    &mut self.intra,
                    x0,
                    y0,
                    log2_cb_size,
                    intra_split,
                    flags.ctb_up_flag,
                    flags.ctb_left_flag,
                )?;
            }
        } else {
            intra_defaults(&mut self.intra, self.sps, x0, y0, log2_cb_size, self.pic);
            let half = cb_size >> 1;
            let quarter = cb_size >> 2;
            match part_mode {
                PartMode::Part2Nx2N => {
                    self.prediction_unit(x0, y0, cb_size, cb_size, log2_cb_size, 0, flags, false)?;
                }
                PartMode::Part2NxN => {
                    self.prediction_unit(x0, y0, cb_size, half, log2_cb_size, 0, flags, false)?;
                    self.prediction_unit(
                        x0,
                        y0 + half,
                        cb_size,
                        half,
                        log2_cb_size,
                        1,
                        flags,
                        false,
                    )?;
                }
                PartMode::PartNx2N => {
                    self.prediction_unit(x0, y0, half, cb_size, log2_cb_size, 0, flags, false)?;
                    self.prediction_unit(
                        x0 + half,
                        y0,
                        half,
                        cb_size,
                        log2_cb_size,
                        1,
                        flags,
                        false,
                    )?;
                }
                PartMode::Part2NxnU => {
                    self.prediction_unit(x0, y0, cb_size, quarter, log2_cb_size, 0, flags, false)?;
                    self.prediction_unit(
                        x0,
                        y0 + quarter,
                        cb_size,
                        cb_size - quarter,
                        log2_cb_size,
                        1,
                        flags,
                        false,
                    )?;
                }
                PartMode::Part2NxnD => {
                    self.prediction_unit(
                        x0,
                        y0,
                        cb_size,
                        cb_size - quarter,
                        log2_cb_size,
                        0,
                        flags,
                        false,
                    )?;
                    self.prediction_unit(
                        x0,
                        y0 + cb_size - quarter,
                        cb_size,
                        quarter,
                        log2_cb_size,
                        1,
                        flags,
                        false,
                    )?;
                }
                PartMode::PartNlx2N => {
                    self.prediction_unit(x0, y0, quarter, cb_size, log2_cb_size, 0, flags, false)?;
                    self.prediction_unit(
                        x0 + quarter,
                        y0,
                        cb_size - quarter,
                        cb_size,
                        log2_cb_size,
                        1,
                        flags,
                        false,
                    )?;
                }
                PartMode::PartNrx2N => {
                    self.prediction_unit(
                        x0,
                        y0,
                        cb_size - quarter,
                        cb_size,
                        log2_cb_size,
                        0,
                        flags,
                        false,
                    )?;
                    self.prediction_unit(
                        x0 + cb_size - quarter,
                        y0,
                        quarter,
                        cb_size,
                        log2_cb_size,
                        1,
                        flags,
                        false,
                    )?;
                }
                PartMode::PartNxN => {
                    for (px, py, idx) in [
                        (x0, y0, 0),
                        (x0 + half, y0, 1),
                        (x0, y0 + half, 2),
                        (x0 + half, y0 + half, 3),
                    ] {
                        self.prediction_unit(px, py, half, half, log2_cb_size, idx, flags, false)?;
                    }
                }
            }
        }

        if !pcm_flag {
            let mut rqt_root_cbf = true;
            if !pred_mode_intra {
                let merged_2n2n = part_mode == PartMode::Part2Nx2N && self.pu_merge_flag;
                if !merged_2n2n {
                    rqt_root_cbf = self.cabac.decode_no_residual_syntax_flag() != 0;
                }
            }
            self.intra_split = intra_split;
            self.part_mode = part_mode;
            self.pred_mode_intra = pred_mode_intra;
            self.cu_transquant_bypass = cu_transquant_bypass;
            self.max_trafo_depth = if pred_mode_intra {
                self.sps.max_transform_hierarchy_depth_intra + u32::from(intra_split)
            } else {
                self.sps.max_transform_hierarchy_depth_inter
            };
            if rqt_root_cbf {
                let cbf = [0i32; 2];
                self.hls_transform_tree(
                    x0,
                    y0,
                    x0,
                    y0,
                    x0,
                    y0,
                    log2_cb_size,
                    log2_cb_size,
                    0,
                    0,
                    &cbf,
                    &cbf,
                )?;
            } else if !self.sh.disable_deblocking_filter_flag {
                boundary_strengths(self.pic, self.sps, x0, y0, log2_cb_size, true);
            }
        }
        let _ = min_cb_width;
        if self.pps.cu_qp_delta_enabled_flag && !self.is_cu_qp_delta_coded {
            self.set_qp(x0, y0, log2_cb_size);
        }
        self.set_qp_tab(x0, y0, log2_cb_size);
        self.set_ct_depth(x0, y0, log2_cb_size, ct_depth);
        Ok(())
    }

    fn skip_flag_at(&self, x_cb: usize, y_cb: usize) -> bool {
        self.pic
            .skip_flag
            .get(y_cb * self.pic.min_cb_width + x_cb)
            .copied()
            .unwrap_or(0)
            != 0
    }
    fn clear_skip(&mut self, x_cb: usize, y_cb: usize, len: usize) {
        for y in 0..len {
            for x in 0..len {
                if let Some(v) = self
                    .pic
                    .skip_flag
                    .get_mut((y_cb + y) * self.pic.min_cb_width + x_cb + x)
                {
                    *v = 0;
                }
            }
        }
    }
    fn fill_skip(&mut self, x_cb: usize, y_cb: usize, len: usize, value: u8) {
        for y in 0..len {
            for x in 0..len {
                if let Some(v) = self
                    .pic
                    .skip_flag
                    .get_mut((y_cb + y) * self.pic.min_cb_width + x_cb + x)
                {
                    *v = value;
                }
            }
        }
    }

    /// Set `qp_y` from QP prediction plus a coded delta (FFmpeg's set_qPy).
    fn set_qp(&mut self, x_base: i32, y_base: i32, log2_cb_size: u32) {
        let ctb_mask = (1 << self.sps.log2_ctb_size) - 1;
        let min_delta_mask = (1 << (self.sps.log2_ctb_size - self.pps.diff_cu_qp_delta_depth)) - 1;
        let x_qg = x_base - (x_base & min_delta_mask);
        let y_qg = y_base - (y_base & min_delta_mask);
        let x_cb = (x_qg as u32 >> self.sps.log2_min_cb_size) as usize;
        let y_cb = (y_qg as u32 >> self.sps.log2_min_cb_size) as usize;
        let available_a = (x_base & ctb_mask) != 0 && (x_qg & ctb_mask) != 0;
        let available_b = (y_base & ctb_mask) != 0 && (y_qg & ctb_mask) != 0;
        let qp_pred = if self.first_qp_group || (x_qg == 0 && y_qg == 0) {
            self.first_qp_group = !self.is_cu_qp_delta_coded;
            self.sh.slice_qp
        } else {
            self.q_py_pred
        };
        let qp_a = if !available_a {
            qp_pred
        } else {
            self.pic
                .qp_y_tab
                .get(y_cb * self.pic.min_cb_width + x_cb.saturating_sub(1))
                .copied()
                .unwrap_or(qp_pred)
        };
        let qp_b = if !available_b {
            qp_pred
        } else {
            self.pic
                .qp_y_tab
                .get(y_cb.saturating_sub(1) * self.pic.min_cb_width + x_cb)
                .copied()
                .unwrap_or(qp_pred)
        };
        let pred = (qp_a + qp_b + 1) >> 1;
        let off = self.sps.qp_bd_offset as i32;
        self.qp_y = if self.cu_qp_delta != 0 {
            (pred + self.cu_qp_delta + 52 + 2 * off).rem_euclid(52 + off) - off
        } else {
            pred
        };
        self.q_py_pred = self.qp_y;
        let _ = log2_cb_size;
    }

    /// `hls_transform_tree()` from hevcdec.c.
    #[allow(clippy::too_many_arguments)]
    fn hls_transform_tree(
        &mut self,
        x0: i32,
        y0: i32,
        x_base: i32,
        y_base: i32,
        cb_x_base: i32,
        cb_y_base: i32,
        log2_cb_size: u32,
        log2_trafo_size: u32,
        trafo_depth: u32,
        blk_idx: u32,
        base_cbf_cb: &[i32; 2],
        base_cbf_cr: &[i32; 2],
    ) -> Result<()> {
        let mut cbf_cb = *base_cbf_cb;
        let mut cbf_cr = *base_cbf_cr;
        if self.intra_split && trafo_depth == 1 {
            let i = (blk_idx & 3) as usize;
            self.intra.mode = self.intra.modes[i];
            self.intra.mode_c = if self.sps.chroma_format_idc == 3 {
                self.intra.modes_c[i]
            } else {
                self.intra.modes_c[0]
            };
        } else if !self.intra_split {
            self.intra.mode = self.intra.modes[0];
            self.intra.mode_c = self.intra.modes_c[0];
        }

        let split = if log2_trafo_size <= self.sps.log2_max_trafo_size
            && log2_trafo_size > self.sps.log2_min_tb_size
            && trafo_depth < self.max_trafo_depth
            && !(self.intra_split && trafo_depth == 0)
        {
            self.cabac.decode_split_transform_flag(log2_trafo_size) != 0
        } else {
            let inter_split = self.sps.max_transform_hierarchy_depth_inter == 0
                && !self.pred_mode_intra
                && self.part_mode != PartMode::Part2Nx2N
                && trafo_depth == 0;
            log2_trafo_size > self.sps.log2_max_trafo_size
                || (self.intra_split && trafo_depth == 0)
                || inter_split
        };

        if self.sps.chroma_format_idc != 0
            && (log2_trafo_size > 2 || self.sps.chroma_format_idc == 3)
        {
            if trafo_depth == 0 || cbf_cb[0] != 0 {
                cbf_cb[0] = self.cabac.decode_cbf_cb_cr(trafo_depth as usize) as i32;
                if self.sps.chroma_format_idc == 2 && (!split || log2_trafo_size == 3) {
                    cbf_cb[1] = self.cabac.decode_cbf_cb_cr(trafo_depth as usize) as i32;
                }
            }
            if trafo_depth == 0 || cbf_cr[0] != 0 {
                cbf_cr[0] = self.cabac.decode_cbf_cb_cr(trafo_depth as usize) as i32;
                if self.sps.chroma_format_idc == 2 && (!split || log2_trafo_size == 3) {
                    cbf_cr[1] = self.cabac.decode_cbf_cb_cr(trafo_depth as usize) as i32;
                }
            }
        }

        if split {
            let half = 1i32 << (log2_trafo_size - 1);
            for (x, y, idx) in [
                (x0, y0, 0),
                (x0 + half, y0, 1),
                (x0, y0 + half, 2),
                (x0 + half, y0 + half, 3),
            ] {
                self.hls_transform_tree(
                    x,
                    y,
                    x0,
                    y0,
                    cb_x_base,
                    cb_y_base,
                    log2_cb_size,
                    log2_trafo_size - 1,
                    trafo_depth + 1,
                    idx,
                    &cbf_cb,
                    &cbf_cr,
                )?;
            }
            return Ok(());
        }

        let min_tu = 1i32 << self.sps.log2_min_tb_size;
        let mut cbf_luma = 1i32;
        if self.pred_mode_intra
            || trafo_depth != 0
            || cbf_cb[0] != 0
            || cbf_cr[0] != 0
            || (self.sps.chroma_format_idc == 2 && (cbf_cb[1] != 0 || cbf_cr[1] != 0))
        {
            cbf_luma = self.cabac.decode_cbf_luma(trafo_depth as usize) as i32;
        }
        self.hls_transform_unit(
            x0,
            y0,
            x_base,
            y_base,
            cb_x_base,
            cb_y_base,
            log2_cb_size,
            log2_trafo_size,
            blk_idx,
            cbf_luma,
            &cbf_cb,
            &cbf_cr,
        )?;

        if cbf_luma != 0 {
            for y in (0..(1i32 << log2_trafo_size)).step_by(min_tu as usize) {
                for x in (0..(1i32 << log2_trafo_size)).step_by(min_tu as usize) {
                    let tx = ((x0 + x) as u32 >> self.sps.log2_min_tb_size) as usize;
                    let ty = ((y0 + y) as u32 >> self.sps.log2_min_tb_size) as usize;
                    if let Some(v) = self.pic.cbf_luma.get_mut(ty * self.pic.min_tb_width + tx) {
                        *v = 1;
                    }
                }
            }
        }
        if !self.sh.disable_deblocking_filter_flag {
            boundary_strengths(self.pic, self.sps, x0, y0, log2_trafo_size, true);
        }
        Ok(())
    }

    /// `hls_transform_unit()` from hevcdec.c.
    #[allow(clippy::too_many_arguments)]
    fn hls_transform_unit(
        &mut self,
        x0: i32,
        y0: i32,
        x_base: i32,
        y_base: i32,
        cb_x_base: i32,
        cb_y_base: i32,
        log2_cb_size: u32,
        log2_trafo_size: u32,
        blk_idx: u32,
        cbf_luma: i32,
        cbf_cb: &[i32; 2],
        cbf_cr: &[i32; 2],
    ) -> Result<()> {
        let size = 1i32 << log2_trafo_size;
        let log2_c = log2_trafo_size.saturating_sub(self.sps.hshift[1]);
        let size_c = 1i32 << log2_c;
        let mut scan_idx = ScanType::Diag;
        let mut scan_idx_c = ScanType::Diag;

        if self.pred_mode_intra {
            let na = wedeo_codec_hevc_refs::set_neighbour_available(
                x0,
                y0,
                size,
                size,
                self.sps.log2_ctb_size,
                self.ctb_flags,
            );
            intra_predict_tu(
                self.pic,
                self.sps,
                self.pps,
                self.geo,
                na,
                x0,
                y0,
                log2_trafo_size,
                0,
                self.intra.mode,
            )?;
        }

        let any_cbf = cbf_luma != 0
            || cbf_cb[0] != 0
            || cbf_cr[0] != 0
            || (self.sps.chroma_format_idc == 2 && (cbf_cb[1] != 0 || cbf_cr[1] != 0));
        if any_cbf {
            if self.pps.cu_qp_delta_enabled_flag && !self.is_cu_qp_delta_coded {
                let abs = self.cabac.decode_cu_qp_delta_abs()?;
                let sign = if abs != 0 {
                    self.cabac.decode_cu_qp_delta_sign_flag()
                } else {
                    0
                };
                self.cu_qp_delta = if sign != 0 { -abs } else { abs };
                self.is_cu_qp_delta_coded = true;
                self.set_qp(cb_x_base, cb_y_base, log2_cb_size);
            }
            if self.sh.cu_chroma_qp_offset_enabled_flag
                && (cbf_cb[0] != 0 || cbf_cr[0] != 0)
                && !self.cu_transquant_bypass
                && !self.is_cu_chroma_qp_offset_coded
            {
                let flag = self.cabac.decode_cu_chroma_qp_offset_flag();
                let mut idx = 0i32;
                if flag != 0 && self.pps.chroma_qp_offset_list_len_minus1 > 0 {
                    idx = self.cabac.decode_cu_chroma_qp_offset_idx(
                        self.pps.chroma_qp_offset_list_len_minus1 as i32,
                    );
                }
                if flag != 0 {
                    self.cu_qp_offset_cb = self
                        .pps
                        .cb_qp_offset_list
                        .get(idx as usize)
                        .copied()
                        .unwrap_or(0);
                    self.cu_qp_offset_cr = self
                        .pps
                        .cr_qp_offset_list
                        .get(idx as usize)
                        .copied()
                        .unwrap_or(0);
                } else {
                    self.cu_qp_offset_cb = 0;
                    self.cu_qp_offset_cr = 0;
                }
                self.is_cu_chroma_qp_offset_coded = true;
            }
            if self.pred_mode_intra && log2_trafo_size < 4 {
                if (6..=14).contains(&self.intra.mode) {
                    scan_idx = ScanType::Vert;
                } else if (22..=30).contains(&self.intra.mode) {
                    scan_idx = ScanType::Horiz;
                }
                if (6..=14).contains(&self.intra.mode_c) {
                    scan_idx_c = ScanType::Vert;
                } else if (22..=30).contains(&self.intra.mode_c) {
                    scan_idx_c = ScanType::Horiz;
                }
            }

            let mut coeffs = vec![0i32; (1usize << (2 * log2_trafo_size.max(2))).max(1024)];
            let ctx = crate::residual::ResidualCtx {
                sps: self.sps,
                pps: self.pps,
                slice_cb_qp_offset: self.sh.slice_cb_qp_offset,
                slice_cr_qp_offset: self.sh.slice_cr_qp_offset,
                cu_qp_offset_cb: self.cu_qp_offset_cb,
                cu_qp_offset_cr: self.cu_qp_offset_cr,
                cu_transquant_bypass: self.cu_transquant_bypass,
                qp_y: self.qp_y,
                is_intra: self.pred_mode_intra,
                intra_pred_mode: self.intra.mode,
                intra_pred_mode_c: self.intra.mode_c,
            };
            if cbf_luma != 0 {
                crate::residual::decode_residual(
                    self.cabac,
                    &ctx,
                    self.pic,
                    x0 as usize,
                    y0 as usize,
                    log2_trafo_size,
                    scan_idx,
                    0,
                    &mut coeffs,
                )?;
            }
            if self.sps.chroma_format_idc != 0
                && (log2_trafo_size > 2 || self.sps.chroma_format_idc == 3)
            {
                for i in 0..usize::from(self.sps.chroma_format_idc == 2) + 1 {
                    let cy = y0 + (i as i32) * size_c;
                    if self.pred_mode_intra {
                        self.predict_chroma_tu(
                            x0,
                            cy,
                            size << self.sps.hshift[1],
                            size_c << self.sps.vshift[1],
                            log2_c,
                            1,
                        )?;
                    }
                    if cbf_cb[i] != 0 {
                        crate::residual::decode_residual(
                            self.cabac,
                            &ctx,
                            self.pic,
                            x0 as usize,
                            cy as usize,
                            log2_c,
                            scan_idx_c,
                            1,
                            &mut coeffs,
                        )?;
                    }
                }
                for i in 0..usize::from(self.sps.chroma_format_idc == 2) + 1 {
                    let cy = y0 + (i as i32) * size_c;
                    if self.pred_mode_intra {
                        self.predict_chroma_tu(
                            x0,
                            cy,
                            size << self.sps.hshift[1],
                            size_c << self.sps.vshift[1],
                            log2_c,
                            2,
                        )?;
                    }
                    if cbf_cr[i] != 0 {
                        crate::residual::decode_residual(
                            self.cabac,
                            &ctx,
                            self.pic,
                            x0 as usize,
                            cy as usize,
                            log2_c,
                            scan_idx_c,
                            2,
                            &mut coeffs,
                        )?;
                    }
                }
            } else if self.sps.chroma_format_idc != 0 && blk_idx == 3 {
                for comp in 1..=2 {
                    for i in 0..usize::from(self.sps.chroma_format_idc == 2) + 1 {
                        let cy = y_base + (i as i32) * size;
                        if self.pred_mode_intra {
                            self.predict_chroma_tu(
                                x_base,
                                cy,
                                size << 1,
                                size << self.sps.vshift[1],
                                log2_trafo_size,
                                comp,
                            )?;
                        }
                        let cbf = if comp == 1 { cbf_cb[i] } else { cbf_cr[i] };
                        if cbf != 0 {
                            crate::residual::decode_residual(
                                self.cabac,
                                &ctx,
                                self.pic,
                                x_base as usize,
                                cy as usize,
                                log2_trafo_size,
                                scan_idx_c,
                                comp,
                                &mut coeffs,
                            )?;
                        }
                    }
                }
            }
        } else if self.sps.chroma_format_idc != 0 && self.pred_mode_intra {
            // FFmpeg's hls_transform_unit() still predicts both chroma planes
            // when every CBF of the TU is zero (hevcdec.c:1506-1533); no
            // residual coding, QP delta or scan selection happens in this path.
            if log2_trafo_size > 2 || self.sps.chroma_format_idc == 3 {
                for i in 0..usize::from(self.sps.chroma_format_idc == 2) + 1 {
                    let cy = y0 + (i as i32) * size_c;
                    self.predict_chroma_tu(
                        x0,
                        cy,
                        size << self.sps.hshift[1],
                        size_c << self.sps.vshift[1],
                        log2_c,
                        1,
                    )?;
                    self.predict_chroma_tu(
                        x0,
                        cy,
                        size << self.sps.hshift[1],
                        size_c << self.sps.vshift[1],
                        log2_c,
                        2,
                    )?;
                }
            } else if blk_idx == 3 {
                for i in 0..usize::from(self.sps.chroma_format_idc == 2) + 1 {
                    let cy = y_base + (i as i32) * size;
                    self.predict_chroma_tu(
                        x_base,
                        cy,
                        size << 1,
                        size << self.sps.vshift[1],
                        log2_trafo_size,
                        1,
                    )?;
                    self.predict_chroma_tu(
                        x_base,
                        cy,
                        size << 1,
                        size << self.sps.vshift[1],
                        log2_trafo_size,
                        2,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Predict one chroma transform block (`s->hpc.intra_pred[...]`).
    #[allow(clippy::too_many_arguments)] // mirrors the hls_transform_unit call sites
    fn predict_chroma_tu(
        &mut self,
        x: i32,
        y: i32,
        horiz_size: i32,
        vert_size: i32,
        log2_size: u32,
        c_idx: usize,
    ) -> Result<()> {
        let na = wedeo_codec_hevc_refs::set_neighbour_available(
            x,
            y,
            horiz_size,
            vert_size,
            self.sps.log2_ctb_size,
            self.ctb_flags,
        );
        intra_predict_tu(
            self.pic,
            self.sps,
            self.pps,
            self.geo,
            na,
            x,
            y,
            log2_size,
            c_idx,
            self.intra.mode_c,
        )
    }

    /// Decode one prediction unit (FFmpeg's `hls_prediction_unit`).
    #[allow(clippy::too_many_arguments)] // mirrors FFmpeg's hls_prediction_unit signature
    fn prediction_unit(
        &mut self,
        x0: i32,
        y0: i32,
        n_pb_w: i32,
        n_pb_h: i32,
        log2_cb_size: u32,
        part_idx: usize,
        flags: NeighbourCtbFlags,
        skip: bool,
    ) -> Result<()> {
        let na = wedeo_codec_hevc_refs::set_neighbour_available(
            x0,
            y0,
            n_pb_w,
            n_pb_h,
            self.sps.log2_ctb_size,
            flags,
        );
        let mut merge_idx = 0usize;
        if !skip {
            self.pu_merge_flag = self.cabac.decode_merge_flag() != 0;
        }
        let merge = skip || self.pu_merge_flag;
        if merge && self.sh.max_num_merge_cand > 1 {
            merge_idx = self.cabac.decode_merge_idx(self.sh.max_num_merge_cand) as usize;
        }

        let current_mv = if merge {
            let temporal = make_temporal(self.dpb, &self.rpl, self.sh, self.collocated);
            let cur_mvf = &self.pic.tab_mvf;
            luma_mv_merge_mode(
                self.geo,
                na,
                cur_mvf,
                &self.rpl,
                self.sh,
                self.part_mode_for_mv(),
                part_idx,
                x0,
                y0,
                n_pb_w,
                n_pb_h,
                log2_cb_size,
                x0,
                y0,
                merge_idx,
                temporal.as_ref(),
            )
        } else {
            self.amvp(x0, y0, n_pb_w, n_pb_h, na)?
        };

        // Store the MV field for all PUs in this block.
        let pu = |v: i32| (v as u32 >> self.sps.log2_min_pu_size) as usize;
        for j in 0..((n_pb_h >> self.sps.log2_min_pu_size) as usize) {
            for i in 0..((n_pb_w >> self.sps.log2_min_pu_size) as usize) {
                if let Some(f) = self.pic.tab_mvf.get_mut(pu(x0) + i, pu(y0) + j) {
                    *f = current_mv;
                }
            }
        }

        mc_block(
            self.pictures,
            self.pic,
            &self.rpl,
            self.sps,
            self.pps,
            self.sh,
            current_mv,
            x0,
            y0,
            n_pb_w,
            n_pb_h,
        )?;
        Ok(())
    }

    fn amvp(
        &mut self,
        x0: i32,
        y0: i32,
        n_pb_w: i32,
        n_pb_h: i32,
        na: NeighbourAvailability,
    ) -> Result<MvField> {
        let temporal = make_temporal(self.dpb, &self.rpl, self.sh, self.collocated);
        let mut mv = MvField {
            pred_flag: PredFlag::INTRA,
            ..Default::default()
        };
        let mut inter_pred = InterPredIdc::PredL0;
        if self.sh.slice_type == SliceType::B {
            inter_pred = self
                .cabac
                .decode_inter_pred_idc(n_pb_w, n_pb_h, self.ct_depth as usize);
        }
        if inter_pred != InterPredIdc::PredL1 {
            if self.sh.nb_refs[L0] != 0 {
                mv.ref_idx[0] = self.cabac.decode_ref_idx_lx(self.sh.nb_refs[L0] as i32) as i8;
            }
            mv.pred_flag = PredFlag::L0;
            let (mvd_x, mvd_y) = self.cabac.decode_mvd_coding();
            let mvp_flag = self.cabac.decode_mvp_lx_flag() as usize;
            let params = MvpParams {
                geo: self.geo,
                na,
                cur_mvf: &self.pic.tab_mvf,
                cur_rpl: &self.rpl,
                poc: self.sh.poc,
                temporal: temporal.as_ref(),
            };
            luma_mv_mvp_mode(&params, x0, y0, n_pb_w, n_pb_h, &mut mv, mvp_flag, L0);
            mv.mv[0].x = mv.mv[0].x.wrapping_add(mvd_x as i16);
            mv.mv[0].y = mv.mv[0].y.wrapping_add(mvd_y as i16);
        }
        if inter_pred != InterPredIdc::PredL0 {
            if self.sh.nb_refs[L1] != 0 {
                mv.ref_idx[1] = self.cabac.decode_ref_idx_lx(self.sh.nb_refs[L1] as i32) as i8;
            }
            let mvd = if self.sh.mvd_l1_zero_flag && inter_pred == InterPredIdc::PredBi {
                (0, 0)
            } else {
                self.cabac.decode_mvd_coding()
            };
            mv.pred_flag = PredFlag(mv.pred_flag.bits() | PredFlag::L1.bits());
            let mvp_flag = self.cabac.decode_mvp_lx_flag() as usize;
            let params = MvpParams {
                geo: self.geo,
                na,
                cur_mvf: &self.pic.tab_mvf,
                cur_rpl: &self.rpl,
                poc: self.sh.poc,
                temporal: temporal.as_ref(),
            };
            luma_mv_mvp_mode(&params, x0, y0, n_pb_w, n_pb_h, &mut mv, mvp_flag, L1);
            mv.mv[1].x = mv.mv[1].x.wrapping_add(mvd.0 as i16);
            mv.mv[1].y = mv.mv[1].y.wrapping_add(mvd.1 as i16);
        }
        Ok(mv)
    }

    fn part_mode_for_mv(&self) -> wedeo_codec_hevc_refs::PartMode {
        use wedeo_codec_hevc_refs::PartMode as Rp;
        match self.part_mode {
            PartMode::Part2Nx2N => Rp::N2Nx2N,
            PartMode::Part2NxN => Rp::N2xN,
            PartMode::PartNx2N => Rp::Nx2N,
            PartMode::PartNxN => Rp::NxN,
            PartMode::Part2NxnU => Rp::N2xnU,
            PartMode::Part2NxnD => Rp::N2xnD,
            PartMode::PartNlx2N => Rp::NLx2N,
            PartMode::PartNrx2N => Rp::NRx2N,
        }
    }

    /// I_PCM sample byte alignment (FFmpeg's `skip_bytes`).
    fn decode_pcm(&mut self, x0: i32, y0: i32, log2_cb_size: u32) -> Result<()> {
        let size = 1usize << log2_cb_size;
        let pos = self.cabac.pos();
        let mut br = BitReader::new(&self.slice_data[pos.min(self.slice_data.len())..]);
        let depth = self.sps.pcm.bit_depth as usize;
        let shift = self.sps.bit_depth as usize - depth;
        for y in 0..size {
            for x in 0..size {
                let v = br.read_bits(depth)? << shift;
                self.pic
                    .set_sample(0, x0 as usize + x, y0 as usize + y, v as u16);
            }
        }
        self.cabac.reinit()?;
        Ok(())
    }

    fn set_pcm(&mut self, x0: i32, y0: i32, log2_cb_size: u32) {
        let size = 1 << log2_cb_size;
        let log2_min_pu = self.sps.log2_min_pu_size;
        for y in 0..size {
            for x in 0..size {
                let px = ((x0 + x) as u32 >> log2_min_pu) as usize;
                let py = ((y0 + y) as u32 >> log2_min_pu) as usize;
                if let Some(v) = self.pic.is_pcm.get_mut(py * self.pic.min_pu_width + px) {
                    *v = 1;
                }
            }
        }
    }
}

/// Build the collocated context for temporal MV derivation.
fn make_temporal<'a>(
    dpb: &'a Dpb,
    rpl: &'a [RefPicList; 2],
    sh: &SliceHeader,
    collocated: Option<FrameRef>,
) -> Option<TemporalCtx<'a>> {
    let frame = dpb.resolve(collocated?)?;
    Some(TemporalCtx {
        poc: sh.poc,
        col_frame: frame,
        cur_rpl: rpl,
        collocated_list: sh.collocated_list,
    })
}

/// Motion compensation for one PU (FFmpeg's `hls_prediction_unit` MC part).
#[allow(clippy::too_many_arguments)]
fn mc_block(
    pictures: &PictureStore,
    pic: &mut Picture,
    rpl: &[RefPicList; 2],
    sps: &Sps,
    pps: &Pps,
    sh: &SliceHeader,
    mv: MvField,
    x0: i32,
    y0: i32,
    w: i32,
    h: i32,
) -> Result<()> {
    let w = w as usize;
    let h = h as usize;
    let bit_depth = sps.bit_depth as u8;
    let weighted = (sh.slice_type == SliceType::P && pps.weighted_pred_flag)
        || (sh.slice_type == SliceType::B && pps.weighted_bipred_flag);
    let _ = weighted;

    let get_ref = |list: usize, idx: usize| -> Option<&Picture> {
        let handle = *rpl.get(list)?.r#ref.get(idx)?;
        handle.and_then(|h| pictures.get(h))
    };
    let ref0 = if mv.pred_flag.has_l0() {
        get_ref(L0, mv.ref_idx[0].max(0) as usize)
    } else {
        None
    };
    let ref1 = if mv.pred_flag.has_l1() {
        get_ref(L1, mv.ref_idx[1].max(0) as usize)
    } else {
        None
    };
    let (Some(r0), _) = (ref0, ()) else {
        return Ok(());
    };

    let luma_params = |mv: Mv| -> dsp::inter::McParams {
        let mx = (i32::from(mv.x) & 3) as usize;
        let my = (i32::from(mv.y) & 3) as usize;
        dsp::inter::McParams::new(bit_depth, mx, my).unwrap_or(dsp::inter::McParams {
            bit_depth,
            mx: 0,
            my: 0,
        })
    };
    let chroma_params = |mv: Mv| -> dsp::inter::McParams {
        let hshift = sps.hshift[1];
        let vshift = sps.vshift[1];
        let mx = ((i32::from(mv.x) & ((1 << (2 + hshift)) - 1)) << (1 - hshift)) as usize;
        let my = ((i32::from(mv.y) & ((1 << (2 + vshift)) - 1)) << (1 - vshift)) as usize;
        dsp::inter::McParams::new(sps.bit_depth_chroma as u8, mx, my).unwrap_or(
            dsp::inter::McParams {
                bit_depth: sps.bit_depth_chroma as u8,
                mx: 0,
                my: 0,
            },
        )
    };

    let luma_origin = |mv: Mv| -> (usize, usize) {
        (
            (crate::picture::PAD as i32 + x0 + (i32::from(mv.x) >> 2)).max(0) as usize,
            (crate::picture::PAD as i32 + y0 + (i32::from(mv.y) >> 2)).max(0) as usize,
        )
    };
    let chroma_origin = |mv: Mv| -> (usize, usize) {
        (
            (crate::picture::PAD as i32
                + (x0 >> sps.hshift[1])
                + (i32::from(mv.x) >> (2 + sps.hshift[1])))
                .max(0) as usize,
            (crate::picture::PAD as i32
                + (y0 >> sps.vshift[1])
                + (i32::from(mv.y) >> (2 + sps.vshift[1])))
                .max(0) as usize,
        )
    };

    let dst_base = pic.index(0, x0 as usize, y0 as usize);
    let dst = &mut pic.planes[0][dst_base..];
    let pwt = sh.pred_weight_table.as_ref();
    if mv.pred_flag == PredFlag::L0 {
        let (rx, ry) = luma_origin(mv.mv[0]);
        let params = luma_params(mv.mv[0]);
        let idx = mv.ref_idx[0].max(0) as usize;
        if weighted
            && pwt.is_some_and(|t| {
                t.luma_log2_weight_denom > 0
                    || t.luma_weight_l0[idx] != (1 << t.luma_log2_weight_denom)
            })
        {
            let t = pwt.unwrap();
            let weight = dsp::inter::UniWeight {
                denom: t.luma_log2_weight_denom as u8,
                wx: t.luma_weight_l0[idx],
                ox: t.luma_offset_l0[idx],
            };
            dsp::inter::uni_w_luma(
                dst,
                pic.stride[0],
                &r0.planes[0],
                r0.stride[0],
                w,
                h,
                rx,
                ry,
                params,
                weight,
            )
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        } else {
            dsp::inter::uni_luma(
                dst,
                pic.stride[0],
                &r0.planes[0],
                r0.stride[0],
                w,
                h,
                rx,
                ry,
                params,
            )
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        }
    } else if mv.pred_flag == PredFlag::L1 && ref1.is_some() {
        let r1 = ref1.unwrap();
        let (rx, ry) = luma_origin(mv.mv[1]);
        let params = luma_params(mv.mv[1]);
        dsp::inter::uni_luma(
            dst,
            pic.stride[0],
            &r1.planes[0],
            r1.stride[0],
            w,
            h,
            rx,
            ry,
            params,
        )
        .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
    } else if mv.pred_flag == PredFlag::BI {
        let Some(r1) = ref1 else { return Ok(()) };
        let (rx0, ry0) = luma_origin(mv.mv[0]);
        let (rx1, ry1) = luma_origin(mv.mv[1]);
        let p0 = luma_params(mv.mv[0]);
        let p1 = luma_params(mv.mv[1]);
        let mut second = vec![0i16; h * dsp::MAX_PB_SIZE];
        dsp::inter::put_luma(
            &mut second,
            dsp::MAX_PB_SIZE,
            &r1.planes[0],
            r1.stride[0],
            w,
            h,
            rx1,
            ry1,
            p1,
        )
        .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        dsp::inter::bi_luma(
            dst,
            pic.stride[0],
            &r0.planes[0],
            r0.stride[0],
            &second,
            dsp::MAX_PB_SIZE,
            w,
            h,
            rx0,
            ry0,
            p0,
        )
        .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
    }

    if sps.chroma_format_idc == 0 {
        return Ok(());
    }
    let cw = w >> sps.hshift[1];
    let ch = h >> sps.vshift[1];
    for c in 1..=2 {
        let dst_base = pic.index(
            c,
            x0 as usize >> sps.hshift[1],
            y0 as usize >> sps.vshift[1],
        );
        let dst = &mut pic.planes[c][dst_base..];
        if mv.pred_flag == PredFlag::L0 {
            let (rx, ry) = chroma_origin(mv.mv[0]);
            dsp::inter::uni_chroma(
                dst,
                pic.stride[c],
                &r0.planes[c],
                r0.stride[c],
                cw,
                ch,
                rx,
                ry,
                chroma_params(mv.mv[0]),
            )
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        } else if mv.pred_flag == PredFlag::L1 && ref1.is_some() {
            let r1 = ref1.unwrap();
            let (rx, ry) = chroma_origin(mv.mv[1]);
            dsp::inter::uni_chroma(
                dst,
                pic.stride[c],
                &r1.planes[c],
                r1.stride[c],
                cw,
                ch,
                rx,
                ry,
                chroma_params(mv.mv[1]),
            )
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        } else if mv.pred_flag == PredFlag::BI {
            let Some(r1) = ref1 else { return Ok(()) };
            let (rx0, ry0) = chroma_origin(mv.mv[0]);
            let (rx1, ry1) = chroma_origin(mv.mv[1]);
            let mut second = vec![0i16; ch * dsp::MAX_PB_SIZE];
            dsp::inter::put_chroma(
                &mut second,
                dsp::MAX_PB_SIZE,
                &r1.planes[c],
                r1.stride[c],
                cw,
                ch,
                rx1,
                ry1,
                chroma_params(mv.mv[1]),
            )
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
            dsp::inter::bi_chroma(
                dst,
                pic.stride[c],
                &r0.planes[c],
                r0.stride[c],
                &second,
                dsp::MAX_PB_SIZE,
                cw,
                ch,
                rx0,
                ry0,
                chroma_params(mv.mv[0]),
            )
            .map_err(|e| wedeo_core::Error::Other(e.to_string()))?;
        }
    }
    Ok(())
}

impl Decoder for HevcDecoder {
    fn send_packet(&mut self, packet: Option<&wedeo_core::packet::Packet>) -> Result<()> {
        match packet {
            Some(p) => {
                let pts = p.pts;
                self.decode_packet(p.data.data(), pts)
            }
            None => {
                self.draining = true;
                Ok(())
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        loop {
            if let Some(handle) = self.output_queue.pop_front() {
                let pic = match self.pictures.get(handle) {
                    Some(p) => p,
                    None => continue,
                };
                // FFmpeg's ff_hevc_output_frames: nb_output <= max_output and
                // nb_dpb <= max_dpb. We only emit frames already removed from
                // the DPB here, so their samples are complete.
                let fmt = match (self.sps_pixfmt(handle), pic.bit_depth) {
                    (Some(f), _) => f,
                    (None, 8) => PixelFormat::Yuv420p,
                    (None, 10) => PixelFormat::Yuv420p10le,
                    (None, _) => return Err(Error::PatchwelcomeNotImplemented),
                };
                let (crop_l, crop_t, crop_r, crop_b) = self.pic_crop(handle);
                let (width, height, planes) = pic.cropped_planes(crop_l, crop_t, crop_r, crop_b);
                let planes = planes
                    .into_iter()
                    .map(|(data, linesize)| FramePlane {
                        buffer: Buffer::from_slice(&data),
                        offset: 0,
                        linesize,
                    })
                    .collect::<Vec<_>>();
                let mut frame = Frame::new_video(width as u32, height as u32, fmt);
                frame.pts = self.pts_for(handle);
                frame.pkt_dts = frame.pts;
                if let wedeo_core::frame::FrameData::Video(ref mut video) = frame.data {
                    video.planes = planes;
                    video.picture_type = self.pic_type(handle);
                }
                if self.pic_key(handle) {
                    frame.flags |= wedeo_core::frame::FrameFlags::KEY;
                }
                return Ok(frame);
            }
            if self.draining {
                self.flush_outputs(false)?;
                if self.output_queue.is_empty() {
                    return Err(Error::Eof);
                }
            } else {
                return Err(Error::Again);
            }
        }
    }

    fn flush(&mut self) {
        self.ps = ParamSets::new();
        self.dpb.flush();
        self.pictures.clear();
        self.output_queue.clear();
        self.cur_ref = None;
        self.draining = false;
        self.poc_tid0 = 0;
        self.last_eos = false;
        self.eos = false;
        self.slice_initialized = false;
        self.last_sps_id = None;
        self.filter_pps = None;
        self.filter_sps = None;
        let _ = &self.params;
    }

    fn descriptor(&self) -> &CodecDescriptor {
        &self.descriptor
    }
}

// Small accessors used by `receive_frame`. They keep the borrow of `self`
// simple by deferring lookups until after the picture has been cloned.
impl HevcDecoder {
    fn sps_pixfmt(&self, handle: FrameRef) -> Option<PixelFormat> {
        let sps_id = self.ps.pps_list.iter().flatten().find_map(|p| {
            if self
                .ps
                .sps_list
                .get(p.sps_id as usize)
                .and_then(Option::as_ref)
                .is_some()
            {
                Some(p.sps_id)
            } else {
                None
            }
        })?;
        self.ps
            .sps_list
            .get(sps_id as usize)
            .and_then(Option::as_ref)
            .and_then(|s| s.pix_fmt)
            .or_else(|| {
                self.pictures
                    .get(handle)
                    .and_then(|p| match (p.chroma_format_idc, p.bit_depth) {
                        (0, 8) => Some(PixelFormat::Gray8),
                        (1, 8) => Some(PixelFormat::Yuv420p),
                        (1, 10) => Some(PixelFormat::Yuv420p10le),
                        _ => None,
                    })
            })
    }
    fn pic_crop(&self, _handle: FrameRef) -> (u32, u32, u32, u32) {
        // Framecrc requires cropped dimensions; use the active SPS window.
        self.ps
            .sps_list
            .iter()
            .flatten()
            .find_map(|s| {
                if s.width > 0 {
                    Some((
                        s.output_window.left_offset,
                        s.output_window.top_offset,
                        s.output_window.right_offset,
                        s.output_window.bottom_offset,
                    ))
                } else {
                    None
                }
            })
            .unwrap_or((0, 0, 0, 0))
    }
    fn pts_for(&self, _handle: FrameRef) -> i64 {
        self.pts
    }
    fn pic_type(&self, _handle: FrameRef) -> PictureType {
        PictureType::None
    }
    fn pic_key(&self, _handle: FrameRef) -> bool {
        false
    }
}

struct HevcDecoderFactory;

impl DecoderFactory for HevcDecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Hevc,
            name: "hevc",
            long_name: "HEVC / H.265 (native)",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::DR1,
            properties: CodecProperties::LOSSY.union(CodecProperties::REORDER),
            priority: 100,
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(HevcDecoder::new(params)?))
    }
}

inventory::submit!(&HevcDecoderFactory as &dyn DecoderFactory);
