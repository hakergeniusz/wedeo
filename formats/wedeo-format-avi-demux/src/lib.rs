// AVI demuxer for wedeo, translated from FFmpeg's libavformat/avidec.c.
//
// RIFF layout:
//   'RIFF' <size> 'AVI '
//     LIST 'hdrl'
//       'avih' <main AVI header>
//       LIST 'strl' (one per stream)
//         'strh' <stream header>
//         'strf' <stream format: BITMAPINFOHEADER or WAVEFORMATEX>
//         'strd' <optional codec private data>
//     LIST 'INFO' <metadata>
//     LIST 'movi'
//       '##dc' <compressed video> / '##wb' <audio> / '##pc' <palette>
//     'idx1' <optional AVI index>
//
// Timestamps are derived from each stream's `strh` scale/rate pair; the AVI
// frame counter is advanced by `get_duration()` exactly as in FFmpeg.

mod tags;

#[cfg(test)]
mod tests;

use tracing::{debug, trace};
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::buffer::Buffer;
use wedeo_core::channel_layout::ChannelLayout;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::metadata::Metadata;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::Rational;
use wedeo_format::demuxer::{
    Demuxer, DemuxerHeader, InputFormatDescriptor, InputFormatFlags, PROBE_SCORE_MAX, ProbeData,
    SeekFlags, Stream,
};
use wedeo_format::io::BufferedIo;
use wedeo_format::registry::DemuxerFactory;

use tags::{bmp_tag_to_codec_id, wav_tag_to_codec_id};

// ---------------------------------------------------------------------------
// Constants from libavformat/avi.h and the FFmpeg source
// ---------------------------------------------------------------------------

/// `AVIF_MUSTUSEINDEX`: force non-interleaved mode.
const AVIF_MUSTUSEINDEX: u32 = 0x0000_0020;
/// `AVIIF_INDEX`: idx1 entry flag marking a keyframe.
const AVIIF_INDEX: u32 = 0x0000_0010;

const TAG_RIFF: u32 = u32::from_le_bytes(*b"RIFF");
const TAG_ON2: u32 = u32::from_le_bytes(*b"ON2 ");
const TAG_LIST: u32 = u32::from_le_bytes(*b"LIST");
const TAG_MOVI: u32 = u32::from_le_bytes(*b"movi");
const TAG_INFO: u32 = u32::from_le_bytes(*b"INFO");
const TAG_AVIH: u32 = u32::from_le_bytes(*b"avih");
const TAG_AMVH: u32 = u32::from_le_bytes(*b"amvh");
const TAG_STRH: u32 = u32::from_le_bytes(*b"strh");
const TAG_STRF: u32 = u32::from_le_bytes(*b"strf");
const TAG_STRD: u32 = u32::from_le_bytes(*b"strd");
const TAG_IDX1: u32 = u32::from_le_bytes(*b"idx1");
const TAG_IDIT: u32 = u32::from_le_bytes(*b"IDIT");
const TAG_NDX: u32 = u32::from_le_bytes(*b"ndx ");
const TAG_JUNK: u32 = u32::from_le_bytes(*b"JUNK");
const TAG_VIDS: u32 = u32::from_le_bytes(*b"vids");
const TAG_AUDS: u32 = u32::from_le_bytes(*b"auds");
const TAG_TXTS: u32 = u32::from_le_bytes(*b"txts");
const TAG_DATS: u32 = u32::from_le_bytes(*b"dats");
const TAG_PADS: u32 = u32::from_le_bytes(*b"pads");
const TAG_DXSB: u32 = u32::from_le_bytes(*b"DXSB");
const TAG_DXSA: u32 = u32::from_le_bytes(*b"DXSA");

/// AVI metadata tags read by `avi_read_tag()` (avidec.c:323).
const METADATA_TAGS: [u32; 9] = [
    u32::from_le_bytes(*b"strn"),
    u32::from_le_bytes(*b"isbj"),
    u32::from_le_bytes(*b"inam"),
    u32::from_le_bytes(*b"iart"),
    u32::from_le_bytes(*b"icop"),
    u32::from_le_bytes(*b"icmt"),
    u32::from_le_bytes(*b"ignr"),
    u32::from_le_bytes(*b"iprd"),
    u32::from_le_bytes(*b"isft"),
];

/// Valid RIFF form types (`avi_headers` in avidec.c).
const AVI_HEADERS: [[u8; 4]; 5] = [*b"AVI ", *b"AVIX", *b"AVI\x19", *b"ON2f", *b"AMV "];

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct IndexEntry {
    /// Absolute file offset of the chunk payload.
    pos: u64,
    /// Frame counter (or byte counter) at this entry.
    timestamp: i64,
    size: u32,
    flags: u32,
}

#[derive(Debug, Clone)]
struct AviStreamState {
    /// Current frame (video) or byte (audio) counter used to compute the PTS.
    frame_offset: i64,
    /// Bytes remaining in the current `movi` chunk.
    remaining: u32,
    packet_size: u32,
    handler: u32,
    scale: u32,
    rate: u32,
    /// Size of one sample (or packet) in bytes, from `strh`.
    sample_size: i32,
    /// Block align variable used to emulate bugs in the MS dshow demuxer.
    dshow_block_align: u32,
    /// Temporary storage used while parsing idx1.
    cum_len: i64,
    /// Normally 'd'<<8 + 'c' or 'w'<<8 + 'b'.
    prefix: u32,
    prefix_count: i32,
    /// Number of frames declared in `strh`.
    nb_frames: i64,
    index: Vec<IndexEntry>,
    codec_type: MediaType,
}

impl AviStreamState {
    fn new() -> Self {
        Self {
            frame_offset: 0,
            remaining: 0,
            packet_size: 0,
            handler: 0,
            scale: 1,
            rate: 25,
            sample_size: 0,
            dshow_block_align: 0,
            cum_len: 0,
            prefix: 0,
            prefix_count: 0,
            nb_frames: 0,
            index: Vec::new(),
            codec_type: MediaType::Data,
        }
    }
}

struct AviDemuxer {
    streams_state: Vec<AviStreamState>,
    riff_end: u64,
    movi_end: u64,
    fsize: u64,
    movi_list: u64,
    last_pkt_pos: u64,
    /// Currently active stream for a partially read chunk; -1 means resync.
    stream_index: i32,
    index_loaded: bool,
    non_interleaved: bool,
    header_read: bool,
    frame_period: u32,
}

impl AviDemuxer {
    fn new() -> Self {
        Self {
            streams_state: Vec::new(),
            riff_end: 0,
            movi_end: 0,
            fsize: 0,
            movi_list: 0,
            last_pkt_pos: 0,
            stream_index: -1,
            index_loaded: false,
            non_interleaved: false,
            header_read: false,
            frame_period: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tag_bytes(tag: u32) -> [u8; 4] {
    tag.to_le_bytes()
}

/// `get_duration()` from avidec.c:147.
fn get_duration(ast: &AviStreamState, len: i32) -> i64 {
    if ast.sample_size != 0 {
        i64::from(len)
    } else if ast.dshow_block_align != 0 {
        (i64::from(len) + i64::from(ast.dshow_block_align) - 1) / i64::from(ast.dshow_block_align)
    } else {
        1
    }
}

/// `get_stream_idx()` from avidec.c:1238. Returns 100 for an invalid tag.
fn get_stream_idx(d: &[u8; 2]) -> usize {
    if d[0].is_ascii_digit() && d[1].is_ascii_digit() {
        ((d[0] - b'0') as usize) * 10 + (d[1] - b'0') as usize
    } else {
        100
    }
}

/// Read a chunk header, translating a clean EOF into `InvalidData` for the
/// header parser (matching `if (avio_feof(pb)) return AVERROR_INVALIDDATA;`).
fn read_chunk_header(io: &mut BufferedIo) -> Result<(u32, u32)> {
    let tag = match io.read_u32le() {
        Ok(v) => v,
        Err(Error::Eof) => return Err(Error::InvalidData),
        Err(e) => return Err(e),
    };
    let size = match io.read_u32le() {
        Ok(v) => v,
        Err(Error::Eof) => return Err(Error::InvalidData),
        Err(e) => return Err(e),
    };
    Ok((tag, size))
}

/// Skip a chunk padded to an even boundary, as done for most header tags.
fn skip_chunk(io: &mut BufferedIo, size: u32) -> Result<()> {
    io.skip(u64::from(size) + u64::from(size & 1))
}

/// Read metadata strings from a `LIST INFO` chunk (`ff_read_riff_info`).
fn read_riff_info(io: &mut BufferedIo, size: u64, metadata: &mut Metadata) -> Result<()> {
    let end = io.tell()?.saturating_add(size);
    while io.tell()? + 8 <= end {
        let tag = match io.read_u32le() {
            Ok(v) => v,
            Err(_) => break,
        };
        let chunk_size = match io.read_u32le() {
            Ok(v) => v,
            Err(_) => break,
        };
        let pos = io.tell()?;
        if u64::from(chunk_size) > end.saturating_sub(pos) {
            break;
        }
        let value = io.read_bytes(chunk_size as usize)?;
        if chunk_size & 1 != 0 {
            io.skip(1)?;
        }
        let key = String::from_utf8_lossy(&tag_bytes(tag)).into_owned();
        let value = value.split(|b| *b == 0).next().unwrap_or(&value);
        metadata.set(key, String::from_utf8_lossy(value).into_owned());
    }
    Ok(())
}

/// `avi_read_tag()` from avidec.c:323. String metadata is NUL terminated and
/// chunk sizes are padded to an even boundary.
fn read_avi_tag(
    io: &mut BufferedIo,
    tag: u32,
    size: u32,
    metadata: Option<&mut Metadata>,
) -> Result<()> {
    if size == u32::MAX {
        return Err(Error::InvalidArgument);
    }
    let padded = size.wrapping_add(size & 1);
    let mut value = io.read_bytes(padded as usize)?;
    let end = value.iter().position(|b| *b == 0).unwrap_or(value.len());
    value.truncate(end);
    if let Some(metadata) = metadata {
        let key = String::from_utf8_lossy(&tag_bytes(tag)).into_owned();
        metadata.set(key, String::from_utf8_lossy(&value).into_owned());
    }
    Ok(())
}

/// `ff_get_extradata()` equivalent: read up to `len` bytes (truncated files
/// yield whatever is left, matching `av_get_packet` behaviour).
fn get_extradata(io: &mut BufferedIo, len: usize) -> Result<Vec<u8>> {
    io.read_up_to(len)
}

// ---------------------------------------------------------------------------
// Header parsing helpers (inlined from avidec.c)
// ---------------------------------------------------------------------------

impl AviDemuxer {
    /// Parse the `avih` / `amvh` main header (avidec.c:589-604).
    fn parse_avih(
        &mut self,
        io: &mut BufferedIo,
        size: u32,
        avih_width: &mut u32,
        avih_height: &mut u32,
    ) -> Result<()> {
        self.frame_period = io.read_u32le()?;
        let _max_bytes_per_sec = io.read_u32le()?;
        let _padding = io.read_u32le()?;
        let flags = io.read_u32le()?;
        self.non_interleaved |= flags & AVIF_MUSTUSEINDEX != 0;

        io.skip(8)?;
        let _total_frames = io.read_u32le()?;
        let _initial_frames = io.read_u32le()?;
        *avih_width = io.read_u32le()?;
        *avih_height = io.read_u32le()?;

        let consumed = 10 * 4;
        if size > consumed {
            skip_chunk(io, size - consumed)?;
        }
        Ok(())
    }

    /// Parse a `strh` stream header (avidec.c:605-762). A `pads` stream is
    /// skipped and returns `None`.
    #[allow(clippy::too_many_arguments)]
    fn parse_strh(
        &mut self,
        io: &mut BufferedIo,
        size: u32,
        stream_index: &mut i32,
        codec_type: &mut MediaType,
        amv_file_format: bool,
    ) -> Result<Option<(AviStreamState, Stream)>> {
        let mut tag1 = io.read_u32le()?;
        let handler = io.read_u32le()?;
        if tag1 == TAG_PADS {
            skip_chunk(io, size.saturating_sub(8))?;
            return Ok(None);
        }

        *stream_index += 1;
        let idx = *stream_index as usize;
        let mut state = AviStreamState::new();
        state.handler = handler;

        if amv_file_format {
            tag1 = if idx == 0 { TAG_VIDS } else { TAG_AUDS };
        }

        let _flags = io.read_u32le()?;
        let _priority = io.read_u16le()?;
        let _language = io.read_u16le()?;
        let _initial_frame = io.read_u32le()?;
        state.scale = io.read_u32le()?;
        state.rate = io.read_u32le()?;
        if state.scale == 0 || state.rate == 0 {
            if self.frame_period != 0 {
                state.rate = 1_000_000;
                state.scale = self.frame_period;
            } else {
                state.rate = 25;
                state.scale = 1;
            }
        }

        let start = io.read_u32le()?;
        let nb_frames = io.read_u32le()?;
        let _buffer_size = io.read_u32le()?;
        let _quality = io.read_u32le()?;
        state.sample_size = io.read_i32le()?;
        if state.sample_size < 0 {
            state.sample_size = 0;
        }
        state.cum_len = i64::from(start) * i64::from(state.sample_size.max(1));
        state.frame_offset = state.cum_len;
        state.nb_frames = i64::from(nb_frames);

        *codec_type = match tag1 {
            TAG_VIDS => MediaType::Video,
            TAG_AUDS => MediaType::Audio,
            TAG_TXTS => MediaType::Subtitle,
            TAG_DATS => MediaType::Data,
            _ => MediaType::Data,
        };
        state.codec_type = *codec_type;
        if *codec_type == MediaType::Video {
            state.sample_size = 0;
        }

        if size > 48 {
            skip_chunk(io, size - 48)?;
        }

        let mut params = CodecParameters::new(CodecId::None, *codec_type);
        params.time_base = Rational::new(state.scale as i32, state.rate as i32);
        params.codec_tag = handler;
        let mut stream = Stream::new(idx, params);
        stream.time_base = Rational::new(state.scale as i32, state.rate as i32);
        stream.nb_frames = i64::from(nb_frames);
        if *codec_type == MediaType::Video {
            stream.duration = i64::from(nb_frames);
        }
        Ok(Some((state, stream)))
    }

    /// Parse a `strf` chunk: BITMAPINFOHEADER for video, WAVEFORMATEX for audio.
    #[allow(clippy::too_many_arguments)]
    fn parse_strf(
        &mut self,
        io: &mut BufferedIo,
        size: u32,
        stream_index: usize,
        amv_file_format: bool,
        avih_width: u32,
        avih_height: u32,
        stream: &mut Stream,
    ) -> Result<()> {
        let codec_type = self.streams_state[stream_index].codec_type;
        match codec_type {
            MediaType::Video => {
                let params = &mut stream.codec_params;
                if amv_file_format {
                    params.width = avih_width;
                    params.height = avih_height;
                    params.codec_id = CodecId::Mpeg4;
                    params.codec_tag = u32::from_le_bytes(*b"AMV ");
                    skip_chunk(io, size)?;
                    return Ok(());
                }

                let esize = io.read_u32le()?;
                let width = io.read_i32le()?;
                let height = io.read_i32le()?;
                let _planes = io.read_u16le()?;
                let bits = io.read_u16le()?;
                let compression = io.read_u32le()?;
                let _image_size = io.read_u32le()?;
                let _xppm = io.read_u32le()?;
                let _yppm = io.read_u32le()?;
                let _clr_used = io.read_u32le()?;
                let _clr_important = io.read_u32le()?;

                if compression == TAG_DXSB || compression == TAG_DXSA {
                    params.media_type = MediaType::Subtitle;
                    params.codec_tag = compression;
                    params.codec_id = CodecId::SubXsub;
                    self.streams_state[stream_index].codec_type = MediaType::Subtitle;
                    return Ok(());
                }

                params.width = width.unsigned_abs();
                params.height = height.unsigned_abs();
                params.bits_per_coded_sample = u32::from(bits);
                params.codec_tag = compression;
                params.codec_id = bmp_tag_to_codec_id(compression);
                if params.codec_id == CodecId::None {
                    // If codec is not found yet, try with the mov tags.
                    params.codec_id = match &compression.to_le_bytes() {
                        b"avc1" => CodecId::H264,
                        b"hvc1" => CodecId::Hevc,
                        b"vp09" => CodecId::Vp9,
                        _ => CodecId::None,
                    };
                }

                // This is needed to get the pict type which is necessary for
                // generating correct pts.
                if compression == 0 && params.height > 0 {
                    params.extradata.extend_from_slice(b"BottomUp");
                }

                let mut consumed = 40u32;
                if size > 40 && u64::from(size) < self.fsize {
                    let mut extradata_size = size - 40;
                    if esize == size - 1 && esize & 1 != 0 {
                        extradata_size = esize - 40;
                    }
                    let data = get_extradata(io, extradata_size as usize)?;
                    consumed += data.len() as u32;
                    params.extradata = data;
                }
                if params.extradata.len() & 1 != 0 {
                    io.skip(1)?;
                    consumed += 1;
                }
                if size > consumed {
                    skip_chunk(io, size - consumed)?;
                }
            }
            MediaType::Audio => {
                let params = &mut stream.codec_params;
                if size < 14 {
                    return Err(Error::InvalidData);
                }
                let id = io.read_u16le()?;
                let channels = io.read_u16le()?;
                let sample_rate = io.read_u32le()?;
                let byte_rate = io.read_u32le()?;
                let block_align = io.read_u16le()?;
                let bits = if size == 14 { 8 } else { io.read_u16le()? };

                params.media_type = MediaType::Audio;
                params.codec_tag = u32::from(id);
                params.codec_id = wav_tag_to_codec_id(id, bits);
                params.sample_rate = sample_rate;
                params.channel_layout = ChannelLayout::unspec(i32::from(channels));
                params.bits_per_coded_sample = u32::from(bits);
                params.block_align = u32::from(block_align);
                params.bit_rate = i64::from(byte_rate) * 8;

                let ast = &mut self.streams_state[stream_index];
                ast.dshow_block_align = u32::from(block_align);
                if ast.sample_size != 0
                    && block_align != 0
                    && ast.sample_size != i32::from(block_align)
                {
                    ast.sample_size = i32::from(block_align);
                }

                let mut consumed = if size == 14 { 14 } else { 16 };
                if size >= 18 {
                    let cb_size = io.read_u16le()?;
                    let cb = u32::from(cb_size).min(size.saturating_sub(18));
                    if cb > 0 {
                        params.extradata = io.read_bytes(cb as usize)?;
                    }
                    consumed = 18 + params.extradata.len() as u32;
                }
                if size & 1 != 0 {
                    io.skip(1)?;
                    consumed += 1;
                }
                if size > consumed {
                    skip_chunk(io, size - consumed)?;
                }
            }
            _ => {
                stream.codec_params.media_type = MediaType::Data;
                stream.codec_params.codec_id = CodecId::None;
                stream.codec_params.codec_tag = 0;
                skip_chunk(io, size)?;
            }
        }
        Ok(())
    }

    /// `avi_read_idx1()` from avidec.c:1625-1709 (simplified: no data_offset
    /// correction, which is zero for well-formed AVI files).
    fn load_idx1(&mut self, io: &mut BufferedIo, size: u32) -> Result<()> {
        let nb_entries = (size / 16) as usize;
        if nb_entries == 0 {
            return Err(Error::InvalidData);
        }
        let base = self.movi_list.wrapping_add(4);
        let mut any_key = false;
        for _ in 0..nb_entries {
            let tag = io.read_u32le()?;
            let flags = io.read_u32le()?;
            let pos = io.read_u32le()?;
            let len = io.read_u32le()?;

            let index = (tag & 0xff).wrapping_sub(u32::from(b'0')) * 10
                + (tag >> 8 & 0xff).wrapping_sub(u32::from(b'0'));
            if index as usize >= self.streams_state.len() {
                continue;
            }
            // Skip 'xxpc' palette change entries in the index.
            if tag >> 16 & 0xff == u32::from(b'p') && tag >> 24 & 0xff == u32::from(b'c') {
                continue;
            }
            let ast = &mut self.streams_state[index as usize];
            let entry = IndexEntry {
                pos: base.wrapping_add(u64::from(pos)),
                timestamp: ast.cum_len,
                size: len,
                flags,
            };
            if len != 0 && ast.index.last().is_none_or(|e| e.pos != entry.pos) {
                trace!(pos = entry.pos, size = entry.size, "AVI idx1 entry");
                ast.index.push(entry);
            }
            ast.cum_len += get_duration(ast, len as i32);
            any_key |= flags & AVIIF_INDEX != 0;
        }
        if !any_key {
            for ast in &mut self.streams_state {
                if let Some(first) = ast.index.first_mut() {
                    first.flags |= AVIIF_INDEX;
                }
            }
        }
        Ok(())
    }

    /// `avi_load_index()` from avidec.c:1814-1859.
    fn load_index(&mut self, io: &mut BufferedIo) -> Result<()> {
        let pos = io.tell()?;
        if self.movi_end == 0 || self.movi_end > self.fsize.saturating_add(8) {
            return Ok(());
        }
        if io.seek(self.movi_end).is_err() {
            let _ = io.seek(pos);
            return Ok(());
        }
        while let Ok(tag) = io.read_u32le() {
            let size = match io.read_u32le() {
                Ok(v) => v,
                Err(_) => break,
            };
            let next = io
                .tell()?
                .wrapping_add(u64::from(size))
                .wrapping_add(u64::from(size & 1));
            if tag == TAG_IDX1 {
                if self.load_idx1(io, size).is_ok() {
                    break;
                }
            } else if tag == TAG_LIST && size >= 4 {
                let _list_type = io.read_u32le()?;
            }
            if next > self.fsize.saturating_add(8) || io.seek(next).is_err() {
                break;
            }
        }
        let _ = io.seek(pos);
        Ok(())
    }

    /// `avi_sync()` from avidec.c:1252-1404. Scans for the next `##dc` /
    /// `##wb` chunk in the `movi` list and records its payload size.
    fn avi_sync(&mut self, io: &mut BufferedIo, exit_early: bool) -> Result<()> {
        let mut d = [0xffu32; 8];
        let mut i = io.tell()?;
        let sync = i;
        loop {
            for j in 0..7 {
                d[j] = d[j + 1];
            }
            d[7] = match io.read_u8() {
                Ok(v) => u32::from(v),
                Err(Error::Eof) => return Err(Error::Eof),
                Err(e) => return Err(e),
            };
            let size = d[4] | (d[5] << 8) | (d[6] << 16) | (d[7] << 24);
            let tag = u32::from_le_bytes([d[0] as u8, d[1] as u8, d[2] as u8, d[3] as u8]);

            if i.wrapping_add(u64::from(size)) > self.fsize || d[0] > 127 {
                i = i.wrapping_add(1);
                continue;
            }

            // parse ix##, JUNK, idx1 and ndx chunks
            if (d[0] == u32::from(b'i') && d[1] == u32::from(b'x'))
                || tag == TAG_JUNK
                || tag == TAG_IDX1
                || tag == TAG_NDX
            {
                io.skip(u64::from(size))?;
                i = io.tell()?;
                continue;
            }
            // parse stray LIST
            if tag == TAG_LIST {
                io.skip(4)?;
                i = io.tell()?;
                continue;
            }

            // A one-byte alignment heuristic from FFmpeg.
            if (i.wrapping_sub(self.last_pkt_pos) & 1) == 0
                && get_stream_idx(&[d[1] as u8, d[2] as u8]) < self.streams_state.len()
            {
                i = i.wrapping_add(1);
                continue;
            }

            let n = get_stream_idx(&[d[0] as u8, d[1] as u8]);
            // detect ##ix chunk and skip
            if d[2] == u32::from(b'i') && d[3] == u32::from(b'x') && n < self.streams_state.len() {
                io.skip(u64::from(size))?;
                i = io.tell()?;
                continue;
            }
            // ##wc chunks carry a chunk counter and are skipped by FFmpeg.
            if d[2] == u32::from(b'w') && d[3] == u32::from(b'c') && n < self.streams_state.len() {
                io.skip(16 * 3 + 8)?;
                i = io.tell()?;
                continue;
            }

            if n < self.streams_state.len() {
                let ast = &mut self.streams_state[n];
                if d[2] == u32::from(b'p') && d[3] == u32::from(b'c') && size <= 4 * 256 + 4 {
                    // Palette change chunk.
                    let first = io.read_u8()?;
                    let last = first.wrapping_add(io.read_u8()?).wrapping_sub(1);
                    let _flags = io.read_u16le()?;
                    let mut k = first;
                    loop {
                        let _bgr = io.read_u32be()?;
                        if k == last {
                            break;
                        }
                        k = k.wrapping_add(1);
                    }
                    i = io.tell()?;
                    continue;
                }
                let matches_prefix = d[2] * 256 + d[3] == ast.prefix;
                if ((ast.prefix_count < 5 || sync + 9 > i) && d[2] < 128 && d[3] < 128)
                    || matches_prefix
                {
                    if exit_early {
                        return Ok(());
                    }
                    if matches_prefix {
                        ast.prefix_count += 1;
                    } else {
                        ast.prefix = d[2] * 256 + d[3];
                        ast.prefix_count = 0;
                    }
                    // avi_sync(): discard zero-sized chunks but still advance
                    // the frame counter (FFmpeg's AVDISCARD_DEFAULT path).
                    if size == 0 {
                        ast.frame_offset = ast.frame_offset.wrapping_add(get_duration(ast, 0));
                        i = io.tell()?;
                        continue;
                    }
                    self.stream_index = n as i32;
                    ast.packet_size = size.wrapping_add(8);
                    ast.remaining = size;
                    return Ok(());
                }
            }

            i = i.wrapping_add(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Demuxer trait implementation
// ---------------------------------------------------------------------------

impl Demuxer for AviDemuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        self.stream_index = -1;

        // get_riff(): check the RIFF header.
        let riff_tag = io.read_u32le()?;
        let riff_size = io.read_u32le()?;
        let mut header = [0u8; 4];
        io.read_exact(&mut header)?;
        if (riff_tag != TAG_RIFF && riff_tag != TAG_ON2) || !AVI_HEADERS.contains(&header) {
            return Err(Error::InvalidData);
        }
        // header[7] == 0x19 means "generated by a totally broken muxer".
        self.riff_end = 8u64.wrapping_add(u64::from(riff_size));
        self.fsize = io.size().unwrap_or(self.riff_end);
        if self.fsize == 0 || self.fsize < self.riff_end {
            self.fsize = self.riff_end;
        }

        let mut streams: Vec<Stream> = Vec::new();
        let mut metadata = Metadata::new();
        let mut codec_type = MediaType::Data;
        let mut stream_index: i32 = -1;
        let mut avih_width = 0u32;
        let mut avih_height = 0u32;
        let mut amv_file_format = false;

        loop {
            let (tag, size) = read_chunk_header(io)?;
            trace!(tag = ?tag_bytes(tag), size, "AVI header tag");

            match tag {
                TAG_LIST => {
                    let tag1 = io.read_u32le()?;
                    if tag1 == TAG_MOVI {
                        self.movi_list = io.tell()?.saturating_sub(4);
                        self.movi_end = if size != 0 {
                            self.movi_list
                                .wrapping_add(u64::from(size))
                                .wrapping_add(u64::from(size & 1))
                        } else {
                            self.fsize
                        };
                        debug!(
                            movi_list = self.movi_list,
                            movi_end = self.movi_end,
                            "AVI movi list"
                        );
                        break;
                    } else if tag1 == TAG_INFO {
                        if size < 4 {
                            return Err(Error::InvalidData);
                        }
                        read_riff_info(io, u64::from(size - 4), &mut metadata)?;
                    }
                    // Otherwise the list contents are parsed as top-level
                    // chunks (FFmpeg does not skip unknown lists here).
                }
                TAG_AVIH | TAG_AMVH => {
                    amv_file_format |= tag == TAG_AMVH;
                    self.parse_avih(io, size, &mut avih_width, &mut avih_height)?;
                }
                TAG_STRH => {
                    if let Some((state, stream)) = self.parse_strh(
                        io,
                        size,
                        &mut stream_index,
                        &mut codec_type,
                        amv_file_format,
                    )? {
                        self.streams_state.push(state);
                        streams.push(stream);
                    }
                }
                TAG_STRF => {
                    if size == 0
                        && (codec_type == MediaType::Audio || codec_type == MediaType::Video)
                    {
                        break;
                    }
                    let idx = stream_index;
                    if idx < 0 || idx as usize >= self.streams_state.len() {
                        skip_chunk(io, size)?;
                    } else {
                        self.parse_strf(
                            io,
                            size,
                            idx as usize,
                            amv_file_format,
                            avih_width,
                            avih_height,
                            &mut streams[idx as usize],
                        )?;
                    }
                }
                TAG_STRD => {
                    let idx = stream_index;
                    if idx < 0 || idx as usize >= self.streams_state.len() {
                        skip_chunk(io, size)?;
                    } else {
                        let st = &mut streams[idx as usize];
                        if !st.codec_params.extradata.is_empty()
                            || st.codec_params.codec_tag == u32::from_le_bytes(*b"H264")
                        {
                            skip_chunk(io, size)?;
                        } else if size < 1 << 30 {
                            st.codec_params.extradata = get_extradata(io, size as usize)?;
                            if size & 1 != 0 {
                                io.skip(1)?;
                            }
                        } else {
                            skip_chunk(io, size)?;
                        }
                    }
                }
                TAG_IDX1 | TAG_NDX | TAG_JUNK => {
                    skip_chunk(io, size)?;
                }
                TAG_IDIT => {
                    // IDIT holds an AVI creation-time string (e.g.
                    // "Mon Mar 10 15:04:43 2003"). Keep it verbatim.
                    let date_size = size.min(63);
                    let mut date = io.read_bytes(date_size.wrapping_add(date_size & 1) as usize)?;
                    if let Some(end) = date.iter().position(|b| *b == 0) {
                        date.truncate(end);
                    }
                    let value = String::from_utf8_lossy(&date).into_owned();
                    if let Some(last) = streams.last_mut() {
                        last.metadata.set("creation_time", value);
                    } else {
                        metadata.set("creation_time", value);
                    }
                    if size > date_size {
                        skip_chunk(io, size - date_size)?;
                    }
                }
                tag if METADATA_TAGS.contains(&tag) => {
                    if let Some(last) = streams.last_mut() {
                        read_avi_tag(io, tag, size, Some(&mut last.metadata))?;
                    } else {
                        read_avi_tag(io, tag, size, Some(&mut metadata))?;
                    }
                }
                _ if size > 1_000_000 => {
                    // "Something went wrong during header parsing, I will
                    // ignore it and try to continue anyway."
                    debug!(tag = ?tag_bytes(tag), size, "oversized AVI header tag, stopping");
                    self.movi_list = io.tell()?.saturating_sub(4);
                    self.movi_end = self.fsize;
                    break;
                }
                _ => {
                    skip_chunk(io, size)?;
                }
            }
        }

        if stream_index != streams.len() as i32 - 1 || streams.is_empty() {
            return Err(Error::InvalidData);
        }

        // avi_load_index(): the idx1 index sits at the end of the movi list.
        self.load_index(io)?;
        self.index_loaded = true;

        for (i, st) in streams.iter_mut().enumerate() {
            st.nb_frames = self.streams_state[i].nb_frames;
        }

        self.header_read = true;
        Ok(DemuxerHeader {
            streams,
            metadata,
            duration: 0,
            start_time: 0,
        })
    }

    fn read_packet(&mut self, io: &mut BufferedIo) -> Result<Packet> {
        if !self.header_read {
            return Err(Error::InvalidData);
        }
        if self.stream_index < 0 {
            self.avi_sync(io, false)?;
        }

        let n = self.stream_index as usize;
        if n >= self.streams_state.len() {
            return Err(Error::InvalidData);
        }

        // avi_read_packet(): size selection for audio vs video streams.
        let mut size = {
            let ast = &self.streams_state[n];
            if ast.sample_size <= 1 {
                i32::MAX
            } else if ast.sample_size < 32 {
                1024 * ast.sample_size
            } else {
                ast.sample_size
            }
        };
        if size < 0 || (size as u64) > u64::from(self.streams_state[n].remaining) {
            size = self.streams_state[n].remaining as i32;
        }

        let pos = io.tell()?;
        self.last_pkt_pos = pos;
        let data = if size == 0 {
            Vec::new()
        } else {
            io.read_up_to(size as usize)?
        };
        if data.is_empty() && size > 0 {
            return Err(Error::Eof);
        }

        let ast = &mut self.streams_state[n];
        let mut dts = ast.frame_offset;
        if ast.sample_size != 0 {
            dts /= i64::from(ast.sample_size);
        }

        // Keyframe detection: video streams with an index use idx1 flags;
        // AVI has no keyframe flag for all other packets.
        let mut is_key = true;
        if ast.codec_type == MediaType::Video
            && let Some(e) = ast.index.iter().find(|e| e.timestamp == ast.frame_offset)
        {
            is_key = e.flags & AVIIF_INDEX != 0;
        }

        let duration = get_duration(ast, data.len() as i32);
        let mut pkt = Packet::new();
        pkt.data = Buffer::from_slice(&data);
        pkt.pts = dts;
        pkt.dts = dts;
        pkt.duration = duration;
        pkt.stream_index = n;
        pkt.pos = pos as i64;
        if is_key {
            pkt.flags |= PacketFlags::KEY;
        }

        ast.frame_offset = ast.frame_offset.wrapping_add(duration);
        ast.remaining = ast.remaining.wrapping_sub(data.len() as u32);
        if ast.remaining == 0 {
            self.stream_index = -1;
            ast.packet_size = 0;
        }

        trace!(
            stream = n,
            dts,
            size = data.len(),
            remaining = ast.remaining,
            "AVI packet"
        );
        Ok(pkt)
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
// DemuxerFactory + registration
// ---------------------------------------------------------------------------

struct AviDemuxerFactory;

impl DemuxerFactory for AviDemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "avi",
            long_name: "AVI (Audio Video Interleaved)",
            extensions: "avi",
            mime_types: "video/x-msvideo",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        let buf = data.buf;
        if buf.len() < 12 {
            return 0;
        }
        let first = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if first != TAG_RIFF && first != TAG_ON2 {
            return 0;
        }
        let form = [buf[8], buf[9], buf[10], buf[11]];
        if AVI_HEADERS.contains(&form) {
            PROBE_SCORE_MAX
        } else {
            0
        }
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(AviDemuxer::new()))
    }
}

inventory::submit!(&AviDemuxerFactory as &dyn DemuxerFactory);
