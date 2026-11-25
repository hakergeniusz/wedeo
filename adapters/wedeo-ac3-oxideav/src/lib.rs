#![forbid(unsafe_code)]
//! AC-3 and E-AC-3 adapters backed by the pure-Rust `oxideav-ac3` crate
//! (MIT). No upstream source is copied here: this crate only converts between
//! wedeo's and oxideav's frame/packet/parameter types.
//!
//! Both codecs are self-describing syncframes, so no extradata is needed.

use std::collections::VecDeque;

use oxideav_core::{
    AudioFrame as OAudioFrame, CodecId as OCodecId, CodecParameters as OParams,
    Decoder as ODecoder, Encoder as OEncoder, Frame as OFrame, Packet as OPacket,
    SampleFormat as OSample,
};

use wedeo_codec::decoder::{CodecParameters as WParams, Decoder as WDecoder};
use wedeo_codec::descriptor::{CodecCapabilities, CodecProperties};
use wedeo_codec::encoder::{Encoder as WEncoder, EncoderBuilder};
use wedeo_codec::registry::{DecoderFactory, EncoderFactory};
use wedeo_core::buffer::Buffer;
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

fn layout(channels: u16) -> wedeo_core::channel_layout::ChannelLayout {
    use wedeo_core::channel_layout::ChannelLayout;
    match channels {
        1 => ChannelLayout::mono(),
        2 => ChannelLayout::stereo(),
        6 => ChannelLayout::surround_5_1(),
        8 => ChannelLayout::surround_7_1(),
        n => ChannelLayout::unspec(n as i32),
    }
}

fn decoder_params(p: &WParams, is_eac3: bool) -> OParams {
    let id = if is_eac3 { "eac3" } else { "ac3" };
    let mut out = OParams::audio(OCodecId::new(id));
    out.sample_rate = Some(if p.sample_rate > 0 {
        p.sample_rate
    } else {
        48_000
    });
    out.channels = Some(p.channel_layout.nb_channels.max(1) as u16);
    out.sample_format = Some(OSample::S16);
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn encoder_params(b: &EncoderBuilder) -> OParams {
    let mut p = OParams::audio(OCodecId::new("ac3"));
    p.sample_rate = Some(b.sample_rate);
    p.channels = Some(b.channel_layout.nb_channels.max(1) as u16);
    p.sample_format = Some(OSample::S16);
    if b.bit_rate > 0 {
        p.bit_rate = Some(b.bit_rate as u64);
    }
    for (k, v) in b.options.iter() {
        p.options.insert(k, v);
    }
    p
}

fn time_base(tb: Rational) -> oxideav_core::TimeBase {
    oxideav_core::TimeBase::new(tb.num.max(1) as i64, tb.den.max(1) as i64)
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct OxideavAc3Decoder {
    inner: Box<dyn ODecoder>,
    channels: u16,
    eac3: bool,
    /// The oxideav decoder rejects a second packet until its previous output
    /// is pulled, so buffered frames are drained here before the next send.
    frames: VecDeque<WFrame>,
    draining: bool,
}

impl OxideavAc3Decoder {
    fn new(params: WParams, is_eac3: bool) -> Result<Self> {
        let channels = params.channel_layout.nb_channels.max(1) as u16;
        let p = decoder_params(&params, is_eac3);
        let inner = if is_eac3 {
            oxideav_ac3::decoder::make_eac3_decoder(&p)
        } else {
            oxideav_ac3::decoder::make_decoder(&p)
        }
        .map_err(to_w_error)?;
        Ok(Self {
            inner,
            channels,
            eac3: is_eac3,
            frames: VecDeque::new(),
            draining: false,
        })
    }

    fn drain_frames(&mut self) -> Result<()> {
        loop {
            match self.inner.receive_frame() {
                Ok(frame) => {
                    let frame = self.convert_frame(frame)?;
                    self.frames.push_back(frame);
                }
                Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }

    fn convert_frame(&self, frame: OFrame) -> Result<WFrame> {
        let OFrame::Audio(a) = frame else {
            return Err(Error::InvalidData);
        };
        let (rate, channels) = self
            .inner
            .output_params()
            .map(|p| {
                (
                    p.sample_rate.unwrap_or(48_000),
                    p.channels.unwrap_or(self.channels),
                )
            })
            .unwrap_or((48_000, self.channels));
        let mut data = Vec::new();
        for plane in a.data {
            data.extend_from_slice(&plane);
        }
        let mut out = WFrame::new_audio(a.samples, WSample::S16, rate, layout(channels));
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

impl WDecoder for OxideavAc3Decoder {
    fn send_packet(&mut self, packet: Option<&WPacket>) -> Result<()> {
        self.drain_frames()?;
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
        if let Some(frame) = self.frames.pop_front() {
            return Ok(frame);
        }
        self.drain_frames()?;
        self.frames.pop_front().ok_or(if self.draining {
            Error::Eof
        } else {
            Error::Again
        })
    }

    fn flush(&mut self) {
        let _ = self.inner.flush();
        self.draining = false;
    }

    fn descriptor(&self) -> &wedeo_codec::descriptor::CodecDescriptor {
        static AC3: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::Ac3,
                media_type: MediaType::Audio,
                name: "ac3_oxideav",
                long_name: "AC-3 Audio [pure-Rust oxideav-ac3]",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            };
        static EAC3: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::Eac3,
                media_type: MediaType::Audio,
                name: "eac3_oxideav",
                long_name: "E-AC-3 Audio [pure-Rust oxideav-ac3]",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            };
        if self.eac3 { &EAC3 } else { &AC3 }
    }
}

macro_rules! decoder_factory {
    ($name:ident, $id:expr, $codec_name:literal, $long_name:literal, $eac3:expr) => {
        struct $name;

        impl DecoderFactory for $name {
            fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
                static DESC: wedeo_codec::decoder::DecoderDescriptor =
                    wedeo_codec::decoder::DecoderDescriptor {
                        codec_id: $id,
                        name: $codec_name,
                        long_name: $long_name,
                        media_type: MediaType::Audio,
                        capabilities: CodecCapabilities::empty(),
                        properties: CodecProperties::LOSSY,
                        priority: 40,
                    };
                &DESC
            }

            fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
                Ok(Box::new(OxideavAc3Decoder::new(params, $eac3)?))
            }
        }

        inventory::submit!(&$name as &dyn DecoderFactory);
    };
}

decoder_factory!(
    OxideavAc3DecoderFactory,
    CodecId::Ac3,
    "ac3_oxideav",
    "AC-3 Audio [pure-Rust oxideav-ac3]",
    false
);
decoder_factory!(
    OxideavEac3DecoderFactory,
    CodecId::Eac3,
    "eac3_oxideav",
    "E-AC-3 Audio [pure-Rust oxideav-ac3]",
    true
);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct OxideavAc3Encoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    draining: bool,
}

impl OxideavAc3Encoder {
    fn new(b: &EncoderBuilder, eac3: bool) -> Result<Self> {
        if b.sample_format != WSample::S16 {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        let p = encoder_params(b);
        let inner = if eac3 {
            oxideav_ac3::eac3::encoder::make_encoder(&p)
        } else {
            oxideav_ac3::encoder::make_encoder(&p)
        }
        .map_err(to_w_error)?;
        Ok(Self {
            inner,
            pending: VecDeque::new(),
            draining: false,
        })
    }

    fn drain(&mut self) -> Result<()> {
        loop {
            match self.inner.receive_packet() {
                Ok(pkt) => {
                    let mut out = WPacket::from_slice(&pkt.data);
                    out.stream_index = pkt.stream_index as usize;
                    out.pts = pkt.pts.unwrap_or(NOPTS);
                    out.dts = pkt.dts.or(pkt.pts).unwrap_or(NOPTS);
                    out.duration = pkt.duration.unwrap_or(0);
                    if pkt.flags.keyframe {
                        out.flags.insert(WPacketFlags::KEY);
                    }
                    self.pending.push_back(out);
                }
                Err(oxideav_core::Error::NeedMore) => break,
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }
}

impl WEncoder for OxideavAc3Encoder {
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
}

macro_rules! encoder_factory {
    ($name:ident, $id:expr, $codec_name:literal, $long_name:literal, $eac3:expr) => {
        struct $name;

        impl EncoderFactory for $name {
            fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
                static DESC: wedeo_codec::encoder::EncoderDescriptor =
                    wedeo_codec::encoder::EncoderDescriptor {
                        codec_id: $id,
                        name: $codec_name,
                        long_name: $long_name,
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
                Ok(Box::new(OxideavAc3Encoder::new(&builder, $eac3)?))
            }
        }

        inventory::submit!(&$name as &dyn EncoderFactory);
    };
}

encoder_factory!(
    OxideavAc3EncoderFactory,
    CodecId::Ac3,
    "ac3_oxideav",
    "AC-3 Audio [pure-Rust oxideav-ac3 encoder]",
    false
);
encoder_factory!(
    OxideavEac3EncoderFactory,
    CodecId::Eac3,
    "eac3_oxideav",
    "E-AC-3 Audio [pure-Rust oxideav-ac3 encoder]",
    true
);
