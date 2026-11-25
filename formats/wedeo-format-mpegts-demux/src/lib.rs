#![forbid(unsafe_code)]
//! MPEG-2 Transport Stream (aka DVB) demuxer.
//!
//! Ported from FFmpeg's `libavformat/mpegts.c` (with `mpegts.h`) with the same
//! architecture: 188-byte packet synchronisation, PSI section reassembly for
//! PAT/PMT, PID-to-stream mapping, and a PES assembly state machine carrying
//! PTS/DTS.
//!
//! Fibre to the home:
//!  - `constants.rs` - FFmpeg's TS constants copied verbatim
//!  - `section.rs` - PSI section reassembly and CRC
//!  - `pes.rs` - PES assembly and timestamp parsing
//!  - this file - TS packet handling, PAT/PMT callbacks, `Demuxer` trait

mod constants;
mod pes;
mod section;

use std::collections::{HashMap, VecDeque};

use tracing::{debug, trace, warn};
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::Packet;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_format::demuxer::{
    Demuxer, DemuxerHeader, InputFormatDescriptor, InputFormatFlags, PROBE_SCORE_MAX, ProbeData,
    SeekFlags, Stream,
};
use wedeo_format::io::BufferedIo;
use wedeo_format::registry::DemuxerFactory;

use constants::{
    SYNC_BYTE, TS_DVHS_PACKET_SIZE, TS_FEC_PACKET_SIZE, TS_PACKET_SIZE, descriptor, stream_type,
};
use pes::PesContext;
use section::{SectionFilter, SectionKind, parse_section_header};

/// FFmpeg's `MpegTSContext::max_packet_size` default (204800).
const MAX_PACKET_SIZE: usize = 204800;
/// Number of TS packets to scan in `read_header()` looking for PAT/PMT.
const HEADER_SCAN_PACKETS: u64 = 20000;
/// `PROBE_PACKET_MAX_BUF` in FFmpeg `mpegts.c`.
const PROBE_PACKET_MAX_BUF: usize = 8192;

/// `MpegTSContext` in FFmpeg `mpegts.c`, reduced to the state required by
/// the synchronous `Demuxer` trait.
struct MpegTsDemuxer {
    /// Raw packet size, including FEC if present.
    raw_packet_size: usize,
    streams: Vec<Stream>,
    /// Elementary PID -> stream index.
    pid_to_stream: HashMap<u16, usize>,
    /// Stream index -> elementary PID.
    stream_to_pid: Vec<u16>,
    /// PMT PIDs advertised by the PAT.
    pmt_pids: Vec<u16>,
    pes: HashMap<u16, PesContext>,
    sections: HashMap<u16, SectionFilter>,
    pending_packets: VecDeque<Packet>,
    last_cc: [i8; constants::NB_PID_MAX],
    header_read: bool,
    last_pos: u64,
}

impl MpegTsDemuxer {
    fn new() -> Self {
        Self {
            raw_packet_size: TS_PACKET_SIZE,
            streams: Vec::new(),
            pid_to_stream: HashMap::new(),
            stream_to_pid: Vec::new(),
            pmt_pids: Vec::new(),
            pes: HashMap::new(),
            sections: HashMap::new(),
            pending_packets: VecDeque::new(),
            last_cc: [-1; constants::NB_PID_MAX],
            header_read: false,
            last_pos: 0,
        }
    }

    /// Read one raw TS packet (skipping FEC/DVHS trailers), resynchronising on
    /// `SYNC_BYTE` if the stream is misaligned. Returns the packet and its
    /// byte offset in the stream.
    fn read_ts_packet(&self, io: &mut BufferedIo) -> Result<Option<(Vec<u8>, u64)>> {
        let size = self.raw_packet_size;
        let data = io.read_up_to(size)?;
        if data.len() < size {
            // FFmpeg's read_packet() returns AVERROR_EOF on a short read.
            return Ok(None);
        }
        let end = io.tell()?;
        let pos = end.saturating_sub(size as u64);
        if data[0] == SYNC_BYTE {
            return Ok(Some((data, pos)));
        }

        // mpegts_resync(): look for the next sync byte.
        for _ in 0..constants::MAX_RESYNC_SIZE {
            let byte = match io.read_u8() {
                Ok(b) => b,
                Err(Error::Eof) => return Ok(None),
                Err(e) => return Err(e),
            };
            if byte == SYNC_BYTE {
                let mut packet = Vec::with_capacity(size);
                packet.push(byte);
                let rest = io.read_up_to(size - 1)?;
                if rest.len() < size - 1 {
                    return Ok(None);
                }
                packet.extend_from_slice(&rest);
                let end = io.tell()?;
                return Ok(Some((packet, end.saturating_sub(size as u64))));
            }
        }
        Err(Error::InvalidData)
    }

    /// `handle_packet()` in FFmpeg `mpegts.c`. `header_scan` is true while
    /// reading the initial scan: PES payloads are discarded there (FFmpeg has
    /// `ts->pkt == NULL` while scanning), but PSI sections are processed.
    fn handle_packet(&mut self, packet: &[u8], pos: u64, header_scan: bool) -> Result<()> {
        if packet.len() < TS_PACKET_SIZE {
            return Ok(());
        }
        let pid = ((u16::from(packet[1]) & 0x1f) << 8) | u16::from(packet[2]);
        let is_start = packet[1] & 0x40 != 0;
        let afc = (packet[3] >> 4) & 3;
        if afc == 0 {
            /* reserved value */
            return Ok(());
        }
        let has_adaptation = afc & 2 != 0;
        let has_payload = afc & 1 != 0;
        let is_discontinuity = has_adaptation
            && packet.get(4).copied().unwrap_or(0) != 0
            && packet.get(5).copied().unwrap_or(0) & 0x80 != 0;

        /* continuity check (currently not used except to mark corrupt) */
        let cc = i32::from(packet[3] & 0x0f);
        let last = i32::from(self.last_cc[pid as usize]);
        let expected_cc = if has_payload { (last + 1) & 0x0f } else { last };
        let cc_ok = pid == constants::NULL_PID || is_discontinuity || last < 0 || expected_cc == cc;
        self.last_cc[pid as usize] = cc as i8;
        if (!cc_ok || packet[1] & 0x80 != 0)
            && let Some(pes) = self.pes.get_mut(&pid)
        {
            pes.flags |= wedeo_core::packet::PacketFlags::CORRUPT;
        }

        let mut p = 4usize;
        if has_adaptation {
            if p >= packet.len() {
                return Ok(());
            }
            let af_len = packet[p] as usize;
            p += af_len + 1;
        }
        if p >= TS_PACKET_SIZE || !has_payload {
            return Ok(());
        }
        let payload = &packet[p..TS_PACKET_SIZE];
        let pes_pos = pos as i64 - self.raw_packet_size as i64;

        if self.sections.contains_key(&pid) {
            let mut sections = Vec::new();
            {
                let filter = self.sections.get_mut(&pid).unwrap();
                if is_start {
                    /* pointer field present */
                    let len = *payload.first().unwrap_or(&0) as usize;
                    if len > payload.len() - 1 {
                        return Ok(());
                    }
                    if len > 0 && cc_ok {
                        sections.extend(filter.write_data(&payload[1..1 + len], false));
                    }
                    if payload.len() > 1 + len && cc_ok {
                        sections.extend(filter.write_data(&payload[1 + len..], true));
                    }
                } else if cc_ok {
                    sections.extend(filter.write_data(payload, false));
                }
            }
            for section in sections {
                self.dispatch_section(pid, section);
            }
        } else if !header_scan && let Some(pes) = self.pes.get_mut(&pid) {
            for pkt in pes.push_data(payload, is_start, pes_pos, MAX_PACKET_SIZE) {
                self.pending_packets.push_back(pkt);
            }
        }
        Ok(())
    }

    /// Route a complete PSI section to the PAT or PMT callback.
    fn dispatch_section(&mut self, pid: u16, section: Vec<u8>) {
        let Some(h) = parse_section_header(&section) else {
            return;
        };
        let crc = u32::from_be_bytes(section[section.len() - 4..].try_into().unwrap());
        let Some(filter) = self.sections.get_mut(&pid) else {
            return;
        };
        if filter.skip_identical(h.version, crc) {
            return;
        }
        match filter.kind {
            SectionKind::Pat => self.handle_pat(&h, &section),
            SectionKind::Pmt => self.handle_pmt(&h, &section, pid),
        }
    }

    /// `pat_cb()` in FFmpeg `mpegts.c`.
    fn handle_pat(&mut self, h: &section::SectionHeader, section: &[u8]) {
        if h.tid != constants::PAT_TID || h.current_next == 0 {
            return;
        }
        let end = section.len() - 4;
        let mut p = 8usize;
        while p + 4 <= end {
            let sid = u16::from_be_bytes([section[p], section[p + 1]]);
            let pmt_pid = u16::from_be_bytes([section[p + 2], section[p + 3]]) & 0x1fff;
            p += 4;
            if pmt_pid <= 0x000f || pmt_pid == constants::NULL_PID {
                warn!(sid, pmt_pid, "Ignoring invalid PAT entry");
                continue;
            }
            if sid == 0x0000 {
                /* NIT info */
                continue;
            }
            trace!(sid, pmt_pid, "PAT entry");
            self.sections
                .entry(pmt_pid)
                .or_insert_with(|| SectionFilter::new(SectionKind::Pmt, true));
            if !self.pmt_pids.contains(&pmt_pid) {
                self.pmt_pids.push(pmt_pid);
            }
        }
    }

    /// `pmt_cb()` in FFmpeg `mpegts.c`.
    fn handle_pmt(&mut self, h: &section::SectionHeader, section: &[u8], _pid: u16) {
        if h.tid != constants::PMT_TID || h.current_next == 0 {
            return;
        }
        let end = section.len() - 4;
        let mut p = 8usize;
        if p + 2 > end {
            return;
        }
        let _pcr_pid = u16::from_be_bytes([section[p], section[p + 1]]) & 0x1fff;
        p += 2;
        if p + 2 > end {
            return;
        }
        let program_info_length = u16::from_be_bytes([section[p], section[p + 1]]) as usize & 0xfff;
        p += 2;
        let program_desc_end = (p + program_info_length).min(end);
        let mut prog_reg_desc = 0u32;
        while p + 2 <= program_desc_end {
            let tag = section[p];
            let len = section[p + 1] as usize;
            p += 2;
            if p + len > program_desc_end {
                break;
            }
            if tag == descriptor::REGISTRATION && len >= 4 {
                prog_reg_desc = u32::from_le_bytes(section[p..p + 4].try_into().unwrap());
            }
            p += len;
        }
        p = program_desc_end;

        while p + 5 <= end {
            let stream_type = section[p];
            let es_pid = u16::from_be_bytes([section[p + 1], section[p + 2]]) & 0x1fff;
            let es_info_len = u16::from_be_bytes([section[p + 3], section[p + 4]]) as usize & 0xfff;
            p += 5;
            let desc_end = (p + es_info_len).min(end);
            let mut es_reg_desc = 0u32;
            let mut language = None;
            let mut dp = p;
            while dp + 2 <= desc_end {
                let tag = section[dp];
                let len = section[dp + 1] as usize;
                dp += 2;
                if dp + len > desc_end {
                    break;
                }
                if tag == descriptor::REGISTRATION && len >= 4 {
                    es_reg_desc = u32::from_le_bytes(section[dp..dp + 4].try_into().unwrap());
                } else if tag == descriptor::ISO_639_LANGUAGE && len >= 3 {
                    language = Some(
                        section[dp..dp + 3]
                            .iter()
                            .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
                            .collect::<String>(),
                    );
                }
                dp += len;
            }
            p = desc_end;

            let (codec_id, media_type) =
                stream_type_to_codec(stream_type, prog_reg_desc, es_reg_desc);
            if codec_id == CodecId::None {
                trace!(stream_type, es_pid, "skipping unknown stream type");
                continue;
            }

            // Re-visiting a PMT (the header scan and then normal playback
            // both read it) must not duplicate streams.
            let stream_index = if let Some(&idx) = self.pid_to_stream.get(&es_pid) {
                idx
            } else {
                let mut params = CodecParameters::new(codec_id, media_type);
                params.codec_tag = u32::from(stream_type);
                params.time_base = Rational::new(1, 90000);
                if media_type == MediaType::Video {
                    params.pixel_format = PixelFormat::Yuv420p;
                }
                let mut stream = Stream::new(self.streams.len(), params);
                stream.time_base = Rational::new(1, 90000);
                if let Some(lang) = language {
                    stream.metadata.set("language", lang);
                }
                let idx = self.streams.len();
                self.streams.push(stream);
                self.pid_to_stream.insert(es_pid, idx);
                self.stream_to_pid.push(es_pid);
                self.pes.insert(es_pid, PesContext::new(Some(idx)));
                debug!(es_pid, idx, ?codec_id, "new MPEG-TS stream");
                idx
            };
            // A PMT version change can update the stream type of an existing PID.
            if let Some(stream) = self.streams.get_mut(stream_index) {
                stream.codec_params.codec_id = codec_id;
                stream.codec_params.media_type = media_type;
            }
        }
    }

    /// Flush partial PES payloads after a seek (FFmpeg `handle_packets()`).
    fn flush_pes(&mut self) {
        for pes in self.pes.values_mut() {
            pes.flush();
        }
    }
}

/// `mpegts_find_stream_type()` / `mpegts_set_stream_info()` in FFmpeg
/// `mpegts.c`, extended with AV1 signalled via a registration descriptor
/// (DVB "AV01" on a private-data stream).
fn stream_type_to_codec(
    stream_type: u8,
    prog_reg_desc: u32,
    es_reg_desc: u32,
) -> (CodecId, MediaType) {
    let reg_desc = if es_reg_desc != 0 {
        es_reg_desc
    } else {
        prog_reg_desc
    };
    // FFmpeg's REGD_types[] in `mpegts.c`, matched with AV_RL32 (little endian).
    let av1_reg = u32::from_le_bytes(*b"AV01");
    let opus_reg = u32::from_le_bytes(*b"Opus");
    let ac3_reg = u32::from_le_bytes(*b"AC-3");
    match stream_type {
        stream_type::VIDEO_MPEG1 | stream_type::VIDEO_MPEG2 => {
            (CodecId::Mpeg2video, MediaType::Video)
        }
        stream_type::AUDIO_MPEG1 | stream_type::AUDIO_MPEG2 => (CodecId::Mp3, MediaType::Audio),
        stream_type::AUDIO_AAC
        | stream_type::AUDIO_MPEG4
        | stream_type::AUDIO_AAC_LATM
        | stream_type::HLS_SE_AUDIO_AAC => (CodecId::Aac, MediaType::Audio),
        stream_type::VIDEO_H264 | stream_type::VIDEO_MVC | stream_type::HLS_SE_VIDEO_H264 => {
            (CodecId::H264, MediaType::Video)
        }
        stream_type::VIDEO_HEVC => (CodecId::Hevc, MediaType::Video),
        stream_type::VIDEO_MPEG4 => (CodecId::Mpeg4, MediaType::Video),
        stream_type::ATSC_AUDIO_AC3
        | stream_type::ATSC_AUDIO_EAC3
        | stream_type::HLS_SE_AUDIO_AC3
        | stream_type::HLS_SE_AUDIO_EAC3 => (CodecId::Ac3, MediaType::Audio),
        stream_type::PRIVATE_DATA if reg_desc == av1_reg => (CodecId::Av1, MediaType::Video),
        stream_type::PRIVATE_DATA if reg_desc == opus_reg => (CodecId::Opus, MediaType::Audio),
        stream_type::PRIVATE_DATA if reg_desc == ac3_reg => (CodecId::Ac3, MediaType::Audio),
        _ => (CodecId::None, MediaType::Data),
    }
}

/// `analyze()` in FFmpeg `mpegts.c`: score how many `SYNC_BYTE`s fall on a
/// given packet-size stride.
fn analyze(buf: &[u8], packet_size: usize, probe: bool) -> i32 {
    let mut stat = vec![0i32; packet_size];
    let mut stat_all = 0i32;
    for i in 0..buf.len().saturating_sub(3) {
        if buf[i] != SYNC_BYTE {
            continue;
        }
        let pid = u16::from_be_bytes([buf[i + 1], buf[i + 2]]) & 0x1fff;
        let asc = buf[i + 3] & 0x30;
        if !probe || pid == constants::NULL_PID || asc != 0 {
            let x = i % packet_size;
            stat[x] += 1;
            stat_all += 1;
        }
    }
    let best = stat.iter().copied().max().unwrap_or(0);
    best - (stat_all - 10 * best).max(0) / 10
}

/// `get_packet_size()` in FFmpeg `mpegts.c` (simplified: score the probe
/// buffer once instead of incrementally).
fn detect_packet_size(buf: &[u8]) -> usize {
    let mut best_size = TS_PACKET_SIZE;
    let mut best_score = i32::MIN;
    for size in [TS_PACKET_SIZE, TS_DVHS_PACKET_SIZE, TS_FEC_PACKET_SIZE] {
        let score = analyze(buf, size, false);
        if score > best_score {
            best_score = score;
            best_size = size;
        }
    }
    if best_score <= 0 {
        TS_PACKET_SIZE
    } else {
        best_size
    }
}

impl Demuxer for MpegTsDemuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        let pos = io.tell()?;
        let size = io.size().unwrap_or(0);
        let probe_len = if size > 0 {
            (size as usize).min(PROBE_PACKET_MAX_BUF * 2)
        } else {
            PROBE_PACKET_MAX_BUF
        };
        let probe = io.read_bytes(probe_len)?;
        self.raw_packet_size = detect_packet_size(&probe);
        io.seek(pos)?;

        trace!(raw_packet_size = self.raw_packet_size, "TS packet size");

        /* first do a scan to get all the services */
        self.sections.insert(
            constants::PAT_PID,
            SectionFilter::new(SectionKind::Pat, true),
        );

        let max_packets = if size > 0 {
            (size / self.raw_packet_size as u64).min(HEADER_SCAN_PACKETS)
        } else {
            HEADER_SCAN_PACKETS
        };
        let mut scanned = 0u64;
        while scanned < max_packets {
            let Some((packet, packet_pos)) = self.read_ts_packet(io)? else {
                break;
            };
            scanned += 1;
            self.handle_packet(&packet, packet_pos, true)?;
        }
        debug!(
            scanned,
            streams = self.streams.len(),
            "MPEG-TS header scan done"
        );

        seek_back(io, pos);
        self.last_pos = pos;
        self.header_read = true;

        Ok(DemuxerHeader {
            streams: self.streams.clone(),
            metadata: Metadata::new(),
            duration: 0,
            start_time: 0,
        })
    }

    fn read_packet(&mut self, io: &mut BufferedIo) -> Result<Packet> {
        if !self.header_read {
            return Err(Error::InvalidData);
        }
        if let Some(pkt) = self.pending_packets.pop_front() {
            return Ok(pkt);
        }

        if io.tell()? != self.last_pos {
            trace!("seek detected, flushing PES buffers");
            self.flush_pes();
        }

        loop {
            let (packet, packet_pos) = match self.read_ts_packet(io)? {
                Some(v) => v,
                None => {
                    /* flush pes data left, in stream order (FFmpeg walks PIDs) */
                    for i in 0..self.stream_to_pid.len() {
                        let pid = self.stream_to_pid[i];
                        if let Some(pes) = self.pes.get_mut(&pid)
                            && let Some(pkt) = pes.flush_packet()
                        {
                            self.pending_packets.push_back(pkt);
                        }
                    }
                    self.last_pos = io.tell()?;
                    return self.pending_packets.pop_front().ok_or(Error::Eof);
                }
            };
            self.handle_packet(&packet, packet_pos, false)?;
            self.last_pos = io.tell()?;
            if let Some(pkt) = self.pending_packets.pop_front() {
                return Ok(pkt);
            }
        }
    }

    fn seek(
        &mut self,
        io: &mut BufferedIo,
        stream_index: usize,
        timestamp: i64,
        flags: SeekFlags,
    ) -> Result<()> {
        if !self.header_read {
            return Err(Error::InvalidData);
        }
        if flags.contains(SeekFlags::BYTE) {
            let raw = self.raw_packet_size as u64;
            let pos = (timestamp as u64) - (timestamp as u64) % raw;
            seek_back(io, pos);
            self.flush_pes();
            self.last_pos = io.tell()?;
            return Ok(());
        }

        if self.stream_to_pid.get(stream_index).is_none() {
            return Err(Error::StreamNotFound);
        }
        seek_back(io, 0);
        self.flush_pes();
        let mut scanner = PesContext::new(Some(stream_index));
        let mut target_pos = None;
        while let Some((packet, packet_pos)) = self.read_ts_packet(io)? {
            let afc = (packet[3] >> 4) & 3;
            if afc & 1 == 0 {
                continue;
            }
            let mut p = 4usize;
            if afc & 2 != 0 {
                p += packet.get(4).copied().unwrap_or(0) as usize + 1;
            }
            if p >= TS_PACKET_SIZE {
                continue;
            }
            let is_start = packet[1] & 0x40 != 0;
            let pes_pos = packet_pos as i64 - self.raw_packet_size as i64;
            for pkt in scanner.push_data(
                &packet[p..TS_PACKET_SIZE],
                is_start,
                pes_pos,
                MAX_PACKET_SIZE,
            ) {
                if pkt.pts != wedeo_core::timestamp::NOPTS_VALUE && pkt.pts >= timestamp {
                    target_pos = Some(pkt.pos.max(0) as u64);
                    break;
                }
            }
            if target_pos.is_some() {
                break;
            }
        }
        self.flush_pes();
        let pos = target_pos.unwrap_or(0);
        seek_back(io, pos);
        self.last_pos = io.tell()?;
        Ok(())
    }
}

/// FFmpeg's `seek_back()`: attempt to seek even on non-seekable sources.
fn seek_back(io: &mut BufferedIo, pos: u64) {
    if let Err(e) = io.seek(pos) {
        warn!(error = %e, "Unable to seek back to the start");
    }
}

struct MpegTsDemuxerFactory;

impl DemuxerFactory for MpegTsDemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "mpegts",
            long_name: "MPEG-TS (MPEG-2 Transport Stream)",
            extensions: "ts,m2ts,mts",
            mime_types: "video/mp2t",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        let buf = data.buf;
        if buf.len() < TS_PACKET_SIZE * 3 {
            return 0;
        }
        // FFmpeg's mpegts_probe(): analyze at 188, 192 and 204 byte strides.
        let mut score = analyze(buf, TS_PACKET_SIZE, true);
        score = score.max(analyze(buf, TS_DVHS_PACKET_SIZE, true));
        score = score.max(analyze(buf, TS_FEC_PACKET_SIZE, true));
        if score <= 6 {
            return 0;
        }
        // A definite match: several sync bytes aligned on a TS stride. Native
        // implementations use 100 so we win probe tie-breaks.
        PROBE_SCORE_MAX
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(MpegTsDemuxer::new()))
    }
}

inventory::submit!(&MpegTsDemuxerFactory as &dyn DemuxerFactory);

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;

    use wedeo_format::io::BufferedIo;

    use super::*;

    /// Minimal in-memory `IoContext` for unit tests.
    struct MemIo {
        data: Vec<u8>,
        pos: u64,
    }

    impl wedeo_format::io::IoContext for MemIo {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let remaining = self.data.len().saturating_sub(self.pos as usize);
            let n = remaining.min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos as usize..self.pos as usize + n]);
            self.pos += n as u64;
            Ok(n)
        }
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            self.data.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(p) => p,
                SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
                SeekFrom::End(d) => (self.data.len() as i64 + d).max(0) as u64,
            };
            Ok(self.pos)
        }
        fn tell(&mut self) -> Result<u64> {
            Ok(self.pos)
        }
        fn size(&mut self) -> Result<u64> {
            Ok(self.data.len() as u64)
        }
        fn is_seekable(&self) -> bool {
            true
        }
    }

    fn mem_io(data: Vec<u8>) -> BufferedIo {
        BufferedIo::new(Box::new(MemIo { data, pos: 0 }))
    }

    /// Build one 188-byte TS packet with a payload.
    fn ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= TS_PACKET_SIZE - 4);
        let mut pkt = vec![0xffu8; TS_PACKET_SIZE]; // adaptation stuffing
        pkt[0] = SYNC_BYTE;
        pkt[1] = (if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1f);
        pkt[2] = pid as u8;
        pkt[3] = 0x10 | (cc & 0x0f); // payload only
        pkt[4..4 + payload.len()].copy_from_slice(payload);
        pkt
    }

    /// Build a section with CRC so `check_crc` succeeds.
    fn section(tid: u8, id: u16, body: &[u8]) -> Vec<u8> {
        let mut s = Vec::new();
        s.push(tid);
        let len = 5 + body.len() + 4; // section_syntax bytes + body + CRC
        s.push(0xb0 | ((len >> 8) as u8 & 0x0f));
        s.push(len as u8);
        s.extend_from_slice(&id.to_be_bytes());
        s.extend_from_slice(&[0xc1, 0x00, 0x00]); // version 0, current_next, sec 0/0
        s.extend_from_slice(body);
        let mut crc = 0xffff_ffffu32;
        for &byte in &s {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04c1_1db7
                } else {
                    crc << 1
                };
            }
        }
        s.extend_from_slice(&crc.to_be_bytes());
        s
    }

    fn pat(pmt_pid: u16) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&1u16.to_be_bytes()); // program_number
        body.extend_from_slice(&(0xe000u16 | pmt_pid).to_be_bytes());
        section(constants::PAT_TID, 1, &body)
    }

    fn pmt(entries: &[(u8, u16)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(0xe000u16 | 0x100).to_be_bytes()); // PCR PID
        body.extend_from_slice(&0xf000u16.to_be_bytes()); // program_info_length
        for &(st, pid) in entries {
            body.push(st);
            body.extend_from_slice(&(0xe000u16 | pid).to_be_bytes());
            body.extend_from_slice(&0xf000u16.to_be_bytes()); // ES_info_length
        }
        section(constants::PMT_TID, 1, &body)
    }

    fn pes(stream_id: u8, pts: i64, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0, 0, 1, stream_id];
        let total = 8 + payload.len();
        p.extend_from_slice(&(total as u16).to_be_bytes());
        p.extend_from_slice(&[0x80, 0x80, 0x05]);
        let v = pts as u64;
        p.extend_from_slice(&[
            0x21 | ((v >> 29) & 0x0e) as u8,
            ((v >> 22) & 0xff) as u8,
            ((v >> 14) & 0xff) as u8 | 0x01,
            ((v >> 7) & 0xff) as u8,
            ((v & 0x7f) as u8) << 1 | 0x01,
        ]);
        p.extend_from_slice(payload);
        p
    }

    fn ts_with_section(pid: u16, sec: &[u8]) -> Vec<u8> {
        let mut payload = vec![0u8]; // pointer_field
        payload.extend_from_slice(sec);
        ts_packet(pid, true, 0, &payload)
    }

    fn synthetic_stream() -> Vec<u8> {
        let mut data = Vec::new();
        // PAT, PMT with H264 (0x1b) on 0x100 and MP3 (0x03) on 0x101.
        data.extend_from_slice(&ts_with_section(constants::PAT_PID, &pat(0x1000)));
        data.extend_from_slice(&ts_with_section(
            0x1000,
            &pmt(&[(0x1b, 0x100), (0x03, 0x101)]),
        ));
        // One H264 PES split into two TS packets and one MP3 PES.
        // 14 header bytes + 270 payload bytes = 284; first TS packet carries
        // exactly 184 payload bytes (no stuffing), second carries 100.
        let mut long_payload = vec![0x55u8; 254];
        long_payload.extend_from_slice(b"h264 frame data!");
        let h264_data = pes(0xe0, 90000, &long_payload);
        let (a, b) = h264_data.split_at(184);
        data.extend_from_slice(&ts_packet(0x100, true, 0, a));
        data.extend_from_slice(&ts_packet(0x100, false, 1, b));
        data.extend_from_slice(&ts_packet(0x101, true, 0, &pes(0xc0, 90000, b"mp3 audio")));
        // Repeat the PAT a few times so probe() sees a strong TS signature.
        for cc in 1..8u8 {
            data.extend_from_slice(&ts_with_section(constants::PAT_PID, &pat(0x1000)));
            let _ = cc;
        }
        data
    }

    #[test]
    fn probe_detects_ts() {
        let data = synthetic_stream();
        let factory = MpegTsDemuxerFactory;
        assert_eq!(
            factory.probe(&ProbeData {
                filename: "x.ts",
                buf: &data
            }),
            PROBE_SCORE_MAX
        );
        assert_eq!(
            factory.probe(&ProbeData {
                filename: "x.ts",
                buf: b"not a transport stream at all"
            }),
            0
        );
    }

    #[test]
    fn parses_pat_pmt_and_pes() {
        let mut io = mem_io(synthetic_stream());
        let mut demuxer = MpegTsDemuxer::new();
        let header = demuxer.read_header(&mut io).unwrap();
        assert_eq!(header.streams.len(), 2);
        assert_eq!(header.streams[0].codec_params.codec_id, CodecId::H264);
        assert_eq!(header.streams[0].codec_params.media_type, MediaType::Video);
        assert_eq!(header.streams[1].codec_params.codec_id, CodecId::Mp3);
        assert_eq!(header.streams[1].codec_params.media_type, MediaType::Audio);
        assert_eq!(header.streams[0].time_base, Rational::new(1, 90000));

        let mut packets = Vec::new();
        loop {
            match demuxer.read_packet(&mut io) {
                Ok(pkt) => packets.push(pkt),
                Err(Error::Eof) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].stream_index, 0);
        let mut expected = vec![0x55u8; 254];
        expected.extend_from_slice(b"h264 frame data!");
        assert_eq!(packets[0].data.data(), expected);
        assert_eq!(packets[0].pts, 90000);
        assert_eq!(packets[0].dts, 90000);
        assert_eq!(packets[1].stream_index, 1);
        assert_eq!(packets[1].data.data(), b"mp3 audio");
    }

    #[test]
    fn detects_204_byte_packets() {
        let mut data = Vec::new();
        let sec = pat(0x1000);
        for _ in 0..8 {
            let mut pkt = ts_with_section(0, &sec);
            pkt.extend_from_slice(&[0xff; 16]); // FEC bytes
            data.extend_from_slice(&pkt);
        }
        assert_eq!(detect_packet_size(&data), TS_FEC_PACKET_SIZE);

        let mut io = mem_io(data);
        let mut demuxer = MpegTsDemuxer::new();
        let header = demuxer.read_header(&mut io).unwrap();
        assert_eq!(header.streams.len(), 0); // only PAT, no PMT
        assert_eq!(demuxer.raw_packet_size, TS_FEC_PACKET_SIZE);
    }

    #[test]
    fn av1_registration_descriptor() {
        assert_eq!(
            stream_type_to_codec(stream_type::PRIVATE_DATA, u32::from_le_bytes(*b"AV01"), 0).0,
            CodecId::Av1
        );
        assert_eq!(
            stream_type_to_codec(stream_type::VIDEO_H264, 0, 0).0,
            CodecId::H264
        );
        assert_eq!(
            stream_type_to_codec(stream_type::AUDIO_AAC, 0, 0).0,
            CodecId::Aac
        );
    }

    #[test]
    fn seeks_by_timestamp_and_byte() {
        let mut io = mem_io(synthetic_stream());
        let mut demuxer = MpegTsDemuxer::new();
        demuxer.read_header(&mut io).unwrap();

        demuxer.seek(&mut io, 0, 90000, SeekFlags::empty()).unwrap();
        let pkt = demuxer.read_packet(&mut io).unwrap();
        assert_eq!(pkt.stream_index, 0);
        assert_eq!(pkt.pts, 90000);
        assert_eq!(pkt.data.data().len(), 270);

        demuxer.seek(&mut io, 0, 0, SeekFlags::BYTE).unwrap();
        let pkt = demuxer.read_packet(&mut io).unwrap();
        assert_eq!(pkt.stream_index, 0);
        assert_eq!(pkt.pts, 90000);
    }

    #[test]
    fn registration_and_probe_are_linked() {
        let factory = wedeo_format::registry::find_demuxer_by_name("mpegts").unwrap();
        assert_eq!(factory.descriptor().name, "mpegts");
        assert_eq!(factory.descriptor().extensions, "ts,m2ts,mts");
        assert!(factory.create().is_ok());
    }

    #[test]
    fn detect_packet_size_defaults_to_188() {
        assert_eq!(detect_packet_size(b"garbage data"), TS_PACKET_SIZE);
    }
}
