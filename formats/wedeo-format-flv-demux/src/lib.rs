//! FLV (Flash Video) demuxer, translated from FFmpeg's `libavformat/flvdec.c`.
//!
//! The FLV container is described in Annex E of the Flash Video File Format
//! Specification.  Files start with a 9-byte header, followed by a 4-byte
//! `PreviousTagSize0` field, followed by a sequence of tags:
//!
//! ```text
//! [Signature "FLV"][Version][Flags][Header size (u32be)]
//! [PreviousTagSize0 = 0 (u32be)]
//! [Tag type (u8)][Data size (u24be)][Timestamp (u24be)][Timestamp ext (u8)]
//! [Stream ID (u24be, always 0)][Tag data][PreviousTagSize (u32be)]
//! ...
//! ```
//!
//! Tag types are audio (`0x08`), video (`0x09`) and script data (`0x12`).
//! AVC/AAC data begins with a packet-type byte: `0` marks a sequence header
//! (extradata) and `1` marks coded frames.  AVC coded frames carry a 3-byte
//! composition-time offset (CTS) before the length-prefixed NAL units.
//!
//! The wedeo `Demuxer` API requires all streams to be known when `read_header`
//! returns, while FFmpeg creates streams lazily as tags are read.  To bridge
//! the two, `read_header` performs one forward pass over the (seekable) file:
//! it discovers streams, extracts extradata and metadata, and buffers the
//! resulting packets.  `read_packet` then drains that buffer in file order.

mod amf;

use std::collections::VecDeque;

use tracing::{debug, trace, warn};
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::{Packet, PacketFlags, PacketSideData, PacketSideDataType};
use wedeo_core::rational::Rational;
use wedeo_format::demuxer::{
    Demuxer, DemuxerHeader, InputFormatDescriptor, InputFormatFlags, PROBE_SCORE_MAX, ProbeData,
    SeekFlags, Stream,
};
use wedeo_format::io::BufferedIo;
use wedeo_format::registry::DemuxerFactory;

// ---------------------------------------------------------------------------
// Constants (libavformat/flv.h)
// ---------------------------------------------------------------------------

const FLV_HEADER_FLAG_HASVIDEO: u8 = 1;
const FLV_HEADER_FLAG_HASAUDIO: u8 = 4;

const FLV_TAG_TYPE_AUDIO: u8 = 0x08;
const FLV_TAG_TYPE_VIDEO: u8 = 0x09;
const FLV_TAG_TYPE_META: u8 = 0x12;

const FLV_AUDIO_CHANNEL_MASK: u8 = 0x01;
const FLV_AUDIO_SAMPLESIZE_MASK: u8 = 0x02;
const FLV_AUDIO_SAMPLERATE_MASK: u8 = 0x0c;
const FLV_AUDIO_CODECID_MASK: u8 = 0xf0;

const FLV_VIDEO_CODECID_MASK: u8 = 0x0f;
const FLV_VIDEO_FRAMETYPE_MASK: u8 = 0x70;
const FLV_IS_EX_HEADER: u8 = 0x80;

const FLV_STEREO: u8 = 1;

// Audio codec IDs (flv.h: shifted by FLV_AUDIO_CODECID_OFFSET = 4).
const FLV_CODECID_PCM: u8 = 0x00;
const FLV_CODECID_MP3: u8 = 0x20;
const FLV_CODECID_PCM_LE: u8 = 0x30;
const FLV_CODECID_EX_HEADER: u8 = 0x90;
const FLV_CODECID_AAC: u8 = 0xa0;

// Video codec IDs (flv.h).
const FLV_CODECID_H263: u8 = 2;
const FLV_CODECID_VP6: u8 = 4;
const FLV_CODECID_VP6A: u8 = 5;
const FLV_CODECID_H264: u8 = 7;
const FLV_CODECID_MPEG4: u8 = 9;
const FLV_CODECID_X_HEVC: u8 = 12;

// Enhanced FLV packet types (flv.h).
const PACKET_TYPE_SEQUENCE_START: u8 = 0;
const PACKET_TYPE_CODED_FRAMES: u8 = 1;
const PACKET_TYPE_METADATA: u8 = 4;
const PACKET_TYPE_MULTITRACK: u8 = 6;
const PACKET_TYPE_MOD_EX: u8 = 7;

const AUDIO_PACKET_TYPE_MULTITRACK: u8 = 5;

const FLV_FRAME_KEY: u8 = 1 << 4; // FLV_FRAME_KEY in flv.h

// Multitrack type bits: `MultitrackTypeOneTrack = 0x00`,
// `MultitrackTypeManyTracksManyCodecs = 0x20` (flv.h).  They are not needed
// here because the tag already carries a fourcc and track index per stream.

/// Stream time base: `avpriv_set_pts_info(st, 32, 1, 1000); /* 32 bit pts in ms */`.
const FLV_TIME_BASE: Rational = Rational::new(1, 1000);

// ---------------------------------------------------------------------------
// Helper readers
// ---------------------------------------------------------------------------

/// `avio_rb24()` is not a method on `BufferedIo`; read three big-endian bytes.
fn read_u24be(io: &mut BufferedIo) -> Result<u32> {
    let b = io.read_bytes(3)?;
    Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32)
}

fn u24be(data: &[u8]) -> Result<u32> {
    if data.len() < 3 {
        return Err(Error::InvalidData);
    }
    Ok(((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32)
}

/// Sign extension of a 24-bit composition-time offset, mirroring
/// `int32_t cts = (avio_rb24(s->pb) + 0xff800000) ^ 0xff800000;` (flvdec.c:1804).
fn sign_extend_cts(raw: u32) -> i64 {
    (raw.wrapping_add(0xff80_0000) ^ 0xff80_0000) as i32 as i64
}

/// FourCC helper: `MKBETAG('a','v','c','1')` etc. (big-endian packed u32).
fn mkbetag(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

// ---------------------------------------------------------------------------
// Stream bookkeeping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Video,
    Audio,
    Data,
}

impl StreamKind {
    fn media_type(self) -> MediaType {
        match self {
            Self::Video => MediaType::Video,
            Self::Audio => MediaType::Audio,
            Self::Data => MediaType::Data,
        }
    }
}

/// A binding from (media kind, fourcc/track index) to a created stream index.
#[derive(Debug, Clone, Copy)]
struct StreamBinding {
    kind: StreamKind,
    track_idx: u8,
    codec_id: CodecId,
    stream_index: usize,
}

/// State accumulated while scanning the file in `read_header`.
struct ScanContext {
    streams: Vec<Stream>,
    bindings: Vec<StreamBinding>,
    metadata: Metadata,
    duration: i64,
    packets: Vec<Packet>,
    video_bit_rate: i64,
    audio_bit_rate: i64,
    /// Extradata queued by a later sequence header, attached to the next packet
    /// of that stream (FFmpeg's `flv->new_extradata`).
    queued_extradata: Vec<Option<Vec<u8>>>,
}

impl ScanContext {
    fn new() -> Self {
        Self {
            streams: Vec::new(),
            bindings: Vec::new(),
            metadata: Metadata::new(),
            duration: 0,
            packets: Vec::new(),
            video_bit_rate: 0,
            audio_bit_rate: 0,
            queued_extradata: Vec::new(),
        }
    }

    /// Find a stream matching `(kind, track_idx, codec_id)`, creating one when
    /// none exists.  This mirrors FFmpeg's `flv_same_audio_codec()` /
    /// `flv_same_video_codec()` lookup followed by `create_stream()`.
    fn stream_for(&mut self, kind: StreamKind, track_idx: u8, codec_id: CodecId) -> usize {
        if let Some(b) = self.bindings.iter().find(|b| {
            b.kind == kind
                && b.track_idx == track_idx
                && (b.codec_id == codec_id || b.codec_id == CodecId::None)
        }) {
            let idx = b.stream_index;
            if self.streams[idx].codec_params.codec_id == CodecId::None {
                self.streams[idx].codec_params.codec_id = codec_id;
            }
            return idx;
        }

        let index = self.streams.len();
        let mut params = CodecParameters::new(codec_id, kind.media_type());
        params.time_base = FLV_TIME_BASE;
        if kind == StreamKind::Audio {
            params.bit_rate = self.audio_bit_rate;
        } else if kind == StreamKind::Video {
            params.bit_rate = self.video_bit_rate;
        }
        let mut stream = Stream::new(index, params);
        stream.time_base = FLV_TIME_BASE;
        self.streams.push(stream);
        self.queued_extradata.push(None);
        self.bindings.push(StreamBinding {
            kind,
            track_idx,
            codec_id,
            stream_index: index,
        });
        debug!(index, ?kind, track_idx, ?codec_id, "FLV stream created");
        index
    }

    fn set_extradata(&mut self, stream_index: usize, data: &[u8]) {
        self.streams[stream_index].codec_params.extradata = data.to_vec();
        debug!(stream_index, size = data.len(), "FLV extradata extracted");
    }
}

// ---------------------------------------------------------------------------
// Codec mapping (flvdec.c: flv_set_audio_codec / flv_set_video_codec)
// ---------------------------------------------------------------------------

fn audio_codec_from_flags(flags: u8, bits: u32) -> CodecId {
    match flags & FLV_AUDIO_CODECID_MASK {
        FLV_CODECID_PCM | FLV_CODECID_PCM_LE => {
            if bits == 8 {
                CodecId::PcmU8
            } else {
                CodecId::PcmS16le // little-endian host, matching FFmpeg's default build
            }
        }
        FLV_CODECID_MP3 => CodecId::Mp3,
        FLV_CODECID_AAC => CodecId::Aac,
        _ => CodecId::None,
    }
}

fn video_codec_from_flags(flags: u8) -> CodecId {
    match flags & FLV_VIDEO_CODECID_MASK {
        FLV_CODECID_H264 => CodecId::H264,
        FLV_CODECID_MPEG4 => CodecId::Mpeg4,
        FLV_CODECID_X_HEVC => CodecId::Hevc,
        // FLV1 (Sorenson), VP6/VP6A, Screen etc. have no wedeo CodecId yet.
        FLV_CODECID_H263 | FLV_CODECID_VP6 | FLV_CODECID_VP6A => CodecId::None,
        _ => CodecId::None,
    }
}

fn codec_from_fourcc(fourcc: u32) -> (StreamKind, CodecId) {
    match fourcc {
        x if x == mkbetag(b'a', b'v', b'c', b'1') => (StreamKind::Video, CodecId::H264),
        x if x == mkbetag(b'h', b'v', b'c', b'1') => (StreamKind::Video, CodecId::Hevc),
        x if x == mkbetag(b'v', b'p', b'0', b'9') => (StreamKind::Video, CodecId::Vp9),
        x if x == mkbetag(b'a', b'v', b'0', b'1') => (StreamKind::Video, CodecId::Av1),
        x if x == mkbetag(b'v', b'v', b'c', b'1') => (StreamKind::Video, CodecId::None),
        x if x == mkbetag(b'm', b'p', b'4', b'a') => (StreamKind::Audio, CodecId::Aac),
        x if x == mkbetag(b'O', b'p', b'u', b's') => (StreamKind::Audio, CodecId::Opus),
        x if x == mkbetag(b'.', b'm', b'p', b'3') => (StreamKind::Audio, CodecId::Mp3),
        x if x == mkbetag(b'f', b'L', b'a', b'C') => (StreamKind::Audio, CodecId::Flac),
        x if x == mkbetag(b'a', b'c', b'-', b'3') => (StreamKind::Audio, CodecId::Ac3),
        x if x == mkbetag(b'e', b'c', b'-', b'3') => (StreamKind::Audio, CodecId::None),
        _ => (StreamKind::Data, CodecId::None),
    }
}

/// Codecs for which FLV carries an explicit one-byte packet type and, for the
/// H.264 family, a 3-byte CTS (flvdec.c:1765-1818).
fn has_packet_type(codec_id: CodecId) -> bool {
    matches!(
        codec_id,
        CodecId::Aac
            | CodecId::Opus
            | CodecId::Flac
            | CodecId::H264
            | CodecId::Mpeg4
            | CodecId::Hevc
            | CodecId::Av1
            | CodecId::Vp9
    )
}

fn has_cts(codec_id: CodecId) -> bool {
    matches!(codec_id, CodecId::Mpeg4 | CodecId::H264 | CodecId::Hevc)
}

// ---------------------------------------------------------------------------
// AMF metadata handling (flvdec.c: flv_read_metabody / amf_parse_object)
// ---------------------------------------------------------------------------

fn parse_metadata(data: &[u8], ctx: &mut ScanContext) {
    let mut r = amf::AmfReader::new(data);
    let name = match r.read_value() {
        Ok(v) => v,
        Err(_) => return,
    };
    match name.as_str() {
        Some("onMetaData") | Some("onCuePoint") | Some("|RtmpSampleAccess") => {}
        _ => return, // onTextData / onCaption / unknown: ignored by this demuxer
    }

    let obj = match r.read_value() {
        Ok(v) => v,
        Err(_) => return,
    };
    let entries = match obj.as_entries() {
        Some(e) => e,
        None => return,
    };

    for (key, value) in entries {
        match key.as_str() {
            "duration" => {
                if let Some(v) = value.as_f64()
                    && (0.0..(i64::MAX as f64 / 1_000_000.0)).contains(&v)
                {
                    ctx.duration = (v * 1_000_000.0) as i64;
                }
            }
            "videodatarate" => {
                if let Some(v) = value.as_f64()
                    && (0.0..(i32::MAX as f64 / 1024.0)).contains(&v)
                {
                    ctx.video_bit_rate = (v * 1024.0) as i64;
                }
            }
            "audiodatarate" => {
                if let Some(v) = value.as_f64()
                    && (0.0..(i32::MAX as f64 / 1024.0)).contains(&v)
                {
                    ctx.audio_bit_rate = (v * 1024.0) as i64;
                }
            }
            // Keys that FFmpeg consumes for stream configuration rather than
            // exporting as generic metadata (flvdec.c:818-831).
            "filesize" | "width" | "height" | "framerate" | "videocodecid" | "audiosamplerate"
            | "audiosamplesize" | "stereo" | "audiocodecid" | "datastream" => {}
            _ => {
                let text = match value {
                    amf::AmfValue::Bool(b) => {
                        if *b {
                            "true".to_string()
                        } else {
                            "false".to_string()
                        }
                    }
                    amf::AmfValue::Number(n) => format!("{n:.0}"),
                    amf::AmfValue::String(s) => s.clone(),
                    amf::AmfValue::Date(ms, _) if ms.is_finite() => format!("{ms:.0}"),
                    _ => continue,
                };
                ctx.metadata.set(key, text);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tag scanning
// ---------------------------------------------------------------------------

struct TagHeader {
    tag_type: u8,
    data_size: usize,
    dts: i64,
    pos: u64,
}

/// Read the 11-byte FLV tag header.  Returns `None` on a clean EOF before the
/// type byte, and `Err(Eof)` for a truncated header.
fn read_tag_header(io: &mut BufferedIo) -> Result<Option<TagHeader>> {
    let pos = io.tell()?;
    let tag_type = match io.read_u8() {
        Ok(t) => t & 0x1f,
        Err(Error::Eof) => return Ok(None),
        Err(e) => return Err(e),
    };
    let data_size = read_u24be(io)? as usize;
    let ts_low = read_u24be(io)?;
    let ts_ext = io.read_u8()? as u32;
    let _stream_id = read_u24be(io)?;
    let dts = ((ts_ext << 24) | ts_low) as i32 as i64;
    Ok(Some(TagHeader {
        tag_type,
        data_size,
        dts,
        pos,
    }))
}

/// Parse the ModEx extended header (flvdec.c:1325-1379).  Returns the new
/// packet type and advances `data` past the consumed bytes.
fn parse_mod_ex<'a>(data: &'a [u8], dts: &mut i64) -> Result<(u8, &'a [u8])> {
    let mut rest = data;
    let first = *rest.first().ok_or(Error::InvalidData)?;
    let mut ex_size = first as usize + 1;
    rest = &rest[1..];
    if ex_size == 256 {
        if rest.len() < 2 {
            return Err(Error::InvalidData);
        }
        ex_size = u16::from_be_bytes([rest[0], rest[1]]) as usize + 1;
        rest = &rest[2..];
    }
    if ex_size >= rest.len() {
        return Err(Error::InvalidData);
    }
    let ex_data = &rest[..ex_size];
    rest = &rest[ex_size..];
    let ex_type = rest[0];
    rest = &rest[1..];
    let pkt_type = ex_type & 0x0f;
    let ex_kind = ex_type & 0xf0;
    if ex_kind == 0 && ex_size == 3 {
        let nano = ((ex_data[0] as u32) << 16) | ((ex_data[1] as u32) << 8) | ex_data[2] as u32;
        *dts += (nano / 1_000_000) as i64;
    }
    Ok((pkt_type, rest))
}

/// Process one tag's data, updating `ctx` (streams / extradata / metadata) and
/// pushing a packet when the tag carries coded media.
fn process_tag_data(ctx: &mut ScanContext, hdr: &TagHeader, data: &[u8]) -> Result<()> {
    let mut dts = hdr.dts;
    let mut pts = i64::MIN; // AV_NOPTS_VALUE in framecrc output terms

    match hdr.tag_type {
        FLV_TAG_TYPE_AUDIO => {
            let flags = *data.first().ok_or(Error::InvalidData)?;
            let bits = if flags & FLV_AUDIO_SAMPLESIZE_MASK != 0 {
                16
            } else {
                8
            };
            let channels = if flags & FLV_AUDIO_CHANNEL_MASK == FLV_STEREO {
                2
            } else {
                1
            };
            let sample_rate = 44_100i32
                .wrapping_shl(((flags & FLV_AUDIO_SAMPLERATE_MASK) >> 2) as u32)
                .wrapping_shr(3);

            let mut rest = &data[1..];
            let mut enhanced = false;
            let mut pkt_type = 0u8;
            let mut multitrack = false;
            let mut track_idx = 0u8;
            let (kind, codec_id) = if flags & FLV_AUDIO_CODECID_MASK == FLV_CODECID_EX_HEADER {
                enhanced = true;
                pkt_type = flags & 0x0f;
                while pkt_type == PACKET_TYPE_MOD_EX {
                    let (t, r) = parse_mod_ex(rest, &mut dts)?;
                    pkt_type = t;
                    rest = r;
                }
                if pkt_type == AUDIO_PACKET_TYPE_MULTITRACK {
                    let types = *rest.first().ok_or(Error::InvalidData)?;
                    pkt_type = types & 0x0f;
                    multitrack = true;
                    rest = &rest[1..];
                }
                if rest.len() < 4 {
                    return Err(Error::InvalidData);
                }
                let fourcc = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                rest = &rest[4..];
                if multitrack {
                    track_idx = *rest.first().ok_or(Error::InvalidData)?;
                    rest = &rest[1..];
                }
                codec_from_fourcc(fourcc)
            } else {
                (StreamKind::Audio, audio_codec_from_flags(flags, bits))
            };

            let stream_idx = ctx.stream_for(kind, track_idx, codec_id);
            if !enhanced {
                let params = &mut ctx.streams[stream_idx].codec_params;
                if params.sample_rate == 0 || params.channel_layout.nb_channels == 0 {
                    params.sample_rate = sample_rate as u32;
                    params.bits_per_coded_sample = bits;
                    set_default_layout(params, channels);
                }
            }

            let mut payload = rest;
            if has_packet_type(codec_id) {
                let t = *payload.first().ok_or(Error::InvalidData)?;
                pkt_type = t;
                payload = &payload[1..];
            }

            if pkt_type == PACKET_TYPE_SEQUENCE_START && has_packet_type(codec_id) {
                if !payload.is_empty() {
                    if ctx.streams[stream_idx].codec_params.extradata.is_empty() {
                        ctx.set_extradata(stream_idx, payload);
                    } else {
                        // A later sequence header is queued as new extradata
                        // for the next packet (FFmpeg's `flv_queue_extradata`).
                        ctx.queued_extradata[stream_idx] = Some(payload.to_vec());
                    }
                }
                return Ok(());
            }

            if payload.is_empty() {
                return Ok(());
            }
            push_packet(
                ctx, stream_idx, hdr, dts, pts, payload,
                true, // audio packets are always keyframes
            );
        }

        FLV_TAG_TYPE_VIDEO => {
            let flags = *data.first().ok_or(Error::InvalidData)?;
            let enhanced = flags & FLV_IS_EX_HEADER != 0;
            let codec_id_from_flags = flags & FLV_VIDEO_CODECID_MASK;
            // `pkt_type = enhanced_flv ? codec_id : 0;` (flvdec.c:1481)
            let mut pkt_type = if enhanced { codec_id_from_flags } else { 0 };
            let mut rest = &data[1..];
            let mut multitrack = false;
            let mut track_idx = 0u8;

            while pkt_type == PACKET_TYPE_MOD_EX {
                let (t, r) = parse_mod_ex(rest, &mut dts)?;
                pkt_type = t;
                rest = r;
            }

            if pkt_type == PACKET_TYPE_MULTITRACK {
                let types = *rest.first().ok_or(Error::InvalidData)?;
                pkt_type = types & 0x0f;
                multitrack = true;
                rest = &rest[1..];
            }

            let (kind, codec_id) = if enhanced {
                if rest.len() < 4 {
                    return Err(Error::InvalidData);
                }
                let fourcc = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                rest = &rest[4..];
                if multitrack {
                    track_idx = *rest.first().ok_or(Error::InvalidData)?;
                    rest = &rest[1..];
                }
                codec_from_fourcc(fourcc)
            } else {
                (StreamKind::Video, video_codec_from_flags(flags))
            };

            let frame_type = flags & FLV_VIDEO_FRAMETYPE_MASK;
            if frame_type == 5 << 4 {
                // FLV_FRAME_VIDEO_INFO_CMD carries no coded media.
                return Ok(());
            }

            let stream_idx = ctx.stream_for(kind, track_idx, codec_id);

            if enhanced && pkt_type == PACKET_TYPE_METADATA {
                // Color metadata is parsed by FFmpeg; not needed for packet parity.
                return Ok(());
            }

            let mut payload = rest;
            if has_packet_type(codec_id) {
                let t = *payload.first().ok_or(Error::InvalidData)?;
                pkt_type = t;
                payload = &payload[1..];
            }

            if has_cts(codec_id) && (!enhanced || pkt_type == PACKET_TYPE_CODED_FRAMES) {
                let cts = sign_extend_cts(u24be(payload)?);
                pts = dts.saturating_add(cts);
                payload = &payload[3..];
            }

            if pkt_type == PACKET_TYPE_SEQUENCE_START && has_packet_type(codec_id) {
                if !payload.is_empty() {
                    if ctx.streams[stream_idx].codec_params.extradata.is_empty() {
                        ctx.set_extradata(stream_idx, payload);
                    } else {
                        ctx.queued_extradata[stream_idx] = Some(payload.to_vec());
                    }
                }
                return Ok(());
            }

            if payload.is_empty() {
                return Ok(());
            }
            let key = frame_type == FLV_FRAME_KEY;
            push_packet(ctx, stream_idx, hdr, dts, pts, payload, key);
        }

        FLV_TAG_TYPE_META => {
            if hdr.data_size > 18 {
                parse_metadata(data, ctx);
            }
        }

        _ => {
            trace!(
                tag_type = hdr.tag_type,
                size = hdr.data_size,
                "skipping FLV tag"
            );
        }
    }
    Ok(())
}

fn set_default_layout(params: &mut CodecParameters, channels: i32) {
    use wedeo_core::channel_layout::ChannelLayout;
    params.channel_layout = match channels {
        1 => ChannelLayout::mono(),
        2 => ChannelLayout::stereo(),
        n => ChannelLayout::unspec(n),
    };
}

fn push_packet(
    ctx: &mut ScanContext,
    stream_idx: usize,
    hdr: &TagHeader,
    dts: i64,
    pts: i64,
    payload: &[u8],
    key: bool,
) {
    let mut pkt = Packet::new();
    pkt.data = Buffer::from_slice(payload);
    pkt.stream_index = stream_idx;
    pkt.dts = dts;
    pkt.pts = if pts == i64::MIN { dts } else { pts };
    pkt.pos = hdr.pos as i64;
    if key {
        pkt.flags |= PacketFlags::KEY;
    }
    if let Some(extradata) = ctx.queued_extradata[stream_idx].take() {
        pkt.side_data.push(PacketSideData {
            data_type: PacketSideDataType::NewExtradata,
            data: extradata,
        });
    }
    ctx.packets.push(pkt);
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct FlvDemuxer {
    packets: VecDeque<Packet>,
}

impl FlvDemuxer {
    fn new() -> Self {
        Self {
            packets: VecDeque::new(),
        }
    }
}

impl Demuxer for FlvDemuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        // `flv_read_header()`: signature + version are 4 bytes, then flags.
        let mut sig = [0u8; 4];
        io.read_exact(&mut sig)?;
        if &sig[0..3] != b"FLV" || sig[3] >= 5 {
            return Err(Error::InvalidData);
        }
        let flags = io.read_u8()?;
        let offset = io.read_u32be()? as u64;
        if io.is_seekable() {
            io.seek(offset)?;
        } else if offset > 9 {
            warn!(offset, "non-seekable FLV with nontrivial header offset");
        }

        // Annex E.3: `PreviousTagSize0 UI32 Always 0`.
        let prev_tag_size0 = io.read_u32be()?;
        if prev_tag_size0 != 0 {
            warn!(
                prev_tag_size0,
                "first PreviousTagSize0 is not 0 (non-standard FLV)"
            );
        }

        let mut ctx = ScanContext::new();
        while let Some(hdr) = read_tag_header(io)? {
            // `av_get_packet()` accepts a short final packet when the file is
            // truncated; `read_up_to()` matches that behaviour.
            let data = io.read_up_to(hdr.data_size)?;
            if data.is_empty() {
                break;
            }
            let truncated = data.len() < hdr.data_size;
            if let Err(e) = process_tag_data(&mut ctx, &hdr, &data) {
                warn!(error = ?e, pos = hdr.pos, "skipping malformed FLV tag");
            }
            if truncated {
                break;
            }

            // `leave:` consumes the trailing PreviousTagSize field.  Its value
            // is normally `orig_size + 11`; when it is wrong, FFmpeg resyncs.
            // We trust the header-computed next position so that a corrupt
            // size field cannot desynchronise the scan.
            let expected_next = hdr.pos + 11 + hdr.data_size as u64 + 4;
            if io.is_seekable() {
                io.seek(expected_next)?;
            } else {
                // Best effort: skip whatever is left (data already consumed).
                let _ = io.read_u32be();
            }
        }

        debug!(
            streams = ctx.streams.len(),
            packets = ctx.packets.len(),
            duration = ctx.duration,
            "FLV scan complete"
        );

        self.packets.extend(ctx.packets);
        // FFmpeg stores these in `si->missing_streams`; we only surface them in
        // the trace so the header flags are still validated/consumed.
        debug!(
            has_video = flags & FLV_HEADER_FLAG_HASVIDEO != 0,
            has_audio = flags & FLV_HEADER_FLAG_HASAUDIO != 0,
            "FLV header flags"
        );

        Ok(DemuxerHeader {
            streams: ctx.streams,
            metadata: ctx.metadata,
            duration: ctx.duration,
            start_time: 0,
        })
    }

    fn read_packet(&mut self, _io: &mut BufferedIo) -> Result<Packet> {
        self.packets.pop_front().ok_or(Error::Eof)
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
// Registration
// ---------------------------------------------------------------------------

struct FlvDemuxerFactory;

impl DemuxerFactory for FlvDemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "flv",
            long_name: "FLV (Flash Video)",
            extensions: "flv",
            mime_types: "video/x-flv",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    /// `flv_probe()`: `static int probe(const AVProbeData *p, int live)`.
    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        let d = data.buf;
        if d.len() < 10 {
            return 0;
        }
        let offset = u32::from_be_bytes([d[5], d[6], d[7], d[8]]) as usize;
        if d[0] == b'F'
            && d[1] == b'L'
            && d[2] == b'V'
            && d[3] < 5
            && d[5] == 0
            && offset.saturating_add(100) < d.len()
            && offset > 8
        {
            // `int is_live = !memcmp(d + offset + 40, "NGINX RTMP", 10);`
            // A live stream is handled by the separate `live_flv` demuxer.
            let live = d
                .get(offset + 40..offset + 50)
                .is_some_and(|s| s == b"NGINX RTMP");
            if !live {
                return PROBE_SCORE_MAX;
            }
        }
        0
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(FlvDemuxer::new()))
    }
}

inventory::submit!(&FlvDemuxerFactory as &dyn DemuxerFactory);

// ---------------------------------------------------------------------------
// Unit tests with synthetic FLV tags
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_format::io::{BufferedIo, FileIo};

    fn be24(v: u32) -> [u8; 3] {
        [(v >> 16) as u8, (v >> 8) as u8, v as u8]
    }

    fn tag(tag_type: u8, ts: u32, data: &[u8]) -> Vec<u8> {
        let mut t = Vec::new();
        t.push(tag_type);
        t.extend_from_slice(&be24(data.len() as u32));
        t.extend_from_slice(&be24(ts & 0x00ff_ffff));
        t.push((ts >> 24) as u8);
        t.extend_from_slice(&[0, 0, 0]); // stream id
        t.extend_from_slice(data);
        t.extend_from_slice(&((data.len() + 11) as u32).to_be_bytes());
        t
    }

    fn make_flv(tags: &[Vec<u8>]) -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"FLV");
        f.push(1); // version
        f.push(FLV_HEADER_FLAG_HASAUDIO | FLV_HEADER_FLAG_HASVIDEO);
        f.extend_from_slice(&9u32.to_be_bytes());
        f.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
        for t in tags {
            f.extend_from_slice(t);
        }
        f
    }

    fn demux(bytes: &[u8]) -> (DemuxerHeader, Vec<Packet>) {
        let path = std::env::temp_dir().join(format!(
            "wedeo_flv_demux_unit_{}_{}.flv",
            std::process::id(),
            bytes.len()
        ));
        std::fs::write(&path, bytes).unwrap();
        let mut io = BufferedIo::new(Box::new(FileIo::open(path.to_str().unwrap()).unwrap()));
        let mut demuxer = FlvDemuxer::new();
        let header = demuxer.read_header(&mut io).unwrap();
        let mut packets = Vec::new();
        while let Ok(p) = demuxer.read_packet(&mut io) {
            packets.push(p);
        }
        let _ = std::fs::remove_file(&path);
        (header, packets)
    }

    #[test]
    fn probe_detects_flv_and_rejects_live_and_short() {
        let factory = FlvDemuxerFactory;
        let mut buf = vec![0u8; 200];
        buf[0..3].copy_from_slice(b"FLV");
        buf[3] = 1;
        buf[5] = 0;
        buf[5..9].copy_from_slice(&9u32.to_be_bytes());
        let probe = ProbeData {
            filename: "x.flv",
            buf: &buf,
        };
        assert_eq!(factory.probe(&probe), PROBE_SCORE_MAX);

        // "NGINX RTMP" at offset+40 marks a live stream (live_flv demuxer).
        buf[49..59].copy_from_slice(b"NGINX RTMP");
        let probe = ProbeData {
            filename: "x.flv",
            buf: &buf,
        };
        assert_eq!(factory.probe(&probe), 0);

        let probe = ProbeData {
            filename: "x.flv",
            buf: &buf[..4],
        };
        assert_eq!(factory.probe(&probe), 0);
    }

    #[test]
    fn reads_timestamp_extended_byte() {
        // Audio tag (MP3) with timestamp 0x010203 == 66051 ms.
        let mut data = vec![FLV_CODECID_MP3 | 0x04]; // 44.1kHz, stereo, 16-bit
        data.extend_from_slice(&[0xaa, 0xbb]);
        let bytes = make_flv(&[tag(FLV_TAG_TYPE_AUDIO, 0x0001_0203, &data)]);
        let (header, packets) = demux(&bytes);
        assert_eq!(header.streams.len(), 1);
        assert_eq!(header.streams[0].codec_params.codec_id, CodecId::Mp3);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].dts, 0x0001_0203);
        assert_eq!(packets[0].pts, 0x0001_0203);
        assert_eq!(packets[0].data.data(), &[0xaa, 0xbb]);
        assert!(packets[0].flags.contains(PacketFlags::KEY));
        assert_eq!(packets[0].stream_index, 0);
    }

    #[test]
    fn avc_sequence_header_extraction_and_cts() {
        // avcC: configurationVersion=1, profile=66, compat=0, level=30,
        // lengthSizeMinusOne=3 (4-byte lengths), 1 SPS of 2 bytes, 1 PPS.
        let extradata = [1u8, 66, 0, 30, 0xff, 0xe1, 0, 2, 0x67, 0x42, 1, 0, 1, 0x68];
        // Even a sequence-start tag carries the 3-byte CTS before the avcC
        // record (flvdec.c:1794-1818 reads CTS before checking packet type).
        let mut seq = vec![FLV_CODECID_H264 | FLV_FRAME_KEY, 0, 0, 0, 0];
        seq.extend_from_slice(&extradata);
        let mut frames = vec![FLV_CODECID_H264 | FLV_FRAME_KEY, 1];
        frames.extend_from_slice(&[0, 0, 40]); // CTS = 40 ms
        frames.extend_from_slice(&[0, 0, 0, 3, 0x65, 0x88, 0x84]); // 1 NAL

        let bytes = make_flv(&[
            tag(FLV_TAG_TYPE_VIDEO, 100, &seq),
            tag(FLV_TAG_TYPE_VIDEO, 200, &frames),
        ]);
        let (header, packets) = demux(&bytes);
        assert_eq!(header.streams.len(), 1);
        assert_eq!(header.streams[0].codec_params.codec_id, CodecId::H264);
        assert_eq!(header.streams[0].codec_params.extradata, extradata);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].dts, 200);
        assert_eq!(packets[0].pts, 240);
        assert_eq!(packets[0].data.data(), &[0, 0, 0, 3, 0x65, 0x88, 0x84]);
        assert!(packets[0].flags.contains(PacketFlags::KEY));
    }

    #[test]
    fn negative_cts_sign_extension() {
        assert_eq!(sign_extend_cts(0), 0);
        assert_eq!(sign_extend_cts(1), 1);
        assert_eq!(sign_extend_cts(0x7f_ffff), 8_388_607);
        assert_eq!(sign_extend_cts(0x80_0000), -8_388_608);
        assert_eq!(sign_extend_cts(0xff_ffff), -1);
    }

    #[test]
    fn aac_sequence_header_extraction() {
        let mut seq = vec![FLV_CODECID_AAC | 0x04, 0];
        seq.extend_from_slice(&[0x12, 0x10]); // AudioSpecificConfig
        let frames = vec![FLV_CODECID_AAC | 0x04, 1, 0x21, 0x10, 0x90];
        let bytes = make_flv(&[
            tag(FLV_TAG_TYPE_AUDIO, 0, &seq),
            tag(FLV_TAG_TYPE_AUDIO, 23, &frames),
        ]);
        let (header, packets) = demux(&bytes);
        assert_eq!(header.streams[0].codec_params.codec_id, CodecId::Aac);
        assert_eq!(header.streams[0].codec_params.extradata, [0x12, 0x10]);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data.data(), &[0x21, 0x10, 0x90]);
        assert_eq!(packets[0].stream_index, 0);
    }

    #[test]
    fn script_metadata_sets_duration_and_metadata() {
        // AMF: "onMetaData" string, then mixed array with duration=12.5 and
        // a custom string key.
        let mut script = vec![0x02];
        script.extend_from_slice(&10u16.to_be_bytes());
        script.extend_from_slice(b"onMetaData");
        script.push(0x08);
        script.extend_from_slice(&0u32.to_be_bytes());
        script.extend_from_slice(&8u16.to_be_bytes());
        script.extend_from_slice(b"duration");
        script.push(0x00);
        script.extend_from_slice(&12.5f64.to_bits().to_be_bytes());
        script.extend_from_slice(&6u16.to_be_bytes());
        script.extend_from_slice(b"author");
        script.push(0x02);
        script.extend_from_slice(&5u16.to_be_bytes());
        script.extend_from_slice(b"alice");
        script.extend_from_slice(&0u16.to_be_bytes());
        script.push(0x09);

        let bytes = make_flv(&[tag(FLV_TAG_TYPE_META, 0, &script)]);
        let (header, packets) = demux(&bytes);
        assert!(packets.is_empty());
        assert_eq!(header.duration, 12_500_000);
        assert_eq!(header.metadata.get("author"), Some("alice"));
    }

    #[test]
    fn stream_order_follows_first_tag() {
        // Audio first, then video: FFmpeg creates audio stream 0, video stream 1.
        let audio = [FLV_CODECID_MP3 | 0x04, 0x01, 0x02];
        let video = [FLV_CODECID_H264 | (2 << 4), 1, 0, 0, 0, 3, 0x65, 0x00, 0x00];
        let bytes = make_flv(&[
            tag(FLV_TAG_TYPE_AUDIO, 0, &audio),
            tag(FLV_TAG_TYPE_VIDEO, 40, &video),
        ]);
        let (header, packets) = demux(&bytes);
        assert_eq!(header.streams.len(), 2);
        assert_eq!(header.streams[0].codec_params.media_type, MediaType::Audio);
        assert_eq!(header.streams[1].codec_params.media_type, MediaType::Video);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].stream_index, 0);
        assert_eq!(packets[1].stream_index, 1);
        assert!(!packets[1].flags.contains(PacketFlags::KEY));
    }
}
