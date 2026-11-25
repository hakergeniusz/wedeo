#![forbid(unsafe_code)]
//! FLV muxer, translated from FFmpeg's `libavformat/flvenc.c` (with constants
//! from `libavformat/flv.h`).
//!
//! File structure (all multi-byte integers big-endian):
//!
//!   [FLV header: "FLV" 01 flags 00000009 00000000]
//!   [script-data tag: onMetaData]
//!   [video/audio sequence-header tags (AVC/AAC, ...)]
//!   [media tags ...]
//!   [AVC/MPEG4 end-of-sequence tag]
//!
//! Every tag is `tag-type (1) | data-size (3) | timestamp (3+1) | stream-id (3)
//! | data | previous-tag-size (4)`. The previous-tag-size field equals
//! `data-size + 11`.
//!
//! AMF0 is used for the script-data tag. `put_amf_string` writes a 16-bit
//! big-endian length followed by the bytes, and `put_amf_double` writes the
//! `AMF_DATA_TYPE_NUMBER` marker followed by the raw IEEE-754 bits - exactly
//! matching `av_double2int()` in FFmpeg.
//!
//! FFmpeg patches the metadata's duration/filesize in `write_trailer` using
//! `avio_seek`. We build the metadata tag in memory first (so no seeking is
//! required for the header) and remember the absolute offsets of the two
//! doubles, which are patched in place when the stream is seekable. For
//! non-seekable output the initial guesses remain, matching FFmpeg's
//! behaviour (it logs a warning and leaves them untouched).
//!
//! Multitrack (enhanced FLV) features that require a per-stream `track_idx`
//! are not exposed by the Wedeo `Muxer` interface; the single-track path is
//! implemented in full. HEVC/VP9/AV1/Opus/FLAC/AC-3 use the enhanced-RTMP
//! "ex-header" layout with the already-built codec configuration record from
//! `extradata` (FFmpeg's `ff_isom_write_hvcc`/`vpcc`/`av1c` transforms are a
//! pipeline responsibility in Wedeo, as there is no bitstream-filter hook on
//! the muxer).

use tracing::{error, trace, warn};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketFlags, PacketSideDataType};
use wedeo_format::demuxer::Stream;
use wedeo_format::io::BufferedIo;
use wedeo_format::muxer::{Muxer, OutputFormatDescriptor, OutputFormatFlags};
use wedeo_format::registry::MuxerFactory;

// --- flv.h constants -------------------------------------------------------

const FLV_HEADER_FLAG_HASVIDEO: u8 = 1;
const FLV_HEADER_FLAG_HASAUDIO: u8 = 4;

const FLV_TAG_TYPE_AUDIO: u8 = 0x08;
const FLV_TAG_TYPE_VIDEO: u8 = 0x09;
const FLV_TAG_TYPE_META: u8 = 0x12;

const FLV_IS_EX_HEADER: u8 = 0x80;
const FLV_CODECID_EX_HEADER: u8 = 9 << 4;

const FLV_SAMPLESSIZE_8BIT: u8 = 0;
const FLV_SAMPLESSIZE_16BIT: u8 = 1 << 1;
const FLV_SAMPLERATE_11025HZ: u8 = 1 << 2;
const FLV_SAMPLERATE_22050HZ: u8 = 2 << 2;
const FLV_SAMPLERATE_44100HZ: u8 = 3 << 2;

const FLV_FRAME_KEY: u8 = 1 << 4;
const FLV_FRAME_INTER: u8 = 2 << 4;

// Video CodecID values (flv_video_codec_ids).
const FLV_CODECID_H263: u8 = 2;
const FLV_CODECID_MPEG4: u8 = 9;
const FLV_CODECID_H264: u8 = 7;

// Enhanced-RTMP packet types (flv.h).
const PACKET_TYPE_SEQUENCE_START: u8 = 0;
const PACKET_TYPE_CODEDFRAMES: u8 = 1;
const PACKET_TYPE_CODEDFRAMES_X: u8 = 3;
const AUDIO_PACKET_TYPE_SEQUENCE_START: u8 = 0;
const AUDIO_PACKET_TYPE_CODEDFRAMES: u8 = 1;

// AMF0 data types (flv.h).
const AMF_DATA_TYPE_NUMBER: u8 = 0x00;
const AMF_DATA_TYPE_BOOL: u8 = 0x01;
const AMF_DATA_TYPE_STRING: u8 = 0x02;
const AMF_DATA_TYPE_MIXEDARRAY: u8 = 0x08;
const AMF_END_OF_OBJECT: u8 = 0x09;

// 1 << 24: FFmpeg rejects packets whose tag would be this large.
const MAX_TAG_DATA_SIZE: usize = 1 << 24;

// --- big-endian writers ----------------------------------------------------

fn wb8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn wb16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn wb24(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
}

fn wb32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn io_wb24(io: &mut BufferedIo, v: u32) -> Result<()> {
    io.write_bytes(&[(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn io_put_timestamp(io: &mut BufferedIo, ts: i64) -> Result<()> {
    io_wb24(io, (ts as u32) & 0x00FF_FFFF)?;
    io.write_u8(((ts >> 24) & 0x7F) as u8)
}

fn io_put_f64(io: &mut BufferedIo, d: f64) -> Result<()> {
    io.write_bytes(&d.to_bits().to_be_bytes())
}

fn put_amf_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    wb16(out, len as u16);
    out.extend_from_slice(&bytes[..len]);
}

/// `put_amf_double()` from flvenc.c. Returns the offset of the 8-byte value
/// (after the type marker) so the trailer can patch it in place.
fn put_amf_double(out: &mut Vec<u8>, d: f64) -> usize {
    wb8(out, AMF_DATA_TYPE_NUMBER);
    let value_pos = out.len();
    out.extend_from_slice(&d.to_bits().to_be_bytes());
    value_pos
}

fn put_amf_bool(out: &mut Vec<u8>, b: bool) {
    wb8(out, AMF_DATA_TYPE_BOOL);
    wb8(out, u8::from(b));
}

/// Write one complete FLV tag, including the trailing "previous tag size".
fn write_tag(io: &mut BufferedIo, tag_type: u8, data: &[u8], ts: i64) -> Result<()> {
    write_tag_ex(io, tag_type, data, ts, 0)
}

/// `write_tag` with an explicit StreamID. FFmpeg's `flv->reserved` is 5 when a
/// stream has codec_tag 5 and is written in the StreamID field of media tags.
fn write_tag_ex(
    io: &mut BufferedIo,
    tag_type: u8,
    data: &[u8],
    ts: i64,
    stream_id: u8,
) -> Result<()> {
    io.write_u8(tag_type)?;
    io_wb24(io, data.len() as u32)?;
    io_put_timestamp(io, ts)?;
    io.write_bytes(&[0, 0, stream_id])?;
    io.write_bytes(data)?;
    io.write_u32be((data.len() + 11) as u32)
}

// --- codec helpers ---------------------------------------------------------

fn make_tag(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

/// The subset of `flv_video_codec_ids` available in Wedeo's `CodecId`, plus
/// the fourcc-based ids used by enhanced FLV.
fn video_codec_tag(codec_id: CodecId) -> Option<u32> {
    match codec_id {
        CodecId::H263 => Some(FLV_CODECID_H263 as u32),
        CodecId::Mpeg4 => Some(FLV_CODECID_MPEG4 as u32),
        CodecId::H264 => Some(FLV_CODECID_H264 as u32),
        CodecId::Hevc => Some(make_tag(b'h', b'v', b'c', b'1')),
        CodecId::Av1 => Some(make_tag(b'a', b'v', b'0', b'1')),
        CodecId::Vp9 => Some(make_tag(b'v', b'p', b'0', b'9')),
        _ => None,
    }
}

/// The subset of `flv_audio_codec_ids` available in Wedeo's `CodecId`.
/// Returns the *unshifted* 4-bit codec id (FFmpeg stores `tag >> 4`).
fn audio_codec_id(codec_id: CodecId) -> Option<u8> {
    match codec_id {
        CodecId::PcmU8 | CodecId::PcmS16be => Some(0),
        CodecId::Mp3 => Some(2),
        CodecId::PcmS16le => Some(3),
        CodecId::PcmAlaw => Some(7),
        CodecId::PcmMulaw => Some(8),
        // FLV_CODECID_EX_HEADER >> 4 - only needed for the codec support check.
        CodecId::Opus | CodecId::Flac | CodecId::Ac3 => Some(9),
        CodecId::Aac => Some(10),
        _ => None,
    }
}

fn codec_fourcc(codec_id: CodecId) -> Option<[u8; 4]> {
    match codec_id {
        CodecId::Aac => Some(*b"mp4a"),
        CodecId::Opus => Some(*b"Opus"),
        CodecId::Flac => Some(*b"fLaC"),
        CodecId::Mp3 => Some(*b".mp3"),
        CodecId::Ac3 => Some(*b"ac-3"),
        CodecId::H264 => Some(*b"avc1"),
        CodecId::Hevc => Some(*b"hvc1"),
        CodecId::Av1 => Some(*b"av01"),
        CodecId::Vp9 => Some(*b"vp09"),
        _ => None,
    }
}

fn is_extended_video(codec_id: CodecId) -> bool {
    matches!(codec_id, CodecId::Hevc | CodecId::Av1 | CodecId::Vp9)
}

fn is_extended_audio(codec_id: CodecId) -> bool {
    matches!(codec_id, CodecId::Opus | CodecId::Flac | CodecId::Ac3)
}

/// `get_audio_flags()` from flvenc.c.
fn get_audio_flags(par: &Stream) -> Result<u8> {
    let codec_id = par.codec_params.codec_id;
    let sample_rate = par.codec_params.sample_rate as i32;
    let channels = par.codec_params.channel_layout.nb_channels;
    let mut flags = if par.codec_params.bits_per_coded_sample == 16 {
        FLV_SAMPLESSIZE_16BIT
    } else {
        FLV_SAMPLESSIZE_8BIT
    };

    if codec_id == CodecId::Aac {
        // Specs force these parameters.
        return Ok(FLV_CODECID_AAC | FLV_SAMPLERATE_44100HZ | FLV_SAMPLESSIZE_16BIT | 1);
    }
    if is_extended_audio(codec_id) {
        // Only needed for the codec support check; real packets use the
        // enhanced-RTMP ex-header layout.
        return Ok(FLV_CODECID_EX_HEADER);
    }

    match sample_rate {
        48000 if codec_id == CodecId::Mp3 => {
            // 48khz mp3 is stored with 44k1 samplerate identifier.
            flags |= FLV_SAMPLERATE_44100HZ;
        }
        44100 => flags |= FLV_SAMPLERATE_44100HZ,
        22050 => flags |= FLV_SAMPLERATE_22050HZ,
        11025 => flags |= FLV_SAMPLERATE_11025HZ,
        // nellymoser only / not MP3 - no NellyMosher in Wedeo, so reject.
        16000 | 8000 | 5512 => {
            warn!(
                sample_rate,
                codec = codec_id.name(),
                "FLV does not support sample rate"
            );
            return Err(Error::InvalidArgument);
        }
        _ => {
            warn!(
                sample_rate,
                codec = codec_id.name(),
                "FLV does not support sample rate"
            );
            return Err(Error::InvalidArgument);
        }
    }

    if channels > 1 {
        flags |= 1; // FLV_STEREO
    }

    let codec_bits = audio_codec_id(codec_id).ok_or_else(|| {
        error!(
            codec = codec_id.name(),
            "Audio codec not compatible with FLV"
        );
        Error::InvalidArgument
    })?;

    flags |= match codec_id {
        CodecId::Mp3 => (codec_bits << 4) | FLV_SAMPLESSIZE_16BIT,
        CodecId::PcmU8 => (codec_bits << 4) | FLV_SAMPLESSIZE_8BIT,
        CodecId::PcmS16be | CodecId::PcmS16le | CodecId::PcmMulaw | CodecId::PcmAlaw => {
            (codec_bits << 4) | FLV_SAMPLESSIZE_16BIT
        }
        _ => return Err(Error::InvalidArgument),
    };
    Ok(flags)
}

const FLV_CODECID_AAC: u8 = 10 << 4;

// --- AVC configuration record ---------------------------------------------

/// Find NAL units in an Annex-B byte stream and return their payload ranges.
fn annexb_units(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    for (n, &begin) in starts.iter().enumerate() {
        // A 4-byte start code (00 00 00 01) leaves one trailing zero in the
        // preceding unit; trim it, as ff_nal_parse_units_buf() does.
        let mut end = starts.get(n + 1).copied().unwrap_or(data.len());
        if end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            units.push((begin, end));
        }
    }
    units
}

/// `ff_isom_write_avcc()` from libavformat/avc.c. When `extradata` is already
/// an AVCDecoderConfigurationRecord (starts with 0x01), FFmpeg writes it
/// verbatim; otherwise an Annex-B SPS/PPS stream is converted.
fn build_avcc(extradata: &[u8]) -> Result<Vec<u8>> {
    if extradata.len() <= 6 {
        return Err(Error::InvalidData);
    }
    let has_start_code = extradata.len() >= 4
        && ((extradata[0] == 0 && extradata[1] == 0 && extradata[2] == 1)
            || (extradata[0] == 0 && extradata[1] == 0 && extradata[2] == 0 && extradata[3] == 1));
    if !has_start_code {
        return Ok(extradata.to_vec());
    }

    let mut sps: Vec<u8> = Vec::new();
    let mut pps: Vec<u8> = Vec::new();
    let mut nb_sps: u8 = 0;
    let mut nb_pps: u8 = 0;
    for (begin, end) in annexb_units(extradata) {
        let nal = &extradata[begin..end];
        if nal.is_empty() {
            continue;
        }
        match nal[0] & 0x1f {
            7 => {
                // SPS
                nb_sps = nb_sps.wrapping_add(1);
                if end - begin > u16::MAX as usize {
                    return Err(Error::InvalidData);
                }
                wb16(&mut sps, (end - begin) as u16);
                sps.extend_from_slice(nal);
            }
            8 => {
                // PPS
                nb_pps = nb_pps.wrapping_add(1);
                if end - begin > u16::MAX as usize {
                    return Err(Error::InvalidData);
                }
                wb16(&mut pps, (end - begin) as u16);
                pps.extend_from_slice(nal);
            }
            _ => {}
        }
    }
    if sps.len() < 6 || pps.is_empty() || nb_sps == 0 {
        return Err(Error::InvalidData);
    }

    let mut out = Vec::with_capacity(sps.len() + pps.len() + 6);
    wb8(&mut out, 1); // configurationVersion
    wb8(&mut out, sps[3]); // profile
    wb8(&mut out, sps[4]); // profile compatibility
    wb8(&mut out, sps[5]); // level
    wb8(&mut out, 0xff); // 6 bits reserved + 2 bits NAL length - 1
    wb8(&mut out, 0xe0 | (nb_sps & 0x1f));
    out.extend_from_slice(&sps);
    wb8(&mut out, nb_pps);
    out.extend_from_slice(&pps);
    Ok(out)
}

// --- per-stream state ------------------------------------------------------

#[derive(Clone, Debug)]
struct StreamCfg {
    index: usize,
    codec_id: CodecId,
    media_type: MediaType,
    /// FLV codec id for video, the 4-bit FLV id for audio.
    codec_tag: u32,
    audio_flags: u8,
    extradata: Vec<u8>,
    width: u32,
    height: u32,
    bit_rate: i64,
    sample_rate: u32,
    channels: i32,
    sample_size: u8,
    last_ts: i64,
    track_idx: usize,
}

impl StreamCfg {
    fn from_stream(stream: &Stream, track_idx: usize) -> Result<Self> {
        let par = &stream.codec_params;
        let codec_id = par.codec_id;
        let (codec_tag, audio_flags) = match par.media_type {
            MediaType::Video => {
                let tag = video_codec_tag(codec_id).ok_or_else(|| {
                    error!(
                        codec = codec_id.name(),
                        "Video codec not compatible with FLV"
                    );
                    Error::InvalidArgument
                })?;
                (tag, 0)
            }
            MediaType::Audio => {
                let flags = get_audio_flags(stream)?;
                let tag = audio_codec_id(codec_id).ok_or(Error::InvalidArgument)? as u32;
                (tag, flags)
            }
            MediaType::Data | MediaType::Subtitle => (0, 0),
            MediaType::Attachment => return Err(Error::InvalidArgument),
        };

        Ok(Self {
            index: stream.index,
            codec_id,
            media_type: par.media_type,
            codec_tag,
            audio_flags,
            extradata: par.extradata.clone(),
            width: par.width,
            height: par.height,
            bit_rate: par.bit_rate,
            sample_rate: par.sample_rate,
            channels: par.channel_layout.nb_channels,
            sample_size: if par.bits_per_coded_sample == 16 {
                16
            } else {
                8
            },
            last_ts: -1,
            track_idx,
        })
    }

    fn needs_sequence_header(&self) -> bool {
        match self.codec_id {
            // Legacy MP3 has no sequence header; only enhanced/multitrack MP3
            // carries the ex-header sequence start.
            CodecId::Mp3 => self.track_idx != 0,
            CodecId::Aac
            | CodecId::H264
            | CodecId::Mpeg4
            | CodecId::Hevc
            | CodecId::Av1
            | CodecId::Vp9
            | CodecId::Opus
            | CodecId::Flac
            | CodecId::Ac3 => true,
            _ => false,
        }
    }

    /// `flv_write_codec_header()` from flvenc.c (single-track path).
    fn build_codec_header(&self) -> Result<Option<Vec<u8>>> {
        if !self.needs_sequence_header() {
            return Ok(None);
        }
        let mut out = Vec::new();

        if self.media_type == MediaType::Audio {
            if is_extended_audio(self.codec_id)
                || (self.codec_id == CodecId::Mp3 && self.track_idx != 0)
            {
                if self.track_idx != 0 {
                    wb8(&mut out, FLV_CODECID_EX_HEADER | 0x05 /* Multitrack */);
                    wb8(&mut out, AUDIO_PACKET_TYPE_SEQUENCE_START);
                } else {
                    wb8(
                        &mut out,
                        FLV_CODECID_EX_HEADER | AUDIO_PACKET_TYPE_SEQUENCE_START,
                    );
                }
                if let Some(fourcc) = codec_fourcc(self.codec_id) {
                    out.extend_from_slice(&fourcc);
                }
                if self.track_idx != 0 {
                    wb8(&mut out, self.track_idx as u8);
                }
                out.extend_from_slice(&self.extradata);
            } else if self.codec_id == CodecId::Aac {
                wb8(&mut out, self.audio_flags);
                wb8(&mut out, 0); // AAC sequence header
                out.extend_from_slice(&self.extradata);
            }
        } else {
            if is_extended_video(self.codec_id)
                || (self.codec_id == CodecId::H264 && self.track_idx != 0)
            {
                if self.track_idx != 0 {
                    wb8(
                        &mut out,
                        FLV_IS_EX_HEADER | 0x06 /* Multitrack */ | FLV_FRAME_KEY,
                    );
                    wb8(&mut out, PACKET_TYPE_SEQUENCE_START);
                } else {
                    wb8(
                        &mut out,
                        FLV_IS_EX_HEADER | PACKET_TYPE_SEQUENCE_START | FLV_FRAME_KEY,
                    );
                }
                if let Some(fourcc) = codec_fourcc(self.codec_id) {
                    out.extend_from_slice(&fourcc);
                }
                if self.track_idx != 0 {
                    wb8(&mut out, self.track_idx as u8);
                }
                // FFmpeg calls ff_isom_write_hvcc/vpcc/av1c here. Wedeo passes
                // the configuration record from extradata through unchanged.
                out.extend_from_slice(&self.extradata);
            } else {
                wb8(&mut out, self.codec_tag as u8 | FLV_FRAME_KEY);
                wb8(&mut out, 0); // AVC sequence header
                wb24(&mut out, 0); // composition time
                if self.codec_id == CodecId::H264 {
                    out.extend_from_slice(&build_avcc(&self.extradata)?);
                } else if self.codec_id == CodecId::Mpeg4 {
                    out.extend_from_slice(&self.extradata);
                }
            }
        }
        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }
}

// --- muxer state -----------------------------------------------------------

/// FLV muxer state. Mirrors FFmpeg's `FLVContext` for the fields that are
/// reachable through the Wedeo `Muxer` interface.
struct FlvMuxer {
    streams: Vec<StreamCfg>,
    audio_index: Option<usize>,
    video_index: Option<usize>,
    data_index: Option<usize>,
    framerate: f64,
    duration_guess: f64,
    duration_offset: Option<u64>,
    filesize_offset: Option<u64>,
    /// FFmpeg's `flv->duration`, in milliseconds.
    duration_ms: i64,
    delay: Option<i64>,
    reserved: u8,
}

impl FlvMuxer {
    fn new() -> Self {
        Self {
            streams: Vec::new(),
            audio_index: None,
            video_index: None,
            data_index: None,
            framerate: 0.0,
            duration_guess: 0.0,
            duration_offset: None,
            filesize_offset: None,
            duration_ms: 0,
            delay: None,
            reserved: 0,
        }
    }

    fn cfg_index(&self, stream_index: usize) -> Option<usize> {
        self.streams.iter().position(|c| c.index == stream_index)
    }

    /// Build the script-data tag body. Returns the body plus the offsets of
    /// the `duration` and `filesize` doubles inside it.
    fn build_metadata(&self) -> (Vec<u8>, usize, usize) {
        let mut out = Vec::new();

        // First event name as a string: "onMetaData".
        wb8(&mut out, AMF_DATA_TYPE_STRING);
        put_amf_string(&mut out, "onMetaData");

        // Mixed array (hash) with string/type/data tuples.
        wb8(&mut out, AMF_DATA_TYPE_MIXEDARRAY);
        let count_pos = out.len();
        wb32(&mut out, 0); // patched below
        let mut count: u32 = 2; // duration + filesize

        put_amf_string(&mut out, "duration");
        let duration_pos = put_amf_double(&mut out, self.duration_guess);

        if let Some(i) = self.video_index {
            let v = &self.streams[i];
            put_amf_string(&mut out, "width");
            put_amf_double(&mut out, v.width as f64);

            put_amf_string(&mut out, "height");
            put_amf_double(&mut out, v.height as f64);

            put_amf_string(&mut out, "videodatarate");
            put_amf_double(&mut out, v.bit_rate as f64 / 1024.0);

            if self.framerate != 0.0 {
                put_amf_string(&mut out, "framerate");
                put_amf_double(&mut out, self.framerate);
                count = count.wrapping_add(1);
            }

            put_amf_string(&mut out, "videocodecid");
            put_amf_double(&mut out, v.codec_tag as f64);
            count = count.wrapping_add(4);
        }

        if let Some(i) = self.audio_index {
            let a = &self.streams[i];
            put_amf_string(&mut out, "audiodatarate");
            put_amf_double(&mut out, a.bit_rate as f64 / 1024.0);

            put_amf_string(&mut out, "audiosamplerate");
            put_amf_double(&mut out, a.sample_rate as f64);

            put_amf_string(&mut out, "audiosamplesize");
            put_amf_double(&mut out, a.sample_size as f64);

            put_amf_string(&mut out, "stereo");
            put_amf_bool(&mut out, a.channels == 2);

            put_amf_string(&mut out, "audiocodecid");
            put_amf_double(&mut out, a.codec_tag as f64);
            count = count.wrapping_add(5);
        }

        if self.data_index.is_some() {
            put_amf_string(&mut out, "datastream");
            put_amf_double(&mut out, 0.0);
            count = count.wrapping_add(1);
        }

        put_amf_string(&mut out, "filesize");
        let filesize_pos = put_amf_double(&mut out, 0.0); // patched by write_trailer

        put_amf_string(&mut out, "");
        wb8(&mut out, AMF_END_OF_OBJECT);

        let count_bytes = count.to_be_bytes();
        out[count_pos..count_pos + 4].copy_from_slice(&count_bytes);
        (out, duration_pos, filesize_pos)
    }

    /// `put_eos_tag()` from flvenc.c.
    fn write_eos_tag(&self, io: &mut BufferedIo, cfg: &StreamCfg) -> Result<()> {
        let mut tag = cfg.codec_tag as u8 | FLV_FRAME_KEY;
        tag |= 1 << 4;
        let mut data = vec![tag, 2]; // AVC end of sequence
        wb24(&mut data, 0); // always 0 for AVC EOS
        write_tag(io, FLV_TAG_TYPE_VIDEO, &data, cfg.last_ts)
    }
}

impl Muxer for FlvMuxer {
    fn write_header(&mut self, io: &mut BufferedIo, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::StreamNotFound);
        }

        let mut video_ctr = 0usize;
        let mut audio_ctr = 0usize;
        for stream in streams {
            let par = &stream.codec_params;
            match par.media_type {
                MediaType::Video => {
                    let cfg = StreamCfg::from_stream(stream, video_ctr)?;
                    if self.video_index.is_none() {
                        self.video_index = Some(self.streams.len());
                    }
                    self.framerate = if stream.time_base.num > 0 && stream.time_base.den > 0 {
                        stream.time_base.den as f64 / stream.time_base.num as f64
                    } else {
                        0.0
                    };
                    self.streams.push(cfg);
                    video_ctr = video_ctr.wrapping_add(1);
                }
                MediaType::Audio => {
                    let cfg = StreamCfg::from_stream(stream, audio_ctr)?;
                    if self.audio_index.is_none() {
                        self.audio_index = Some(self.streams.len());
                    }
                    self.streams.push(cfg);
                    audio_ctr = audio_ctr.wrapping_add(1);
                }
                MediaType::Data | MediaType::Subtitle => {
                    let cfg = StreamCfg::from_stream(stream, 0)?;
                    if self.data_index.is_none() {
                        self.data_index = Some(self.streams.len());
                    }
                    self.streams.push(cfg);
                }
                MediaType::Attachment => return Err(Error::InvalidArgument),
            }
        }

        // Guess the duration from stream metadata, as FFmpeg does with
        // `s->duration / AV_TIME_BASE`. The trailer patches it when packets
        // were written.
        for stream in streams {
            let seconds = stream.duration as f64 * stream.time_base.to_f64();
            if seconds.is_finite() && seconds > self.duration_guess {
                self.duration_guess = seconds;
            }
        }

        // "FLV" + version + flags + offset of first tag + previous tag size.
        io.write_bytes(b"FLV")?;
        io.write_u8(1)?;
        let mut flags = 0u8;
        if self.audio_index.is_some() {
            flags |= FLV_HEADER_FLAG_HASAUDIO;
        }
        if self.video_index.is_some() {
            flags |= FLV_HEADER_FLAG_HASVIDEO;
        }
        io.write_u8(flags)?;
        io.write_u32be(9)?;
        io.write_u32be(0)?;
        let mut pos = 13u64;

        // FFmpeg writes a special 8-byte message for streams with codec_tag 5.
        for stream in streams {
            if stream.codec_params.codec_tag == 5 {
                io.write_u8(8)?; // message type
                io_wb24(io, 0)?; // include flags
                io_wb24(io, 0)?; // timestamp
                io.write_u32be(0)?; // reserved
                io.write_u32be(11)?; // size
                self.reserved = 5;
                pos = pos.wrapping_add(11);
            }
        }

        // Script-data tag. Build it in memory so no seeking is needed here.
        let (metadata, duration_pos, filesize_pos) = self.build_metadata();
        self.duration_offset = Some(pos + 11 + duration_pos as u64);
        self.filesize_offset = Some(pos + 11 + filesize_pos as u64);
        write_tag(io, FLV_TAG_TYPE_META, &metadata, 0)?;

        // Sequence headers (AVC/AAC/...).
        for i in 0..self.streams.len() {
            let data = self.streams[i].build_codec_header()?;
            if let Some(data) = data {
                let tag_type = if self.streams[i].media_type == MediaType::Audio {
                    FLV_TAG_TYPE_AUDIO
                } else {
                    FLV_TAG_TYPE_VIDEO
                };
                write_tag(io, tag_type, &data, 0)?;
            }
        }

        trace!(streams = streams.len(), flags, "FLV header written");
        Ok(())
    }

    fn write_packet(&mut self, io: &mut BufferedIo, packet: &Packet) -> Result<()> {
        let cfg_index = self
            .cfg_index(packet.stream_index)
            .ok_or(Error::StreamNotFound)?;
        let cfg = self.streams[cfg_index].clone();
        let mut data = packet.data.data().to_vec();

        // FFmpeg's flv_check_bitstream() would attach aac_adtstoasc / bsf
        // extract_extradata. Wedeo has no muxer-side BSF hook, so the pipeline
        // must supply raw AAC and AVCC/HVCC extradata.
        if cfg.codec_id == CodecId::Aac
            && data.len() > 2
            && (u16::from_be_bytes([data[0], data[1]]) & 0xfff0) == 0xfff0
        {
            warn!("Malformed AAC bitstream detected: use the aac_adtstoasc bitstream filter");
            return Err(Error::InvalidData);
        }

        if self.delay.is_none() {
            self.delay = Some(packet.dts.wrapping_neg());
        }
        if packet.dts < self.delay.unwrap_or(0).wrapping_neg() {
            warn!("Packets are not in the proper order with respect to DTS");
            return Err(Error::InvalidArgument);
        }
        if matches!(
            cfg.codec_id,
            CodecId::H264 | CodecId::Mpeg4 | CodecId::Hevc | CodecId::Av1 | CodecId::Vp9
        ) && packet.pts == wedeo_core::timestamp::NOPTS_VALUE
        {
            error!("Packet is missing PTS");
            return Err(Error::InvalidArgument);
        }

        if cfg.last_ts < packet.dts {
            self.streams[cfg_index].last_ts = packet.dts;
        }
        // Conservative pre-check (FFmpeg: size + flags_size >= 1 << 24).
        if data.len() + 32 >= MAX_TAG_DATA_SIZE {
            error!(size = data.len(), "Too large FLV packet");
            return Err(Error::InvalidArgument);
        }

        let ts = packet.dts;
        let frametype = if packet.flags.contains(PacketFlags::KEY) {
            FLV_FRAME_KEY
        } else {
            FLV_FRAME_INTER
        };

        let tag_type = match cfg.media_type {
            MediaType::Video => FLV_TAG_TYPE_VIDEO,
            MediaType::Audio => FLV_TAG_TYPE_AUDIO,
            MediaType::Data | MediaType::Subtitle => FLV_TAG_TYPE_META,
            MediaType::Attachment => return Err(Error::InvalidArgument),
        };

        let mut tag_data = Vec::with_capacity(data.len() + 16);

        if cfg.media_type == MediaType::Data || cfg.media_type == MediaType::Subtitle {
            if cfg.codec_id == CodecId::SubText {
                // The text is written as an AMF0 string with a 16-bit length.
                wb8(&mut tag_data, AMF_DATA_TYPE_STRING);
                put_amf_string(&mut tag_data, "onTextData");
                wb8(&mut tag_data, AMF_DATA_TYPE_MIXEDARRAY);
                wb32(&mut tag_data, 2);
                put_amf_string(&mut tag_data, "type");
                wb8(&mut tag_data, AMF_DATA_TYPE_STRING);
                put_amf_string(&mut tag_data, "Text");
                put_amf_string(&mut tag_data, "text");
                wb8(&mut tag_data, AMF_DATA_TYPE_STRING);
                let text = String::from_utf8_lossy(&data);
                put_amf_string(&mut tag_data, &text);
                put_amf_string(&mut tag_data, "");
                wb8(&mut tag_data, AMF_END_OF_OBJECT);
            } else {
                tag_data.append(&mut data);
            }
        } else if is_extended_video(cfg.codec_id)
            || (cfg.codec_id == CodecId::H264 && cfg.track_idx != 0)
        {
            let h26456 = matches!(cfg.codec_id, CodecId::H264 | CodecId::Hevc);
            let mut pkttype = PACKET_TYPE_CODEDFRAMES;
            if h26456 && packet.pts == packet.dts {
                pkttype = PACKET_TYPE_CODEDFRAMES_X;
            }
            if cfg.track_idx != 0 {
                wb8(&mut tag_data, FLV_IS_EX_HEADER | 0x06 | frametype);
                wb8(&mut tag_data, pkttype);
            } else {
                wb8(&mut tag_data, FLV_IS_EX_HEADER | pkttype | frametype);
            }
            if let Some(fourcc) = codec_fourcc(cfg.codec_id) {
                tag_data.extend_from_slice(&fourcc);
            }
            if cfg.track_idx != 0 {
                wb8(&mut tag_data, cfg.track_idx as u8);
            }
            if h26456 && pkttype == PACKET_TYPE_CODEDFRAMES {
                wb24(&mut tag_data, packet.pts.wrapping_sub(packet.dts) as u32);
            }
            tag_data.extend_from_slice(&data);
        } else if is_extended_audio(cfg.codec_id)
            || (cfg.codec_id == CodecId::Aac && cfg.track_idx != 0)
        {
            if cfg.track_idx != 0 {
                wb8(&mut tag_data, FLV_CODECID_EX_HEADER | 0x05);
                wb8(&mut tag_data, AUDIO_PACKET_TYPE_CODEDFRAMES);
            } else {
                wb8(
                    &mut tag_data,
                    FLV_CODECID_EX_HEADER | AUDIO_PACKET_TYPE_CODEDFRAMES,
                );
            }
            if let Some(fourcc) = codec_fourcc(cfg.codec_id) {
                tag_data.extend_from_slice(&fourcc);
            }
            if cfg.track_idx != 0 {
                wb8(&mut tag_data, cfg.track_idx as u8);
            }
            tag_data.extend_from_slice(&data);
        } else {
            let flags = if cfg.media_type == MediaType::Video {
                cfg.codec_tag as u8 | frametype
            } else {
                cfg.audio_flags
            };
            wb8(&mut tag_data, flags);
            match cfg.codec_id {
                CodecId::Aac => {
                    wb8(&mut tag_data, 1); // AAC raw
                }
                CodecId::H264 | CodecId::Mpeg4 => {
                    wb8(&mut tag_data, 1); // AVC NALU
                    wb24(&mut tag_data, packet.pts.wrapping_sub(packet.dts) as u32);
                }
                _ => {}
            }
            tag_data.extend_from_slice(&data);
        }

        if tag_data.len() >= MAX_TAG_DATA_SIZE {
            error!(size = tag_data.len(), "Too large FLV packet");
            return Err(Error::InvalidArgument);
        }
        write_tag_ex(io, tag_type, &tag_data, ts, self.reserved)?;

        // flv->duration = FFMAX(flv->duration, pkt->pts + flv->delay + pkt->duration)
        if matches!(cfg.media_type, MediaType::Video | MediaType::Audio) {
            let packet_end = packet
                .pts
                .wrapping_add(self.delay.unwrap_or(0))
                .wrapping_add(packet.duration);
            if packet_end > self.duration_ms {
                self.duration_ms = packet_end;
            }
        }

        // NEW_EXTRADATA updates the codec header tag in place in FFmpeg; with
        // our in-memory header we append a fresh sequence-header tag instead.
        for side in &packet.side_data {
            if side.data_type == PacketSideDataType::NewExtradata && !side.data.is_empty() {
                self.streams[cfg_index].extradata = side.data.clone();
                if let Some(new_header) = self.streams[cfg_index].build_codec_header()? {
                    write_tag(io, tag_type, &new_header, ts)?;
                }
            }
        }

        trace!(
            stream_index = packet.stream_index,
            ts,
            size = data.len(),
            "FLV packet written"
        );
        Ok(())
    }

    fn write_trailer(&mut self, io: &mut BufferedIo) -> Result<()> {
        // Add the AVC end-of-sequence tag for H.264/MPEG-4 video streams.
        for i in 0..self.streams.len() {
            let isvideo = self.streams[i].media_type == MediaType::Video;
            let codec = self.streams[i].codec_id;
            if isvideo && (codec == CodecId::H264 || codec == CodecId::Mpeg4) {
                let cfg = self.streams[i].clone();
                self.write_eos_tag(io, &cfg)?;
            }
        }

        let end = io.tell()?;
        if io.is_seekable() {
            if let Some(offset) = self.duration_offset {
                io.seek(offset)?;
                io_put_f64(io, self.duration_ms as f64 / 1000.0)?;
            }
            if let Some(offset) = self.filesize_offset {
                io.seek(offset)?;
                io_put_f64(io, end as f64)?;
            }
            io.seek(end)?;
        } else {
            warn!("non-seekable output: FLV duration/filesize not patched");
        }
        io.flush()?;
        Ok(())
    }
}

/// Muxer factory registration, following `wedeo-format-ivf-mux`.
struct FlvMuxerFactory;

impl MuxerFactory for FlvMuxerFactory {
    fn descriptor(&self) -> &OutputFormatDescriptor {
        static DESC: OutputFormatDescriptor = OutputFormatDescriptor {
            name: "flv",
            long_name: "FLV (Flash Video)",
            extensions: "flv",
            mime_types: "video/x-flv",
            flags: OutputFormatFlags::GLOBALHEADER
                .union(OutputFormatFlags::VARIABLE_FPS)
                .union(OutputFormatFlags::TS_NONSTRICT),
            audio_codec: CodecId::Mp3,
            video_codec: CodecId::H263,
        };
        &DESC
    }

    fn create(&self) -> Result<Box<dyn Muxer>> {
        Ok(Box::new(FlvMuxer::new()))
    }
}

inventory::submit!(&FlvMuxerFactory as &dyn MuxerFactory);

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;
    use std::sync::{Arc, Mutex};

    use wedeo_codec::decoder::CodecParameters;
    use wedeo_core::rational::Rational;
    use wedeo_format::io::IoContext;

    use super::*;

    /// In-memory IoContext, shared with the test through `Arc<Mutex<_>>`.
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
            assert!(self.seekable, "seek() on non-seekable MemoryIo");
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

    fn video_stream(id: CodecId, w: u32, h: u32, tb: Rational, duration: i64) -> Stream {
        let mut par = CodecParameters::new(id, MediaType::Video);
        par.width = w;
        par.height = h;
        let mut s = Stream::new(0, par);
        s.time_base = tb;
        s.duration = duration;
        s
    }

    fn audio_stream(
        id: CodecId,
        rate: u32,
        channels: i32,
        bit_rate: i64,
        extradata: &[u8],
    ) -> Stream {
        let mut par = CodecParameters::new(id, MediaType::Audio);
        par.sample_rate = rate;
        par.channel_layout = if channels == 1 {
            wedeo_core::channel_layout::ChannelLayout::mono()
        } else {
            wedeo_core::channel_layout::ChannelLayout::stereo()
        };
        par.bit_rate = bit_rate;
        par.bits_per_coded_sample = 16;
        par.extradata = extradata.to_vec();
        Stream::new(0, par)
    }

    fn be24(b: &[u8]) -> u32 {
        (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32
    }

    fn f64_at(b: &[u8]) -> f64 {
        f64::from_bits(u64::from_be_bytes(b[..8].try_into().unwrap()))
    }

    fn write_all(
        streams: &[Stream],
        packets: &[(i64, i64, i64, &[u8], PacketFlags)],
        seekable: bool,
    ) -> Vec<u8> {
        let (io, shared) = MemoryIo::new(seekable);
        let mut io = BufferedIo::new(Box::new(io));
        let mut muxer = FlvMuxer::new();
        muxer.write_header(&mut io, streams).unwrap();
        for (pts, dts, duration, payload, flags) in packets {
            let mut p = Packet::from_slice(payload);
            p.pts = *pts;
            p.dts = *dts;
            p.duration = *duration;
            p.flags = *flags;
            p.stream_index = 0;
            muxer.write_packet(&mut io, &p).unwrap();
        }
        muxer.write_trailer(&mut io).unwrap();
        io.flush().unwrap();
        shared.lock().unwrap().clone()
    }

    #[test]
    fn flv_header_and_metadata_layout() {
        let mut stream = video_stream(CodecId::H264, 64, 48, Rational::new(1, 5), 2);
        stream.codec_params.extradata = vec![
            0x01, 0x42, 0xc0, 0x0a, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x42, 0x00, 0x00, 0x03,
        ];
        let bytes = write_all(&[stream], &[], true);

        assert_eq!(&bytes[0..3], b"FLV");
        assert_eq!(bytes[3], 1); // version
        assert_eq!(bytes[4], FLV_HEADER_FLAG_HASVIDEO);
        assert_eq!(u32::from_be_bytes(bytes[5..9].try_into().unwrap()), 9);
        assert_eq!(u32::from_be_bytes(bytes[9..13].try_into().unwrap()), 0);

        // First tag is script data.
        assert_eq!(bytes[13], FLV_TAG_TYPE_META);
        let meta_size = be24(&bytes[14..17]) as usize;
        assert_eq!(meta_size, 159, "onMetaData size must match ffmpeg flvenc.c");
        // metadata tag + AVC sequence-header tag + AVC end-of-sequence tag.
        assert_eq!(bytes.len(), 13 + 15 + meta_size + (15 + 18) + (15 + 5));
        assert_eq!(&bytes[24..27], &[0x02, 0x00, 0x0a]); // AMF string "onMetaData"
        assert_eq!(&bytes[27..37], b"onMetaData");
        assert_eq!(bytes[37], AMF_DATA_TYPE_MIXEDARRAY);
        assert_eq!(u32::from_be_bytes(bytes[38..42].try_into().unwrap()), 7);
    }

    #[test]
    fn amf_primitives_match_ffmpeg() {
        let mut v = Vec::new();
        put_amf_string(&mut v, "onMetaData");
        assert_eq!(&v[..2], &[0x00, 0x0a]);
        assert_eq!(&v[2..], b"onMetaData");

        let mut v = Vec::new();
        let pos = put_amf_double(&mut v, 0.4);
        assert_eq!(v[0], AMF_DATA_TYPE_NUMBER);
        assert_eq!(pos, 1);
        assert_eq!(
            f64::from_bits(u64::from_be_bytes(v[1..9].try_into().unwrap())),
            0.4
        );

        let mut v = Vec::new();
        put_amf_bool(&mut v, true);
        assert_eq!(v, vec![AMF_DATA_TYPE_BOOL, 1]);
    }

    #[test]
    fn audio_flags_for_aac_and_pcm() {
        let aac = audio_stream(CodecId::Aac, 44100, 2, 32000, &[0x12, 0x10]);
        assert_eq!(get_audio_flags(&aac).unwrap(), 0xaf);

        let mp3 = audio_stream(CodecId::Mp3, 44100, 2, 128000, &[]);
        let flags = get_audio_flags(&mp3).unwrap();
        assert_eq!(
            flags,
            2 << 4 | FLV_SAMPLERATE_44100HZ | FLV_SAMPLESSIZE_16BIT | 1
        );

        let pcm = audio_stream(CodecId::PcmS16le, 44100, 1, 0, &[]);
        let flags = get_audio_flags(&pcm).unwrap();
        assert_eq!(
            flags,
            3 << 4 | FLV_SAMPLERATE_44100HZ | FLV_SAMPLESSIZE_16BIT
        );

        let mut bad = audio_stream(CodecId::Mp3, 48000, 2, 0, &[]);
        bad.codec_params.sample_rate = 22050;
        assert!(get_audio_flags(&bad).is_ok());
    }

    #[test]
    fn avcc_passthrough_and_annexb_conversion() {
        let avcc = [0x01, 0x42, 0xc0, 0x0a, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x42];
        assert_eq!(build_avcc(&avcc).unwrap(), avcc);

        // Annex-B: 4-byte start code + SPS, 3-byte start code + PPS.
        let annexb = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x0a, 0x00, 0x00, 0x01, 0x68, 0xce,
        ];
        let out = build_avcc(&annexb).unwrap();
        assert_eq!(out[0], 1);
        assert_eq!(out[1], 0x42); // profile from SPS
        assert_eq!(out[4], 0xff);
        assert_eq!(out[5], 0xe1); // one SPS
    }

    #[test]
    fn rejects_empty_and_unsupported_streams() {
        let (io, _) = MemoryIo::new(true);
        let mut io = BufferedIo::new(Box::new(io));
        let mut muxer = FlvMuxer::new();
        assert_eq!(muxer.write_header(&mut io, &[]), Err(Error::StreamNotFound));

        let stream = video_stream(CodecId::Vp8, 64, 48, Rational::new(1, 30), 0);
        let (io, _) = MemoryIo::new(true);
        let mut io = BufferedIo::new(Box::new(io));
        assert_eq!(
            muxer.write_header(&mut io, &[stream]),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn audio_sequence_header_and_raw_tag() {
        let mut stream = audio_stream(CodecId::Aac, 44100, 2, 32000, &[0x12, 0x10]);
        stream.duration = 123;
        stream.time_base = Rational::new(1, 1000);
        let packets = [
            (0, 0, 23, &b"\x01\x02\x03"[..], PacketFlags::KEY),
            (23, 23, 23, &b"\x04\x05"[..], PacketFlags::empty()),
        ];
        let bytes = write_all(&[stream], &packets, true);

        assert_eq!(bytes[4], FLV_HEADER_FLAG_HASAUDIO);
        // Metadata tag: type, size, ts, streamid.
        assert_eq!(bytes[13], FLV_TAG_TYPE_META);
        let meta_size = be24(&bytes[14..17]) as usize;
        // Sequence header tag follows the metadata tag.
        let seq = 13 + 15 + meta_size;
        assert_eq!(bytes[seq], FLV_TAG_TYPE_AUDIO);
        assert_eq!(be24(&bytes[seq + 1..]), 4); // af 00 12 10
        assert_eq!(&bytes[seq + 11..seq + 15], &[0xaf, 0x00, 0x12, 0x10]);
        assert_eq!(
            u32::from_be_bytes(bytes[seq + 15..seq + 19].try_into().unwrap()),
            15
        );

        // First raw AAC tag.
        let raw = seq + 19;
        assert_eq!(bytes[raw], FLV_TAG_TYPE_AUDIO);
        assert_eq!(be24(&bytes[raw + 1..]), 5); // af 01 + 3 bytes
        assert_eq!(&bytes[raw + 11..raw + 14], &[0xaf, 0x01, 0x01]);
        assert_eq!(be24(&bytes[raw + 4..]), 0); // timestamp 0
    }

    #[test]
    fn video_packet_timestamps_and_eos() {
        let mut stream = video_stream(CodecId::H264, 64, 48, Rational::new(1, 5), 2);
        stream.codec_params.extradata = vec![
            0x01, 0x42, 0xc0, 0x0a, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x42, 0x00, 0x00, 0x03,
        ];
        let packets = [
            (
                0,
                0,
                200,
                &b"\x00\x00\x00\x02\x65\xaa"[..],
                PacketFlags::KEY,
            ),
            (
                200,
                200,
                200,
                &b"\x00\x00\x00\x02\x41\xbb"[..],
                PacketFlags::empty(),
            ),
        ];
        let bytes = write_all(&[stream], &packets, true);
        assert_eq!(bytes[4], FLV_HEADER_FLAG_HASVIDEO);

        // Walk tags and verify types, timestamps and sizes.
        let mut pos = 13usize;
        let mut tags = Vec::new();
        while pos < bytes.len() {
            let ty = bytes[pos];
            let size = be24(&bytes[pos + 1..]) as usize;
            let ts = be24(&bytes[pos + 4..]) as i64 | ((bytes[pos + 7] as i64 & 0x7f) << 24);
            let prev =
                u32::from_be_bytes(bytes[pos + 11 + size..pos + 15 + size].try_into().unwrap());
            assert_eq!(prev as usize, size + 11);
            tags.push((ty, size, ts));
            pos += 15 + size;
        }
        assert_eq!(tags.len(), 5); // metadata, sequence header, 2 packets, EOS
        assert_eq!(tags[0].0, FLV_TAG_TYPE_META);
        assert_eq!(tags[1].0, FLV_TAG_TYPE_VIDEO);
        assert_eq!(
            &bytes[13 + 15 + be24(&bytes[14..17]) as usize + 11..][..5],
            &[0x17, 0, 0, 0, 0]
        );
        assert_eq!(tags[2], (FLV_TAG_TYPE_VIDEO, 11, 0));
        assert_eq!(tags[3], (FLV_TAG_TYPE_VIDEO, 11, 200));
        assert_eq!(tags[4], (FLV_TAG_TYPE_VIDEO, 5, 200)); // EOS tag

        // Duration patched to 0.4 s; filesize patched to final length.
        let meta_data = 13 + 11;
        let mut off = meta_data
            + 1 + 2 + 10 /* AMF string: marker + length + "onMetaData" */
            + 1 /* mixedarray */
            + 4 /* count */;
        off += 1 + 2 + 8 /* "duration" key: marker + length + bytes */;
        assert_eq!(f64_at(&bytes[off..]), 0.4);
    }

    #[test]
    fn trailer_patch_on_non_seekable_output_is_skipped() {
        let mut stream = audio_stream(CodecId::Aac, 44100, 2, 32000, &[0x12, 0x10]);
        stream.duration = 0;
        stream.time_base = Rational::new(1, 1000);
        let bytes = write_all(&[stream], &[], false);
        assert_eq!(bytes[13], FLV_TAG_TYPE_META);
        // No EOS tag for audio; duration stays at the initial guess (0.0).
        let meta_data = 13 + 11;
        let mut off = meta_data + 1 + 2 + 10 + 1 + 4;
        off += 1 + 2 + 8;
        assert_eq!(f64_at(&bytes[off..]), 0.0);
    }

    #[test]
    fn mp3_and_pcm_audio_tags() {
        let mp3 = audio_stream(CodecId::Mp3, 44100, 2, 128000, &[]);
        let bytes = write_all(
            &[mp3],
            &[(0, 0, 26, &b"\xff\xfb\x90\x64"[..], PacketFlags::KEY)],
            true,
        );
        let meta_size = be24(&bytes[14..17]) as usize;
        let tag = 13 + 15 + meta_size; // no sequence header for MP3
        assert_eq!(bytes[tag], FLV_TAG_TYPE_AUDIO);
        assert_eq!(be24(&bytes[tag + 1..]), 5); // 1 flag byte + 4 payload bytes
        assert_eq!(bytes[tag + 11], 0x2f); // MP3 | 44.1kHz | 16-bit | stereo
        assert_eq!(&bytes[tag + 12..tag + 16], b"\xff\xfb\x90\x64");

        let pcm = audio_stream(CodecId::PcmS16le, 22050, 1, 0, &[]);
        let bytes = write_all(
            &[pcm],
            &[(0, 0, 10, &b"\x01\x02"[..], PacketFlags::empty())],
            true,
        );
        let tag = 13 + 15 + be24(&bytes[14..17]) as usize;
        assert_eq!(
            bytes[tag + 11],
            3 << 4 | FLV_SAMPLERATE_22050HZ | FLV_SAMPLESSIZE_16BIT
        );
    }

    #[test]
    fn extended_video_and_audio_headers() {
        let mut hevc = video_stream(CodecId::Hevc, 64, 48, Rational::new(1, 25), 1);
        hevc.codec_params.extradata = vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07];
        let bytes = write_all(
            &[hevc],
            &[(0, 0, 40, &b"\x26\x01\xaa"[..], PacketFlags::KEY)],
            true,
        );
        let meta_size = be24(&bytes[14..17]) as usize;
        let seq = 13 + 15 + meta_size;
        assert_eq!(bytes[seq], FLV_TAG_TYPE_VIDEO);
        assert_eq!(bytes[seq + 11], 0x90); // ex-header + sequence start + key
        assert_eq!(&bytes[seq + 12..seq + 16], b"hvc1");
        assert_eq!(&bytes[seq + 16..seq + 23], &[1, 2, 3, 4, 5, 6, 7]);
        let pkt = seq + 15 + be24(&bytes[seq + 1..]) as usize;
        assert_eq!(bytes[pkt + 11], 0x93); // ex-header + coded frames X + key
        assert_eq!(&bytes[pkt + 12..pkt + 16], b"hvc1");
        assert_eq!(&bytes[pkt + 16..pkt + 19], b"\x26\x01\xaa");

        let mut opus = audio_stream(CodecId::Opus, 48000, 2, 64000, &[0x80, 0x01]);
        opus.duration = 20;
        opus.time_base = Rational::new(1, 1000);
        let bytes = write_all(
            &[opus],
            &[(0, 0, 20, &b"\xfc\xaa"[..], PacketFlags::empty())],
            true,
        );
        let meta_size = be24(&bytes[14..17]) as usize;
        let seq = 13 + 15 + meta_size;
        assert_eq!(bytes[seq], FLV_TAG_TYPE_AUDIO);
        assert_eq!(bytes[seq + 11], 0x90); // ex-header + sequence start
        assert_eq!(&bytes[seq + 12..seq + 16], b"Opus");
        assert_eq!(&bytes[seq + 16..seq + 18], &[0x80, 0x01]);
        let pkt = seq + 15 + be24(&bytes[seq + 1..]) as usize;
        assert_eq!(bytes[pkt + 11], 0x91); // ex-header + coded frames
        assert_eq!(&bytes[pkt + 12..pkt + 16], b"Opus");
    }

    #[test]
    fn factory_descriptor() {
        let factory = FlvMuxerFactory;
        let d = factory.descriptor();
        assert_eq!(d.name, "flv");
        assert_eq!(d.long_name, "FLV (Flash Video)");
        assert_eq!(d.extensions, "flv");
        assert_eq!(d.mime_types, "video/x-flv");
        assert_eq!(d.video_codec, CodecId::H263);
        assert_eq!(d.audio_codec, CodecId::Mp3);
        assert!(d.flags.contains(OutputFormatFlags::GLOBALHEADER));
        assert!(factory.create().is_ok());
    }
}
