#![forbid(unsafe_code)]
//! FFV1 adapters backed by the pure-Rust `oxideav-ffv1` crate (MIT).
//! No upstream source is copied here; this crate converts between wedeo's
//! and oxideav's frame/packet/parameter types.
//!
//! FFV1 is lossless. Version 3 streams carry an RFC 9043 configuration
//! record in container extradata, which both directions pass through
//! verbatim; version 1 streams carry their parameters inline.

use std::collections::VecDeque;

use oxideav_core::registry::CodecRegistry;
use oxideav_core::{
    CodecId as OCodecId, CodecParameters as OParams, Decoder as ODecoder, Encoder as OEncoder,
    Frame as OFrame, Packet as OPacket, PixelFormat as OPixel, VideoFrame as OVideoFrame,
    VideoPlane as OVideoPlane,
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
use wedeo_core::pixel_format::PixelFormat as WPixel;
use wedeo_core::rational::Rational;

const NOPTS: i64 = i64::MIN;

fn to_w_error(e: oxideav_core::Error) -> Error {
    match e {
        oxideav_core::Error::NeedMore => Error::Again,
        oxideav_core::Error::Eof => Error::Eof,
        other => Error::Other(other.to_string()),
    }
}

fn to_oxideav_pixel(pf: WPixel) -> OPixel {
    match pf {
        WPixel::Yuv420p => OPixel::Yuv420P,
        WPixel::Yuv422p => OPixel::Yuv422P,
        WPixel::Yuv444p => OPixel::Yuv444P,
        WPixel::Gray8 => OPixel::Gray8,
        _ => OPixel::Yuv420P,
    }
}

fn to_wedeo_pixel(pf: OPixel) -> WPixel {
    match pf {
        OPixel::Yuv420P => WPixel::Yuv420p,
        OPixel::Yuv422P => WPixel::Yuv422p,
        OPixel::Yuv444P => WPixel::Yuv444p,
        OPixel::Gray8 => WPixel::Gray8,
        _ => WPixel::Yuv420p,
    }
}

fn registry() -> CodecRegistry {
    let mut r = CodecRegistry::new();
    oxideav_ffv1::register_codecs(&mut r);
    r
}

fn decoder_params(p: &WParams) -> OParams {
    let mut out = OParams::video(OCodecId::new("ffv1"));
    out.width = Some(p.width);
    out.height = Some(p.height);
    if p.pixel_format != WPixel::None {
        out.pixel_format = Some(to_oxideav_pixel(p.pixel_format));
    }
    out.extradata = p.extradata.clone();
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn encoder_params(b: &EncoderBuilder) -> OParams {
    let mut p = OParams::video(OCodecId::new("ffv1"));
    p.width = Some(b.width);
    p.height = Some(b.height);
    p.pixel_format = Some(to_oxideav_pixel(b.pixel_format));
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

struct OxideavFfv1Decoder {
    inner: Box<dyn ODecoder>,
    width: u32,
    height: u32,
    pixel_format: WPixel,
    frames: VecDeque<WFrame>,
    draining: bool,
}

impl OxideavFfv1Decoder {
    fn new(params: WParams) -> Result<Self> {
        let width = params.width;
        let height = params.height;
        let pixel_format = params.pixel_format;
        let p = decoder_params(&params);
        let inner = registry().first_decoder(&p).map_err(to_w_error)?;
        Ok(Self {
            inner,
            width,
            height,
            pixel_format,
            frames: VecDeque::new(),
            draining: false,
        })
    }

    fn convert_frame(&self, frame: OFrame) -> Result<WFrame> {
        let OFrame::Video(v) = frame else {
            return Err(Error::InvalidData);
        };
        let pf = self
            .inner
            .output_params()
            .and_then(|p| p.pixel_format)
            .map(to_wedeo_pixel)
            .unwrap_or(self.pixel_format);
        let (width, height) = self
            .inner
            .output_params()
            .and_then(|p| p.width.zip(p.height))
            .unwrap_or((self.width, self.height));
        let mut out = WFrame::new_video(width, height, pf);
        out.pts = v.pts.unwrap_or(NOPTS);
        out.duration = 0;
        if let FrameData::Video(video) = &mut out.data {
            video.planes = v
                .planes
                .into_iter()
                .filter(|p| p.stride > 0 && p.stride < (isize::MAX as usize))
                .map(|p| FramePlane {
                    buffer: Buffer::from_slice(&p.data),
                    offset: 0,
                    linesize: p.stride,
                })
                .collect();
        }
        Ok(out)
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
}

impl WDecoder for OxideavFfv1Decoder {
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
                self.inner.send_packet(&op).map_err(to_w_error)?;
                self.drain_frames()
            }
            None => {
                if !self.draining {
                    self.draining = true;
                    self.inner.flush().map_err(to_w_error)?;
                    self.drain_frames()?;
                }
                Ok(())
            }
        }
    }

    fn receive_frame(&mut self) -> Result<WFrame> {
        self.frames.pop_front().ok_or(if self.draining {
            Error::Eof
        } else {
            Error::Again
        })
    }

    fn flush(&mut self) {
        let _ = self.inner.flush();
        self.frames.clear();
        self.draining = false;
    }

    fn descriptor(&self) -> &wedeo_codec::descriptor::CodecDescriptor {
        static DESC: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::Ffv1,
                media_type: MediaType::Video,
                name: "ffv1_oxideav",
                long_name: "FFV1 Video [pure-Rust oxideav-ffv1, lossless]",
                properties: CodecProperties::LOSSLESS,
                profiles: &[],
            };
        &DESC
    }
}

struct OxideavFfv1DecoderFactory;

impl DecoderFactory for OxideavFfv1DecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static DESC: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::Ffv1,
                name: "ffv1_oxideav",
                long_name: "FFV1 Video [pure-Rust oxideav-ffv1, lossless]",
                media_type: MediaType::Video,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSLESS,
                priority: 50,
            };
        &DESC
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavFfv1Decoder::new(params)?))
    }
}

inventory::submit!(&OxideavFfv1DecoderFactory as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct OxideavFfv1Encoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    extradata: Vec<u8>,
    draining: bool,
}

impl OxideavFfv1Encoder {
    fn new(b: &EncoderBuilder) -> Result<Self> {
        let p = encoder_params(b);
        let inner = registry().first_encoder(&p).map_err(to_w_error)?;
        let extradata = inner.output_params().extradata.clone();
        Ok(Self {
            inner,
            pending: VecDeque::new(),
            extradata,
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

impl WEncoder for OxideavFfv1Encoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let video = frame.video().ok_or(Error::InvalidData)?;
                let planes: Vec<OVideoPlane> = video
                    .planes
                    .iter()
                    .map(|p| OVideoPlane {
                        stride: p.linesize,
                        data: p.buffer.data()[p.offset..].to_vec(),
                    })
                    .collect();
                let of = OFrame::Video(OVideoFrame {
                    pts: if frame.pts == NOPTS {
                        None
                    } else {
                        Some(frame.pts)
                    },
                    planes,
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
        if self.extradata.is_empty() {
            None
        } else {
            Some(self.extradata.clone())
        }
    }
}

struct OxideavFfv1EncoderFactory;

impl EncoderFactory for OxideavFfv1EncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static DESC: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::Ffv1,
                name: "ffv1_oxideav",
                long_name: "FFV1 Video [pure-Rust oxideav-ffv1 encoder, lossless]",
                media_type: MediaType::Video,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSLESS,
                supported_sample_formats: &[],
                supported_pixel_formats: &[
                    WPixel::Yuv420p,
                    WPixel::Yuv422p,
                    WPixel::Yuv444p,
                    WPixel::Gray8,
                ],
                priority: 50,
            };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavFfv1Encoder::new(&builder)?))
    }
}

inventory::submit!(&OxideavFfv1EncoderFactory as &dyn EncoderFactory);
