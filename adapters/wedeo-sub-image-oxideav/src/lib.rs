#![forbid(unsafe_code)]
//! Bitmap subtitle (PGS, DVB subtitles, VobSub) adapters backed by the
//! pure-Rust `oxideav-sub-image` crate (MIT). No upstream source is copied
//! here: the adapter converts between wedeo's packet/video-frame types and
//! oxideav's packet/frame types.
//!
//! # CREDITS.md provenance note
//!
//! `oxideav-sub-image` 0.0.9 (MIT, https://github.com/OxideAV/oxideav-sub-image,
//! author Mark Karpeles) is used as a Cargo dependency only. This adapter
//! crate is LGPL-2.1-or-later. The upstream crate decodes PGS (`.sup`), DVB
//! subtitles and VobSub to RGBA video frames; it encodes PGS and DVB
//! subtitles. VobSub encoding does not exist upstream.
//!
//! # Codec id caveat
//!
//! wedeo has no PGS codec id. Until `CodecId::SubPgs` is added, PGS is
//! registered under `CodecId::SubDvdSubtitle` (name `pgs_oxideav`, priority
//! 50) while VobSub shares that id at priority 40. DVB subtitles use
//! `CodecId::SubDvbSubtitle`. See the handoff report for the exact edits.

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

/// `BITMAP_SUB | LOSSLESS`; bitflags' `BitOr` is not const, so combine in const
/// context via `bits()` + `from_bits_truncate()` (both const in bitflags 2.x).
const SUB_BITMAP_LOSSLESS: CodecProperties = CodecProperties::from_bits_truncate(
    CodecProperties::BITMAP_SUB.bits() | CodecProperties::LOSSLESS.bits(),
);
const SUB_BITMAP_LOSSY: CodecProperties = CodecProperties::from_bits_truncate(
    CodecProperties::BITMAP_SUB.bits() | CodecProperties::LOSSY.bits(),
);

fn to_w_error(e: oxideav_core::Error) -> Error {
    match e {
        oxideav_core::Error::NeedMore => Error::Again,
        oxideav_core::Error::Eof => Error::Eof,
        other => Error::Other(other.to_string()),
    }
}

fn time_base(tb: Rational) -> oxideav_core::TimeBase {
    oxideav_core::TimeBase::new(tb.num.max(1) as i64, tb.den.max(1) as i64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BitmapKind {
    Pgs,
    Dvb,
    VobSub,
}

impl BitmapKind {
    fn o_codec_id(self) -> &'static str {
        match self {
            Self::Pgs => oxideav_sub_image::PGS_CODEC_ID,
            Self::Dvb => oxideav_sub_image::DVBSUB_CODEC_ID,
            Self::VobSub => oxideav_sub_image::VOBSUB_CODEC_ID,
        }
    }
}

fn make_decoder_inner(kind: BitmapKind, p: &OParams) -> Result<Box<dyn ODecoder>> {
    match kind {
        BitmapKind::Pgs => oxideav_sub_image::pgs::make_decoder(p).map_err(to_w_error),
        BitmapKind::Dvb => oxideav_sub_image::dvbsub::make_decoder(p).map_err(to_w_error),
        BitmapKind::VobSub => oxideav_sub_image::vobsub::make_decoder(p).map_err(to_w_error),
    }
}

fn make_encoder_inner(kind: BitmapKind, p: &OParams) -> Result<Box<dyn OEncoder>> {
    match kind {
        BitmapKind::Pgs => oxideav_sub_image::pgs::make_encoder(p).map_err(to_w_error),
        BitmapKind::Dvb => oxideav_sub_image::dvbsub::make_encoder(p).map_err(to_w_error),
        // The upstream crate has no VobSub encoder (it needs a lock-step
        // .idx + .sub pair), so there is nothing to call here.
        BitmapKind::VobSub => Err(Error::PatchwelcomeNotImplemented),
    }
}

fn decoder_params(p: &WParams, kind: BitmapKind) -> OParams {
    let mut out = OParams::video(OCodecId::new(kind.o_codec_id()));
    out.width = (p.width > 0).then_some(p.width);
    out.height = (p.height > 0).then_some(p.height);
    if p.pixel_format != WPixel::None {
        out.pixel_format = Some(OPixel::Rgba);
    }
    out.extradata = p.extradata.clone();
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn encoder_params(b: &EncoderBuilder, kind: BitmapKind) -> OParams {
    let mut out = OParams::video(OCodecId::new(kind.o_codec_id()));
    out.width = (b.width > 0).then_some(b.width);
    out.height = (b.height > 0).then_some(b.height);
    out.pixel_format = Some(OPixel::Rgba);
    for (k, v) in b.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn convert_frame(frame: OFrame, tb: Rational) -> Result<WFrame> {
    let OFrame::Video(video) = frame else {
        return Err(Error::InvalidData);
    };
    let plane = video.planes.first().ok_or(Error::InvalidData)?;
    if plane.stride == 0 || plane.stride % 4 != 0 || plane.data.len() < plane.stride {
        return Err(Error::InvalidData);
    }
    let width = plane.stride / 4;
    let height = plane.data.len() / plane.stride;
    let mut out = WFrame::new_video(width as u32, height as u32, WPixel::Rgba);
    out.pts = video.pts.unwrap_or(NOPTS);
    out.duration = 0;
    out.time_base = tb;
    if let FrameData::Video(dst) = &mut out.data {
        dst.planes = vec![FramePlane {
            buffer: Buffer::from_slice(&plane.data),
            offset: 0,
            linesize: plane.stride,
        }];
    }
    Ok(out)
}

fn convert_to_oframe(frame: &WFrame) -> Result<OFrame> {
    let video = frame.video().ok_or(Error::InvalidData)?;
    if video.format != WPixel::Rgba {
        return Err(Error::PatchwelcomeNotImplemented);
    }
    let planes = video
        .planes
        .iter()
        .map(|p| OVideoPlane {
            stride: p.linesize,
            data: p.buffer.data()[p.offset..].to_vec(),
        })
        .collect();
    Ok(OFrame::Video(OVideoFrame {
        pts: if frame.pts == NOPTS {
            None
        } else {
            Some(frame.pts)
        },
        planes,
    }))
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct OxideavBitmapSubtitleDecoder {
    inner: Box<dyn ODecoder>,
    kind: BitmapKind,
    tb: Rational,
    frames: VecDeque<WFrame>,
    draining: bool,
}

impl OxideavBitmapSubtitleDecoder {
    fn new(params: WParams, kind: BitmapKind) -> Result<Self> {
        let tb = params.time_base;
        let p = decoder_params(&params, kind);
        let inner = make_decoder_inner(kind, &p)?;
        Ok(Self {
            inner,
            kind,
            tb,
            frames: VecDeque::new(),
            draining: false,
        })
    }

    fn drain_frames(&mut self) -> Result<()> {
        loop {
            match self.inner.receive_frame() {
                Ok(frame) => {
                    let frame = convert_frame(frame, self.tb)?;
                    self.frames.push_back(frame);
                }
                Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }
}

impl WDecoder for OxideavBitmapSubtitleDecoder {
    fn send_packet(&mut self, packet: Option<&WPacket>) -> Result<()> {
        self.drain_frames()?;
        match packet {
            Some(pkt) => {
                let mut op = OPacket::new(0, time_base(self.tb), pkt.data.data().to_vec());
                if pkt.pts != NOPTS {
                    op.pts = Some(pkt.pts);
                }
                if pkt.dts != NOPTS {
                    op.dts = Some(pkt.dts);
                }
                if pkt.duration > 0 {
                    op.duration = Some(pkt.duration);
                }
                if pkt.flags.contains(WPacketFlags::KEY) {
                    op.flags.keyframe = true;
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
        static PGS: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::SubDvdSubtitle,
                media_type: MediaType::Subtitle,
                name: "pgs_oxideav",
                long_name: "PGS bitmap subtitle [pure-Rust oxideav-sub-image]",
                properties: SUB_BITMAP_LOSSLESS,
                profiles: &[],
            };
        static DVB: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::SubDvbSubtitle,
                media_type: MediaType::Subtitle,
                name: "dvbsub_oxideav",
                long_name: "DVB bitmap subtitle [pure-Rust oxideav-sub-image]",
                properties: SUB_BITMAP_LOSSLESS,
                profiles: &[],
            };
        static VOBSUB: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::SubDvdSubtitle,
                media_type: MediaType::Subtitle,
                name: "vobsub_oxideav",
                long_name: "VobSub/DVD bitmap subtitle [pure-Rust oxideav-sub-image]",
                properties: SUB_BITMAP_LOSSLESS,
                profiles: &[],
            };
        match self.kind {
            BitmapKind::Pgs => &PGS,
            BitmapKind::Dvb => &DVB,
            BitmapKind::VobSub => &VOBSUB,
        }
    }
}

struct BitmapDecoderFactory(BitmapKind);

impl DecoderFactory for BitmapDecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static PGS: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::SubDvdSubtitle,
                name: "pgs_oxideav",
                long_name: "PGS bitmap subtitle [pure-Rust oxideav-sub-image]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_BITMAP_LOSSLESS,
                priority: 50,
            };
        static DVB: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::SubDvbSubtitle,
                name: "dvbsub_oxideav",
                long_name: "DVB bitmap subtitle [pure-Rust oxideav-sub-image]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_BITMAP_LOSSLESS,
                priority: 50,
            };
        static VOBSUB: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::SubDvdSubtitle,
                name: "vobsub_oxideav",
                long_name: "VobSub/DVD bitmap subtitle [pure-Rust oxideav-sub-image]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_BITMAP_LOSSLESS,
                priority: 40,
            };
        match self.0 {
            BitmapKind::Pgs => &PGS,
            BitmapKind::Dvb => &DVB,
            BitmapKind::VobSub => &VOBSUB,
        }
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavBitmapSubtitleDecoder::new(params, self.0)?))
    }
}

static PGS_DECODER_FACTORY: BitmapDecoderFactory = BitmapDecoderFactory(BitmapKind::Pgs);
static DVB_DECODER_FACTORY: BitmapDecoderFactory = BitmapDecoderFactory(BitmapKind::Dvb);
static VOBSUB_DECODER_FACTORY: BitmapDecoderFactory = BitmapDecoderFactory(BitmapKind::VobSub);
inventory::submit!(&PGS_DECODER_FACTORY as &dyn DecoderFactory);
inventory::submit!(&DVB_DECODER_FACTORY as &dyn DecoderFactory);
inventory::submit!(&VOBSUB_DECODER_FACTORY as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

pub struct OxideavBitmapSubtitleEncoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    draining: bool,
}

impl OxideavBitmapSubtitleEncoder {
    fn new(builder: &EncoderBuilder, kind: BitmapKind) -> Result<Self> {
        let p = encoder_params(builder, kind);
        let inner = make_encoder_inner(kind, &p)?;
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
                    out.pts = pkt.pts.unwrap_or(NOPTS);
                    out.dts = pkt.dts.or(pkt.pts).unwrap_or(NOPTS);
                    out.duration = pkt.duration.unwrap_or(0);
                    if pkt.flags.keyframe {
                        out.flags.insert(WPacketFlags::KEY);
                    }
                    self.pending.push_back(out);
                }
                Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }
}

impl WEncoder for OxideavBitmapSubtitleEncoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let of = convert_to_oframe(frame)?;
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
        self.pending.pop_front().ok_or(if self.draining {
            Error::Eof
        } else {
            Error::Again
        })
    }

    fn flush(&mut self) {
        let _ = self.inner.flush();
        self.pending.clear();
        self.draining = false;
    }
}

struct BitmapEncoderFactory(BitmapKind);

impl EncoderFactory for BitmapEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static PGS: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::SubDvdSubtitle,
                name: "pgs_oxideav",
                long_name: "PGS bitmap subtitle [pure-Rust oxideav-sub-image encoder]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_BITMAP_LOSSY,
                supported_sample_formats: &[],
                supported_pixel_formats: &[WPixel::Rgba],
                priority: 50,
            };
        static DVB: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::SubDvbSubtitle,
                name: "dvbsub_oxideav",
                long_name: "DVB bitmap subtitle [pure-Rust oxideav-sub-image encoder]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_BITMAP_LOSSY,
                supported_sample_formats: &[],
                supported_pixel_formats: &[WPixel::Rgba],
                priority: 50,
            };
        match self.0 {
            BitmapKind::Pgs => &PGS,
            BitmapKind::Dvb => &DVB,
            // No VobSub encoder factory: the upstream crate does not provide
            // `make_encoder` for it.
            BitmapKind::VobSub => &PGS,
        }
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavBitmapSubtitleEncoder::new(
            &builder, self.0,
        )?))
    }
}

static PGS_ENCODER_FACTORY: BitmapEncoderFactory = BitmapEncoderFactory(BitmapKind::Pgs);
static DVB_ENCODER_FACTORY: BitmapEncoderFactory = BitmapEncoderFactory(BitmapKind::Dvb);
inventory::submit!(&PGS_ENCODER_FACTORY as &dyn EncoderFactory);
inventory::submit!(&DVB_ENCODER_FACTORY as &dyn EncoderFactory);

// ---------------------------------------------------------------------------
// Adapter-level tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_frame(w: u32, h: u32) -> WFrame {
        let mut frame = WFrame::new_video(w, h, WPixel::Rgba);
        let mut data = vec![0u8; (w as usize) * (h as usize) * 4];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let at = (y * w as usize + x) * 4;
                data[at] = 0x22;
                data[at + 1] = 0x44;
                data[at + 2] = 0x66;
                data[at + 3] = if x == 1 && y == 1 { 0xFF } else { 0 };
            }
        }
        if let FrameData::Video(v) = &mut frame.data {
            v.planes = vec![FramePlane {
                buffer: Buffer::from_slice(&data),
                offset: 0,
                linesize: w as usize * 4,
            }];
        }
        frame
    }

    #[test]
    fn pgs_decode_and_encode_round_trip() {
        let data = oxideav_sub_image::pgs::build_demo_display_set(
            (4, 2),
            (2, 1),
            (1, 1),
            &[(1, [0x22, 0x44, 0x66, 0xFF])],
            &[1, 1],
        );
        let mut params = WParams::new(CodecId::SubDvdSubtitle, MediaType::Subtitle);
        params.time_base = Rational::new(1, 90_000);
        let mut dec = OxideavBitmapSubtitleDecoder::new(params, BitmapKind::Pgs).unwrap();
        let mut pkt = WPacket::from_slice(&data);
        pkt.pts = 90_000;
        dec.send_packet(Some(&pkt)).unwrap();
        let frame = dec.receive_frame().unwrap();
        let video = frame.video().unwrap();
        assert_eq!(video.width, 4);
        assert_eq!(video.height, 2);
        assert_eq!(video.format, WPixel::Rgba);
        assert_eq!(frame.pts, 90_000);

        let mut builder = EncoderBuilder::new(CodecId::SubDvdSubtitle, MediaType::Subtitle);
        builder.pixel_format = WPixel::Rgba;
        builder.time_base = Rational::new(1, 90_000);
        let mut enc = OxideavBitmapSubtitleEncoder::new(&builder, BitmapKind::Pgs).unwrap();
        enc.send_frame(Some(&rgba_frame(4, 2))).unwrap();
        let out = enc.receive_packet().unwrap();
        assert!(out.data.data().starts_with(b"PG"), "PGS packet prefix");
    }

    #[test]
    fn dvb_decode_and_encode_round_trip() {
        let pixels = vec![0u8; 4 * 2];
        let data = oxideav_sub_image::dvbsub::build_demo_pes((4, 2), &pixels, 4, 2);
        let mut params = WParams::new(CodecId::SubDvbSubtitle, MediaType::Subtitle);
        params.time_base = Rational::new(1, 90_000);
        let mut dec = OxideavBitmapSubtitleDecoder::new(params, BitmapKind::Dvb).unwrap();
        let mut pkt = WPacket::from_slice(&data);
        pkt.pts = 90_000;
        dec.send_packet(Some(&pkt)).unwrap();
        let frame = dec.receive_frame().unwrap();
        assert_eq!(frame.video().unwrap().width, 4);
        assert_eq!(frame.video().unwrap().height, 2);

        let mut builder = EncoderBuilder::new(CodecId::SubDvbSubtitle, MediaType::Subtitle);
        builder.pixel_format = WPixel::Rgba;
        builder.time_base = Rational::new(1, 90_000);
        let mut enc = OxideavBitmapSubtitleEncoder::new(&builder, BitmapKind::Dvb).unwrap();
        enc.send_frame(Some(&rgba_frame(4, 2))).unwrap();
        let out = enc.receive_packet().unwrap();
        assert_eq!(&out.data.data()[..2], &[0x20, 0x00], "DVB PES prefix");
    }

    #[test]
    fn vobsub_decode_and_missing_encoder() {
        let data = oxideav_sub_image::vobsub::build_demo_spu(2, 2, &[0, 1, 1, 0]);
        let mut params = WParams::new(CodecId::SubDvdSubtitle, MediaType::Subtitle);
        params.time_base = Rational::new(1, 90_000);
        let mut dec = OxideavBitmapSubtitleDecoder::new(params, BitmapKind::VobSub).unwrap();
        dec.send_packet(Some(&WPacket::from_slice(&data))).unwrap();
        let frame = dec.receive_frame().unwrap();
        assert_eq!(frame.video().unwrap().width, 2);
        assert_eq!(frame.video().unwrap().height, 2);

        let builder = EncoderBuilder::new(CodecId::SubDvdSubtitle, MediaType::Subtitle);
        let enc = OxideavBitmapSubtitleEncoder::new(&builder, BitmapKind::VobSub);
        assert!(enc.is_err(), "upstream has no VobSub encoder");
    }

    #[test]
    fn factories_are_discoverable() {
        for name in ["pgs_oxideav", "dvbsub_oxideav", "vobsub_oxideav"] {
            assert!(wedeo_codec::registry::find_decoder_by_name(name).is_some());
        }
        assert!(wedeo_codec::registry::find_encoder_by_name("pgs_oxideav").is_some());
        assert!(wedeo_codec::registry::find_encoder_by_name("dvbsub_oxideav").is_some());
        assert_eq!(
            wedeo_codec::registry::find_decoder(CodecId::SubDvdSubtitle)
                .unwrap()
                .descriptor()
                .name,
            "pgs_oxideav",
            "PGS wins the shared DVD-subtitle codec id"
        );
    }
}
