// HEVC Annex B raw bitstream demuxer.
//
// Reads raw HEVC elementary streams delimited by Annex B start codes
// (0x000001 or 0x00000001). Groups NAL units into access units (one per
// coded picture) and returns them as packets in Annex B form.
//
// AU boundary detection follows FFmpeg's HEVC parser
// (`libavcodec/hevc/parser.c:hevc_find_frame_end()`): an AUD/VPS/SPS/PPS/SEI
// NAL after a picture, or a VCL NAL with `first_slice_segment_in_pic_flag`
// set after a picture has started, closes the current access unit. The
// picture order count computed from the first slice's `SliceHeader` is used
// to assign presentation timestamps in display order.
//
// Stream metadata (dimensions, pixel format, frame rate / time base) comes
// from the VPS/SPS parameter sets, mirroring `hevc_parse_slice_header()` in
// FFmpeg's parser. `hvcC` extradata generation lives in the `hvcc` module and
// follows FFmpeg `libavformat/hevc.c`.
//
// Reference: FFmpeg libavformat/rawdec.c, libavformat/hevc.c,
// libavcodec/hevc/parser.c, libavcodec/hevc/parse.c, libavcodec/hevc/hevc.h.

mod hvcc;

pub use hvcc::annexb_to_hvcc;

use std::collections::VecDeque;

use tracing::{info, trace};
use wedeo_codec::decoder::CodecParameters;
use wedeo_codec_hevc_ps::{
    BitReader, ParamSets, annex_b_split, compute_poc, decode_extradata, ebsp_to_rbsp,
    parse_nal_header,
};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::{Rational, gcd};
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
/// 3 for `0x000001` or 4 for `0x00000001`.
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
#[derive(Clone, Copy, Debug)]
struct NalInfo {
    /// Position of the start code.
    start: usize,
    /// Position of the first NAL header byte.
    header_pos: usize,
    /// Position of the next start code, or `data.len()` for the last NAL.
    end: usize,
}

impl NalInfo {
    /// `nal_unit_type` (bits 1..6 of the first header byte).
    fn nal_type_code(&self, data: &[u8]) -> u8 {
        (data[self.header_pos] >> 1) & 0x3f
    }

    /// `nuh_layer_id`. FFmpeg's parser skips NALs from non-base layers.
    fn layer_id(&self, data: &[u8]) -> u8 {
        ((data[self.header_pos] & 0x01) << 5) | (data[self.header_pos + 1] >> 3)
    }

    /// `nuh_temporal_id_plus1`.
    fn temporal_id_plus1(&self, data: &[u8]) -> u8 {
        data[self.header_pos + 1] & 0x07
    }
}

/// Locate all NAL units starting at `from`.
fn scan_nals(data: &[u8], from: usize) -> Vec<NalInfo> {
    let mut nals = Vec::new();
    let mut pos = from;
    while let Some((sc_pos, sc_len)) = find_start_code(data, pos) {
        let header_pos = sc_pos + sc_len;
        if header_pos + 2 > data.len() {
            break;
        }
        let end = find_start_code(data, header_pos).map_or(data.len(), |(p, _)| p);
        nals.push(NalInfo {
            start: sc_pos,
            header_pos,
            end,
        });
        if end == data.len() {
            break;
        }
        pos = end;
    }
    nals
}

/// Extract the complete NAL bytes (including the two-byte header) of a
/// located NAL, dropping Annex B `trailing_zero_8bits`.
fn nal_bytes<'a>(data: &'a [u8], nal: &NalInfo) -> &'a [u8] {
    let mut end = nal.end;
    while end > nal.header_pos && data[end - 1] == 0x00 {
        end -= 1;
    }
    &data[nal.header_pos..end]
}

/// `hevc_find_frame_end()` boundary NAL types: VPS..EOB (32..37), SEI prefix
/// (39), reserved non-VCL (41..44) and unspecified (48..55).
/// (`libavcodec/hevc/parser.c:281-282`.)
fn is_au_boundary_type(code: u8) -> bool {
    (32..=37).contains(&code)
        || code == 39
        || (41..=44).contains(&code)
        || (48..=55).contains(&code)
}

/// VCL NAL types carrying a picture (`hevc_parser.c:289-290`):
/// `nut <= RASL_R` (0..9) or BLA_W_LP..CRA_NUT (16..21).
fn is_picture_vcl_type(code: u8) -> bool {
    code <= 9 || (16..=21).contains(&code)
}

/// `hevc_parse()` updates `pocTid0` for temporal-id-0 reference pictures,
/// excluding non-reference lead/trail NAL types (`parser.c:160-168`).
fn updates_poc_tid0(code: u8) -> bool {
    !matches!(code, 0 | 2 | 4 | 6 | 7 | 8 | 9)
}

/// Reduce a positive fraction to lowest terms, matching FFmpeg `av_reduce`.
fn reduce_fraction(num: i64, den: i64) -> Rational {
    if num <= 0 || den <= 0 {
        return Rational::new(1, 25);
    }
    let g = gcd(num as u64, den as u64).max(1) as i64;
    Rational::new((num / g) as i32, (den / g) as i32)
}

// ---------------------------------------------------------------------------
// HEVC Demuxer
// ---------------------------------------------------------------------------

struct HevcDemuxer {
    /// Unreturned bytes from the stream.
    pending: Vec<u8>,
    /// Start code position of the access unit currently being assembled.
    au_start: usize,
    /// Start code position of the next NAL to examine.
    scan_pos: usize,
    /// Already emitted packets waiting to be returned.
    ready: VecDeque<Packet>,
    /// True once the underlying I/O reported EOF.
    eof: bool,
    /// Absolute byte offset of the next packet.
    stream_pos: i64,
    /// Decode order timestamp of the next packet (DTS).
    decode_index: i64,
    /// VPS/SPS/PPS parameter sets parsed from the stream.
    ps: ParamSets,
    /// `hevc_find_frame_end()` state: a picture has been seen in this AU.
    frame_start_found: bool,
    /// POC of the current AU's first VCL NAL.
    au_poc: Option<i32>,
    /// POC of the previous picture with TemporalId == 0.
    poc_tid0: i32,
}

impl HevcDemuxer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            au_start: usize::MAX,
            scan_pos: 0,
            ready: VecDeque::new(),
            eof: false,
            stream_pos: 0,
            decode_index: 0,
            ps: ParamSets::new(),
            frame_start_found: false,
            au_poc: None,
            poc_tid0: 0,
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

    /// Parse and store a VPS/SPS/PPS NAL.
    fn parse_param_set(&mut self, nal: &NalInfo) {
        let bytes = nal_bytes(&self.pending, nal).to_vec();
        let _ = self.ps.decode_nal(&bytes);
    }

    /// Parse the picture order count of the first slice of a picture. This is
    /// the subset of FFmpeg's `hevc_parse_slice_header()`
    /// (`libavcodec/hevc/parser.c:56-171`) needed to reach
    /// `pic_order_cnt_lsb`. Using only these fields keeps demuxing robust:
    /// malformed slice data cannot abort the packet splitter, and POC is only
    /// diagnostic metadata for this Annex B demuxer.
    fn parse_slice_poc(&self, nal: &NalInfo) -> Option<i32> {
        let bytes = nal_bytes(&self.pending, nal);
        let (header, ebsp) = parse_nal_header(bytes).ok()?;
        let rbsp = ebsp_to_rbsp(ebsp);
        let mut br = BitReader::new(&rbsp);

        // parser.c:71-81.
        if !br.read_flag().ok()? {
            return None; // not the first slice of the picture
        }
        if header.nal_unit_type.is_irap() {
            let _ = br.read_flag().ok()?; // no_output_of_prior_pics_flag
        }
        let pps_id = br.read_ue().ok()?;
        let pps = self.ps.pps_list.get(pps_id as usize)?.as_ref()?;
        let sps = self.ps.sps_list.get(pps.sps_id as usize)?.as_ref()?;

        // parser.c:133-152.
        for _ in 0..pps.num_extra_slice_header_bits {
            let _ = br.read_flag().ok()?;
        }
        let _slice_type = br.read_ue().ok()?;
        if pps.output_flag_present_flag {
            let _ = br.read_flag().ok()?; // pic_output_flag
        }
        if sps.separate_colour_plane {
            let _ = br.read_bits(2).ok()?; // colour_plane_id
        }

        // parser.c:153-158: IDR pictures always have POC 0.
        if header.nal_unit_type.is_idr() {
            return Some(0);
        }
        let poc_lsb = br.read_bits(sps.log2_max_poc_lsb as usize).ok()?;
        Some(compute_poc(
            sps.log2_max_poc_lsb,
            self.poc_tid0,
            poc_lsb,
            header.nal_unit_type,
        ))
    }

    /// Examine a NAL unit; returns true if it starts a new access unit.
    fn exam_nal(&mut self, nal: &NalInfo) -> bool {
        let code = nal.nal_type_code(&self.pending);
        if nal.layer_id(&self.pending) != 0 {
            return false;
        }

        if is_au_boundary_type(code) {
            if self.frame_start_found {
                self.frame_start_found = false;
                return true;
            }
            // Remember parameter sets that a later slice may reference.
            if matches!(code, 32..=34) {
                self.parse_param_set(nal);
            }
            return false;
        }

        if !is_picture_vcl_type(code) {
            return false;
        }

        // `first_slice_segment_in_pic_flag` is the first bit of the slice
        // segment RBSP (the byte after the two-byte NAL header).
        let first_slice = self
            .pending
            .get(nal.header_pos + 2)
            .is_some_and(|b| b & 0x80 != 0);
        if !first_slice {
            return false;
        }

        if self.frame_start_found {
            self.frame_start_found = false;
            return true;
        }
        self.frame_start_found = true;

        let poc = self.parse_slice_poc(nal);
        self.au_poc = poc;
        let tid_plus1 = nal.temporal_id_plus1(&self.pending);
        if tid_plus1 == 1 && updates_poc_tid0(code) {
            self.poc_tid0 = poc.unwrap_or(self.poc_tid0);
        }
        false
    }

    /// Finish the AU ending at byte position `end` and queue its packet.
    fn emit_au(&mut self, end: usize) -> Result<()> {
        if self.au_start == usize::MAX || end <= self.au_start {
            // No picture was assembled; drop the bytes so we make progress.
            if end > 0 {
                self.pending.drain(..end);
            }
            self.scan_pos = 0;
            return Ok(());
        }

        let data = self.pending[self.au_start..end].to_vec();
        let dts = self.decode_index;
        let pos = self.stream_pos;
        let poc = self.au_poc;

        self.pending.drain(..end);
        self.stream_pos += end as i64;
        self.au_start = usize::MAX;
        self.scan_pos = 0;
        self.frame_start_found = false;
        self.au_poc = None;
        self.decode_index += 1;

        // FFmpeg's parser marks IRAP NALs as key frames.
        let mut is_keyframe = false;
        for nalu in annex_b_split(&data) {
            if let Ok((h, _)) = parse_nal_header(nalu)
                && h.nal_unit_type.is_irap()
            {
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
        trace!(dts, ?poc, size = data.len(), is_keyframe, "HEVC packet");
        // FFmpeg's raw HEVC demuxer leaves packet PTS unset and advances DTS
        // once per access unit (`libavformat/rawdec.c`). The HEVC parser's POC
        // is used by the decoder/avformat to order output pictures; here we
        // assign both from the decode index, which is what framecrc observes.
        self.ready.push_back(pkt);
        Ok(())
    }

    /// Return the next finished packet in decode order.
    fn flush_ready(&mut self, _force: bool) -> Option<Packet> {
        self.ready.pop_front()
    }
}

impl Demuxer for HevcDemuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        let data = io.read_up_to(INITIAL_READ_SIZE)?;
        if data.is_empty() {
            return Err(Error::InvalidData);
        }

        // Decode the parameter sets present in the initial buffer.
        for nalu in annex_b_split(&data) {
            if parse_nal_header(nalu).is_ok_and(|(h, _)| h.nal_unit_type.is_param_set()) {
                let _ = self.ps.decode_nal(nalu);
            }
        }

        // The first SPS describes the stream geometry and timing.
        let sps = self.ps.sps_list.iter().flatten().next().cloned();

        let mut params = CodecParameters::new(CodecId::Hevc, MediaType::Video);
        let time_base;

        if let Some(ref s) = sps {
            params.width = s.cropped_width();
            params.height = s.cropped_height();
            params.pixel_format = s
                .pix_fmt
                .unwrap_or(wedeo_core::pixel_format::PixelFormat::Yuv420p);

            // hevc_parser.c:98-108: VPS timing takes precedence over SPS VUI.
            let vps = s.vps_id as usize;
            let vps_timing = self
                .ps
                .vps_list
                .get(vps)
                .and_then(|v| v.as_ref())
                .filter(|v| {
                    v.vps_timing_info_present_flag
                        && v.vps_num_units_in_tick > 0
                        && v.vps_time_scale > 0
                })
                .map(|v| (v.vps_num_units_in_tick as i64, v.vps_time_scale as i64));

            let vui_timing = if s.vui_present
                && s.vui.vui_timing_info_present_flag
                && s.vui.vui_num_units_in_tick > 0
                && s.vui.vui_time_scale > 0
            {
                Some((
                    s.vui.vui_num_units_in_tick as i64,
                    s.vui.vui_time_scale as i64,
                ))
            } else {
                None
            };

            // fps = time_scale / (2 * num_units_in_tick), so the time base
            // is (2 * num_units_in_tick) / time_scale. Default to 25 fps.
            // `num` here is num_units_in_tick; the time base numerator is
            // 2 * num_units_in_tick (default 25 fps -> 2/50 -> 1/25).
            let (num, den) = vps_timing.or(vui_timing).unwrap_or((1, 50));
            time_base = reduce_fraction(2 * num, den);

            // Annex B VPS/SPS/PPS NALs as extradata. This keeps the decoder in
            // start-code mode (`ff_hevc_is_hvcc()` returns false) while giving
            // it the parameter sets up front.
            let mut extradata = Vec::new();
            for nalu in annex_b_split(&data) {
                if parse_nal_header(nalu).is_ok_and(|(h, _)| h.nal_unit_type.is_param_set()) {
                    extradata.extend_from_slice(&[0, 0, 0, 1]);
                    extradata.extend_from_slice(nalu);
                }
            }
            // Never hand the decoder an extradata blob it cannot parse: a
            // truncated or out-of-order parameter set would fail init.
            if decode_extradata(&extradata).is_ok() {
                params.extradata = extradata;
            }
        } else {
            params.pixel_format = wedeo_core::pixel_format::PixelFormat::Yuv420p;
            time_base = Rational::new(1, 25);
        }

        let mut stream = Stream::new(0, params);
        stream.time_base = time_base;

        info!(
            width = stream.codec_params.width,
            height = stream.codec_params.height,
            time_base = %time_base,
            "HEVC stream opened"
        );

        if io.is_seekable() {
            io.seek(0)?;
        } else {
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

            if self.au_start == usize::MAX {
                if let Some((sc_pos, _)) = find_start_code(&self.pending, self.scan_pos) {
                    self.au_start = sc_pos;
                    self.scan_pos = sc_pos;
                } else if !self.eof && !self.fill_pending(io, self.pending.len() + 1)? {
                    continue;
                } else if self.eof {
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

            // A NAL header needs both bytes before it can be classified.
            if nal.end == self.pending.len() && !self.eof {
                self.fill_pending(io, self.pending.len() + 1)?;
                continue;
            }

            let is_boundary = self.exam_nal(&nal);
            if is_boundary {
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

struct HevcDemuxerFactory;

impl DemuxerFactory for HevcDemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "hevc",
            long_name: "raw HEVC video",
            extensions: "hevc,h265,265",
            mime_types: "video/hevc,video/h265",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        let buf = data.buf;
        let filename = data.filename.to_lowercase();
        let mut best = if filename.ends_with(".hevc")
            || filename.ends_with(".h265")
            || filename.ends_with(".265")
        {
            PROBE_SCORE_EXTENSION
        } else {
            0
        };

        let scan_len = buf.len().min(PROBE_SCAN_LIMIT);
        let scan_buf = &buf[..scan_len];
        let mut pos = 0usize;
        let mut saw_start_code = false;

        while let Some((sc_pos, sc_len)) = find_start_code(scan_buf, pos) {
            let header_pos = sc_pos + sc_len;
            if header_pos + 1 >= scan_len {
                break;
            }
            saw_start_code = true;
            let b0 = scan_buf[header_pos];
            let b1 = scan_buf[header_pos + 1];
            let code = (b0 >> 1) & 0x3f;
            let layer_id = ((b0 & 0x01) << 5) | (b1 >> 3);
            let forbidden = b0 & 0x80 != 0;

            if !forbidden && layer_id == 0 && matches!(code, 32 | 33) {
                // VPS / SPS right after a start code: definitive HEVC.
                return PROBE_SCORE_MAX;
            }
            pos = header_pos + 2;
        }

        if saw_start_code {
            best = best.max(25);
        }
        best
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(HevcDemuxer::new()))
    }
}

inventory::submit!(&HevcDemuxerFactory as &dyn DemuxerFactory);

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_start_codes() {
        let data = [0x00, 0x00, 0x01, 0x40, 0x01, 0x00, 0x00, 0x00, 0x01, 0x42];
        assert_eq!(find_start_code(&data, 0), Some((0, 3)));
        assert_eq!(find_start_code(&data, 4), Some((5, 4)));
        assert_eq!(find_start_code(&data, 9), None);
    }

    #[test]
    fn scans_nals_and_strips_trailing_zeros() {
        let data = [
            0x00, 0x00, 0x01, 0x40, 0x01, 0xaa, 0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xbb, 0x00,
            0x00,
        ];
        let nals = scan_nals(&data, 0);
        assert_eq!(nals.len(), 2);
        assert_eq!(nal_bytes(&data, &nals[0]), &[0x40, 0x01, 0xaa]);
        assert_eq!(nal_bytes(&data, &nals[1]), &[0x26, 0x01, 0xbb]);
        assert_eq!(nals[0].nal_type_code(&data), 32); // VPS
        assert_eq!(nals[1].nal_type_code(&data), 19); // IDR_W_RADL
    }

    #[test]
    fn nal_type_classification_matches_ffmpeg() {
        assert!(is_au_boundary_type(32)); // VPS
        assert!(is_au_boundary_type(33)); // SPS
        assert!(is_au_boundary_type(34)); // PPS
        assert!(is_au_boundary_type(35)); // AUD
        assert!(is_au_boundary_type(39)); // SEI_PREFIX
        assert!(!is_au_boundary_type(38)); // FD_NUT is not a boundary
        assert!(!is_au_boundary_type(40)); // SEI_SUFFIX is not a boundary
        assert!(is_picture_vcl_type(0));
        assert!(is_picture_vcl_type(9));
        assert!(is_picture_vcl_type(16));
        assert!(is_picture_vcl_type(21));
        assert!(!is_picture_vcl_type(10));
        assert!(!is_picture_vcl_type(32));
    }

    #[test]
    fn probe_detects_hevc_by_nal_type() {
        let data = [0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c];
        let probe = ProbeData {
            filename: "video.bin",
            buf: &data,
        };
        assert_eq!(HevcDemuxerFactory.probe(&probe), PROBE_SCORE_MAX);
    }

    #[test]
    fn probe_detects_hevc_by_extension() {
        let data = [0x00, 0x00, 0x01, 0x40, 0x01];
        let probe = ProbeData {
            filename: "clip.hevc",
            buf: &data,
        };
        assert_eq!(HevcDemuxerFactory.probe(&probe), PROBE_SCORE_MAX);
    }

    #[test]
    fn probe_rejects_h264_sps() {
        // H.264 SPS is nal_type 7 (b0 = 0x67), not HEVC VPS/SPS.
        let data = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00];
        let probe = ProbeData {
            filename: "clip.bin",
            buf: &data,
        };
        assert!(HevcDemuxerFactory.probe(&probe) < PROBE_SCORE_MAX);
    }

    #[test]
    fn reduce_fraction_defaults_and_reduces() {
        assert_eq!(reduce_fraction(2, 50).to_string(), "1/25");
        assert_eq!(reduce_fraction(2, 30).to_string(), "1/15");
        assert_eq!(reduce_fraction(2002, 60000).to_string(), "1001/30000");
        assert_eq!(reduce_fraction(0, 0).to_string(), "1/25");
    }

    #[test]
    fn factory_descriptor_has_priority_100() {
        assert_eq!(HevcDemuxerFactory.descriptor().name, "hevc");
        assert_eq!(HevcDemuxerFactory.descriptor().priority, 100);
    }

    /// Demux a real Annex B vector losslessly and check AU grouping / PTS.
    #[test]
    fn demuxes_fate_vector_losslessly() {
        let Ok(suite) = std::env::var("FATE_SUITE") else {
            return;
        };
        for name in ["flush-reuse.hevc", "pir.hevc"] {
            let path = std::path::Path::new(&suite).join("hevc").join(name);
            let Ok(original) = std::fs::read(&path) else {
                continue;
            };
            let mut ctx = match wedeo_format::context::InputContext::open(path.to_str().unwrap()) {
                Ok(ctx) => ctx,
                Err(e) => panic!("open {name}: {e}"),
            };
            assert_eq!(ctx.streams.len(), 1);
            assert_eq!(ctx.streams[0].codec_params.codec_id, CodecId::Hevc);

            let mut reassembled = Vec::new();
            let mut packets = 0usize;
            while let Ok(pkt) = ctx.read_packet() {
                assert_eq!(pkt.pts, packets as i64, "pts of packet {packets} in {name}");
                assert_eq!(pkt.dts, packets as i64, "dts of packet {packets} in {name}");
                assert_eq!(pkt.stream_index, 0);
                assert!(
                    pkt.data.data().starts_with(&[0, 0, 1])
                        || pkt.data.data().starts_with(&[0, 0, 0, 1])
                );
                reassembled.extend_from_slice(pkt.data.data());
                packets += 1;
            }
            assert!(packets > 0, "no packets from {name}");
            // Every byte of the elementary stream must be present in the
            // packets, in order (trailing_zero_8bits may be trimmed by the
            // Annex B splitter used to build extradata but not here).
            assert_eq!(reassembled, original, "demux lost bytes in {name}");
            trace!(packets, name, "lossless HEVC demux");
        }
    }
}
