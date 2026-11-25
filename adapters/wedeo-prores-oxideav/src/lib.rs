#![forbid(unsafe_code)]
//! ProRes adapters backed by the pure-Rust `oxideav-prores` crate (MIT).
//! No upstream source is copied here; this crate converts between wedeo's and
//! oxideav's frame/packet/parameter types.
//!
//! ProRes is self-describing (profile travels in the container's sample-entry
//! FourCC), so neither codec needs extradata.

use std::collections::VecDeque;

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
        WPixel::Yuv422p => OPixel::Yuv422P,
        WPixel::Yuv422p10le => OPixel::Yuv422P10Le,
        WPixel::Yuv444p => OPixel::Yuv444P,
        WPixel::Yuv444p10le => OPixel::Yuv444P10Le,
        _ => OPixel::Yuv422P,
    }
}

fn to_wedeo_pixel(pf: OPixel) -> WPixel {
    match pf {
        OPixel::Yuv422P => WPixel::Yuv422p,
        OPixel::Yuv422P10Le => WPixel::Yuv422p10le,
        OPixel::Yuv444P => WPixel::Yuv444p,
        OPixel::Yuv444P10Le => WPixel::Yuv444p10le,
        _ => WPixel::Yuv422p,
    }
}

fn decoder_params(p: &WParams) -> OParams {
    let mut out = OParams::video(OCodecId::new("prores"));
    out.width = Some(p.width);
    out.height = Some(p.height);
    out.pixel_format = Some(to_oxideav_pixel(p.pixel_format));
    out
}

fn encoder_params(b: &EncoderBuilder) -> OParams {
    let mut p = OParams::video(OCodecId::new("prores"));
    p.width = Some(b.width);
    p.height = Some(b.height);
    // ProRes has no 4:2:0 profile; 4:2:0 input is upsampled in send_frame.
    let pf = match b.pixel_format {
        WPixel::Yuv420p => WPixel::Yuv422p,
        WPixel::Yuv420p10le => WPixel::Yuv422p10le,
        other => other,
    };
    p.pixel_format = Some(to_oxideav_pixel(pf));
    if b.time_base.num > 0 {
        p.frame_rate = Some(oxideav_core::Rational::new(
            b.time_base.den as i64,
            b.time_base.num as i64,
        ));
    }
    if b.bit_rate > 0 {
        p.bit_rate = Some(b.bit_rate as u64);
    }
    p
}

fn time_base(tb: Rational) -> oxideav_core::TimeBase {
    oxideav_core::TimeBase::new(tb.num.max(1) as i64, tb.den.max(1) as i64)
}

/// Duplicate each chroma row (4:2:0 -> 4:2:2 keeps chroma width, doubles
/// chroma height).
fn upsample_chroma(plane: &FramePlane, chroma_w: usize, chroma_h: usize, bps: usize) -> Vec<u8> {
    let src = &plane.buffer.data()[plane.offset..];
    let stride = chroma_w * bps;
    let mut out = Vec::with_capacity(stride * chroma_h * 2);
    for y in 0..chroma_h {
        let start = y * plane.linesize;
        let end = (start + stride).min(src.len());
        if start >= end {
            break;
        }
        out.extend_from_slice(&src[start..end]);
        out.extend_from_slice(&src[start..end]);
    }
    out
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct OxideavProresDecoder {
    inner: Box<dyn ODecoder>,
    width: u32,
    height: u32,
    pixel_format: WPixel,
    /// oxideav requires the previous frame to be pulled before the next
    /// packet is accepted, so frames are buffered here.
    frames: VecDeque<WFrame>,
    draining: bool,
}

impl OxideavProresDecoder {
    fn new(params: WParams) -> Result<Self> {
        let width = params.width;
        let height = params.height;
        let pixel_format = params.pixel_format;
        let p = decoder_params(&params);
        let inner = oxideav_prores::decoder::make_decoder(&p).map_err(to_w_error)?;
        Ok(Self {
            inner,
            width,
            height,
            pixel_format,
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
}

impl WDecoder for OxideavProresDecoder {
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
        static DESC: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::Prores,
                media_type: MediaType::Video,
                name: "prores_oxideav",
                long_name: "ProRes Video [pure-Rust oxideav-prores]",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            };
        &DESC
    }
}

struct OxideavProresDecoderFactory;

impl DecoderFactory for OxideavProresDecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static DESC: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::Prores,
                name: "prores_oxideav",
                long_name: "ProRes Video [pure-Rust oxideav-prores]",
                media_type: MediaType::Video,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                priority: 50,
            };
        &DESC
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavProresDecoder::new(params)?))
    }
}

inventory::submit!(&OxideavProresDecoderFactory as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct OxideavProresEncoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    convert_420: bool,
    draining: bool,
}

impl OxideavProresEncoder {
    fn new(b: &EncoderBuilder) -> Result<Self> {
        let p = encoder_params(b);
        let inner = oxideav_prores::encoder::make_encoder(&p).map_err(to_w_error)?;
        let convert_420 = matches!(b.pixel_format, WPixel::Yuv420p | WPixel::Yuv420p10le);
        Ok(Self {
            inner,
            pending: VecDeque::new(),
            convert_420,
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

impl WEncoder for OxideavProresEncoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let video = frame.video().ok_or(Error::InvalidData)?;
                tracing::debug!(
                    planes = ?video.planes.iter().map(|p| (p.linesize, p.buffer.data().len(), p.offset)).collect::<Vec<_>>(),
                    "oxideav ProRes input planes"
                );
                let planes: Vec<OVideoPlane> = if self.convert_420 && video.planes.len() >= 3 {
                    let bps = if video.format == WPixel::Yuv420p10le {
                        2
                    } else {
                        1
                    };
                    let chroma_w = (video.width as usize) / 2;
                    let chroma_h = (video.height as usize) / 2;
                    let luma = OVideoPlane {
                        stride: video.planes[0].linesize,
                        data: video.planes[0].buffer.data()[video.planes[0].offset..].to_vec(),
                    };
                    let cb = upsample_chroma(&video.planes[1], chroma_w, chroma_h, bps);
                    let cr = upsample_chroma(&video.planes[2], chroma_w, chroma_h, bps);
                    vec![
                        luma,
                        OVideoPlane {
                            stride: chroma_w * bps,
                            data: cb,
                        },
                        OVideoPlane {
                            stride: chroma_w * bps,
                            data: cr,
                        },
                    ]
                } else {
                    video
                        .planes
                        .iter()
                        .map(|p| OVideoPlane {
                            stride: p.linesize,
                            data: p.buffer.data()[p.offset..].to_vec(),
                        })
                        .collect()
                };
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
}

struct OxideavProresEncoderFactory;

impl EncoderFactory for OxideavProresEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static DESC: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::Prores,
                name: "prores_oxideav",
                long_name: "ProRes Video [pure-Rust oxideav-prores encoder]",
                media_type: MediaType::Video,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                supported_sample_formats: &[],
                supported_pixel_formats: &[
                    WPixel::Yuv420p,
                    WPixel::Yuv420p10le,
                    WPixel::Yuv422p,
                    WPixel::Yuv422p10le,
                    WPixel::Yuv444p,
                    WPixel::Yuv444p10le,
                ],
                priority: 50,
            };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavProresEncoder::new(&builder)?))
    }
}

inventory::submit!(&OxideavProresEncoderFactory as &dyn EncoderFactory);
