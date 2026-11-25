//! Vorbis encoder adapter backed by the pure-Rust `vorbis-enc` crate
//! (BSD-3-Clause, aoTuV b6.03). No upstream source is copied here.
//!
//! `vorbis-enc` writes a complete Ogg stream through `std::io::Write` and has
//! no packet-level API. This adapter captures those bytes, parses Ogg pages
//! back into packets, and exposes them through wedeo's `Encoder` trait. The
//! three Vorbis header packets are returned as Xiph-laced `extradata`, which
//! is what the Matroska muxer needs for an `A_VORBIS` track.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::num::{NonZeroU8, NonZeroU32};
use std::sync::{Arc, Mutex};

use vorbis_enc::{
    BitrateManagementStrategy, Encoder as VEncoder, EncoderBuilder as VorbisEncoderBuilder,
};
use wedeo_codec::descriptor::{CodecCapabilities, CodecProperties};
use wedeo_codec::encoder::{Encoder as WEncoder, EncoderBuilder};
use wedeo_codec::registry::EncoderFactory;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame as WFrame;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet as WPacket, PacketFlags as WPacketFlags};
use wedeo_core::rational::{Rational, rescale_q};
use wedeo_core::sample_format::SampleFormat as WSample;

/// In-memory sink shared with the encoder through an `Arc`.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Incremental Ogg page -> Vorbis packet parser.
#[derive(Default)]
struct PageParser {
    pending: Vec<u8>,
    packet: Vec<u8>,
    packets: VecDeque<(Vec<u8>, i64)>,
    headers: Vec<Vec<u8>>,
}

impl PageParser {
    fn feed(&mut self, bytes: &[u8], sample_rate: u32, time_base: Rational) -> Result<()> {
        self.pending.extend_from_slice(bytes);
        let mut consumed = 0;
        while self.pending.len() - consumed >= 27 {
            let page = &self.pending[consumed..];
            if &page[..4] != b"OggS" {
                return Err(Error::InvalidData);
            }
            let nsegs = page[26] as usize;
            let header_len = 27 + nsegs;
            if page.len() < header_len {
                break;
            }
            let granule = i64::from_le_bytes(page[6..14].try_into().unwrap());
            let mut body = header_len;
            let mut end_packet = false;
            for &seg in &page[27..header_len] {
                if body + seg as usize > page.len() {
                    return Err(Error::InvalidData);
                }
                self.packet
                    .extend_from_slice(&page[body..body + seg as usize]);
                body += seg as usize;
                if seg < 255 {
                    let payload = std::mem::take(&mut self.packet);
                    if self.headers.len() < 3 {
                        self.headers.push(payload);
                    } else {
                        let pts = rescale_q(
                            granule,
                            Rational::new(1, sample_rate.max(1) as i32),
                            time_base,
                        );
                        self.packets.push_back((payload, pts));
                    }
                    end_packet = true;
                }
            }
            if !end_packet && !self.packet.is_empty() {
                // Packet continues on the next page; keep granule for later.
            }
            consumed += body;
        }
        self.pending.drain(..consumed);
        Ok(())
    }
}

/// ISO/IEC 13818-3 / Xiph lacing of the first three Vorbis headers.
fn xiph_headers(packets: &[Vec<u8>]) -> Vec<u8> {
    let lens: Vec<usize> = packets.iter().map(Vec::len).collect();
    let mut out = Vec::new();
    out.push(2u8);
    for &len in lens.iter().take(2) {
        let mut len = len;
        while len >= 255 {
            out.push(255);
            len -= 255;
        }
        out.push(len as u8);
    }
    for packet in packets {
        out.extend_from_slice(packet);
    }
    out
}

fn to_w_error(e: vorbis_enc::Error) -> Error {
    Error::Other(format!("vorbis-enc: {e}"))
}

struct OxideavVorbisEncoder {
    inner: Option<VEncoder<Sink>>,
    sink: Sink,
    parser: PageParser,
    pending: VecDeque<WPacket>,
    sample_rate: u32,
    channels: u16,
    time_base: Rational,
    extradata: Vec<u8>,
    draining: bool,
}

impl OxideavVorbisEncoder {
    fn new(b: &EncoderBuilder) -> Result<Self> {
        let sample_rate = b.sample_rate.max(1);
        let channels = b.channel_layout.nb_channels.max(1) as u16;
        let rate = NonZeroU32::new(sample_rate).ok_or(Error::InvalidArgument)?;
        let ch = NonZeroU8::new(u8::try_from(channels).map_err(|_| Error::InvalidArgument)?)
            .ok_or(Error::InvalidArgument)?;
        let sink = Sink::default();
        let mut builder = VorbisEncoderBuilder::new(rate, ch, sink.clone())
            .stream_serial(0)
            .bitrate_management_strategy(BitrateManagementStrategy::QualityVbr {
                target_quality: 0.5,
            });
        if let Some(q) = b.options.get("quality")
            && let Ok(target_quality) = q.parse::<f32>()
        {
            builder = builder.bitrate_management_strategy(BitrateManagementStrategy::QualityVbr {
                target_quality,
            });
        }
        let inner = builder.build().map_err(to_w_error)?;
        let mut parser = PageParser::default();
        let initial = sink.0.lock().unwrap().clone();
        parser.feed(&initial, sample_rate, b.time_base)?;
        sink.0.lock().unwrap().clear();
        let extradata = if parser.headers.len() == 3 {
            xiph_headers(&parser.headers)
        } else {
            Vec::new()
        };
        Ok(Self {
            inner: Some(inner),
            sink,
            parser,
            pending: VecDeque::new(),
            sample_rate,
            channels,
            time_base: b.time_base,
            extradata,
            draining: false,
        })
    }

    fn pump(&mut self) -> Result<()> {
        let bytes = self.sink.0.lock().unwrap().clone();
        self.parser.feed(&bytes, self.sample_rate, self.time_base)?;
        // Re-feed only newly appended bytes next time by tracking length is
        // unnecessary: the parser discards complete pages and retains only
        // the trailing partial page.
        self.sink.0.lock().unwrap().clear();
        while let Some((data, pts)) = self.parser.packets.pop_front() {
            let mut pkt = WPacket::from_slice(&data);
            pkt.pts = pts;
            pkt.dts = pts;
            pkt.duration = 0;
            pkt.flags.insert(WPacketFlags::KEY);
            self.pending.push_back(pkt);
        }
        Ok(())
    }

    fn deinterleave(&self, frame: &WFrame) -> Result<Vec<Vec<f32>>> {
        let audio = frame.audio().ok_or(Error::InvalidData)?;
        let channels = self.channels as usize;
        match audio.format {
            WSample::Fltp | WSample::Dblp => {
                let mut out = Vec::with_capacity(channels);
                for plane in audio.planes.iter().take(channels) {
                    let bytes = &plane.buffer.data()[plane.offset..];
                    let samples = if audio.format == WSample::Fltp { 4 } else { 8 };
                    let values = bytes
                        .chunks_exact(samples)
                        .map(|c| {
                            if samples == 4 {
                                f32::from_le_bytes([c[0], c[1], c[2], c[3]])
                            } else {
                                f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                                    as f32
                            }
                        })
                        .collect();
                    out.push(values);
                }
                Ok(out)
            }
            WSample::Flt | WSample::Dbl | WSample::S16 | WSample::S32 => {
                let plane = audio.planes.first().ok_or(Error::InvalidData)?;
                let bytes = &plane.buffer.data()[plane.offset..];
                let nb_samples = audio.nb_samples as usize;
                let mut out = vec![Vec::with_capacity(nb_samples); channels];
                match audio.format {
                    WSample::Flt => {
                        for (i, c) in bytes.chunks_exact(4).take(nb_samples).enumerate() {
                            out[i % channels].push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
                        }
                    }
                    WSample::Dbl => {
                        for (i, c) in bytes.chunks_exact(8).take(nb_samples).enumerate() {
                            out[i % channels].push(f64::from_le_bytes([
                                c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7],
                            ]) as f32);
                        }
                    }
                    WSample::S16 => {
                        for (i, c) in bytes.chunks_exact(2).take(nb_samples).enumerate() {
                            let v = i16::from_le_bytes([c[0], c[1]]);
                            out[i % channels].push(v as f32 / 32768.0);
                        }
                    }
                    WSample::S32 => {
                        for (i, c) in bytes.chunks_exact(4).take(nb_samples).enumerate() {
                            let v = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                            out[i % channels].push(v as f32 / 2147483648.0);
                        }
                    }
                    _ => unreachable!(),
                }
                Ok(out)
            }
            _ => Err(Error::PatchwelcomeNotImplemented),
        }
    }
}

impl WEncoder for OxideavVorbisEncoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let planes = self.deinterleave(frame)?;
                let inner = self.inner.as_mut().ok_or(Error::InvalidData)?;
                inner
                    .encode_audio_block(planes.iter().map(|p| p.as_slice()))
                    .map_err(to_w_error)?;
            }
            None => {
                if !self.draining {
                    self.draining = true;
                    let inner = self.inner.take().ok_or(Error::InvalidData)?;
                    let _ = inner.finish().map_err(to_w_error)?;
                }
            }
        }
        self.pump()
    }

    fn receive_packet(&mut self) -> Result<WPacket> {
        if let Some(pkt) = self.pending.pop_front() {
            return Ok(pkt);
        }
        if self.draining {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.pending.clear();
        self.draining = false;
    }

    fn extradata(&self) -> Option<Vec<u8>> {
        if self.extradata.is_empty() {
            None
        } else {
            Some(self.extradata.clone())
        }
    }
}

struct OxideavVorbisEncoderFactory;

impl EncoderFactory for OxideavVorbisEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static DESC: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::Vorbis,
                name: "vorbis_oxideav",
                long_name: "Vorbis Audio [pure-Rust vorbis-enc / aoTuV]",
                media_type: MediaType::Audio,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                supported_sample_formats: &[
                    WSample::S16,
                    WSample::S32,
                    WSample::Flt,
                    WSample::Dbl,
                    WSample::Fltp,
                    WSample::Dblp,
                ],
                supported_pixel_formats: &[],
                priority: 50,
            };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavVorbisEncoder::new(&builder)?))
    }
}

inventory::submit!(&OxideavVorbisEncoderFactory as &dyn EncoderFactory);
