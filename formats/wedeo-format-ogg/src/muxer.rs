//! Ogg muxer for Vorbis and Opus audio.
//!
//! Mirrors FFmpeg's `libavformat/oggenc.c`: each logical bitstream gets its
//! own serial number and page sequence, headers are written first (the first
//! header page carries BOS), and the last page of each bitstream carries EOS
//! and the final granule position.
//!
//! The Wedeo `Muxer` trait passes encoded packets with a stream time base but
//! no granule field, so the granule position is derived the same way FFmpeg's
//! `ogg_write_packet_internal()` does it: `granule = pts + duration` rescaled
//! into granule units (the Vorbis stream rate, or 48 kHz for Opus), plus the
//! codec initial padding (OpusHead pre-skip) for Opus. Packets without a PTS
//! fall back to accumulating decoded durations (`VorbisParser` block sizes /
//! the Opus TOC byte) and then PTS deltas.
//!
//! Pages flush when full, when the remaining space is smaller than the next
//! packet, or once the page spans the default `page_duration` of 1 second.
//!
//! The muxer deliberately does not write Theora or FLAC streams: Wedeo has no
//! encoder emitting their Xiph extradata in the packet API yet.

use tracing::{debug, trace, warn};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::packet::Packet;
use wedeo_core::rational::{Rational, rescale_q};
use wedeo_format::demuxer::Stream;
use wedeo_format::io::BufferedIo;
use wedeo_format::muxer::{Muxer, OutputFormatDescriptor, OutputFormatFlags};
use wedeo_format::registry::MuxerFactory;

use crate::mapping::opus_duration;
use crate::page::{MAX_SEGMENTS, PageBuilder, lacing_values};
use crate::vorbis_parser::{VorbisParser, split_xiph_headers};

/// Codecs the Ogg muxer can write header packets for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OggAudioCodec {
    Vorbis,
    Opus,
}

/// Per-stream muxer state. Mirrors FFmpeg's `OGGStreamContext`.
pub(crate) struct StreamState {
    pub(crate) codec: OggAudioCodec,
    pub(crate) time_base: Rational,
    /// Granule position unit (Vorbis stream rate, 48000 for Opus).
    pub(crate) granule_rate: u32,
    pub(crate) serial: u32,
    pub(crate) page: PageBuilder,
    pub(crate) vorbis_parser: Option<VorbisParser>,
    /// Granule position of the current packet, as FFmpeg computes it:
    /// `pts + duration` in granule units, plus `pre_skip` for Opus.
    pub(crate) granule: u64,
    /// Codec initial padding (Opus pre-skip), in granule units.
    pub(crate) pre_skip: i64,
    /// Granule position where the current page started (FFmpeg's
    /// `page.start_granule`), used for the preferred page duration.
    pub(crate) page_start: Option<i64>,
    pub(crate) last_pts: i64,
    pub(crate) have_pts: bool,
}

impl StreamState {
    fn new(
        codec: OggAudioCodec,
        time_base: Rational,
        granule_rate: u32,
        serial: u32,
        vorbis_parser: Option<VorbisParser>,
        pre_skip: i64,
    ) -> Self {
        Self {
            codec,
            time_base,
            granule_rate,
            serial,
            page: PageBuilder::new(serial),
            vorbis_parser,
            granule: 0,
            pre_skip,
            page_start: None,
            last_pts: 0,
            have_pts: false,
        }
    }

    /// Number of samples a packet adds to the granule position.
    fn packet_duration(&mut self, data: &[u8], packet_duration: i64) -> i64 {
        match self.codec {
            OggAudioCodec::Vorbis => {
                if let Some(parser) = &mut self.vorbis_parser {
                    let d = parser.frame(data).unwrap_or(0);
                    if d > 0 {
                        return d;
                    }
                }
            }
            OggAudioCodec::Opus => {
                let d = opus_duration(data).unwrap_or(0);
                if d > 0 {
                    return d;
                }
            }
        }

        // FFmpeg's oggenc derives a missing duration from the time delta
        // between consecutive packets, expressed in granule units.
        if packet_duration > 0 && self.granule_rate > 0 {
            let rate = Rational::new(1, self.granule_rate as i32);
            let d = rescale_q(packet_duration, self.time_base, rate);
            if d > 0 {
                return d;
            }
        }
        0
    }

    /// Update the granule position for a packet.
    ///
    /// FFmpeg's `ogg_write_packet_internal()` uses
    /// `granule = pkt->pts + pkt->duration` (rescaled into granule units),
    /// plus the codec's initial padding for Opus. Only when the packet
    /// carries no PTS does it fall back to accumulating decoded durations.
    fn advance_granule(&mut self, data: &[u8], packet: &Packet) {
        if packet.pts != i64::MIN {
            let rate = Rational::new(1, self.granule_rate as i32);
            let end = packet.pts.wrapping_add(packet.duration.max(0));
            let granule = rescale_q(end, self.time_base, rate).wrapping_add(self.pre_skip);
            if self.page_start.is_none() {
                self.page_start = Some(rescale_q(packet.pts, self.time_base, rate));
            }
            if granule >= 0 {
                self.granule = granule as u64;
                self.last_pts = packet.pts;
                self.have_pts = true;
                return;
            }
        }

        let mut samples = self.packet_duration(data, packet.duration);

        if samples <= 0
            && self.have_pts
            && packet.pts != i64::MIN
            && packet.pts > self.last_pts
            && self.granule_rate > 0
        {
            let rate = Rational::new(1, self.granule_rate as i32);
            let delta = rescale_q(packet.pts - self.last_pts, self.time_base, rate);
            if delta > 0 {
                samples = delta;
            }
        }
        if samples > 0 {
            self.granule = self.granule.wrapping_add(samples as u64);
        }
        if packet.pts != i64::MIN {
            self.last_pts = packet.pts;
            self.have_pts = true;
        }
    }

    /// FFmpeg's default `page_duration` option is 1 second: flush the
    /// current page once its granule span reaches that.
    fn page_span_exceeded(&self) -> bool {
        let Some(start) = self.page_start else {
            return false;
        };
        let span = self.granule as i64 - start;
        let span_us = rescale_q(
            span,
            Rational::new(1, self.granule_rate as i32),
            Rational::new(1, 1_000_000),
        );
        span_us >= 1_000_000
    }
}

/// Ogg muxer instance.
pub(crate) struct OggMuxer {
    streams: Vec<StreamState>,
    /// Deterministic serial override, used by `set_option("serial", ...)`.
    serial_override: Option<u32>,
    header_written: bool,
    finished: bool,
}

impl OggMuxer {
    pub(crate) fn new() -> Self {
        Self {
            streams: Vec::new(),
            serial_override: None,
            header_written: false,
            finished: false,
        }
    }

    /// A deterministic, non-cryptographic serial, as FFmpeg uses a random
    /// 32-bit value from `av_get_random_seed()`.
    fn next_serial() -> u32 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        nanos.rotate_left(13) ^ std::process::id().wrapping_mul(0x9e37_79b9)
    }

    /// Add one packet to a stream's current page, splitting it across pages
    /// when it does not fit. Each piece is one lacing value, exactly as
    /// `ogg_stream_packetin()` writes it.
    fn add_packet(
        io: &mut BufferedIo,
        state: &mut StreamState,
        data: &[u8],
        granule: u64,
    ) -> Result<()> {
        // FFmpeg avoids a continued page when the remaining space is smaller
        // than the whole packet: flush first so the packet starts fresh.
        if !state.page.is_empty() && MAX_SEGMENTS * 255 - state.page.body_len() < data.len() {
            state.page.flush(io, false)?;
            state.page_start = Some(state.granule as i64);
        }
        let lacing = lacing_values(data.len());
        let mut offset = 0usize;
        for (i, &segment) in lacing.iter().enumerate() {
            if state.page.is_full() {
                // When the page ends in the middle of the packet the next
                // page carries CONT. A page boundary exactly between packets
                // is a normal page with no continuation.
                state.page.flush(io, false)?;
                state.page.set_continued(i > 0);
                state.page_start = Some(state.granule as i64);
            }
            let len = usize::from(segment);
            state.page.add(&[segment], &data[offset..offset + len])?;
            offset += len;
        }
        debug_assert_eq!(offset, data.len());
        state.page.set_granule(granule);
        Ok(())
    }

    fn write_header_pages(&mut self, io: &mut BufferedIo, headers: &[Vec<u8>]) -> Result<()> {
        let serial = self.streams.last().map_or(0, |s| s.serial);
        // Index-based to avoid holding a mutable borrow of `self.streams`
        // while calling the static helper.
        let idx = self.streams.len() - 1;
        for (i, packet) in headers.iter().enumerate() {
            Self::add_packet(io, &mut self.streams[idx], packet, 0)?;
            if i == 0 || i + 1 == headers.len() {
                // First header alone on the BOS page, remaining headers on a
                // separate page, matching FFmpeg's oggenc header layout.
                self.streams[idx].page.flush(io, false)?;
            }
        }
        debug!(serial, packets = headers.len(), "Ogg headers written");
        Ok(())
    }
}

impl Muxer for OggMuxer {
    fn write_header(&mut self, io: &mut BufferedIo, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::StreamNotFound);
        }
        self.streams.clear();
        self.finished = false;

        let base_serial = self.serial_override.unwrap_or_else(Self::next_serial);
        let mut prepared: Vec<(StreamState, Vec<Vec<u8>>)> = Vec::with_capacity(streams.len());
        for (i, stream) in streams.iter().enumerate() {
            let params = &stream.codec_params;
            let (codec, headers, granule_rate, parser, pre_skip) = match params.codec_id {
                CodecId::Vorbis => {
                    let headers = vorbis_headers(&params.extradata)?;
                    let rate = if params.sample_rate > 0 {
                        params.sample_rate
                    } else {
                        let id = &headers[0];
                        u32::from_le_bytes([id[12], id[13], id[14], id[15]])
                    };
                    if rate == 0 {
                        return Err(Error::InvalidData);
                    }
                    let parser = match VorbisParser::new(&params.extradata) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            debug!(?e, "Vorbis granule parser unavailable, using packet timing");
                            None
                        }
                    };
                    (OggAudioCodec::Vorbis, headers, rate, parser, 0)
                }
                CodecId::Opus => {
                    let channels = params.channel_layout.nb_channels.max(1) as u8;
                    let headers = opus_headers(&params.extradata, params.sample_rate, channels)?;
                    // FFmpeg adds the codec's initial padding (the OpusHead
                    // pre-skip, rescaled from the input rate to the 48 kHz
                    // granule clock) to every packet's granule position.
                    let pre_skip = headers[0]
                        .get(10..12)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]) as i64)
                        .unwrap_or(0);
                    let sample_rate = if params.sample_rate > 0 {
                        params.sample_rate
                    } else {
                        48_000
                    };
                    let pre_skip = rescale_q(
                        pre_skip,
                        Rational::new(1, sample_rate as i32),
                        Rational::new(1, 48_000),
                    );
                    (OggAudioCodec::Opus, headers, 48_000, None, pre_skip)
                }
                _ => {
                    warn!(codec_id = ?params.codec_id, "unsupported codec for Ogg muxing");
                    return Err(Error::InvalidArgument);
                }
            };

            prepared.push((
                StreamState::new(
                    codec,
                    stream.time_base,
                    granule_rate,
                    base_serial.wrapping_add(i as u32),
                    parser,
                    pre_skip,
                ),
                headers,
            ));
        }

        for (state, headers) in prepared {
            self.streams.push(state);
            // The helper needs the stream in `self.streams` to add packets.
            self.write_header_pages(io, &headers)?;
        }
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, io: &mut BufferedIo, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::InvalidArgument);
        }
        if self.finished {
            return Err(Error::InvalidArgument);
        }
        let idx = packet.stream_index;
        let state = self.streams.get_mut(idx).ok_or(Error::StreamNotFound)?;
        let data = packet.data.data();
        state.advance_granule(data, packet);
        let granule = state.granule;
        Self::add_packet(io, state, data, granule)?;
        // FFmpeg's default page_duration is 1 s: flush once the page spans
        // at least that much (pages also flush when full, inside add_packet).
        if state.page_span_exceeded() && !state.page.is_empty() {
            state.page.flush(io, false)?;
            state.page_start = Some(state.granule as i64);
        }
        trace!(
            stream_index = idx,
            size = data.len(),
            granule,
            "Ogg packet written"
        );
        Ok(())
    }

    fn write_trailer(&mut self, io: &mut BufferedIo) -> Result<()> {
        if !self.header_written || self.finished {
            return Ok(());
        }
        for state in &mut self.streams {
            if state.page.is_empty() {
                // Even a header-only stream ends with an EOS page.
                state.page.set_granule(state.granule);
            }
            state.page.flush(io, true)?;
        }
        self.finished = true;
        io.flush()
    }

    fn set_option(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "serial" => {
                self.serial_override =
                    Some(value.parse::<u32>().map_err(|_| Error::InvalidArgument)?);
                Ok(())
            }
            _ => {
                warn!(key, "unknown Ogg muxer option");
                Ok(())
            }
        }
    }
}

/// Split Xiph-laced Vorbis extradata into its three header packets and
/// validate their leading bytes (FFmpeg's `ff_split_xiph_headers()` /
/// `vorbis_header()` check).
pub(crate) fn vorbis_headers(extradata: &[u8]) -> Result<Vec<Vec<u8>>> {
    let parts = split_xiph_headers(extradata, 30)?;
    let expected: [(u8, &[u8]); 3] = [(1, b"vorbis"), (3, b"vorbis"), (5, b"vorbis")];
    for (part, (packet_type, magic)) in parts.iter().zip(expected.iter()) {
        if part.1 == 0 || part.0.first() != Some(packet_type) || !part.0[1..].starts_with(magic) {
            return Err(Error::InvalidData);
        }
    }
    if parts[0].1 != 30 {
        return Err(Error::InvalidData);
    }
    Ok(parts.iter().map(|p| p.0.to_vec()).collect())
}

/// Build the two Opus header packets.
///
/// `extradata` is `OpusHead` (as produced by the Ogg demuxer) optionally
/// followed by `OpusTags`. When it is missing, a minimal mono/stereo head and
/// an empty comment header are synthesized from the stream parameters.
pub(crate) fn opus_headers(
    extradata: &[u8],
    sample_rate: u32,
    channels: u8,
) -> Result<Vec<Vec<u8>>> {
    let head: Vec<u8> = if extradata.starts_with(b"OpusHead") {
        let family = *extradata.get(18).ok_or(Error::InvalidData)?;
        let head_len = if family == 0 {
            19usize
        } else {
            let streams = *extradata.get(19).ok_or(Error::InvalidData)? as usize;
            let coupled = *extradata.get(20).ok_or(Error::InvalidData)? as usize;
            21usize.saturating_add(streams).saturating_add(coupled)
        };
        if extradata.len() < head_len || extradata[8] & 0xf0 != 0 {
            return Err(Error::InvalidData);
        }
        extradata[..head_len].to_vec()
    } else if extradata.is_empty() {
        let mut h = Vec::with_capacity(19);
        h.extend_from_slice(b"OpusHead");
        h.push(1); // version
        h.push(channels.max(1));
        h.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
        let input_rate = if sample_rate > 0 { sample_rate } else { 48_000 };
        h.extend_from_slice(&input_rate.to_le_bytes());
        h.extend_from_slice(&0i16.to_le_bytes()); // gain
        h.push(0); // mapping family
        h
    } else {
        return Err(Error::InvalidData);
    };

    let comments =
        if extradata.len() > head.len() && extradata[head.len()..].starts_with(b"OpusTags") {
            extradata[head.len()..].to_vec()
        } else {
            let mut tags = Vec::new();
            tags.extend_from_slice(b"OpusTags");
            let vendor = b"wedeo";
            tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
            tags.extend_from_slice(vendor);
            tags.extend_from_slice(&0u32.to_le_bytes());
            tags
        };

    Ok(vec![head, comments])
}

/// Muxer factory registration, following the other format crates.
pub(crate) struct OggMuxerFactory;

impl MuxerFactory for OggMuxerFactory {
    fn descriptor(&self) -> &OutputFormatDescriptor {
        static DESC: OutputFormatDescriptor = OutputFormatDescriptor {
            name: "ogg",
            long_name: "Ogg",
            extensions: "ogg",
            mime_types: "application/ogg",
            flags: OutputFormatFlags::GLOBALHEADER,
            audio_codec: CodecId::Vorbis,
            video_codec: CodecId::Theora,
        };
        &DESC
    }

    fn create(&self) -> Result<Box<dyn Muxer>> {
        Ok(Box::new(OggMuxer::new()))
    }
}
