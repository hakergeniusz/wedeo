//! AAC decoder and encoder adapters backed by the pure-Rust `oxideav-aac`
//! crate (MIT). No source from it is copied here: the adapter only converts
//! between wedeo's and oxideav's frame/packet/parameter types.
//!
//! The decoder deliberately sits below Symphonia's AAC decoder so existing
//! bitexact audio tests keep using the same implementation. The encoder is
//! the first AAC encoder in wedeo.

use std::collections::VecDeque;

use oxideav_core::{
    AudioFrame as OAudioFrame, CodecId as OCodecId, CodecParameters as OParams,
    Decoder as ODecoder, Encoder as OEncoder, Frame as OFrame, Packet as OPacket,
    SampleFormat as OSample,
};
use tracing::debug;

use wedeo_codec::decoder::{CodecParameters as WParams, Decoder as WDecoder};
use wedeo_codec::descriptor::{CodecCapabilities, CodecProperties};
use wedeo_codec::encoder::{CodecFlags, Encoder as WEncoder, EncoderBuilder};
use wedeo_codec::registry::{DecoderFactory, EncoderFactory};
use wedeo_core::buffer::Buffer;
use wedeo_core::channel_layout::{Channel, ChannelLayout, ChannelOrder};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame as WFrame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet as WPacket, PacketFlags as WPacketFlags};
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat as WSample;

const NOPTS: i64 = i64::MIN;

fn to_w_error(e: oxideav_core::Error) -> Error {
    match e {
        oxideav_core::Error::NeedMore => Error::Again,
        oxideav_core::Error::Eof => Error::Eof,
        other => Error::Other(other.to_string()),
    }
}

/// AAC channel_configuration values from ISO/IEC 14496-3 Table 1.19.
fn channel_configuration(channels: u16) -> u8 {
    match channels {
        1..=6 => channels as u8,
        8 => 7, // 7.1
        _ => 2, // fall back to stereo for exotic layouts
    }
}

fn wedeo_layout(channels: u16) -> ChannelLayout {
    match channels {
        1 => ChannelLayout::mono(),
        2 => ChannelLayout::stereo(),
        6 => ChannelLayout::surround_5_1(),
        8 => ChannelLayout::surround_7_1(),
        n => ChannelLayout {
            order: ChannelOrder::Unspec,
            nb_channels: n as i32,
            channels: vec![Channel::FrontCenter; n as usize],
        },
    }
}

fn decoder_params(p: &WParams) -> OParams {
    let mut out = OParams::audio(OCodecId::new("aac"));
    out.sample_rate = Some(p.sample_rate.max(1));
    let channels = p.channel_layout.nb_channels.max(1) as u16;
    out.channels = Some(channels);
    out.sample_format = Some(OSample::S16);
    out.extradata = p.extradata.clone();
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn time_base(tb: Rational) -> oxideav_core::TimeBase {
    oxideav_core::TimeBase::new(tb.num.max(1) as i64, tb.den.max(1) as i64)
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct OxideavAacDecoder {
    inner: Box<dyn ODecoder>,
    sample_rate: u32,
    channels: u16,
    draining: bool,
}

impl OxideavAacDecoder {
    fn new(params: WParams) -> Result<Self> {
        let sample_rate = params.sample_rate.max(1);
        let channels = params.channel_layout.nb_channels.max(1) as u16;
        let p = decoder_params(&params);
        let inner = oxideav_aac::codec_decoder::make_decoder(&p).map_err(to_w_error)?;
        Ok(Self {
            inner,
            sample_rate,
            channels,
            draining: false,
        })
    }

    fn convert_frame(&self, frame: OFrame) -> Result<WFrame> {
        let OFrame::Audio(a) = frame else {
            return Err(Error::InvalidData);
        };
        let (sample_rate, channels) = self
            .inner
            .output_params()
            .map(|p| {
                (
                    p.sample_rate.unwrap_or(self.sample_rate),
                    p.channels.unwrap_or(self.channels),
                )
            })
            .unwrap_or((self.sample_rate, self.channels));
        let mut data = Vec::new();
        for plane in a.data {
            data.extend_from_slice(&plane);
        }
        let mut out =
            WFrame::new_audio(a.samples, WSample::S16, sample_rate, wedeo_layout(channels));
        out.pts = a.pts.unwrap_or(NOPTS);
        out.duration = a.samples as i64;
        if let FrameData::Audio(audio) = &mut out.data {
            audio.planes = vec![FramePlane {
                buffer: Buffer::from_slice(&data),
                offset: 0,
                linesize: data.len(),
            }];
        }
        Ok(out)
    }
}

impl WDecoder for OxideavAacDecoder {
    fn send_packet(&mut self, packet: Option<&WPacket>) -> Result<()> {
        match packet {
            Some(pkt) => {
                let mut op =
                    OPacket::new(0, time_base(Rational::new(1, 1)), pkt.data.data().to_vec());
                if pkt.pts != NOPTS {
                    op.pts = Some(pkt.pts);
                }
                if pkt.dts != NOPTS {
                    op.dts = Some(pkt.dts);
                }
                if pkt.duration > 0 {
                    op.duration = Some(pkt.duration);
                }
                self.inner.send_packet(&op).map_err(to_w_error)
            }
            None => {
                if !self.draining {
                    self.draining = true;
                    self.inner.flush().map_err(to_w_error)?;
                }
                Ok(())
            }
        }
    }

    fn receive_frame(&mut self) -> Result<WFrame> {
        loop {
            let frame = self.inner.receive_frame().map_err(to_w_error)?;
            if !frame.is_stream_params() {
                let frame = self.convert_frame(frame)?;
                return Ok(frame);
            }
        }
    }

    fn flush(&mut self) {
        let _ = self.inner.flush();
        self.draining = false;
    }

    fn descriptor(&self) -> &wedeo_codec::descriptor::CodecDescriptor {
        static DESC: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::Aac,
                media_type: MediaType::Audio,
                name: "aac_oxideav",
                long_name: "AAC Audio [pure-Rust oxideav-aac]",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            };
        &DESC
    }
}

struct OxideavAacDecoderFactory;

impl DecoderFactory for OxideavAacDecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static DESC: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::Aac,
                name: "aac_oxideav",
                long_name: "AAC Audio [pure-Rust oxideav-aac]",
                media_type: MediaType::Audio,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                priority: 40, // Symphonia stays the default at 50
            };
        &DESC
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavAacDecoder::new(params)?))
    }
}

inventory::submit!(&OxideavAacDecoderFactory as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

pub struct OxideavAacEncoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    sample_rate: u32,
    channels: u16,
    global_header: bool,
    draining: bool,
}

impl OxideavAacEncoder {
    fn new(builder: &EncoderBuilder) -> Result<Self> {
        if builder.sample_format != WSample::S16 {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        let sample_rate = builder.sample_rate.max(1);
        let channels = builder.channel_layout.nb_channels.max(1) as u16;
        let mut p = OParams::audio(OCodecId::new("aac"));
        p.sample_rate = Some(sample_rate);
        p.channels = Some(channels);
        p.sample_format = Some(OSample::S16);
        if builder.bit_rate > 0 {
            p.bit_rate = Some(builder.bit_rate as u64);
        }
        for (k, v) in builder.options.iter() {
            p.options.insert(k, v);
        }
        let global_header = builder.flags.contains(CodecFlags::GLOBAL_HEADER);
        let inner = oxideav_aac::codec_encoder::make_encoder(&p).map_err(to_w_error)?;
        debug!(
            sample_rate,
            channels, global_header, "oxideav AAC encoder opened"
        );
        Ok(Self {
            inner,
            pending: VecDeque::new(),
            sample_rate,
            channels,
            global_header,
            draining: false,
        })
    }

    fn drain(&mut self) -> Result<()> {
        loop {
            match self.inner.receive_packet() {
                Ok(pkt) => self.pending.push_back(self.convert_packet(pkt)),
                Err(oxideav_core::Error::NeedMore) => break,
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }

    fn convert_packet(&self, pkt: OPacket) -> WPacket {
        let mut data = pkt.data.clone();
        if self.global_header && data.len() >= 7 && data[0] == 0xff && data[1] & 0xf0 == 0xf0 {
            // MP4 carries raw_data_block access units, not ADTS frames.
            if let Ok((_, offset)) = oxideav_aac::adts::AdtsHeader::parse(&data) {
                data = data[offset.min(data.len())..].to_vec();
            }
        }
        let mut out = WPacket::from_slice(&data);
        out.stream_index = pkt.stream_index as usize;
        out.pts = pkt.pts.unwrap_or(NOPTS);
        out.dts = pkt.dts.unwrap_or(NOPTS);
        out.duration = pkt.duration.unwrap_or(0);
        if pkt.flags.keyframe {
            out.flags.insert(WPacketFlags::KEY);
        }
        out
    }
}

impl WEncoder for OxideavAacEncoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let audio = frame.audio().ok_or(Error::InvalidData)?;
                if audio.format != WSample::S16 {
                    return Err(Error::PatchwelcomeNotImplemented);
                }
                let mut data = Vec::new();
                for plane in &audio.planes {
                    data.extend_from_slice(&plane.buffer.data()[plane.offset..]);
                }
                let of = OFrame::Audio(OAudioFrame {
                    samples: audio.nb_samples,
                    pts: if frame.pts == NOPTS {
                        None
                    } else {
                        Some(frame.pts)
                    },
                    data: vec![data],
                });
                self.inner.send_frame(&of).map_err(to_w_error)?;
                self.drain()
            }
            None => {
                if !self.draining {
                    self.draining = true;
                    self.inner.flush().map_err(to_w_error)?;
                }
                self.drain()
            }
        }
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
        Some(oxideav_aac::asc_writer::aac_lc_asc(
            self.sample_rate,
            channel_configuration(self.channels),
        ))
    }
}

struct OxideavAacEncoderFactory;

impl EncoderFactory for OxideavAacEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static DESC: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::Aac,
                name: "aac_oxideav",
                long_name: "AAC Audio [pure-Rust oxideav-aac encoder]",
                media_type: MediaType::Audio,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                supported_sample_formats: &[WSample::S16],
                supported_pixel_formats: &[],
                priority: 50,
            };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavAacEncoder::new(&builder)?))
    }
}

inventory::submit!(&OxideavAacEncoderFactory as &dyn EncoderFactory);
