//! Matroska / WebM muxer, translated from FFmpeg's `libavformat/matroskaenc.c`.
//!
//! The muxer writes a complete EBML structure:
//!
//! ```text
//! EBML
//!   EBMLVersion / EBMLReadVersion / EBMLMaxIDLength / EBMLMaxSizeLength
//!   DocType / DocTypeVersion / DocTypeReadVersion
//! Segment (unknown size, patched in the trailer)
//!   SeekHead           (reserved space, filled in the trailer)
//!   Info               (TimecodeScale, MuxingApp, WritingApp, Duration)
//!   Tracks             (one TrackEntry per stream)
//!   Tags               (per-track DURATION simple tag, seekable output only)
//!   Cluster            (Timecode + SimpleBlock elements)
//!   Cues               (keyframe index, seekable output only)
//! ```
//!
//! Level-one masters carry a CRC-32 element in Matroska mode, exactly like
//! FFmpeg's `write_crc32` option (which defaults to true for Matroska and is
//! forced off for WebM). The EBML primitives and the CRC live in [`ebml`].

mod ebml;

use ebml::*;
use tracing::{debug, trace, warn};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::{Rational, rescale_q};
use wedeo_format::demuxer::Stream;
use wedeo_format::io::BufferedIo;
use wedeo_format::muxer::{Muxer, OutputFormatDescriptor, OutputFormatFlags};
use wedeo_format::registry::MuxerFactory;

/// Bytes FFmpeg reserves for the SeekHead at the start of a Segment:
/// `MAX_SEEKHEAD_ENTRIES * MAX_SEEKENTRY_SIZE + 14` with
/// `MAX_SEEKHEAD_ENTRIES = 7` and `MAX_SEEKENTRY_SIZE = 21`.
const SEEKHEAD_RESERVED_SIZE: usize = 7 * 21 + 14;
/// Length field used for the Segment and for reserved TrackEntry sizes.
const LONG_SIZE_BYTES: usize = 8;
/// Default file clock: one millisecond (1_000_000 ns per tick). This is the
/// value FFmpeg uses for WebM and for Matroska streams with modest sample
/// rates, and it matches the stream time bases used by the Wedeo pipeline.
const DEFAULT_TIMECODE_SCALE: i64 = 1_000_000;
/// FFmpeg's bitexact application identifiers.
const LIBAVFORMAT_IDENT: &str = "Lavf";
/// Size of a `put_ebml_void()` reservation for a double-precision duration.
const DURATION_PLACEHOLDER_SIZE: usize = 11;
/// FFmpeg's `DURATION_STRING_LENGTH` (includes the trailing NUL byte).
const DURATION_STRING_LENGTH: usize = 19;
/// A level-one CRC-32 placeholder occupies `BF 84 <4 bytes>`; the value
/// starts at byte 2.
const CRC_VALUE_OFFSET: usize = 2;
/// Start a new cluster after five seconds of file time.
const CLUSTER_TIME_LIMIT: i64 = 5000;
/// Start a new cluster after five megabytes of cluster payload.
const CLUSTER_SIZE_LIMIT: i64 = 5 * 1024 * 1024;
/// For video, start a new cluster on a keyframe once 4 KiB have accumulated.
const VIDEO_KEYFRAME_CLUSTER_LIMIT: i64 = 4 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Matroska,
    Webm,
}

impl Mode {
    fn doctype(self) -> &'static str {
        match self {
            Mode::Matroska => "matroska",
            Mode::Webm => "webm",
        }
    }

    /// FFmpeg sets `doctype_version = 4` for Matroska (it supports
    /// SegmentUID), 2 for WebM.
    fn doctype_version(self) -> u64 {
        match self {
            Mode::Matroska => 4,
            Mode::Webm => 2,
        }
    }

    /// FFmpeg's `mkv_init()` forces `write_crc = 0` for WebM.
    fn write_crc(self) -> bool {
        self == Mode::Matroska
    }
}

/// Map a `CodecId` to its Matroska `CodecID` string. These strings come from
/// `ff_mkv_codec_tags[]` in `libavformat/matroska.c`; the WebM subset uses
/// `ff_webm_codec_tags[]`.
fn codec_id_string(codec_id: CodecId, mode: Mode) -> Option<&'static str> {
    if mode == Mode::Webm {
        return match codec_id {
            CodecId::Vp8 => Some("V_VP8"),
            CodecId::Vp9 => Some("V_VP9"),
            CodecId::Av1 => Some("V_AV1"),
            CodecId::Vorbis => Some("A_VORBIS"),
            CodecId::Opus => Some("A_OPUS"),
            CodecId::SubWebvtt => Some("D_WEBVTT/SUBTITLES"),
            _ => None,
        };
    }
    match codec_id {
        CodecId::H264 => Some("V_MPEG4/ISO/AVC"),
        CodecId::Vp8 => Some("V_VP8"),
        CodecId::Vp9 => Some("V_VP9"),
        CodecId::Av1 => Some("V_AV1"),
        CodecId::Hevc => Some("V_MPEGH/ISO/HEVC"),
        CodecId::Mpeg4 => Some("V_MPEG4/ISO/ASP"),
        CodecId::Mpeg2video => Some("V_MPEG2"),
        CodecId::Mpeg1video => Some("V_MPEG1"),
        CodecId::Mjpeg => Some("V_MJPEG"),
        CodecId::Ffv1 => Some("V_FFV1"),
        CodecId::Prores => Some("V_PRORES"),
        CodecId::Rawvideo => Some("V_UNCOMPRESSED"),
        CodecId::Aac => Some("A_AAC"),
        CodecId::Ac3 => Some("A_AC3"),
        CodecId::Opus => Some("A_OPUS"),
        CodecId::Vorbis => Some("A_VORBIS"),
        CodecId::Flac => Some("A_FLAC"),
        CodecId::Mp3 => Some("A_MPEG/L3"),
        CodecId::PcmS16be | CodecId::PcmS24be | CodecId::PcmS32be => Some("A_PCM/INT/BIG"),
        CodecId::PcmS16le | CodecId::PcmS24le | CodecId::PcmS32le | CodecId::PcmU8 => {
            Some("A_PCM/INT/LIT")
        }
        CodecId::PcmF32le | CodecId::PcmF64le => Some("A_PCM/FLOAT/IEEE"),
        CodecId::SubWebvtt => Some("D_WEBVTT/SUBTITLES"),
        CodecId::SubSrt | CodecId::SubText => Some("S_TEXT/UTF8"),
        CodecId::SubSsa => Some("S_TEXT/ASS"),
        _ => None,
    }
}

/// Bit depth reported in the Audio element, matching FFmpeg's
/// `av_get_bits_per_sample()` for the codecs we support.
fn bits_depth(codec_id: CodecId) -> Option<u32> {
    Some(match codec_id {
        CodecId::PcmU8 | CodecId::PcmS8 => 8,
        CodecId::PcmS16le | CodecId::PcmS16be | CodecId::PcmU16le | CodecId::PcmU16be => 16,
        CodecId::PcmS24le | CodecId::PcmS24be | CodecId::PcmU24le | CodecId::PcmU24be => 24,
        CodecId::PcmS32le | CodecId::PcmS32be | CodecId::PcmU32le | CodecId::PcmU32be => 32,
        CodecId::PcmF32le | CodecId::PcmF32be => 32,
        CodecId::PcmF64le | CodecId::PcmF64be => 64,
        _ => return None,
    })
}

/// Per-track state, mirroring FFmpeg's `mkv_track`.
#[derive(Debug, Clone)]
struct Track {
    number: u64,
    uid: u64,
    codec_id_string: &'static str,
    media_type: MediaType,
    time_base: Rational,
    sample_rate: u32,
    channels: u32,
    bit_depth: u32,
    width: u32,
    height: u32,
    extradata: Vec<u8>,
    last_timestamp: i64,
    duration: i64,
    has_cue: bool,
}

impl Track {
    fn from_stream(index: usize, stream: &Stream, mode: Mode) -> Result<Self> {
        let par = &stream.codec_params;
        let codec_id_string = codec_id_string(par.codec_id, mode).ok_or(Error::InvalidArgument)?;
        match par.media_type {
            MediaType::Video | MediaType::Audio => {}
            _ => return Err(Error::InvalidArgument),
        }
        let channels = par.channel_layout.nb_channels.max(0) as u32;
        let bit_depth = bits_depth(par.codec_id)
            .or(if par.bits_per_raw_sample > 0 {
                Some(par.bits_per_raw_sample)
            } else if par.bits_per_coded_sample > 0 {
                Some(par.bits_per_coded_sample)
            } else {
                None
            })
            .unwrap_or(0);
        Ok(Self {
            number: (index + 1) as u64,
            // Bitexact FFmpeg uses the track index + 1 as UID; keep output
            // deterministic by following the same convention.
            uid: (index + 1) as u64,
            codec_id_string,
            media_type: par.media_type,
            time_base: stream.time_base,
            sample_rate: par.sample_rate,
            channels,
            bit_depth,
            width: par.width,
            height: par.height,
            extradata: par.extradata.clone(),
            last_timestamp: 0,
            duration: 0,
            has_cue: false,
        })
    }
}

/// A cue point, mirroring FFmpeg's `mkv_cuepoint`.
#[derive(Debug, Clone)]
struct CuePoint {
    pts: i64,
    stream_index: usize,
    cluster_pos: u64,
    relative_pos: u64,
}

/// Matroska muxer state.
struct MatroskaMuxer {
    mode: Mode,
    tracks: Vec<Track>,
    seekable: bool,
    timestamp_scale: i64,

    segment_content_start: u64,
    seekhead_pos: u64,
    info_pos: u64,
    tracks_pos: u64,
    tags_pos: u64,
    cues_pos: u64,
    end_pos: u64,

    info_buf: Vec<u8>,
    tracks_buf: Vec<u8>,
    tags_buf: Vec<u8>,
    cluster: Vec<u8>,
    cluster_crc_offset: Option<usize>,
    cluster_pos: u64,
    cluster_pts: i64,
    cluster_open: bool,

    cues: Vec<CuePoint>,
    duration: i64,
    header_written: bool,
}

impl MatroskaMuxer {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            tracks: Vec::new(),
            seekable: false,
            timestamp_scale: DEFAULT_TIMECODE_SCALE,
            segment_content_start: 0,
            seekhead_pos: 0,
            info_pos: 0,
            tracks_pos: 0,
            tags_pos: 0,
            cues_pos: 0,
            end_pos: 0,
            info_buf: Vec::new(),
            tracks_buf: Vec::new(),
            tags_buf: Vec::new(),
            cluster: Vec::new(),
            cluster_crc_offset: None,
            cluster_pos: 0,
            cluster_pts: 0,
            cluster_open: false,
            cues: Vec::new(),
            duration: 0,
            header_written: false,
        }
    }

    /// Rescale a timestamp from a stream's time base into the file clock.
    fn to_file_time(&self, stream_index: usize, ts: i64) -> i64 {
        let Some(track) = self.tracks.get(stream_index) else {
            return ts;
        };
        if track.time_base.num == 0 || track.time_base.den == 0 {
            return ts;
        }
        let file_tb = Rational::new(self.timestamp_scale as i32, 1_000_000_000);
        rescale_q(ts, track.time_base, file_tb)
    }

    /// Build the EBML header, identical to FFmpeg's `ebml_write_header()`
    /// apart from the DocType and DocTypeVersion.
    fn build_ebml_header(&self) -> Vec<u8> {
        let mut body = Vec::new();
        put_uint(&mut body, ID_EBML_VERSION, 1);
        put_uint(&mut body, ID_EBML_READ_VERSION, 1);
        put_uint(&mut body, ID_EBML_MAX_ID_LENGTH, 4);
        put_uint(&mut body, ID_EBML_MAX_SIZE_LENGTH, 8);
        put_binary(&mut body, ID_DOCTYPE, self.mode.doctype().as_bytes());
        put_uint(&mut body, ID_DOCTYPE_VERSION, self.mode.doctype_version());
        put_uint(&mut body, ID_DOCTYPE_READ_VERSION, 2);
        let mut buf = Vec::new();
        put_id(&mut buf, ID_EBML);
        put_length(&mut buf, body.len() as u64);
        buf.extend_from_slice(&body);
        buf
    }

    /// Build the Info master with a floating-point Duration element.
    /// Returns the buffer and the offset of the Duration element (FFmpeg's
    /// `mkv->duration_offset`), which is always eleven bytes long.
    fn build_info(&self, duration: i64) -> (Vec<u8>, usize) {
        let mut buf = Vec::new();
        if self.mode.write_crc() {
            put_crc_placeholder(&mut buf);
        }
        put_uint(&mut buf, ID_TIMECODE_SCALE, self.timestamp_scale as u64);
        put_binary(&mut buf, ID_MUXING_APP, LIBAVFORMAT_IDENT.as_bytes());
        put_binary(&mut buf, ID_WRITING_APP, LIBAVFORMAT_IDENT.as_bytes());
        let offset = buf.len();
        // FFmpeg writes the duration directly in file-clock ticks.
        put_float(&mut buf, ID_DURATION, duration as f64);
        (buf, offset)
    }

    /// Add a level-one CRC to a buffer built by `build_info()` / `build_tracks()`
    /// / `build_tags()` / `build_cues()`.
    fn finish_crc(buf: &mut [u8]) {
        if buf.first() == Some(&(ID_CRC32 as u8)) && buf.get(1) == Some(&0x84) {
            finalize_crc(buf, CRC_VALUE_OFFSET);
        }
    }

    /// Wrap a level-one master's children with its EBML ID and length.
    /// FFmpeg's `end_ebml_master_crc32()` writes the ID and a minimal-size
    /// vint before the buffered children.
    fn wrap_master(id: u32, body: Vec<u8>) -> Vec<u8> {
        let mut buf = Vec::with_capacity(body.len() + 8);
        put_id(&mut buf, id);
        put_length(&mut buf, body.len() as u64);
        buf.extend_from_slice(&body);
        buf
    }

    /// Build the Tracks master. FFmpeg's `start_ebml_master(pb, id, 0)`
    /// reserves eight bytes for a TrackEntry's size, so we do the same.
    fn build_tracks(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        if self.mode.write_crc() {
            put_crc_placeholder(&mut buf);
        }
        for track in &self.tracks {
            let mut entry = Vec::new();
            put_uint(&mut entry, ID_TRACK_NUMBER, track.number);
            put_uid(&mut entry, ID_TRACK_UID, track.uid);
            put_uint(&mut entry, ID_TRACK_FLAG_LACING, 0);
            put_binary(&mut entry, ID_TRACK_LANGUAGE, b"und");
            // FFmpeg's default_mode is "passthrough", so no track has
            // AV_DISPOSITION_DEFAULT; FlagDefault is explicitly cleared.
            put_uint(&mut entry, ID_TRACK_FLAG_DEFAULT, 0);
            put_binary(&mut entry, ID_CODEC_ID, track.codec_id_string.as_bytes());

            match track.media_type {
                MediaType::Video => {
                    put_uint(&mut entry, ID_TRACK_TYPE, 1);
                    let mut video = Vec::new();
                    put_uint(&mut video, ID_VIDEO_PIXEL_WIDTH, track.width as u64);
                    put_uint(&mut video, ID_VIDEO_PIXEL_HEIGHT, track.height as u64);
                    // Matroska (but not WebM) writes DisplayUnit = unknown
                    // when there is no sample aspect ratio.
                    if self.mode == Mode::Matroska {
                        put_uint(&mut video, ID_VIDEO_DISPLAY_UNIT, 0);
                    }
                    put_id(&mut entry, ID_TRACK_VIDEO);
                    put_length(&mut entry, video.len() as u64);
                    entry.extend_from_slice(&video);
                }
                MediaType::Audio => {
                    put_uint(&mut entry, ID_TRACK_TYPE, 2);
                    let mut audio = Vec::new();
                    put_uint(&mut audio, ID_AUDIO_CHANNELS, track.channels as u64);
                    put_float(
                        &mut audio,
                        ID_AUDIO_SAMPLING_FREQUENCY,
                        track.sample_rate as f64,
                    );
                    if track.bit_depth > 0 {
                        put_uint(&mut audio, ID_AUDIO_BIT_DEPTH, track.bit_depth as u64);
                    }
                    put_id(&mut entry, ID_TRACK_AUDIO);
                    put_length(&mut entry, audio.len() as u64);
                    entry.extend_from_slice(&audio);
                }
                _ => unreachable!("Track::from_stream only accepts video/audio"),
            }

            // AudioSpecificConfig / AVCDecoderConfigurationRecord / AV1 OBUs
            // are carried verbatim in CodecPrivate. FFmpeg derives some of
            // these from extradata; the Wedeo pipeline supplies them directly.
            if !track.extradata.is_empty() {
                put_binary(&mut entry, ID_CODEC_PRIVATE, &track.extradata);
            }
            put_uint(&mut entry, ID_TRACK_MAX_BLOCK_ADDITION_ID, 0);
            if track.media_type == MediaType::Video {
                // Reserved space for an eventual HDR10+ BlockAdditionMapping.
                put_void(&mut entry, 11);
            }

            put_id(&mut buf, ID_TRACK_ENTRY);
            put_num(&mut buf, entry.len() as u64, LONG_SIZE_BYTES);
            buf.extend_from_slice(&entry);
        }
        buf
    }

    /// Build the Tags master with one DURATION simple tag per track. The
    /// 19-byte string payload keeps element sizes stable so the trailer can
    /// patch values in place.
    fn build_tags(&self) -> (Vec<u8>, Vec<usize>) {
        let mut buf = Vec::new();
        if self.mode.write_crc() {
            put_crc_placeholder(&mut buf);
        }
        let mut duration_offsets = Vec::new();
        for track in &self.tracks {
            let tag_start = buf.len();
            let mut targets = Vec::new();
            put_uid(&mut targets, ID_TAG_TRACK_UID, track.uid);
            let mut simple = Vec::new();
            put_binary(&mut simple, ID_TAG_NAME, b"DURATION");
            put_id(&mut simple, ID_TAG_STRING);
            put_num(&mut simple, DURATION_STRING_LENGTH as u64, 1);
            let local_duration_offset = simple.len();
            simple.resize(simple.len() + DURATION_STRING_LENGTH, 0);

            let mut tag = Vec::new();
            put_id(&mut tag, ID_TARGETS);
            put_length(&mut tag, targets.len() as u64);
            tag.extend_from_slice(&targets);
            let simple_tag_offset = tag.len();
            put_id(&mut tag, ID_SIMPLE_TAG);
            put_length(&mut tag, simple.len() as u64);
            tag.extend_from_slice(&simple);

            let absolute = tag_start
                + id_size(ID_TAG)
                + length_size(tag.len() as u64)
                + simple_tag_offset
                + id_size(ID_SIMPLE_TAG)
                + length_size(simple.len() as u64)
                + local_duration_offset;
            duration_offsets.push(absolute);

            put_id(&mut buf, ID_TAG);
            put_length(&mut buf, tag.len() as u64);
            buf.extend_from_slice(&tag);
        }
        (buf, duration_offsets)
    }

    /// Format FFmpeg's duration string: `%02d:%02d:%012.9f`, padded to 19
    /// bytes with the trailing NUL that FFmpeg writes.
    fn duration_string(seconds: f64) -> [u8; DURATION_STRING_LENGTH] {
        let total = seconds.max(0.0) as u64;
        let hours = total / 3600;
        let minutes = (total / 60) % 60;
        let secs = seconds - hours as f64 * 3600.0 - minutes as f64 * 60.0;
        let text = format!("{hours:02}:{minutes:02}:{secs:012.9}");
        let mut out = [0u8; DURATION_STRING_LENGTH];
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text.as_bytes()[..n]);
        out
    }

    /// Build the SeekHead over the saved level-one element positions.
    fn build_seekhead(&self) -> Vec<u8> {
        let mut body = Vec::new();
        if self.mode.write_crc() {
            put_crc_placeholder(&mut body);
        }
        let mut entries = vec![(ID_INFO, self.info_pos), (ID_TRACKS, self.tracks_pos)];
        if self.seekable {
            entries.push((ID_TAGS, self.tags_pos));
        }
        if !self.cues.is_empty() {
            entries.push((ID_CUES, self.cues_pos));
        }
        for (id, pos) in entries {
            let mut seek = Vec::new();
            let mut raw_id = Vec::new();
            put_id(&mut raw_id, id);
            put_binary(&mut seek, ID_SEEK_ID, &raw_id);
            put_uint(
                &mut seek,
                ID_SEEK_POSITION,
                pos.saturating_sub(self.segment_content_start),
            );
            put_id(&mut body, ID_SEEK);
            put_length(&mut body, seek.len() as u64);
            body.extend_from_slice(&seek);
        }
        Self::finish_crc(&mut body);

        let mut buf = Vec::new();
        put_id(&mut buf, ID_SEEKHEAD);
        put_length(&mut buf, body.len() as u64);
        buf.extend_from_slice(&body);
        let current = buf.len();
        if current < SEEKHEAD_RESERVED_SIZE {
            put_void(&mut buf, SEEKHEAD_RESERVED_SIZE - current);
        }
        buf
    }

    /// Flush the current Cluster into `io`, finalizing its size and CRC.
    fn flush_cluster(&mut self, io: &mut BufferedIo) -> Result<()> {
        if !self.cluster_open {
            return Ok(());
        }
        if self.cluster_crc_offset.take().is_some() {
            Self::finish_crc(&mut self.cluster);
        }
        let payload_len = self.cluster.len();
        self.cluster_pos = io.tell()?;
        let mut header = Vec::new();
        put_id(&mut header, ID_CLUSTER);
        put_length(&mut header, payload_len as u64);
        io.write_bytes(&header)?;
        io.write_bytes(&self.cluster)?;
        self.cluster_open = false;
        self.cluster.clear();
        trace!(
            cluster_pos = self.cluster_pos,
            payload_len, "cluster flushed"
        );
        Ok(())
    }

    /// Start a fresh Cluster for a packet at `ts`.
    fn start_cluster(&mut self, ts: i64) {
        self.cluster.clear();
        self.cluster_crc_offset = if self.mode.write_crc() {
            Some(CRC_VALUE_OFFSET)
        } else {
            None
        };
        if self.cluster_crc_offset.is_some() {
            put_crc_placeholder(&mut self.cluster);
        }
        let cluster_ts = ts.max(0);
        put_uint(&mut self.cluster, ID_CLUSTER_TIMECODE, cluster_ts as u64);
        self.cluster_pts = cluster_ts;
        self.cluster_open = true;
    }

    /// Write a SimpleBlock (or a BlockGroup for subtitle durations) into the
    /// current Cluster buffer. Returns the block's offset from the start of
    /// the Cluster content, used by CueRelativePosition.
    fn write_block(&mut self, packet: &Packet, track_index: usize, ts: i64, keyframe: bool) -> u64 {
        let track = &self.tracks[track_index];
        let relative_pos = self.cluster.len() as u64;
        let data = packet.data.data();

        let mut block = Vec::new();
        put_num(&mut block, track.number, 1);
        // Blocks carry a signed 16-bit offset from the Cluster timecode.
        block.extend_from_slice(&((ts - self.cluster_pts) as i16).to_be_bytes());
        block.push(if keyframe { 0x80 } else { 0x00 });
        block.extend_from_slice(data);

        let duration = packet.duration.max(0);
        if duration > 0 && track.media_type == MediaType::Subtitle {
            let mut group = Vec::new();
            put_id(&mut group, ID_BLOCK);
            put_length(&mut group, block.len() as u64);
            group.extend_from_slice(&block);
            put_sint(&mut group, ID_BLOCK_DURATION, duration);
            put_id(&mut self.cluster, ID_BLOCK_GROUP);
            put_length(&mut self.cluster, group.len() as u64);
            self.cluster.extend_from_slice(&group);
        } else {
            put_id(&mut self.cluster, ID_SIMPLE_BLOCK);
            put_length(&mut self.cluster, block.len() as u64);
            self.cluster.extend_from_slice(&block);
        }

        relative_pos
    }

    /// Bind newly created cue points to the Cluster just flushed.
    fn bind_cues_to_cluster(&mut self) {
        let cluster_pos = self.cluster_pos;
        for cue in self.cues.iter_mut().rev() {
            if cue.cluster_pos != 0 {
                break;
            }
            cue.cluster_pos = cluster_pos;
        }
    }

    /// Build the Cues master from the recorded keyframe positions.
    fn build_cues(&self) -> Vec<u8> {
        let mut body = Vec::new();
        if self.mode.write_crc() {
            put_crc_placeholder(&mut body);
        }
        let mut cues = self.cues.clone();
        cues.sort_by_key(|c| (c.pts, c.cluster_pos, c.stream_index));
        let mut i = 0;
        while i < cues.len() {
            let group_pts = cues[i].pts;
            let group_cluster = cues[i].cluster_pos;
            let mut point = Vec::new();
            put_uint(&mut point, ID_CUE_TIME, group_pts.max(0) as u64);
            let mut j = i;
            while j < cues.len() && cues[j].pts == group_pts && cues[j].cluster_pos == group_cluster
            {
                let cue = &cues[j];
                let mut positions = Vec::new();
                put_uint(
                    &mut positions,
                    ID_CUE_TRACK,
                    self.tracks[cue.stream_index].number,
                );
                put_uint(
                    &mut positions,
                    ID_CUE_CLUSTER_POSITION,
                    cue.cluster_pos.saturating_sub(self.segment_content_start),
                );
                if cue.relative_pos > 0 {
                    put_uint(&mut positions, ID_CUE_RELATIVE_POSITION, cue.relative_pos);
                }
                put_id(&mut point, ID_CUE_TRACK_POSITION);
                put_length(&mut point, positions.len() as u64);
                point.extend_from_slice(&positions);
                j += 1;
            }
            put_id(&mut body, ID_POINT_ENTRY);
            put_length(&mut body, point.len() as u64);
            body.extend_from_slice(&point);
            i = j;
        }
        Self::finish_crc(&mut body);
        let mut buf = Vec::new();
        put_id(&mut buf, ID_CUES);
        put_length(&mut buf, body.len() as u64);
        buf.extend_from_slice(&body);
        buf
    }
}

impl Muxer for MatroskaMuxer {
    fn write_header(&mut self, io: &mut BufferedIo, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::StreamNotFound);
        }
        self.seekable = io.is_seekable();
        self.tracks = streams
            .iter()
            .enumerate()
            .map(|(i, s)| Track::from_stream(i, s, self.mode))
            .collect::<Result<Vec<_>>>()?;

        io.write_bytes(&self.build_ebml_header())?;
        let mut segment_header = Vec::new();
        put_id(&mut segment_header, ID_SEGMENT);
        put_unknown_size(&mut segment_header, LONG_SIZE_BYTES);
        io.write_bytes(&segment_header)?;
        self.segment_content_start = io.tell()?;

        if self.seekable {
            let mut reserved = Vec::new();
            put_void(&mut reserved, SEEKHEAD_RESERVED_SIZE);
            self.seekhead_pos = self.segment_content_start;
            io.write_bytes(&reserved)?;
        }

        let (mut info, _) = self.build_info(0);
        Self::finish_crc(&mut info);
        self.info_buf = Self::wrap_master(ID_INFO, info);
        self.info_pos = io.tell()?;
        io.write_bytes(&self.info_buf)?;

        let mut tracks = self.build_tracks();
        Self::finish_crc(&mut tracks);
        self.tracks_buf = Self::wrap_master(ID_TRACKS, tracks);
        self.tracks_pos = io.tell()?;
        io.write_bytes(&self.tracks_buf)?;

        if self.seekable {
            let (mut tags, _) = self.build_tags();
            Self::finish_crc(&mut tags);
            self.tags_buf = Self::wrap_master(ID_TAGS, tags);
            self.tags_pos = io.tell()?;
            io.write_bytes(&self.tags_buf)?;
        }

        self.end_pos = io.tell()?;
        self.header_written = true;
        debug!(
            mode = self.mode.doctype(),
            streams = streams.len(),
            timecode_scale = self.timestamp_scale,
            "Matroska header written"
        );
        Ok(())
    }

    fn write_packet(&mut self, io: &mut BufferedIo, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::Bug);
        }
        let stream_index = packet.stream_index;
        if stream_index >= self.tracks.len() {
            return Err(Error::StreamNotFound);
        }
        let ts = self.to_file_time(stream_index, packet.pts);
        if ts == i64::MIN || ts == i64::MAX {
            return Err(Error::InvalidArgument);
        }
        let keyframe = packet.flags.contains(PacketFlags::KEY);
        let media_type = self.tracks[stream_index].media_type;
        let has_video = self.tracks.iter().any(|t| t.media_type == MediaType::Video);

        if self.cluster_open {
            let delta = ts - self.cluster_pts;
            let cluster_len = self.cluster.len() as i64;
            let too_late = !(-32768..32768).contains(&delta);
            let too_long = delta > CLUSTER_TIME_LIMIT;
            let too_big = cluster_len > CLUSTER_SIZE_LIMIT;
            let video_keyframe = media_type == MediaType::Video
                && keyframe
                && cluster_len > VIDEO_KEYFRAME_CLUSTER_LIMIT;
            if too_late || too_long || too_big || video_keyframe {
                self.flush_cluster(io)?;
                self.bind_cues_to_cluster();
            }
        }
        if !self.cluster_open {
            self.start_cluster(ts);
        }

        let relative_pos = self.write_block(packet, stream_index, ts, keyframe);
        self.tracks[stream_index].last_timestamp = ts;
        let duration = self.to_file_time(stream_index, packet.duration).max(0);
        let end = ts.saturating_add(duration);
        self.tracks[stream_index].duration = self.tracks[stream_index].duration.max(end);
        self.duration = self.duration.max(end);

        // FFmpeg indexes every video keyframe; for audio-only files it
        // indexes just the first keyframe of each track.
        let needs_cue = keyframe
            && (media_type == MediaType::Video
                || (!has_video && !self.tracks[stream_index].has_cue));
        if needs_cue {
            self.tracks[stream_index].has_cue = true;
            self.cues.push(CuePoint {
                pts: ts,
                stream_index,
                cluster_pos: 0, // patched when the Cluster is flushed
                relative_pos,
            });
        }
        trace!(
            stream_index,
            ts, duration, keyframe, "Matroska block written"
        );
        Ok(())
    }

    fn write_trailer(&mut self, io: &mut BufferedIo) -> Result<()> {
        if !self.header_written {
            return Err(Error::Bug);
        }
        self.flush_cluster(io)?;
        self.bind_cues_to_cluster();

        if !self.seekable {
            // Non-seekable output cannot be patched: the Cluster is complete
            // and the Segment keeps its unknown size. FFmpeg also omits
            // SeekHead, Tags and Cues for non-seekable output.
            io.flush()?;
            return Ok(());
        }

        // Rebuild Info with the final duration, then write the Cues element.
        let (mut info, duration_offset) = self.build_info(self.duration);
        debug_assert_eq!(info.len() - duration_offset, DURATION_PLACEHOLDER_SIZE);
        Self::finish_crc(&mut info);
        self.info_buf = Self::wrap_master(ID_INFO, info);

        let (mut tags, durations) = self.build_tags();
        for (i, track) in self.tracks.iter().enumerate() {
            if let Some(&offset) = durations.get(i) {
                let seconds = track.duration as f64 * (self.timestamp_scale as f64 / 1e9);
                let text = Self::duration_string(seconds);
                tags[offset..offset + DURATION_STRING_LENGTH].copy_from_slice(&text);
            }
        }
        Self::finish_crc(&mut tags);
        self.tags_buf = Self::wrap_master(ID_TAGS, tags);

        self.cues_pos = io.tell()?;
        let cues = self.build_cues();
        io.write_bytes(&cues)?;
        let end_pos = io.tell()?;
        self.end_pos = end_pos;

        let seekhead = self.build_seekhead();
        if seekhead.len() != SEEKHEAD_RESERVED_SIZE {
            warn!(
                actual = seekhead.len(),
                expected = SEEKHEAD_RESERVED_SIZE,
                "SeekHead reserved region size mismatch"
            );
        }

        io.flush()?;

        // Segment size as an eight-byte vint.
        let mut segment_size = Vec::new();
        put_num(
            &mut segment_size,
            end_pos.saturating_sub(self.segment_content_start),
            LONG_SIZE_BYTES,
        );
        io.seek(self.segment_content_start - LONG_SIZE_BYTES as u64)?;
        io.write_bytes(&segment_size)?;

        io.seek(self.seekhead_pos)?;
        io.write_bytes(&seekhead)?;
        io.seek(self.info_pos)?;
        io.write_bytes(&self.info_buf)?;
        io.seek(self.tracks_pos)?;
        io.write_bytes(&self.tracks_buf)?;
        io.seek(self.tags_pos)?;
        io.write_bytes(&self.tags_buf)?;

        io.seek(end_pos)?;
        io.flush()?;
        debug!(
            end_pos,
            segment_size = end_pos - self.segment_content_start,
            "Matroska trailer written"
        );
        Ok(())
    }
}

// --- Factory registrations ------------------------------------------------

struct MatroskaMuxerFactory;

impl MuxerFactory for MatroskaMuxerFactory {
    fn descriptor(&self) -> &OutputFormatDescriptor {
        static DESC: OutputFormatDescriptor = OutputFormatDescriptor {
            name: "matroska",
            long_name: "Matroska",
            extensions: "mkv",
            mime_types: "video/x-matroska",
            flags: OutputFormatFlags::GLOBALHEADER
                .union(OutputFormatFlags::VARIABLE_FPS)
                .union(OutputFormatFlags::TS_NONSTRICT),
            audio_codec: CodecId::Vorbis,
            video_codec: CodecId::H264,
        };
        &DESC
    }

    fn create(&self) -> Result<Box<dyn Muxer>> {
        Ok(Box::new(MatroskaMuxer::new(Mode::Matroska)))
    }
}

struct WebmMuxerFactory;

impl MuxerFactory for WebmMuxerFactory {
    fn descriptor(&self) -> &OutputFormatDescriptor {
        static DESC: OutputFormatDescriptor = OutputFormatDescriptor {
            name: "webm",
            long_name: "WebM",
            extensions: "webm",
            mime_types: "video/webm",
            flags: OutputFormatFlags::GLOBALHEADER
                .union(OutputFormatFlags::VARIABLE_FPS)
                .union(OutputFormatFlags::TS_NONSTRICT),
            audio_codec: CodecId::Opus,
            video_codec: CodecId::Vp9,
        };
        &DESC
    }

    fn create(&self) -> Result<Box<dyn Muxer>> {
        Ok(Box::new(MatroskaMuxer::new(Mode::Webm)))
    }
}

inventory::submit!(&MatroskaMuxerFactory as &dyn MuxerFactory);
inventory::submit!(&WebmMuxerFactory as &dyn MuxerFactory);

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;
    use std::sync::{Arc, Mutex};

    use wedeo_codec::decoder::CodecParameters;
    use wedeo_format::io::IoContext;

    use super::*;

    struct MemoryIo {
        shared: Arc<Mutex<Vec<u8>>>,
        pos: usize,
        seekable: bool,
    }

    impl MemoryIo {
        fn new(seekable: bool) -> (Self, Arc<Mutex<Vec<u8>>>) {
            let shared = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    shared: Arc::clone(&shared),
                    pos: 0,
                    seekable,
                },
                shared,
            )
        }
    }

    impl IoContext for MemoryIo {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            let shared = self.shared.lock().unwrap();
            let remaining = shared.len().saturating_sub(self.pos);
            let n = buf.len().min(remaining);
            buf[..n].copy_from_slice(&shared[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
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
            assert!(self.seekable, "seek on non-seekable MemoryIo");
            let shared = self.shared.lock().unwrap();
            let new_pos = match pos {
                SeekFrom::Start(p) => p as i64,
                SeekFrom::End(off) => shared.len() as i64 + off,
                SeekFrom::Current(off) => self.pos as i64 + off,
            };
            self.pos = new_pos.max(0) as usize;
            Ok(self.pos as u64)
        }

        fn tell(&mut self) -> Result<u64> {
            Ok(self.pos as u64)
        }

        fn size(&mut self) -> Result<u64> {
            Ok(self.shared.lock().unwrap().len() as u64)
        }

        fn is_seekable(&self) -> bool {
            self.seekable
        }
    }

    fn audio_stream(codec_id: CodecId, rate: u32, channels: i32) -> Stream {
        let mut params = CodecParameters::new(codec_id, MediaType::Audio);
        params.sample_rate = rate;
        params.channel_layout = wedeo_core::channel_layout::ChannelLayout::unspec(channels);
        params.bits_per_raw_sample = 16;
        let mut stream = Stream::new(0, params);
        stream.time_base = Rational::new(1, 1000);
        stream
    }

    fn video_stream(codec_id: CodecId, w: u32, h: u32) -> Stream {
        let mut params = CodecParameters::new(codec_id, MediaType::Video);
        params.width = w;
        params.height = h;
        let mut stream = Stream::new(0, params);
        stream.time_base = Rational::new(1, 1000);
        stream
    }

    fn write_file(
        mode: Mode,
        streams: &[Stream],
        packets: &[(usize, i64, i64, &[u8], bool)],
        seekable: bool,
    ) -> Vec<u8> {
        let (io, shared) = MemoryIo::new(seekable);
        let mut io = BufferedIo::new(Box::new(io));
        let mut muxer = MatroskaMuxer::new(mode);
        muxer.write_header(&mut io, streams).unwrap();
        for &(index, pts, duration, payload, key) in packets {
            let mut packet = Packet::from_slice(payload);
            packet.stream_index = index;
            packet.pts = pts;
            packet.dts = pts;
            packet.duration = duration;
            if key {
                packet.flags = PacketFlags::KEY;
            }
            muxer.write_packet(&mut io, &packet).unwrap();
        }
        muxer.write_trailer(&mut io).unwrap();
        io.flush().unwrap();
        shared.lock().unwrap().clone()
    }

    #[test]
    fn ebml_header_matches_ffmpeg_layout() {
        let bytes = write_file(
            Mode::Matroska,
            &[audio_stream(CodecId::PcmS16le, 8000, 1)],
            &[],
            true,
        );
        assert_eq!(&bytes[..4], &[0x1a, 0x45, 0xdf, 0xa3], "EBML header ID");
        assert_eq!(bytes[4], 0xa3, "EBML header size is 35");
        assert_eq!(&bytes[5..9], &[0x42, 0x86, 0x81, 0x01]);
        assert_eq!(&bytes[9..13], &[0x42, 0xf7, 0x81, 0x01]);
        assert_eq!(&bytes[13..17], &[0x42, 0xf2, 0x81, 0x04]);
        assert_eq!(&bytes[17..21], &[0x42, 0xf3, 0x81, 0x08]);
        assert_eq!(&bytes[21..24], &[0x42, 0x82, 0x88]);
        assert_eq!(&bytes[24..32], b"matroska");
        assert_eq!(&bytes[32..36], &[0x42, 0x87, 0x81, 0x04]);
        assert_eq!(&bytes[36..40], &[0x42, 0x85, 0x81, 0x02]);
        assert_eq!(&bytes[40..44], &[0x18, 0x53, 0x80, 0x67], "Segment ID");
        // The trailer patches the reserved eight-byte size field.
        let size = u64::from_be_bytes(bytes[44..52].try_into().unwrap());
        assert_eq!(size >> 56, 1, "eight-byte vint marker");
        assert_eq!((size & 0x00ff_ffff_ffff_ffff) as usize, bytes.len() - 52);
    }

    #[test]
    fn segment_size_is_patched() {
        let bytes = write_file(
            Mode::Matroska,
            &[audio_stream(CodecId::PcmS16le, 8000, 1)],
            &[(0, 0, 25, b"abc", true)],
            true,
        );
        let size = u64::from_be_bytes(bytes[44..52].try_into().unwrap());
        assert_eq!(size >> 56, 1, "eight-byte vint marker");
        assert_eq!((size & ((1u64 << 56) - 1)) as usize, bytes.len() - 52);
    }

    #[test]
    fn info_has_timecode_scale_and_duration() {
        let bytes = write_file(
            Mode::Matroska,
            &[audio_stream(CodecId::PcmS16le, 8000, 1)],
            &[(0, 0, 25, b"abc", true)],
            true,
        );
        assert!(
            bytes
                .windows(7)
                .any(|w| w == [0x2a, 0xd7, 0xb1, 0x83, 0x0f, 0x42, 0x40])
        );
        assert!(
            bytes
                .windows(11)
                .any(|w| w[..3] == [0x44, 0x89, 0x88] && w[3..] == 25.0f64.to_be_bytes())
        );
    }

    #[test]
    fn codec_ids_for_required_codecs() {
        let cases = [
            (CodecId::H264, "V_MPEG4/ISO/AVC"),
            (CodecId::Vp9, "V_VP9"),
            (CodecId::Av1, "V_AV1"),
            (CodecId::Aac, "A_AAC"),
            (CodecId::PcmS16le, "A_PCM/INT/LIT"),
            (CodecId::PcmF32le, "A_PCM/FLOAT/IEEE"),
        ];
        for (codec_id, expected) in cases {
            let stream = if matches!(codec_id, CodecId::H264 | CodecId::Vp9 | CodecId::Av1) {
                video_stream(codec_id, 640, 360)
            } else {
                audio_stream(codec_id, 48000, 2)
            };
            let bytes = write_file(Mode::Matroska, &[stream], &[], true);
            assert!(
                bytes
                    .windows(expected.len())
                    .any(|w| w == expected.as_bytes()),
                "missing CodecID {expected} for {codec_id:?}"
            );
        }
    }

    #[test]
    fn simple_block_layout() {
        let payload = [0x11, 0x22, 0x33, 0x44];
        let bytes = write_file(
            Mode::Webm,
            &[video_stream(CodecId::Vp9, 32, 16)],
            &[(0, 0, 33, &payload, true)],
            true,
        );
        let marker = [
            0xa3, 0x88, 0x81, // SimpleBlock id, size 8, track 1
            0x00, 0x00, // relative timestamp
            0x80, // keyframe flag
        ];
        let pos = bytes
            .windows(marker.len())
            .position(|w| w == marker)
            .unwrap();
        assert_eq!(&bytes[pos + marker.len()..pos + marker.len() + 4], &payload);
    }

    #[test]
    fn webm_has_no_crc_and_uses_webm_doctype() {
        let bytes = write_file(
            Mode::Webm,
            &[video_stream(CodecId::Vp9, 16, 16)],
            &[(0, 0, 40, b"frame", true)],
            true,
        );
        assert_eq!(&bytes[24..28], b"webm");
        assert!(!bytes.windows(6).any(|w| w[0] == 0xbf && w[1] == 0x84));
    }

    #[test]
    fn non_seekable_output_has_unknown_segment_size_and_no_cues() {
        let bytes = write_file(
            Mode::Matroska,
            &[audio_stream(CodecId::PcmS16le, 8000, 1)],
            &[(0, 0, 25, b"abc", true)],
            false,
        );
        assert_eq!(
            &bytes[44..52],
            &[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert!(!bytes.windows(4).any(|w| w == [0x1c, 0x53, 0xbb, 0x6b]));
    }

    #[test]
    fn multiple_clusters_for_large_timestamp_jump() {
        let bytes = write_file(
            Mode::Webm,
            &[video_stream(CodecId::Vp9, 16, 16)],
            &[(0, 0, 40, b"a", true), (0, 100_000, 40, b"b", true)],
            true,
        );
        let cluster_id = [0x1f, 0x43, 0xb6, 0x75];
        assert_eq!(bytes.windows(4).filter(|w| *w == cluster_id).count(), 2);
    }

    #[test]
    fn factories_registered() {
        assert_eq!(
            wedeo_format::registry::find_muxer_by_name("matroska")
                .map(|f| f.descriptor().extensions),
            Some("mkv")
        );
        assert_eq!(
            wedeo_format::registry::find_muxer_by_name("webm").map(|f| f.descriptor().extensions),
            Some("webm")
        );
    }
}
