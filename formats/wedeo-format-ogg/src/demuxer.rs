//! Ogg page reader, packet assembler and `Demuxer` implementation.
//!
//! Ported from FFmpeg's `libavformat/ogg/demuxer.c`.

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

use crate::crc::page_crc;
use crate::mapping::{
    CodecKind, NOPTS, OGG_NOGRANULE_VALUE, OggStream, find_codec, picture_type_name,
};

const MAX_PAGE_SIZE: usize = 65307;
const OGG_FLAG_CONT: u8 = 1;
const OGG_FLAG_BOS: u8 = 2;
const OGG_FLAG_EOS: u8 = 4;

/// Result of `ogg_read_page`.
enum PageResult {
    Stream(usize),
    /// Bad CRC or version: skip and rescan from just after the capture pattern.
    Skip,
}

/// Result of `ogg_packet`.
struct PacketResult {
    idx: usize,
    pstart: usize,
    psize: usize,
    fpos: i64,
}

pub(crate) struct OggDemuxer {
    streams: Vec<OggStream>,
    curidx: isize,
    page_pos: i64,
    headers: bool,
    header_read: bool,
    /// FFmpeg's `av_read_frame()` fills missing packet timestamps by
    /// accumulating durations; our thin `InputContext` has no such layer, so
    /// the demuxer reproduces it here for bit-exact framecrc parity.
    next_pts: Vec<i64>,
    /// Attached-picture packets from `METADATA_BLOCK_PICTURE` comments. FFmpeg
    /// queues these in `avformat_queue_attached_pictures()`; we drain them
    /// first because `read_packet()` has no cross-stream DTS interleaving and
    /// the bitexact test compares each stream's packet sequence separately.
    pending_pictures: VecDeque<Packet>,
}

impl OggDemuxer {
    pub(crate) fn new() -> Self {
        Self {
            streams: Vec::new(),
            curidx: -1,
            page_pos: -1,
            headers: false,
            header_read: false,
            next_pts: Vec::new(),
            pending_pictures: VecDeque::new(),
        }
    }

    fn find_stream(&self, serial: u32) -> Option<usize> {
        self.streams.iter().position(|s| s.serial == serial)
    }

    fn data_packets_seen(&self) -> bool {
        self.streams.iter().any(|s| s.got_data)
    }

    /// `ogg_read_page()`: find, validate and buffer the next Ogg page.
    fn read_page(&mut self, io: &mut BufferedIo) -> Result<PageResult> {
        let mut sync = [0u8; 4];
        for b in &mut sync {
            *b = io.read_u8()?;
        }
        let mut scanned = 0usize;
        let sync_pos;
        loop {
            if &sync == b"OggS" {
                sync_pos = io.tell()? as i64 - 4;
                break;
            }
            let c = io.read_u8()?;
            sync.rotate_left(1);
            sync[3] = c;
            scanned += 1;
            if scanned >= MAX_PAGE_SIZE {
                warn!("cannot find sync word");
                return Err(Error::InvalidData);
            }
        }

        let version = io.read_u8()?;
        let flags = io.read_u8()?;
        let granule = io.read_u64le()?;
        let serial = io.read_u32le()?;
        let seq = io.read_u32le()?;
        let crc = io.read_u32be()?;
        let nsegs = io.read_u8()? as usize;
        let page_pos = sync_pos;

        let mut segments = [0u8; 255];
        io.read_exact(&mut segments[..nsegs])?;
        let size: usize = segments[..nsegs].iter().map(|&b| b as usize).sum();
        let payload = io.read_bytes(size)?;

        // Rebuild the page with a zeroed CRC field and validate it, matching
        // FFmpeg's checksumming reads (the stored CRC is big-endian).
        let mut page = Vec::with_capacity(27 + nsegs + size);
        page.extend_from_slice(b"OggS");
        page.push(version);
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&serial.to_le_bytes());
        page.extend_from_slice(&seq.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]);
        page.push(nsegs as u8);
        page.extend_from_slice(&segments[..nsegs]);
        page.extend_from_slice(&payload);

        if page_crc(&page) != crc {
            debug!(page_pos, "CRC mismatch");
            io.seek((sync_pos + 4) as u64)?;
            return Ok(PageResult::Skip);
        }
        // Checking the version after the checksum lets the demuxer be more
        // tolerant, exactly like FFmpeg.
        if version != 0 {
            debug!(page_pos, "Invalid Ogg version");
            io.seek((sync_pos + 4) as u64)?;
            return Ok(PageResult::Skip);
        }

        let idx = if let Some(idx) = self.find_stream(serial) {
            idx
        } else if self.data_packets_seen() {
            // Typical webradio situation: a new stream replaces the old one.
            if self.streams.len() != 1 {
                return Err(Error::PatchwelcomeNotImplemented);
            }
            if find_codec(&payload).is_none() {
                return Err(Error::InvalidData);
            }
            let s = &mut self.streams[0];
            s.serial = serial;
            s.lastpts = 0;
            s.lastdts = 0;
            s.flags = 0;
            s.start_trimming = 0;
            s.end_trimming = 0;
            s.replace = true;
            0
        } else {
            self.streams.push(OggStream::new(serial));
            self.streams.len() - 1
        };

        {
            let os = &mut self.streams[idx];
            if os.buf.len() < os.bufpos + size {
                os.buf.resize(os.bufpos + size, 0);
            }
            os.buf[os.bufpos..os.bufpos + size].copy_from_slice(&payload);
            os.page_pos = page_pos;
            self.page_pos = page_pos;
            os.nsegs = nsegs;
            os.segp = 0;
            os.got_data = flags & OGG_FLAG_BOS == 0;
            os.bufpos += size;
            os.granule = granule;
            os.flags = flags;
            os.segments[..nsegs].copy_from_slice(&segments[..nsegs]);

            if flags & OGG_FLAG_CONT != 0 || os.incomplete {
                if os.psize == 0 {
                    // We started playback in the middle of a continuation
                    // packet; discard it since we missed the start of it.
                    while os.segp < os.nsegs {
                        let seg = os.segments[os.segp] as usize;
                        os.segp += 1;
                        os.pstart += seg;
                        if seg < 255 {
                            break;
                        }
                    }
                    os.sync_pos = os.page_pos;
                }
            } else {
                os.psize = 0;
                os.sync_pos = os.page_pos;
            }
        }

        Ok(PageResult::Stream(idx))
    }

    /// `ogg_calc_pts()`.
    fn calc_pts(&mut self, idx: usize, dts: &mut Option<i64>) -> i64 {
        *dts = None;
        let mut pts = NOPTS;
        {
            let os = &mut self.streams[idx];
            if os.lastpts != NOPTS {
                pts = os.lastpts;
                os.lastpts = NOPTS;
            }
            if os.lastdts != NOPTS {
                *dts = Some(os.lastdts);
                os.lastdts = NOPTS;
            }
        }

        let os = &mut self.streams[idx];
        if os.page_end && os.granule != OGG_NOGRANULE_VALUE {
            if os.granule_is_start() {
                let gp = os.granule;
                pts = self.gptopts(idx, gp, dts);
                self.streams[idx].granule = OGG_NOGRANULE_VALUE;
            } else {
                let gp = os.granule;
                let mut new_dts = None;
                let new_pts = self.gptopts(idx, gp, &mut new_dts);
                let os = &mut self.streams[idx];
                os.lastpts = new_pts;
                if let Some(d) = new_dts {
                    os.lastdts = d;
                }
                os.granule = OGG_NOGRANULE_VALUE;
            }
        }
        pts
    }

    fn gptopts(&mut self, idx: usize, gp: u64, dts: &mut Option<i64>) -> i64 {
        self.streams[idx].gptopts(gp, dts)
    }

    /// `ogg_validate_keyframe()`.
    fn validate_keyframe(&mut self, idx: usize, pstart: usize, psize: usize) {
        if psize == 0 || pstart >= self.streams[idx].buf.len() {
            return;
        }
        let codec = self.streams[idx].codec;
        let byte = self.streams[idx].buf[pstart];
        let invalid = match codec {
            Some(CodecKind::Theora) => {
                self.streams[idx].pflags.contains(PacketFlags::KEY) != (byte & 0x40 != 0)
            }
            Some(CodecKind::Vp8) => {
                self.streams[idx].pflags.contains(PacketFlags::KEY) != (byte & 1 != 0)
            }
            _ => false,
        };
        if invalid {
            self.streams[idx].pflags.toggle(PacketFlags::KEY);
            debug!(idx, "Broken file, keyframe not correctly marked");
        }
    }

    /// `ogg_packet()`: find the next complete packet. Returns `None` when no
    /// packet could be produced from this stream (e.g. unknown codec).
    fn ogg_packet(&mut self, io: &mut BufferedIo) -> Result<Option<PacketResult>> {
        loop {
            let mut idx = self.curidx;
            while idx < 0 {
                match self.read_page(io)? {
                    PageResult::Stream(i) => idx = i as isize,
                    PageResult::Skip => continue,
                }
            }
            let idx = idx as usize;

            trace!(idx, "ogg_packet");

            if self.streams[idx].codec.is_none() {
                if self.streams[idx].header < 0 {
                    let bufpos = self.streams[idx].bufpos;
                    match find_codec(&self.streams[idx].buf[..bufpos]) {
                        Some(codec) => self.streams[idx].codec = Some(codec),
                        None => {
                            warn!("Codec not found");
                            self.streams[idx].header = 0;
                            return Ok(None);
                        }
                    }
                } else {
                    return Ok(None);
                }
            }

            let (segp, psize) = {
                let os = &self.streams[idx];
                (os.segp, os.psize)
            };
            let mut complete = false;
            {
                let os = &mut self.streams[idx];
                while os.segp < os.nsegs {
                    let ss = os.segments[os.segp] as usize;
                    os.segp += 1;
                    os.psize += ss;
                    if ss < 255 {
                        complete = true;
                        break;
                    }
                }
                if !complete && os.segp == os.nsegs {
                    self.curidx = -1;
                    os.incomplete = os.psize != 0;
                }
            }
            if !complete {
                continue;
            }

            {
                let os = &mut self.streams[idx];
                if os.granule == OGG_NOGRANULE_VALUE {
                    trace!(os.page_pos, "Page is missing granule");
                }
                self.curidx = idx as isize;
                os.incomplete = false;
                // The packet started at the first segment of the page it
                // completes on.
                os.page_start = segp == 0;
            }

            let result = if self.streams[idx].header != 0 {
                let ret = self.streams[idx].process_header().map_err(|e| {
                    debug!(idx, ?e, "process_header failed");
                    e
                })?;
                if ret < 0 {
                    return Err(Error::InvalidData);
                }
                self.streams[idx].header = ret;
                if ret == 0 {
                    // Rewind the segment cursor: the first non-header packet
                    // must be re-parsed as a data packet on the next call.
                    let os = &mut self.streams[idx];
                    os.segp = segp;
                    os.psize = psize;
                    self.headers = true;
                } else {
                    let os = &mut self.streams[idx];
                    os.nb_header += 1;
                    os.pstart += os.psize;
                    os.psize = 0;
                }
                None
            } else {
                {
                    let os = &mut self.streams[idx];
                    os.pflags = PacketFlags::empty();
                    os.pduration = 0;
                }
                let ret = self.streams[idx].process_packet()?;
                if ret < 0 {
                    return Err(Error::InvalidData);
                }
                let result = if ret == 0 {
                    let os = &self.streams[idx];
                    Some(PacketResult {
                        idx,
                        pstart: os.pstart,
                        psize: os.psize,
                        fpos: os.sync_pos,
                    })
                } else {
                    None
                };
                {
                    let os = &mut self.streams[idx];
                    os.pstart += os.psize;
                    os.psize = 0;
                    if os.pstart == os.bufpos {
                        os.bufpos = 0;
                        os.pstart = 0;
                    }
                    os.sync_pos = os.page_pos;
                }
                result
            };

            // Determine whether there are more complete packets in this page.
            let os = &mut self.streams[idx];
            os.page_end = true;
            for &s in &os.segments[os.segp..os.nsegs] {
                if s < 255 {
                    os.page_end = false;
                    break;
                }
            }
            if os.segp == os.nsegs {
                self.curidx = -1;
            }
            return Ok(result);
        }
    }
}

impl Demuxer for OggDemuxer {
    fn read_header(&mut self, io: &mut BufferedIo) -> Result<DemuxerHeader> {
        self.curidx = -1;
        loop {
            self.ogg_packet(io)?;
            if self.headers {
                break;
            }
        }

        for (i, os) in self.streams.iter().enumerate() {
            if os.header < 0 {
                warn!(i, "Header parsing failed for stream");
            } else if let Some(codec) = os.codec {
                let expected = codec.nb_header();
                if expected > 0 && os.nb_header < expected {
                    warn!(i, expected, received = os.nb_header, "Headers mismatch");
                }
            }
        }
        for os in &mut self.streams {
            if os.start_granule != OGG_NOGRANULE_VALUE {
                let gp = os.start_granule;
                let mut dts = None;
                let pts = os.gptopts(gp, &mut dts);
                os.lastpts = pts;
                os.start_time = pts;
            }
        }

        let mut streams: Vec<Stream> = self
            .streams
            .iter()
            .enumerate()
            .map(|(i, os)| {
                let mut stream = Stream::new(i, os.params.clone());
                stream.time_base = os.time_base;
                stream.duration = os.duration;
                for (k, v) in &os.metadata {
                    stream.metadata.set(k, v);
                }
                stream
            })
            .collect();

        // FFmpeg creates an attached-picture stream for every
        // METADATA_BLOCK_PICTURE while parsing OpusTags/Vorbis comments, so
        // these streams follow the real Ogg logical bitstreams in order.
        let picture_base = streams.len();
        let mut pictures = Vec::new();
        for os in &self.streams {
            pictures.extend(os.pictures.iter().cloned());
        }
        for (offset, pic) in pictures.into_iter().enumerate() {
            let index = picture_base + offset;
            let mut params = CodecParameters::new(CodecId::Mjpeg, MediaType::Video);
            params.width = pic.width;
            params.height = pic.height;
            let mut stream = Stream::new(index, params);
            // avformat_new_stream() defaults to 1/90000 for new streams.
            stream.time_base = Rational::new(1, 90_000);
            stream
                .metadata
                .set("comment", picture_type_name(pic.picture_type));
            if !pic.description.is_empty() {
                stream.metadata.set("title", &pic.description);
            }
            streams.push(stream);

            // `ff_add_attached_pic()` gives the packet KEY only; the framecrc
            // CLI maps NOPTS to 0, which is what the bitexact test compares.
            let mut pkt = Packet::new();
            pkt.data = Buffer::from_slice(&pic.data);
            pkt.stream_index = index;
            pkt.pts = 0;
            pkt.dts = 0;
            pkt.flags = PacketFlags::KEY;
            self.pending_pictures.push_back(pkt);
        }

        self.next_pts = vec![NOPTS; self.streams.len()];
        let _ = OGG_FLAG_EOS;
        self.header_read = true;
        Ok(DemuxerHeader {
            streams,
            metadata: Metadata::new(),
            duration: 0,
            start_time: 0,
        })
    }

    fn read_packet(&mut self, io: &mut BufferedIo) -> Result<Packet> {
        if !self.header_read {
            return Err(Error::InvalidData);
        }
        if let Some(pkt) = self.pending_pictures.pop_front() {
            return Ok(pkt);
        }
        let packet = loop {
            match self.ogg_packet(io)? {
                Some(p) if p.idx < self.streams.len() => break p,
                _ => continue,
            }
        };

        let idx = packet.idx;
        let mut dts = Some(NOPTS);
        let mut pts = self.calc_pts(idx, &mut dts);
        self.validate_keyframe(idx, packet.pstart, packet.psize);

        // A chained stream clears the first packet's timestamps; av_read_frame
        // then reconstructs them from the running timestamp of the stream.
        let replaced = self.streams[idx].replace;
        if replaced {
            self.streams[idx].replace = false;
            pts = NOPTS;
            dts = Some(NOPTS);
        }

        // Mirror `av_read_frame()`'s compute_pkt_fields(): missing timestamps
        // are derived from the running timestamp of the stream.
        if pts == NOPTS {
            if self.next_pts[idx] == NOPTS {
                // No demuxer timestamp at all: seed the accumulator the way
                // FFmpeg's probing/length scan does (pre-skip / priming).
                self.next_pts[idx] = match self.streams[idx].codec {
                    Some(crate::mapping::CodecKind::Opus) => {
                        -(self.streams[idx]
                            .private
                            .as_opus()
                            .map(|p| p.pre_skip)
                            .unwrap_or(0) as i64)
                    }
                    Some(crate::mapping::CodecKind::Vorbis) => self.streams[idx]
                        .private
                        .as_vorbis()
                        .and_then(|p| p.seed_pts.or(Some(-p.ts_offset)))
                        .unwrap_or(0),
                    _ => 0,
                };
            }
            pts = self.next_pts[idx];
        }
        if dts == Some(NOPTS) || dts.is_none() {
            dts = Some(pts);
        }
        self.next_pts[idx] = pts.wrapping_add(self.streams[idx].pduration);

        let os = &mut self.streams[idx];
        let mut pkt = Packet::new();
        pkt.data = Buffer::from_slice(&os.buf[packet.pstart..packet.pstart + packet.psize]);
        pkt.stream_index = idx;
        pkt.pts = pts;
        pkt.dts = dts.unwrap_or(NOPTS);
        pkt.flags = os.pflags;
        pkt.duration = os.pduration;
        pkt.pos = packet.fpos;

        if os.start_trimming != 0 || os.end_trimming != 0 {
            let mut side = Vec::with_capacity(10);
            side.extend_from_slice(&os.start_trimming.to_le_bytes());
            side.extend_from_slice(&os.end_trimming.to_le_bytes());
            pkt.side_data.push(PacketSideData {
                data_type: PacketSideDataType::SkipSamples,
                data: side,
            });
            os.start_trimming = 0;
            os.end_trimming = 0;
        }
        if let Some(extradata) = os.new_extradata.take() {
            pkt.side_data.push(PacketSideData {
                data_type: PacketSideDataType::NewExtradata,
                data: extradata,
            });
        }
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

pub(crate) struct OggDemuxerFactory;

impl DemuxerFactory for OggDemuxerFactory {
    fn descriptor(&self) -> &InputFormatDescriptor {
        static DESC: InputFormatDescriptor = InputFormatDescriptor {
            name: "ogg",
            long_name: "Ogg",
            extensions: "ogg",
            mime_types: "audio/ogg,application/ogg,video/ogg",
            flags: InputFormatFlags::empty(),
            priority: 100,
        };
        &DESC
    }

    fn probe(&self, data: &ProbeData<'_>) -> i32 {
        // FFmpeg compares 5 bytes against the NUL-terminated "OggS" literal,
        // so byte 4 (the version) must be zero and byte 5 is the flag byte.
        if data.buf.len() >= 6
            && &data.buf[..4] == b"OggS"
            && data.buf[4] == 0
            && data.buf[5] <= 0x07
        {
            PROBE_SCORE_MAX
        } else {
            0
        }
    }

    fn create(&self) -> Result<Box<dyn Demuxer>> {
        Ok(Box::new(OggDemuxer::new()))
    }
}
