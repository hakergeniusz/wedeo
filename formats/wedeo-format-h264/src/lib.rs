// H.264 Annex B raw bitstream demuxer.
//
// Reads raw H.264 elementary streams delimited by Annex B start codes
// (0x000001 or 0x00000001). Groups NAL units into access units (one
// per coded picture) and returns them as packets.
//
// AU boundary detection follows FFmpeg's H.264 parser
// (`libavcodec/h264_parser.c:h264_find_frame_end()` and `parse_nal_units()`):
// AUD/SEI/SPS/PPS NALs after a slice and a slice whose `first_mb_in_slice`
// does not increase both end the current AU. The ITU-T H.264 Section 7.4.1.2.3
// "first VCL NAL unit of a primary coded picture" conditions (frame_num,
// PPS id, field_pic_flag/bottom_field_flag, nal_ref_idc, POC fields and
// idr_pic_id differences) are checked as well.
//
// Reference: FFmpeg libavformat/h264dec.c, libavcodec/h264_parser.c,
// libavcodec/h264_parse.c (ff_h264_init_poc), ITU-T H.264 Annex B / 7.4.1.2.

use std::collections::VecDeque;

use tracing::{info, trace};
use wedeo_codec::decoder::CodecParameters;
use wedeo_codec_h264::nal::{NalUnitType, split_annex_b};
use wedeo_codec_h264::pps::{Pps, parse_pps};
use wedeo_codec_h264::slice::{SliceHeader, parse_slice_header};
use wedeo_codec_h264::sps::{Sps, parse_sps};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_format::demuxer::{
    Demuxer, DemuxerHeader, InputFormatDescriptor, InputFormatFlags, PROBE_SCORE_EXTENSION,
    PROBE_SCORE_MAX, ProbeData, SeekFlags, Stream,
};
use wedeo_format::io::BufferedIo;
use wedeo_format::registry::DemuxerFactory;

/// Size of the initial read buffer for probing and header parsing.
const INITIAL_READ_SIZE: usize = 64 * 1024;

/// Maximum bytes to scan in probe data looking for start codes.
const PROBE_SCAN_LIMIT: usize = 4096;

/// Read chunk size used while looking for the next access unit boundary.
const READ_CHUNK_SIZE: usize = 32 * 1024;

// ---------------------------------------------------------------------------
// Start code scanning utilities
// ---------------------------------------------------------------------------

/// Find the next Annex B start code in `data` starting at position `pos`.
///
/// Returns `Some((start_code_pos, start_code_len))` where `start_code_len` is
/// 3 for `0x000001` or 4 for `0x00000001`. Returns `None` if no start code found.
fn find_start_code(data: &[u8], pos: usize) -> Option<(usize, usize)> {
    let mut i = pos;
    while i + 2 < data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if data[i + 2] == 0x01 {
                return Some((i, 3));
            }
            if i + 3 < data.len() && data[i + 2] == 0x00 && data[i + 3] == 0x01 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

/// A single Annex B NAL unit located in the pending buffer.
#[derive(Clone, Copy)]
struct NalInfo {
    /// Position of the start code.
    start: usize,
    /// NAL header byte.
    header: u8,
    /// Position of the next start code, or `data.len()` for the last NAL.
    end: usize,
}

impl NalInfo {
    fn nal_type(&self) -> u8 {
        self.header & 0x1F
    }

    fn nal_ref_idc(&self) -> u8 {
        (self.header >> 5) & 0x03
    }
}

/// Locate all NAL units starting at `from`.
fn scan_nals(data: &[u8], from: usize) -> Vec<NalInfo> {
    let mut nals = Vec::new();
    let mut pos = from;
    while let Some((sc_pos, sc_len)) = find_start_code(data, pos) {
        let header_pos = sc_pos + sc_len;
        if header_pos >= data.len() {
            break;
        }
        // forbidden_zero_bit: skip this start code; scan for the next one.
        let header = data[header_pos];
        let end = find_start_code(data, header_pos).map_or(data.len(), |(p, _)| p);
        nals.push(NalInfo {
            start: sc_pos,
            header,
            end,
        });
        if end == data.len() {
            break;
        }
        pos = end;
    }
    nals
}

/// Extract the RBSP (emulation prevention bytes removed) of a located NAL.
fn nal_rbsp(data: &[u8], nal: &NalInfo) -> Result<Vec<u8>> {
    split_annex_b(&data[nal.start..nal.end])
        .into_iter()
        .next()
        .map(|n| n.data)
        .ok_or(Error::InvalidData)
}

/// FFmpeg h264_parser.c:126-127: SEI, SPS, PPS and AUD after a picture close
/// the current access unit. H.264 7.4.1.2.4 adds prefix/subset/SVC NALs.
fn is_au_delimiter_nal_type(nal_type: u8) -> bool {
    matches!(nal_type, 6 | 7 | 8 | 9 | 14 | 15 | 16 | 17 | 18 | 19)
}

/// VCL NAL types carrying a slice_header() (h264_parser.c:132-133).
fn is_slice_nal_type(nal_type: u8) -> bool {
    matches!(nal_type, 1 | 2 | 5)
}

/// Fields from the first VCL NAL of a picture, used for H.264 7.4.1.2.3.
#[derive(Clone)]
struct VclInfo {
    first_mb: u32,
    frame_num: u32,
    pps_id: u32,
    field_pic_flag: bool,
    bottom_field_flag: bool,
    nal_ref_idc: u8,
    is_idr: bool,
    idr_pic_id: u32,
    poc_type: u8,
    poc_lsb: u32,
    delta_poc_bottom: i32,
    delta_poc: [i32; 2],
}

impl VclInfo {
    fn from_header(hdr: &SliceHeader, nal_ref_idc: u8, is_idr: bool, poc_type: u8) -> Self {
        Self {
            first_mb: hdr.first_mb_in_slice,
            frame_num: hdr.frame_num,
            pps_id: hdr.pps_id,
            field_pic_flag: hdr.field_pic_flag,
            bottom_field_flag: hdr.bottom_field_flag,
            nal_ref_idc,
            is_idr,
            idr_pic_id: hdr.idr_pic_id,
            poc_type,
            poc_lsb: hdr.pic_order_cnt_lsb,
            delta_poc_bottom: hdr.delta_pic_order_cnt_bottom,
            delta_poc: hdr.delta_pic_order_cnt,
        }
    }
}

/// H.264 7.4.1.2.3: detection of the first VCL NAL unit of a primary coded
/// picture. `prev` is the previous VCL NAL, `cur` the one being examined.
fn starts_new_picture(prev: &VclInfo, cur: &VclInfo) -> bool {
    prev.frame_num != cur.frame_num
        || prev.pps_id != cur.pps_id
        || prev.field_pic_flag != cur.field_pic_flag
        || (prev.field_pic_flag
            && cur.field_pic_flag
            && prev.bottom_field_flag != cur.bottom_field_flag)
        || (prev.nal_ref_idc == 0) != (cur.nal_ref_idc == 0)
        || (prev.poc_type == 0
            && cur.poc_type == 0
            && (prev.poc_lsb != cur.poc_lsb || prev.delta_poc_bottom != cur.delta_poc_bottom))
        || (prev.poc_type == 1 && cur.poc_type == 1 && prev.delta_poc != cur.delta_poc)
        || prev.is_idr != cur.is_idr
        || (prev.is_idr && cur.is_idr && prev.idr_pic_id != cur.idr_pic_id)
}

/// Picture order count context, ported from FFmpeg's `H264POCContext`
/// (h264_parser.c:59, h264_parse.c:280).
#[derive(Default)]
struct PocContext {
    frame_num: i32,
    frame_num_offset: i32,
    prev_frame_num: i32,
    prev_frame_num_offset: i32,
    poc_lsb: i32,
    prev_poc_lsb: i32,
    poc_msb: i32,
    prev_poc_msb: i32,
    delta_poc_bottom: i32,
    delta_poc: [i32; 2],
}

/// Compute the picture order count from the first slice of a picture.
/// Ported from FFmpeg `ff_h264_init_poc()` (h264_parse.c:280-363).
fn init_poc(sps: &Sps, hdr: &SliceHeader, nal_ref_idc: u8, poc: &mut PocContext) -> Option<i32> {
    let max_frame_num = 1i64 << sps.log2_max_frame_num;

    poc.frame_num_offset = poc.prev_frame_num_offset;
    if i64::from(poc.frame_num) < i64::from(poc.prev_frame_num) {
        poc.frame_num_offset += max_frame_num as i32;
    }

    // FFmpeg PICT_TOP_FIELD=1, PICT_BOTTOM_FIELD=2, PICT_FRAME=3.
    let picture_structure = if !sps.frame_mbs_only_flag && hdr.field_pic_flag {
        if hdr.bottom_field_flag { 2 } else { 1 }
    } else {
        3
    };

    poc.frame_num = hdr.frame_num as i32;
    poc.poc_lsb = hdr.pic_order_cnt_lsb as i32;
    poc.delta_poc_bottom = hdr.delta_pic_order_cnt_bottom;
    poc.delta_poc = hdr.delta_pic_order_cnt;

    let mut field_poc = [i64::from(i32::MAX), i64::from(i32::MAX)];
    if sps.poc_type == 0 {
        let max_poc_lsb = 1i64 << sps.log2_max_poc_lsb;
        if poc.prev_poc_lsb < 0 {
            poc.prev_poc_lsb = poc.poc_lsb;
        }
        if poc.poc_lsb < poc.prev_poc_lsb
            && i64::from(poc.prev_poc_lsb - poc.poc_lsb) >= max_poc_lsb / 2
        {
            poc.poc_msb = poc.prev_poc_msb + max_poc_lsb as i32;
        } else if poc.poc_lsb > poc.prev_poc_lsb
            && i64::from(poc.prev_poc_lsb - poc.poc_lsb) < -(max_poc_lsb / 2)
        {
            poc.poc_msb = poc.prev_poc_msb - max_poc_lsb as i32;
        } else {
            poc.poc_msb = poc.prev_poc_msb;
        }
        let base = i64::from(poc.poc_msb) + i64::from(poc.poc_lsb);
        field_poc[0] = base;
        field_poc[1] = base;
        if picture_structure == 3 {
            field_poc[1] += i64::from(poc.delta_poc_bottom);
        }
    } else if sps.poc_type == 1 {
        let mut abs_frame_num = if !sps.offset_for_ref_frame.is_empty() {
            poc.frame_num_offset + poc.frame_num
        } else {
            0
        };
        if nal_ref_idc == 0 && abs_frame_num > 0 {
            abs_frame_num -= 1;
        }
        let expected_delta_per_poc_cycle: i64 =
            sps.offset_for_ref_frame.iter().map(|&v| i64::from(v)).sum();
        let mut expectedpoc = 0i64;
        if abs_frame_num > 0 {
            let cycle_len = sps.offset_for_ref_frame.len() as i32;
            let poc_cycle_cnt = (abs_frame_num - 1) / cycle_len;
            let frame_num_in_poc_cycle = (abs_frame_num - 1) % cycle_len;
            expectedpoc = i64::from(poc_cycle_cnt) * expected_delta_per_poc_cycle;
            for i in 0..=frame_num_in_poc_cycle {
                expectedpoc += i64::from(sps.offset_for_ref_frame[i as usize]);
            }
        }
        if nal_ref_idc == 0 {
            expectedpoc += i64::from(sps.offset_for_non_ref_pic);
        }
        field_poc[0] = expectedpoc + i64::from(poc.delta_poc[0]);
        field_poc[1] = field_poc[0] + i64::from(sps.offset_for_top_to_bottom_field);
        if picture_structure == 3 {
            field_poc[1] += i64::from(poc.delta_poc[1]);
        }
    } else {
        let mut p = 2i64 * (i64::from(poc.frame_num_offset) + i64::from(poc.frame_num));
        if nal_ref_idc == 0 {
            p -= 1;
        }
        field_poc[0] = p;
        field_poc[1] = p;
    }

    let mut pic_field_poc = [i32::MAX, i32::MAX];
    if picture_structure != 2 {
        pic_field_poc[0] = i32::try_from(field_poc[0]).ok()?;
    }
    if picture_structure != 1 {
        pic_field_poc[1] = i32::try_from(field_poc[1]).ok()?;
    }
    let pic_poc = pic_field_poc[0].min(pic_field_poc[1]);

    // FFmpeg h264_parser.c:561-573: set up prev_ values for the next picture.
    poc.prev_frame_num = poc.frame_num;
    poc.prev_frame_num_offset = poc.frame_num_offset;
    if nal_ref_idc != 0 {
        poc.prev_poc_msb = poc.poc_msb;
        poc.prev_poc_lsb = poc.poc_lsb;
    }

    Some(pic_poc)
}

// ---------------------------------------------------------------------------
// H.264 Demuxer
// ---------------------------------------------------------------------------

/// An access unit waiting for its presentation timestamp. POC defines the
/// display order, so a packet may be decoded before earlier-displayed packets.
struct PendingAu {
    pkt: Packet,
    poc: Option<i32>,
    /// (frame_num, field_pic_flag, bottom_field_flag) of the first slice.
    frame_key: Option<(u32, bool, bool)>,
    assigned: bool,
}

struct H264Demuxer {
    /// Unreturned bytes from the stream.
    pending: Vec<u8>,
    /// Start code position of the access unit currently being assembled.
    au_start: usize,
    /// Start code position of the next NAL to examine.
    scan_pos: usize,
    /// Already emitted packets waiting to be returned.
    ready: VecDeque<PendingAu>,
    /// Number of pictures that can be reordered in display order.
    reorder_depth: usize,
    /// Next presentation timestamp in display order.
    next_pts: i64,
    /// True once the underlying I/O reported EOF.
    eof: bool,
    /// Absolute byte offset of the next packet.
    stream_pos: i64,
    /// Decode order timestamp of the next packet (DTS).
    decode_index: i64,
    /// Parameter sets, needed to parse slice headers.
    sps_list: [Option<Sps>; 32],
    pps_list: [Option<Pps>; 256],
    /// Previous VCL NAL in the current AU, for 7.4.1.2.3.
    prev_vcl: Option<VclInfo>,
    /// Whether the current AU contains a VCL NAL.
    au_has_vcl: bool,
    /// POC of the current AU's first VCL NAL.
    au_poc: Option<i32>,
    /// (frame_num, field_pic_flag, bottom_field_flag) of that slice.
    au_frame_key: Option<(u32, bool, bool)>,
    /// POC state carried between pictures.
    poc_ctx: PocContext,
}

impl H264Demuxer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            au_start: usize::MAX,
            scan_pos: 0,
            ready: VecDeque::new(),
            reorder_depth: 16,
            next_pts: 0,
            eof: false,
            stream_pos: 0,
            decode_index: 0,
            sps_list: std::array::from_fn(|_| None),
            pps_list: std::array::from_fn(|_| None),
            prev_vcl: None,
            au_has_vcl: false,
            au_poc: None,
            au_frame_key: None,
            poc_ctx: PocContext::default(),
        }
    }

    /// Read more data into the pending buffer. Returns false only at EOF.
    fn fill_pending(&mut self, io: &mut BufferedIo, min_size: usize) -> Result<bool> {
        if self.pending.len() >= min_size {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        let need = min_size
            .saturating_sub(self.pending.len())
            .max(READ_CHUNK_SIZE);
        let chunk = io.read_up_to(need)?;
        if chunk.is_empty() {
            self.eof = true;
            return Ok(false);
        }
        self.pending.extend_from_slice(&chunk);
        Ok(true)
    }

    /// Parse an SPS/PPS NAL into the parameter set lists.
    fn parse_param_set(&mut self, nal: &NalInfo) {
        let Ok(rbsp) = nal_rbsp(&self.pending, nal) else {
            return;
        };
        match nal.nal_type() {
            7 => {
                if let Ok(sps) = parse_sps(&rbsp) {
                    let id = sps.sps_id as usize;
                    if id < self.sps_list.len() {
                        self.sps_list[id] = Some(sps);
                    }
                }
            }
            8 => {
                if let Ok(pps) = parse_pps(&rbsp, &self.sps_list) {
                    let id = pps.pps_id as usize;
                    if id < self.pps_list.len() {
                        self.pps_list[id] = Some(pps);
                    }
                }
            }
            _ => {}
        }
    }

    /// Examine a NAL unit; returns true if it starts a new access unit.
    fn exam_nal(&mut self, nal: &NalInfo) -> bool {
        let nal_type = nal.nal_type();

        // h264_parser.c:126-131: non-VCL delimiter after a picture.
        if is_au_delimiter_nal_type(nal_type) {
            if self.au_has_vcl {
                return true;
            }
            // h264_parser.c:432-437: remember parameter sets that a later
            // slice in this AU may refer to.
            if matches!(nal_type, 7 | 8) {
                self.parse_param_set(nal);
            }
            return false;
        }

        if !is_slice_nal_type(nal_type) {
            return false;
        }

        // Parse the slice header. If the parameter sets are not available
        // (or the header is truncated at EOF), fall back to the original
        // first_mb_in_slice check: value 0 is encoded as a single '1' bit.
        let rbsp = match nal_rbsp(&self.pending, nal) {
            Ok(rbsp) => rbsp,
            Err(_) => return false,
        };
        let nal_ref_idc = nal.nal_ref_idc();
        let is_idr = nal_type == 5;
        let hdr = match parse_slice_header(
            &rbsp,
            if is_idr {
                NalUnitType::Idr
            } else {
                NalUnitType::Slice
            },
            nal_ref_idc,
            &self.sps_list,
            &self.pps_list,
        ) {
            Ok(hdr) => hdr,
            Err(_) => {
                let first_mb_zero = rbsp.first().is_some_and(|b| (b & 0x80) != 0);
                return self.au_has_vcl && first_mb_zero;
            }
        };

        let sps = hdr
            .pps_id
            .try_into()
            .ok()
            .and_then(|id: usize| self.pps_list.get(id))
            .and_then(|pps| pps.as_ref())
            .and_then(|pps| usize::try_from(pps.sps_id).ok())
            .and_then(|id| self.sps_list.get(id))
            .and_then(|sps| sps.as_ref());

        let cur = VclInfo::from_header(&hdr, nal_ref_idc, is_idr, sps.map_or(0, |s| s.poc_type));

        if self.au_has_vcl
            && let Some(prev) = &self.prev_vcl
        {
            // FFmpeg h264_parser.c:147-152 detects a boundary when
            // first_mb_in_slice does not increase; H.264 7.4.1.2.3 adds
            // the picture identity checks below.
            if cur.first_mb <= prev.first_mb || starts_new_picture(prev, &cur) {
                return true;
            }
        }

        if !self.au_has_vcl {
            // First VCL of this picture: decode its POC for presentation
            // timestamps (FFmpeg h264_parser.c parse_nal_units()).
            let mut poc = None;
            if let Some(sps) = sps {
                // h264_parser.c:442-448: IDR pictures reset the POC context.
                if is_idr {
                    self.poc_ctx.prev_frame_num = 0;
                    self.poc_ctx.prev_frame_num_offset = 0;
                    self.poc_ctx.prev_poc_msb = 0;
                    self.poc_ctx.prev_poc_lsb = 0;
                }
                // SliceHeader borrows from our lists via indices, so clone
                // the SPS before mutating the POC context.
                let sps_copy = sps.clone();
                poc = init_poc(&sps_copy, &hdr, nal_ref_idc, &mut self.poc_ctx);
            }
            self.au_poc = poc;
            self.au_frame_key = Some((hdr.frame_num, hdr.field_pic_flag, hdr.bottom_field_flag));
            self.au_has_vcl = true;
        }
        self.prev_vcl = Some(cur);
        false
    }

    /// Finish the AU ending at byte position `end` and queue its packet.
    fn emit_au(&mut self, end: usize) -> Result<()> {
        let data = self.pending[self.au_start..end].to_vec();
        let dts = self.decode_index;
        let pos = self.stream_pos;
        let poc = self.au_poc;
        let frame_key = self.au_frame_key;

        self.pending.drain(..end);
        self.stream_pos += end as i64;
        self.au_start = usize::MAX;
        self.scan_pos = 0;
        self.au_has_vcl = false;
        self.prev_vcl = None;
        self.au_poc = None;
        self.au_frame_key = None;
        self.decode_index += 1;

        let mut is_keyframe = false;
        for nalu in split_annex_b(&data) {
            if nalu.nal_type == NalUnitType::Idr {
                is_keyframe = true;
                break;
            }
        }

        let mut pkt = Packet::new();
        pkt.data = Buffer::from_slice(&data);
        pkt.pts = dts;
        pkt.dts = dts;
        pkt.duration = 1;
        pkt.stream_index = 0;
        pkt.pos = pos;
        if is_keyframe {
            pkt.flags = PacketFlags::KEY;
        }
        trace!(dts, ?poc, size = data.len(), is_keyframe, "H.264 packet");
        self.ready.push_back(PendingAu {
            pkt,
            poc,
            frame_key,
            assigned: false,
        });
        Ok(())
    }

    /// Assign PTS in POC (display) order and return the next finished packet.
    ///
    /// A picture may be decoded before pictures that precede it in display
    /// order (B-frames). Keep `reorder_depth` unassigned AUs buffered so the
    /// POC order is known when a PTS is assigned. At EOF the remainder is
    /// flushed in POC order. Complementary fields share one PTS since the
    /// decoder outputs them as a single frame.
    fn flush_ready(&mut self, force: bool) -> Option<Packet> {
        // Sort the unassigned AUs by POC and assign PTS to all but the last
        // `reorder_depth` of them. Those remaining AUs are the ones a future
        // picture could still be reordered before.
        let mut order: Vec<usize> = self
            .ready
            .iter()
            .enumerate()
            .filter(|(_, au)| !au.assigned)
            .map(|(i, _)| i)
            .collect();
        order.sort_by_key(|&i| (self.ready[i].poc.unwrap_or(i32::MAX), self.ready[i].pkt.dts));
        let assignable = if force {
            order.len()
        } else {
            order.len().saturating_sub(self.reorder_depth)
        };

        let mut planned: Vec<(usize, i64)> = Vec::new();
        let mut taken = vec![false; self.ready.len()];
        for &idx in order.iter().take(assignable) {
            if taken[idx] {
                continue;
            }
            let pts = self.next_pts;
            planned.push((idx, pts));
            taken[idx] = true;
            if let Some((frame_num, true, bottom)) = self.ready[idx].frame_key {
                let pair = (frame_num, true, !bottom);
                if let Some(other) = self.ready.iter().enumerate().find_map(|(i, au)| {
                    (!taken[i] && !au.assigned && au.frame_key == Some(pair)).then_some(i)
                }) {
                    planned.push((other, pts));
                    taken[other] = true;
                }
            }
            self.next_pts += 1;
        }
        for (idx, pts) in planned {
            self.ready[idx].pkt.pts = pts;
            self.ready[idx].assigned = true;
        }

        if self.ready.front().is_some_and(|au| au.assigned) {
            self.ready.pop_front().map(|au| au.pkt)
        } else {
            None
        }
    }
}

impl Demuxer for H264Demuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        // Read initial data to find the SPS
        let data = io.read_up_to(INITIAL_READ_SIZE)?;
        if data.is_empty() {
            return Err(Error::InvalidData);
        }

        // Parse NAL units from the initial buffer
        let nalus = split_annex_b(&data);
        let mut sps_parsed: Option<Sps> = None;

        for nalu in &nalus {
            if nalu.nal_type == NalUnitType::Sps
                && let Ok(sps) = parse_sps(&nalu.data)
            {
                sps_parsed = Some(sps);
                break;
            }
        }

        // Build codec parameters
        let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);

        // Determine time_base from SPS VUI timing info, or use 1/25 default
        let time_base;

        if let Some(ref sps) = sps_parsed {
            params.width = sps.width();
            params.height = sps.height();

            // Map chroma_format_idc to pixel format
            params.pixel_format = match sps.chroma_format_idc {
                0 => PixelFormat::Gray8,
                1 => PixelFormat::Yuv420p,
                2 => PixelFormat::Yuv422p,
                3 => PixelFormat::Yuv444p,
                _ => PixelFormat::Yuv420p,
            };

            // Use SPS frame rate for time_base if available
            if let Some(rate) = sps.frame_rate() {
                // time_base = 1/fps = num_units_in_tick*2 / time_scale
                time_base = Rational::new(rate.den, rate.num);
            } else {
                time_base = Rational::new(1, 25);
            }
        } else {
            // No SPS found; set defaults
            params.pixel_format = PixelFormat::Yuv420p;
            time_base = Rational::new(1, 25);
        }

        // VUI bitstream_restriction() signals how many pictures may be
        // reordered. FFmpeg infers a default from the profile/level when it
        // is absent; 16 is the conservative H.264 default used here.
        if let Some(ref sps) = sps_parsed
            && sps.num_reorder_frames >= 0
        {
            self.reorder_depth = (sps.num_reorder_frames as usize).clamp(1, 64);
        }

        let mut stream = Stream::new(0, params);
        stream.time_base = time_base;

        info!(
            width = stream.codec_params.width,
            height = stream.codec_params.height,
            time_base = %time_base,
            "H.264 stream opened"
        );

        // Seek back to start so read_packet gets the full stream
        if io.is_seekable() {
            io.seek(0)?;
        } else {
            // Non-seekable: keep the data we read as pending
            self.pending = data;
        }

        Ok(DemuxerHeader {
            streams: vec![stream],
            metadata: Metadata::new(),
            duration: 0,
            start_time: 0,
        })
    }

    fn read_packet(&mut self, io: &mut BufferedIo) -> Result<Packet> {
        loop {
            if self.eof && self.pending.is_empty() {
                return self.flush_ready(true).ok_or(Error::Eof);
            }
            if let Some(pkt) = self.flush_ready(false) {
                return Ok(pkt);
            }

            if self.pending.is_empty() && !self.eof {
                self.fill_pending(io, READ_CHUNK_SIZE)?;
            }

            if self.pending.is_empty() {
                return Err(Error::Eof);
            }

            // Locate the start code of the first NAL of the current AU.
            if self.au_start == usize::MAX {
                if let Some((sc_pos, _)) = find_start_code(&self.pending, self.scan_pos) {
                    self.au_start = sc_pos;
                    self.scan_pos = sc_pos;
                } else if !self.eof && !self.fill_pending(io, self.pending.len() + 1)? {
                    continue;
                } else if self.eof {
                    // Trailing data without a start code is not a picture.
                    self.pending.clear();
                    return Err(Error::Eof);
                } else {
                    continue;
                }
            }

            let nal = match scan_nals(&self.pending, self.scan_pos).into_iter().next() {
                Some(nal) => nal,
                None => {
                    if self.eof {
                        if self.pending.len() > self.au_start {
                            self.emit_au(self.pending.len())?;
                        }
                        continue;
                    }
                    self.fill_pending(io, self.pending.len() + 1)?;
                    continue;
                }
            };

            // The NAL payload must be terminated by a following start code
            // (or EOF) before its header can be parsed reliably.
            if nal.end == self.pending.len() && !self.eof {
                self.fill_pending(io, self.pending.len() + 1)?;
                continue;
            }

            let is_delimiter = self.exam_nal(&nal);
            if is_delimiter {
                self.emit_au(nal.start)?;
                continue;
            }

            self.scan_pos = nal.end;
            if self.scan_pos == self.pending.len() && self.eof {
                self.emit_au(self.pending.len())?;
            }
        }
    }

    fn seek(
        &mut self,
        _io: &mut BufferedIo,
        _stream_index: usize,
        _timestamp: i64,
        _flags: SeekFlags,
    ) -> Result<()> {
        Err(Error::PatchwelcomeNotImplemented)
    }
}

// ---------------------------------------------------------------------------
// DemuxerFactory
// ---------------------------------------------------------------------------

struct H264DemuxerFactory;

impl DemuxerFactory for H264DemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "h264",
            long_name: "raw H.264 video",
            extensions: "h26l,h264,264,avc",
            mime_types: "video/h264",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        let buf = data.buf;

        // Check file extension for a baseline score
        let ext_score = {
            let filename = data.filename.to_lowercase();
            if filename.ends_with(".264")
                || filename.ends_with(".h264")
                || filename.ends_with(".h26l")
                || filename.ends_with(".avc")
            {
                PROBE_SCORE_EXTENSION
            } else {
                0
            }
        };

        // Scan for Annex B start codes
        let scan_len = buf.len().min(PROBE_SCAN_LIMIT);
        let scan_buf = &buf[..scan_len];

        let mut best = ext_score;

        if let Some((sc_pos, sc_len)) = find_start_code(scan_buf, 0) {
            let nal_header_pos = sc_pos + sc_len;
            if nal_header_pos < scan_len {
                let nal_header = scan_buf[nal_header_pos];
                // Check forbidden_zero_bit
                if (nal_header & 0x80) == 0 {
                    let nal_type = nal_header & 0x1F;
                    if nal_type == 7 {
                        // SPS found right after start code — strong match
                        return PROBE_SCORE_MAX;
                    }
                    // Start codes found but first NAL isn't SPS.
                    // Look further for an SPS.
                    let mut pos = nal_header_pos + 1;
                    while let Some((next_sc_pos, next_sc_len)) = find_start_code(scan_buf, pos) {
                        let next_nal_pos = next_sc_pos + next_sc_len;
                        if next_nal_pos >= scan_len {
                            break;
                        }
                        let next_header = scan_buf[next_nal_pos];
                        if (next_header & 0x80) == 0 {
                            let next_type = next_header & 0x1F;
                            if next_type == 7 {
                                return PROBE_SCORE_MAX;
                            }
                        }
                        pos = next_nal_pos + 1;
                    }
                    // Start codes found but no SPS
                    best = best.max(25);
                }
            }
        }

        best
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(H264Demuxer::new()))
    }
}

inventory::submit!(&H264DemuxerFactory as &dyn DemuxerFactory);
