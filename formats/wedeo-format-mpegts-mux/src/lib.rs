//! MPEG-2 Transport Stream muxer, translated from FFmpeg's
//! `libavformat/mpegtsenc.c`.
//!
//! Packets are 188 bytes long and start with the 0x47 sync byte. The muxer
//! emits the mandatory PSI tables (PAT, PMT, SDT) and wraps elementary
//! stream data in PES packets with PTS/DTS and PCR where applicable.
//!
//! Only the default, non-multiprogram, VBR mode of the FFmpeg muxer is
//! implemented: one DVB service, PMT on PID 0x1000, elementary streams on
//! PIDs starting at 0x0100, PCR carried on the first video stream. Options
//! such as `latm`, `system_b`, `m2ts`, `nit` and `resend_headers` are not
//! available through the frozen `Muxer` trait.

mod crc;

use crc::crc32_mpeg2;
use tracing::{debug, trace};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::{Rational, rescale_q};
use wedeo_core::timestamp::NOPTS_VALUE;
use wedeo_format::demuxer::Stream;
use wedeo_format::io::BufferedIo;
use wedeo_format::muxer::{Muxer, OutputFormatDescriptor, OutputFormatFlags};
use wedeo_format::registry::MuxerFactory;

// ---------------------------------------------------------------------------
// Constants (libavformat/mpegts.h and mpegtsenc.c)
// ---------------------------------------------------------------------------

const TS_PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const STUFFING_BYTE: u8 = 0xFF;

const PAT_PID: u16 = 0x0000;
const SDT_PID: u16 = 0x0011;
const PMT_START_PID: u16 = 0x1000;
const START_PID: u16 = 0x0100;

const PAT_TID: u8 = 0x00;
const PMT_TID: u8 = 0x02;
const SDT_TID: u8 = 0x42;

const PCR_TIME_BASE: i64 = 27_000_000;
const SYSTEM_CLOCK_FREQUENCY_DIVISOR: i64 = 300;
const PAT_PERIOD: i64 = 2_700_000; // 100 ms in the PCR time base
const SDT_PERIOD: i64 = 13_500_000; // 500 ms in the PCR time base

const DEFAULT_PES_HEADER_FREQ: usize = 16;
const DEFAULT_PES_PAYLOAD_SIZE: usize = (DEFAULT_PES_HEADER_FREQ - 1) * 184 + 170;

/// The ffmpeg CLI defaults its muxing delay to 0.7 s
/// (`fftools/ffmpeg_opt.c: o->mux_max_delay = 0.7`). Reproducing that default
/// keeps PTS/DTS and PCR values bitexact with `ffmpeg -f mpegts` output.
const CLI_MAX_DELAY_US: i64 = 700_000;
const DELAY_90K: i64 = CLI_MAX_DELAY_US * 90_000 / 1_000_000; // 63000
const MAX_AUDIO_DELAY_90K: i64 = DELAY_90K / 2; // 31500
const FIRST_PCR: i64 = CLI_MAX_DELAY_US * PCR_TIME_BASE / 1_000_000; // 18900000

const TIME_BASE_90K: Rational = Rational::new(1, 90_000);

// Stream ids (libavformat/mpegts.h)
const STREAM_ID_PRIVATE_STREAM_1: u8 = 0xBD;
const STREAM_ID_AUDIO_STREAM_0: u8 = 0xC0;
const STREAM_ID_VIDEO_STREAM_0: u8 = 0xE0;
const STREAM_ID_METADATA_STREAM: u8 = 0xFC;

// Stream types (ISO/IEC 13818-1 table 2-34)
const STREAM_TYPE_VIDEO_MPEG2: u8 = 0x02;
const STREAM_TYPE_AUDIO_MPEG1: u8 = 0x03;
const STREAM_TYPE_AUDIO_MPEG2: u8 = 0x04;
const STREAM_TYPE_PRIVATE_DATA: u8 = 0x06;
const STREAM_TYPE_AUDIO_AAC: u8 = 0x0F;
const STREAM_TYPE_VIDEO_MPEG4: u8 = 0x10;
const STREAM_TYPE_VIDEO_H264: u8 = 0x1B;
const STREAM_TYPE_VIDEO_HEVC: u8 = 0x24;

const SERVICE_DESCRIPTOR: u8 = 0x48;
const ISO_639_LANGUAGE_DESCRIPTOR: u8 = 0x0A;
const MPEGTS_SERVICE_TYPE_DIGITAL_TV: u8 = 0x01;
const DEFAULT_PROVIDER_NAME: &str = "FFmpeg";
const DEFAULT_SERVICE_NAME: &str = "Service";

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

/// Write one 188-byte transport packet and account for its size in the PCR
/// clock (FFmpeg's `write_packet()`).
fn write_ts_packet(
    io: &mut BufferedIo,
    total_size: &mut i64,
    data: &[u8; TS_PACKET_SIZE],
) -> Result<()> {
    io.write_bytes(data)?;
    *total_size = total_size.wrapping_add(TS_PACKET_SIZE as i64);
    Ok(())
}

/// Set an adaptation-field flag, creating a minimal adaptation field if one
/// is not present yet (FFmpeg's `set_af_flag()`).
fn set_af_flag(pkt: &mut [u8; TS_PACKET_SIZE], flag: u8) {
    if pkt[3] & 0x20 == 0 {
        pkt[3] |= 0x20;
        pkt[4] = 1;
        pkt[5] = 0;
    }
    pkt[5] |= flag;
}

/// Return the offset of the first payload byte (FFmpeg's
/// `get_ts_payload_start()`).
fn payload_start(pkt: &[u8; TS_PACKET_SIZE]) -> usize {
    if pkt[3] & 0x20 != 0 {
        5 + pkt[4] as usize
    } else {
        4
    }
}

/// Grow an existing adaptation field by `size` bytes (FFmpeg's `extend_af()`).
fn extend_af(pkt: &mut [u8; TS_PACKET_SIZE], size: usize) {
    pkt[4] = pkt[4].wrapping_add(size as u8);
}

/// Encode a PCR into 6 bytes (FFmpeg's `write_pcr_bits()`).
fn write_pcr_bits(buf: &mut [u8], pcr: i64) {
    let pcr_low = pcr.rem_euclid(SYSTEM_CLOCK_FREQUENCY_DIVISOR);
    let pcr_high = pcr.div_euclid(SYSTEM_CLOCK_FREQUENCY_DIVISOR) as u64;
    let pcr_low = pcr_low as u64;
    buf[0] = (pcr_high >> 25) as u8;
    buf[1] = (pcr_high >> 17) as u8;
    buf[2] = (pcr_high >> 9) as u8;
    buf[3] = (pcr_high >> 1) as u8;
    buf[4] = ((pcr_high << 7) | (pcr_low >> 8) | 0x7e) as u8;
    buf[5] = pcr_low as u8;
}

/// Encode a 33-bit PTS/DTS field into 5 bytes (FFmpeg's `write_pts()`).
fn write_pts(buf: &mut [u8], fourbits: u8, pts: i64) {
    let pts = pts as u64;
    let mut val = ((fourbits as u64) << 4) | (((pts >> 30) & 0x07) << 1) | 1;
    buf[0] = val as u8;
    val = (((pts >> 15) & 0x7fff) << 1) | 1;
    buf[1] = (val >> 8) as u8;
    buf[2] = val as u8;
    val = ((pts & 0x7fff) << 1) | 1;
    buf[3] = (val >> 8) as u8;
    buf[4] = val as u8;
}

/// Build a PSI section (table header + body + CRC), matching FFmpeg's
/// `mpegts_write_section1()`. `flags` is 0xb000 for PAT/PMT and 0xf000 for
/// SDT/NIT.
fn build_section(tid: u8, id: u16, version: u8, flags: u16, body: &[u8]) -> Vec<u8> {
    let total_len = 3 + 5 + body.len() + 4;
    let mut section = Vec::with_capacity(total_len);
    section.push(tid);
    section.extend_from_slice(&(flags | (body.len() + 5 + 4) as u16).to_be_bytes());
    section.extend_from_slice(&id.to_be_bytes());
    section.push(0xc1 | (version << 1)); // current_next_indicator = 1
    section.push(0); // section_number
    section.push(0); // last_section_number
    section.extend_from_slice(body);
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

/// Split a PSI section into 188-byte TS packets with a pointer field on the
/// first one (FFmpeg's `mpegts_write_section()`).
fn write_section(
    io: &mut BufferedIo,
    total_size: &mut i64,
    cc: &mut u8,
    pid: u16,
    section: &[u8],
) -> Result<()> {
    let mut offset = 0;
    let mut first = true;
    while offset < section.len() {
        let mut packet = [STUFFING_BYTE; TS_PACKET_SIZE];
        let mut q = 0;
        packet[q] = SYNC_BYTE;
        q += 1;
        let mut b = (pid >> 8) as u8 & 0x1f;
        if first {
            b |= 0x40; // payload_unit_start_indicator
        }
        packet[q] = b;
        q += 1;
        packet[q] = pid as u8;
        q += 1;
        *cc = cc.wrapping_add(1) & 0x0f;
        packet[q] = 0x10 | *cc; // payload only
        q += 1;
        if first {
            packet[q] = 0; // pointer_field
            q += 1;
        }
        let len = (TS_PACKET_SIZE - q).min(section.len() - offset);
        packet[q..q + len].copy_from_slice(&section[offset..offset + len]);
        offset += len;
        write_ts_packet(io, total_size, &packet)?;
        first = false;
    }
    Ok(())
}

/// Encode a string the way FFmpeg's `encode_str8()` does: length-prefixed,
/// with a 0x15 UTF-8 marker when the string has multibyte characters.
fn encode_str8(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() > 255 {
        return Err(Error::InvalidArgument);
    }
    let has_multibyte = bytes.iter().any(|&b| b >= 0x80);
    if has_multibyte {
        if bytes.len() > 254 {
            return Err(Error::InvalidArgument);
        }
        let mut out = Vec::with_capacity(bytes.len() + 2);
        out.push(bytes.len() as u8 + 1);
        out.push(0x15);
        out.extend_from_slice(bytes);
        Ok(out)
    } else {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
        Ok(out)
    }
}

/// FFmpeg's `get_dvb_stream_type()` for the codecs Wedeo can mux.
fn stream_type(codec_id: CodecId, sample_rate: u32) -> u8 {
    match codec_id {
        CodecId::Mpeg1video | CodecId::Mpeg2video => STREAM_TYPE_VIDEO_MPEG2,
        CodecId::Mpeg4 => STREAM_TYPE_VIDEO_MPEG4,
        CodecId::H264 => STREAM_TYPE_VIDEO_H264,
        CodecId::Hevc => STREAM_TYPE_VIDEO_HEVC,
        CodecId::Mp2 | CodecId::Mp3 => {
            if (1..32_000).contains(&sample_rate) {
                STREAM_TYPE_AUDIO_MPEG2
            } else {
                STREAM_TYPE_AUDIO_MPEG1
            }
        }
        CodecId::Aac => STREAM_TYPE_AUDIO_AAC,
        _ => STREAM_TYPE_PRIVATE_DATA,
    }
}

/// FFmpeg's `get_pes_stream_id()` for the stream types we support.
fn pes_stream_id(media_type: MediaType, codec_id: CodecId) -> u8 {
    match media_type {
        MediaType::Video => STREAM_ID_VIDEO_STREAM_0,
        MediaType::Audio if matches!(codec_id, CodecId::Mp2 | CodecId::Mp3 | CodecId::Aac) => {
            STREAM_ID_AUDIO_STREAM_0
        }
        MediaType::Audio => STREAM_ID_PRIVATE_STREAM_1,
        MediaType::Data => STREAM_ID_METADATA_STREAM,
        _ => STREAM_ID_PRIVATE_STREAM_1,
    }
}

// ---------------------------------------------------------------------------
// Per-stream and muxer state
// ---------------------------------------------------------------------------

/// Mirrors FFmpeg's `MpegTSWriteStream` for the fields we use.
struct StreamState {
    pid: u16,
    cc: u8,
    media_type: MediaType,
    time_base: Rational,
    stream_type: u8,
    stream_id: u8,
    /// ISO-639 language descriptor for the PMT, if the stream declares one.
    language_descriptor: Option<Vec<u8>>,
    /// Buffered audio payload (FFmpeg accumulates small audio packets).
    payload: Vec<u8>,
    payload_pts: i64,
    payload_dts: i64,
    payload_flags: PacketFlags,
    prev_payload_key: bool,
    first_timestamp_checked: bool,
    /// PCR period in the 27 MHz time base; 0 disables PCR generation.
    pcr_period: i64,
    last_pcr: i64,
}

struct MpegTsMuxer {
    streams: Vec<StreamState>,
    pat_cc: u8,
    sdt_cc: u8,
    pmt_cc: u8,
    transport_stream_id: u16,
    original_network_id: u16,
    service_id: u16,
    tables_version: u8,
    pmt_pid: u16,
    pcr_pid: u16,
    encoded_service_name: Vec<u8>,
    encoded_provider_name: Vec<u8>,
    pes_payload_size: usize,
    first_pcr: i64,
    first_dts_checked: bool,
    total_size: i64,
    last_pat_ts: i64,
    last_sdt_ts: i64,
    pat_section: Vec<u8>,
    pmt_section: Vec<u8>,
    sdt_section: Vec<u8>,
}

impl MpegTsMuxer {
    fn new() -> Self {
        Self {
            streams: Vec::new(),
            pat_cc: 15,
            sdt_cc: 15,
            pmt_cc: 15,
            transport_stream_id: 0x0001,
            original_network_id: 0xff01, // DVB_PRIVATE_NETWORK_START
            service_id: 0x0001,
            tables_version: 0,
            pmt_pid: PMT_START_PID,
            pcr_pid: 0x1fff,
            encoded_service_name: Vec::new(),
            encoded_provider_name: Vec::new(),
            pes_payload_size: (DEFAULT_PES_PAYLOAD_SIZE + 14).div_ceil(184) * 184 - 14,
            first_pcr: FIRST_PCR,
            first_dts_checked: false,
            total_size: 0,
            last_pat_ts: NOPTS_VALUE,
            last_sdt_ts: NOPTS_VALUE,
            pat_section: Vec::new(),
            pmt_section: Vec::new(),
            sdt_section: Vec::new(),
        }
    }

    /// Build the PAT/PMT/SDT sections. FFmpeg emits them lazily on the first
    /// PES packet; we only prepare them here.
    fn build_sections(&mut self) -> Result<()> {
        // PAT: program_number -> PMT pid.
        let mut pat_body = Vec::with_capacity(4);
        pat_body.extend_from_slice(&self.service_id.to_be_bytes());
        pat_body.extend_from_slice(&(0xe000 | self.pmt_pid).to_be_bytes());
        self.pat_section = build_section(
            PAT_TID,
            self.transport_stream_id,
            self.tables_version,
            0xb000,
            &pat_body,
        );

        // PMT: PCR pid, program_info_length = 0, then one entry per stream.
        let mut pmt_body = Vec::with_capacity(5 + 5 * self.streams.len());
        pmt_body.extend_from_slice(&(0xe000 | self.pcr_pid).to_be_bytes());
        pmt_body.extend_from_slice(&0xf000u16.to_be_bytes());
        for st in &self.streams {
            pmt_body.push(st.stream_type);
            pmt_body.extend_from_slice(&(0xe000 | st.pid).to_be_bytes());
            let desc_len = st
                .language_descriptor
                .as_ref()
                .map_or(0, |d| d.len() as u16);
            pmt_body.extend_from_slice(&(0xf000 | desc_len).to_be_bytes());
            if let Some(desc) = &st.language_descriptor {
                pmt_body.extend_from_slice(desc);
            }
        }
        self.pmt_section = build_section(
            PMT_TID,
            self.service_id,
            self.tables_version,
            0xb000,
            &pmt_body,
        );

        // SDT: one service descriptor with provider and service name.
        let mut desc = Vec::with_capacity(
            3 + self.encoded_provider_name.len() + self.encoded_service_name.len(),
        );
        desc.push(SERVICE_DESCRIPTOR);
        desc.push((1 + self.encoded_provider_name.len() + self.encoded_service_name.len()) as u8);
        desc.push(MPEGTS_SERVICE_TYPE_DIGITAL_TV);
        desc.extend_from_slice(&self.encoded_provider_name);
        desc.extend_from_slice(&self.encoded_service_name);

        let mut sdt_body = Vec::with_capacity(3 + 5 + desc.len());
        sdt_body.extend_from_slice(&self.original_network_id.to_be_bytes());
        sdt_body.push(0xff); // reserved_future_use
        sdt_body.extend_from_slice(&self.service_id.to_be_bytes());
        sdt_body.push(0xfc); // no EIT information
        // running_status = 4 ("running") gives the top bit; free_ca_mode = 0.
        let desc_list_len = 0x8000u16 | desc.len() as u16;
        sdt_body.extend_from_slice(&desc_list_len.to_be_bytes());
        sdt_body.extend_from_slice(&desc);
        self.sdt_section = build_section(
            SDT_TID,
            self.transport_stream_id,
            self.tables_version,
            0xf000,
            &sdt_body,
        );
        Ok(())
    }

    /// FFmpeg's `retransmit_si_info()`: send SDT, then PAT and PMT when their
    /// retransmission period has elapsed (or on the first PES packet).
    fn retransmit_si_info(&mut self, io: &mut BufferedIo, force_pat: bool, pcr: i64) -> Result<()> {
        if (pcr != NOPTS_VALUE && self.last_sdt_ts == NOPTS_VALUE)
            || (pcr != NOPTS_VALUE && pcr.wrapping_sub(self.last_sdt_ts) >= SDT_PERIOD)
        {
            if pcr != NOPTS_VALUE {
                self.last_sdt_ts = pcr.max(self.last_sdt_ts);
            }
            let section = self.sdt_section.clone();
            write_section(
                io,
                &mut self.total_size,
                &mut self.sdt_cc,
                SDT_PID,
                &section,
            )?;
        }
        if (pcr != NOPTS_VALUE && self.last_pat_ts == NOPTS_VALUE)
            || (pcr != NOPTS_VALUE && pcr.wrapping_sub(self.last_pat_ts) >= PAT_PERIOD)
            || force_pat
        {
            if pcr != NOPTS_VALUE {
                self.last_pat_ts = pcr.max(self.last_pat_ts);
            }
            let pat = self.pat_section.clone();
            write_section(io, &mut self.total_size, &mut self.pat_cc, PAT_PID, &pat)?;
            let pmt = self.pmt_section.clone();
            write_section(
                io,
                &mut self.total_size,
                &mut self.pmt_cc,
                self.pmt_pid,
                &pmt,
            )?;
        }
        Ok(())
    }

    /// Derive the PCR period from the first video packet duration when no
    /// frame rate is available (FFmpeg uses `avg_frame_rate`, which Wedeo's
    /// `Stream` does not expose).
    fn ensure_pcr_period(&mut self, si: usize, packet: &Packet) {
        let st = &mut self.streams[si];
        if st.pcr_period != 0 || st.media_type != MediaType::Video {
            return;
        }
        let frame_period = if packet.duration > 0 {
            rescale_q(packet.duration, st.time_base, TIME_BASE_90K)
                .wrapping_mul(SYSTEM_CLOCK_FREQUENCY_DIVISOR)
        } else {
            0
        };
        st.pcr_period = if frame_period > 0 && frame_period <= PCR_TIME_BASE / 10 {
            frame_period * (PCR_TIME_BASE / 10 / frame_period)
        } else {
            1
        };
        st.last_pcr = self.first_pcr.wrapping_sub(st.pcr_period);
    }

    /// FFmpeg's `mpegts_write_pes()`: prepend a PES header and segment the
    /// payload into whole 188-byte TS packets, padding the last one with an
    /// oversized adaptation field.
    fn write_pes(
        &mut self,
        io: &mut BufferedIo,
        si: usize,
        payload: &[u8],
        pts: i64,
        dts: i64,
        key: bool,
    ) -> Result<()> {
        let mut force_pat = self.streams[si].media_type == MediaType::Video
            && key
            && !self.streams[si].prev_payload_key;
        let mut is_start = true;
        let mut pos = 0usize;
        let mut remaining = payload.len();

        while remaining > 0 {
            // In VBR mode FFmpeg derives the PCR from the (delay-adjusted)
            // DTS of the packet.
            let pcr = if dts != NOPTS_VALUE {
                dts.wrapping_sub(DELAY_90K)
                    .wrapping_mul(SYSTEM_CLOCK_FREQUENCY_DIVISOR)
            } else {
                NOPTS_VALUE
            };
            self.retransmit_si_info(io, force_pat, pcr)?;
            force_pat = false;

            let st = &mut self.streams[si];
            let mut write_pcr = false;
            if st.pcr_period != 0
                && pcr != NOPTS_VALUE
                && pcr.wrapping_sub(st.last_pcr) >= st.pcr_period
                && is_start
            {
                st.last_pcr =
                    (pcr.wrapping_sub(st.pcr_period)).max(st.last_pcr.wrapping_add(st.pcr_period));
                write_pcr = true;
            }

            // Prepare the TS packet header.
            let mut buf = [STUFFING_BYTE; TS_PACKET_SIZE];
            let mut q = 0usize;
            buf[q] = SYNC_BYTE;
            q += 1;
            let mut val = (st.pid >> 8) as u8 & 0x1f;
            if is_start {
                val |= 0x40;
            }
            buf[q] = val;
            q += 1;
            buf[q] = st.pid as u8;
            q += 1;
            st.cc = st.cc.wrapping_add(1) & 0x0f;
            buf[q] = 0x10 | st.cc; // payload present + continuity counter
            q += 1;

            // Random access indicator for key frames.
            if key && is_start && pts != NOPTS_VALUE {
                if st.pcr_period != 0 {
                    write_pcr = true;
                }
                set_af_flag(&mut buf, 0x40);
                q = payload_start(&buf);
            }
            if write_pcr {
                set_af_flag(&mut buf, 0x10);
                q = payload_start(&buf);
                write_pcr_bits(&mut buf[q..], pcr);
                extend_af(&mut buf, 6);
                q = payload_start(&buf);
            }

            if is_start {
                // PES header: start code prefix + stream id.
                buf[q] = 0x00;
                buf[q + 1] = 0x00;
                buf[q + 2] = 0x01;
                buf[q + 3] = st.stream_id;
                q += 4;

                let mut header_len = 0usize;
                let mut flags = 0u8;
                if pts != NOPTS_VALUE {
                    header_len += 5;
                    flags |= 0x80;
                }
                if dts != NOPTS_VALUE && pts != NOPTS_VALUE && dts != pts {
                    header_len += 5;
                    flags |= 0x40;
                }
                let mut len = remaining + header_len + 3;
                if len > 0xffff {
                    len = 0;
                }
                // FFmpeg omits the PES packet length for video by default.
                if st.media_type == MediaType::Video {
                    len = 0;
                }
                buf[q] = (len >> 8) as u8;
                q += 1;
                buf[q] = len as u8;
                q += 1;
                buf[q] = 0x80; // '10' marker bits
                q += 1;
                buf[q] = flags;
                q += 1;
                buf[q] = header_len as u8;
                q += 1;
                if pts != NOPTS_VALUE {
                    write_pts(&mut buf[q..], flags >> 6, pts);
                    q += 5;
                }
                if dts != NOPTS_VALUE && pts != NOPTS_VALUE && dts != pts {
                    write_pts(&mut buf[q..], 1, dts);
                    q += 5;
                }
                is_start = false;
            }

            let header_len = q;
            let len = (TS_PACKET_SIZE - header_len).min(remaining);
            let stuffing_len = TS_PACKET_SIZE - header_len - len;
            if stuffing_len > 0 {
                if buf[3] & 0x20 != 0 {
                    // An adaptation field is already present: grow it.
                    let afc_len = buf[4] as usize + 1;
                    buf.copy_within(4 + afc_len..header_len, 4 + afc_len + stuffing_len);
                    buf[4] = buf[4].wrapping_add(stuffing_len as u8);
                    buf[4 + afc_len..4 + afc_len + stuffing_len].fill(STUFFING_BYTE);
                } else {
                    // Add an adaptation field containing only stuffing.
                    buf.copy_within(4..header_len, 4 + stuffing_len);
                    buf[3] |= 0x20;
                    buf[4] = (stuffing_len - 1) as u8;
                    if stuffing_len >= 2 {
                        buf[5] = 0x00;
                        buf[6..4 + stuffing_len].fill(STUFFING_BYTE);
                    }
                }
            }
            buf[TS_PACKET_SIZE - len..].copy_from_slice(&payload[pos..pos + len]);
            pos += len;
            remaining -= len;
            write_ts_packet(io, &mut self.total_size, &buf)?;
            trace!(
                si,
                pid = st.pid,
                cc = st.cc,
                len,
                stuffing_len,
                "MPEG-TS packet written"
            );
        }
        self.streams[si].prev_payload_key = key;
        Ok(())
    }

    /// FFmpeg's `mpegts_write_flush()`: emit any buffered audio payloads.
    fn flush(&mut self, io: &mut BufferedIo) -> Result<()> {
        for si in 0..self.streams.len() {
            if self.streams[si].payload.is_empty() {
                continue;
            }
            let payload = std::mem::take(&mut self.streams[si].payload);
            let pts = self.streams[si].payload_pts;
            let dts = self.streams[si].payload_dts;
            let key = self.streams[si].payload_flags.contains(PacketFlags::KEY);
            self.write_pes(io, si, &payload, pts, dts, key)?;
        }
        Ok(())
    }
}

impl Muxer for MpegTsMuxer {
    fn write_header(&mut self, _io: &mut BufferedIo, streams: &[Stream]) -> Result<()> {
        self.encoded_provider_name = encode_str8(DEFAULT_PROVIDER_NAME)?;
        self.encoded_service_name = encode_str8(&format!("{DEFAULT_SERVICE_NAME}01"))?;

        for (i, stream) in streams.iter().enumerate() {
            let par = &stream.codec_params;
            let media_type = par.media_type;
            let mut language_descriptor = None;
            if media_type == MediaType::Audio
                && let Some(lang) = stream.metadata.get("language")
                && lang != "und"
            {
                let codes: Vec<&str> = lang.split(',').filter(|c| c.len() == 3).collect();
                if !codes.is_empty() {
                    let mut desc = vec![ISO_639_LANGUAGE_DESCRIPTOR, (codes.len() * 4) as u8];
                    for code in codes {
                        desc.extend_from_slice(code.as_bytes());
                        desc.push(0); // undefined audio type
                    }
                    language_descriptor = Some(desc);
                }
            }

            self.streams.push(StreamState {
                pid: START_PID.wrapping_add(i as u16),
                cc: 15,
                media_type,
                time_base: stream.time_base,
                stream_type: stream_type(par.codec_id, par.sample_rate),
                stream_id: pes_stream_id(media_type, par.codec_id),
                language_descriptor,
                payload: Vec::with_capacity(self.pes_payload_size),
                payload_pts: NOPTS_VALUE,
                payload_dts: NOPTS_VALUE,
                payload_flags: PacketFlags::empty(),
                prev_payload_key: false,
                first_timestamp_checked: false,
                pcr_period: 0,
                last_pcr: 0,
            });
        }

        // PCR is carried on the first video stream, as in FFmpeg's
        // `select_pcr_streams()`.
        if let Some(st) = self
            .streams
            .iter()
            .find(|s| s.media_type == MediaType::Video)
        {
            self.pcr_pid = st.pid;
        }

        self.build_sections()?;
        debug!(
            streams = streams.len(),
            pmt_pid = self.pmt_pid,
            pcr_pid = self.pcr_pid,
            "MPEG-TS muxer initialized"
        );
        Ok(())
    }

    fn write_packet(&mut self, io: &mut BufferedIo, packet: &Packet) -> Result<()> {
        let si = packet.stream_index;
        if si >= self.streams.len() {
            return Err(Error::StreamNotFound);
        }
        let data = packet.data.data();
        if data.is_empty() {
            return Ok(());
        }

        let time_base = self.streams[si].time_base;
        let mut pts = if packet.pts == NOPTS_VALUE {
            NOPTS_VALUE
        } else {
            rescale_q(packet.pts, time_base, TIME_BASE_90K)
        };
        let mut dts = if packet.dts == NOPTS_VALUE {
            NOPTS_VALUE
        } else {
            rescale_q(packet.dts, time_base, TIME_BASE_90K)
        };

        if !self.first_dts_checked && dts != NOPTS_VALUE {
            self.first_pcr = self
                .first_pcr
                .wrapping_add(dts.wrapping_mul(SYSTEM_CLOCK_FREQUENCY_DIVISOR));
            self.first_dts_checked = true;
        }

        if pts != NOPTS_VALUE {
            pts = pts.wrapping_add(DELAY_90K * 2);
        }
        if dts != NOPTS_VALUE {
            dts = dts.wrapping_add(DELAY_90K * 2);
        }

        if !self.streams[si].first_timestamp_checked && (pts == NOPTS_VALUE || dts == NOPTS_VALUE) {
            return Err(Error::InvalidArgument);
        }
        self.streams[si].first_timestamp_checked = true;

        self.ensure_pcr_period(si, packet);

        let key = packet.flags.contains(PacketFlags::KEY);
        let is_audio = self.streams[si].media_type == MediaType::Audio;
        if !is_audio || data.len() > self.pes_payload_size {
            self.write_pes(io, si, data, pts, dts, key)?;
            return Ok(());
        }

        // Audio: accumulate small packets into one PES, like FFmpeg does.
        let st = &self.streams[si];
        let flush = !st.payload.is_empty()
            && (st.payload.len() + data.len() > self.pes_payload_size
                || (dts != NOPTS_VALUE
                    && st.payload_dts != NOPTS_VALUE
                    && dts.wrapping_sub(st.payload_dts) >= MAX_AUDIO_DELAY_90K));
        if flush {
            let buffered = std::mem::take(&mut self.streams[si].payload);
            let payload_pts = self.streams[si].payload_pts;
            let payload_dts = self.streams[si].payload_dts;
            let payload_key = self.streams[si].payload_flags.contains(PacketFlags::KEY);
            self.write_pes(io, si, &buffered, payload_pts, payload_dts, payload_key)?;
        }
        let st = &mut self.streams[si];
        if st.payload.is_empty() {
            st.payload_pts = pts;
            st.payload_dts = dts;
            st.payload_flags = packet.flags;
        }
        st.payload.extend_from_slice(data);
        Ok(())
    }

    fn write_trailer(&mut self, io: &mut BufferedIo) -> Result<()> {
        self.flush(io)?;
        io.flush()
    }
}

struct MpegTsMuxerFactory;

impl MuxerFactory for MpegTsMuxerFactory {
    fn descriptor(&self) -> &OutputFormatDescriptor {
        static DESC: OutputFormatDescriptor = OutputFormatDescriptor {
            name: "mpegts",
            long_name: "MPEG-TS (MPEG-2 Transport Stream)",
            extensions: "ts,m2ts,mts,m2t",
            mime_types: "video/MP2T",
            flags: OutputFormatFlags::VARIABLE_FPS.union(OutputFormatFlags::NODIMENSIONS),
            audio_codec: CodecId::Mp2,
            video_codec: CodecId::Mpeg2video,
        };
        &DESC
    }

    fn create(&self) -> Result<Box<dyn Muxer>> {
        Ok(Box::new(MpegTsMuxer::new()))
    }
}

inventory::submit!(&MpegTsMuxerFactory as &dyn MuxerFactory);

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;
    use std::sync::{Arc, Mutex};

    use wedeo_codec::decoder::CodecParameters;
    use wedeo_core::media_type::MediaType;
    use wedeo_format::io::{BufferedIo, IoContext};

    use super::*;

    struct MemoryIo {
        shared: Arc<Mutex<Vec<u8>>>,
        pos: usize,
    }

    impl MemoryIo {
        fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
            let shared = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    shared: Arc::clone(&shared),
                    pos: 0,
                },
                shared,
            )
        }
    }

    impl IoContext for MemoryIo {
        fn read(&mut self, _buf: &mut [u8]) -> Result<usize> {
            Ok(0)
        }
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            let mut shared = self.shared.lock().unwrap();
            let end = self.pos + buf.len();
            if end > shared.len() {
                shared.resize(end, 0);
            }
            shared[self.pos..end].copy_from_slice(buf);
            self.pos = end;
            Ok(buf.len())
        }
        fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(p) => p as usize,
                _ => self.pos,
            };
            Ok(self.pos as u64)
        }
        fn tell(&mut self) -> Result<u64> {
            Ok(self.pos as u64)
        }
        fn size(&mut self) -> Result<u64> {
            Ok(self.shared.lock().unwrap().len() as u64)
        }
        fn is_seekable(&self) -> bool {
            true
        }
    }

    fn pid(packet: &[u8]) -> u16 {
        (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16
    }

    fn stream(codec_id: CodecId, media_type: MediaType, sample_rate: u32) -> Stream {
        let params = CodecParameters::new(codec_id, media_type);
        let mut params = params;
        params.sample_rate = sample_rate;
        let mut stream = Stream::new(0, params);
        stream.time_base = Rational::new(1, 90_000);
        stream
    }

    fn mux(streams: &[Stream], packets: &[(usize, i64, i64, i64, u8, PacketFlags)]) -> Vec<u8> {
        let (io, shared) = MemoryIo::new();
        let mut io = BufferedIo::new(Box::new(io));
        let mut muxer = MpegTsMuxer::new();
        muxer.write_header(&mut io, streams).unwrap();
        for (si, pts, dts, dur, byte, flags) in packets {
            let mut packet = Packet::from_slice(&[*byte; 16]);
            packet.stream_index = *si;
            packet.pts = *pts;
            packet.dts = *dts;
            packet.duration = *dur;
            packet.flags = *flags;
            muxer.write_packet(&mut io, &packet).unwrap();
        }
        muxer.write_trailer(&mut io).unwrap();
        io.flush().unwrap();
        shared.lock().unwrap().clone()
    }

    #[test]
    fn all_packets_are_188_bytes_with_sync_byte() {
        let out = mux(
            &[stream(CodecId::Mpeg2video, MediaType::Video, 0)],
            &[
                (0, 0, 0, 3600, 0xaa, PacketFlags::KEY),
                (0, 3600, 3600, 3600, 0xbb, PacketFlags::empty()),
            ],
        );
        assert!(!out.is_empty());
        assert_eq!(out.len() % TS_PACKET_SIZE, 0);
        for packet in out.chunks(TS_PACKET_SIZE) {
            assert_eq!(packet[0], SYNC_BYTE);
        }
    }

    #[test]
    fn first_three_packets_are_sdt_pat_pmt() {
        let out = mux(
            &[stream(CodecId::H264, MediaType::Video, 0)],
            &[(0, 0, 0, 3000, 0xaa, PacketFlags::KEY)],
        );
        assert_eq!(pid(&out[..TS_PACKET_SIZE]), SDT_PID);
        let start = TS_PACKET_SIZE;
        assert_eq!(pid(&out[start..start + TS_PACKET_SIZE]), PAT_PID);
        let start = 2 * TS_PACKET_SIZE;
        assert_eq!(pid(&out[start..start + TS_PACKET_SIZE]), PMT_START_PID);
    }

    #[test]
    fn continuity_counters_wrap_from_zero() {
        let out = mux(
            &[stream(CodecId::Mpeg2video, MediaType::Video, 0)],
            &[
                (0, 0, 0, 3600, 0xaa, PacketFlags::KEY),
                (0, 3600, 3600, 3600, 0xbb, PacketFlags::empty()),
            ],
        );
        // SDT, PAT, PMT all start at CC 0; the video PES then starts at CC 0.
        for i in 0..4 {
            assert_eq!(out[i * TS_PACKET_SIZE + 3] & 0x0f, 0);
        }
        assert_eq!(out[4 * TS_PACKET_SIZE + 3] & 0x0f, 1);
    }

    #[test]
    fn stream_ids_and_types_for_h264_aac_mp3() {
        let cases = [
            (
                CodecId::H264,
                MediaType::Video,
                0u32,
                0x1bu8,
                STREAM_ID_VIDEO_STREAM_0,
            ),
            (
                CodecId::Aac,
                MediaType::Audio,
                48_000,
                0x0f,
                STREAM_ID_AUDIO_STREAM_0,
            ),
            (
                CodecId::Mp3,
                MediaType::Audio,
                44_100,
                0x03,
                STREAM_ID_AUDIO_STREAM_0,
            ),
            (
                CodecId::Mp3,
                MediaType::Audio,
                22_050,
                0x04,
                STREAM_ID_AUDIO_STREAM_0,
            ),
        ];
        for (codec_id, media_type, rate, expected_type, expected_id) in cases {
            let out = mux(
                &[stream(codec_id, media_type, rate)],
                &[(0, 0, 0, 3600, 0xaa, PacketFlags::KEY)],
            );
            let pmt = &out[2 * TS_PACKET_SIZE..3 * TS_PACKET_SIZE];
            // PMT payload starts after 4-byte TS header + pointer field;
            // the stream entry follows the 8-byte section header and the
            // 4-byte PCR pid / program_info_length fields.
            assert_eq!(
                pmt[4 + 1 + 8 + 4],
                expected_type,
                "stream type for {codec_id:?}"
            );
            let pes = &out[3 * TS_PACKET_SIZE..];
            let afc = (pes[3] >> 4) & 3;
            let offset = 4 + if afc & 2 != 0 { 1 + pes[4] as usize } else { 0 };
            assert_eq!(pes[offset + 3], expected_id, "stream id for {codec_id:?}");
        }
    }

    #[test]
    fn factory_descriptor() {
        let factory = MpegTsMuxerFactory;
        let desc = factory.descriptor();
        assert_eq!(desc.name, "mpegts");
        assert_eq!(desc.long_name, "MPEG-TS (MPEG-2 Transport Stream)");
        assert_eq!(desc.extensions, "ts,m2ts,mts,m2t");
        assert_eq!(desc.mime_types, "video/MP2T");
        assert_eq!(desc.video_codec, CodecId::Mpeg2video);
        assert_eq!(desc.audio_codec, CodecId::Mp2);
        assert!(factory.create().is_ok());
    }
}
