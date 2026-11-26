#![forbid(unsafe_code)]
//! Text subtitle (SubRip / WebVTT) decoder and encoder adapters backed by the
//! pure-Rust `oxideav-subtitle` crate (MIT). No upstream source is copied here:
//! the adapter only converts between wedeo's and oxideav's packet/frame/cue
//! types.
//!
//! # CREDITS.md provenance note
//!
//! `oxideav-subtitle` 0.1.5 (MIT, https://github.com/OxideAV/oxideav-subtitle,
//! author Mark Karpeles) is used as a Cargo dependency only. This adapter
//! crate is LGPL-2.1-or-later, like the rest of wedeo. The dependency is built
//! with `default-features = false`: the `text` feature (TrueType shaping via
//! `oxideav-scribe` and rasterisation via `oxideav-raster`) is not needed for
//! cue parsing / emission.
//!
//! # Frame carrier
//!
//! `wedeo_core::Frame` has no subtitle variant, so the inventory-registered
//! `Decoder`/`Encoder` facade carries one cue per zero-sized video frame:
//! plane 0 holds the cue text as UTF-8, `pts`/`duration` are microseconds,
//! and frame metadata records the format. The semantically correct API is
//! also exposed through `wedeo_codec::subtitle::{SubtitleDecoder,
//! SubtitleEncoder}`. Wiring subtitles into the CLI requires a real
//! `FrameData::Subtitle` variant in wedeo-core; see the handoff report.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId as OCodecId, CodecParameters as OParams, Decoder as ODecoder, Encoder as OEncoder,
    Frame as OFrame, Packet as OPacket, SubtitleCue as OCue,
};
use oxideav_subtitle::ir::plain_text;

use wedeo_codec::decoder::{CodecParameters as WParams, Decoder as WDecoder};
use wedeo_codec::descriptor::{CodecCapabilities, CodecProperties};
use wedeo_codec::encoder::{Encoder as WEncoder, EncoderBuilder};
use wedeo_codec::registry::{DecoderFactory, EncoderFactory};
use wedeo_codec::subtitle::{
    Subtitle as WSubtitle, SubtitleDecoder, SubtitleEncoder, SubtitleRect,
};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame as WFrame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet as WPacket, PacketFlags as WPacketFlags};
use wedeo_core::pixel_format::PixelFormat as WPixel;
use wedeo_core::rational::Rational;

const NOPTS: i64 = i64::MIN;
const CARRIER_KIND_KEY: &str = "wedeo.subtitle.kind";
const CARRIER_STYLE_KEY: &str = "wedeo.subtitle.style";

/// `TEXT_SUB | LOSSLESS`; bitflags' `BitOr` is not const, so combine in const
/// context via `bits()` + `from_bits_truncate()` (both const in bitflags 2.x).
const SUB_TEXT_LOSSLESS: CodecProperties = CodecProperties::from_bits_truncate(
    CodecProperties::TEXT_SUB.bits() | CodecProperties::LOSSLESS.bits(),
);

fn to_w_error(e: oxideav_core::Error) -> Error {
    match e {
        oxideav_core::Error::NeedMore => Error::Again,
        oxideav_core::Error::Eof => Error::Eof,
        other => Error::Other(other.to_string()),
    }
}

fn us_time_base() -> Rational {
    Rational::new(1, 1_000_000)
}

/// Which upstream factory (`oxideav_subtitle::codec`) opens the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextKind {
    Srt,
    WebVtt,
}

impl TextKind {
    fn o_codec_id(self) -> &'static str {
        match self {
            Self::Srt => oxideav_subtitle::codec::SRT_CODEC_ID,
            Self::WebVtt => oxideav_subtitle::codec::WEBVTT_CODEC_ID,
        }
    }

    #[cfg(test)]
    fn w_codec_id(self) -> CodecId {
        match self {
            Self::Srt => CodecId::SubSrt,
            Self::WebVtt => CodecId::SubWebvtt,
        }
    }

    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Self::Srt => "srt_oxideav",
            Self::WebVtt => "webvtt_oxideav",
        }
    }

    #[cfg(test)]
    fn long_name(self) -> &'static str {
        match self {
            Self::Srt => "SubRip text subtitle [pure-Rust oxideav-subtitle]",
            Self::WebVtt => "WebVTT text subtitle [pure-Rust oxideav-subtitle]",
        }
    }

    fn carrier_key(self) -> &'static str {
        match self {
            Self::Srt => "srt",
            Self::WebVtt => "webvtt",
        }
    }
}

fn decoder_params(p: &WParams, kind: TextKind) -> OParams {
    let mut out = OParams::subtitle(OCodecId::new(kind.o_codec_id()));
    out.extradata = p.extradata.clone();
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn encoder_params(b: &EncoderBuilder, kind: TextKind) -> OParams {
    let mut out = OParams::subtitle(OCodecId::new(kind.o_codec_id()));
    for (k, v) in b.options.iter() {
        out.options.insert(k, v);
    }
    out
}

// ---------------------------------------------------------------------------
// Cue <-> wedeo conversions (shared by the trait impls and the registry facade)
// ---------------------------------------------------------------------------

fn carrier_text(frame: &WFrame) -> Result<String> {
    let video = frame.video().ok_or(Error::InvalidData)?;
    let plane = video.planes.first().ok_or(Error::InvalidData)?;
    let bytes = plane.buffer.data();
    let start = plane.offset.min(bytes.len());
    let end = start.saturating_add(plane.linesize).min(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

fn cue_to_frame(cue: &OCue, kind: TextKind) -> WFrame {
    let text = plain_text(&cue.segments);
    let mut out = WFrame::new_video(0, 0, WPixel::None);
    out.pts = cue.start_us;
    out.duration = cue.end_us.saturating_sub(cue.start_us).max(0);
    out.time_base = us_time_base();
    out.metadata.set(CARRIER_KIND_KEY, kind.carrier_key());
    if let Some(style) = &cue.style_ref {
        out.metadata.set(CARRIER_STYLE_KEY, style);
    }
    if let FrameData::Video(video) = &mut out.data {
        video.planes = vec![FramePlane {
            buffer: Buffer::from_slice(text.as_bytes()),
            offset: 0,
            linesize: text.len(),
        }];
    }
    out
}

fn frame_to_cue(frame: &WFrame, kind: TextKind) -> Result<OCue> {
    let text = carrier_text(frame)?;
    let start_us = if frame.pts == NOPTS { 0 } else { frame.pts };
    let end_us = start_us.saturating_add(frame.duration.max(0));
    Ok(OCue {
        start_us,
        end_us,
        style_ref: frame.metadata.get(CARRIER_STYLE_KEY).map(str::to_owned),
        positioning: None,
        segments: vec![match kind {
            // ASS override tags pass through the upstream writer verbatim in
            // `Segment::Raw`; see the sibling wedeo-ass-oxideav adapter.
            TextKind::Srt | TextKind::WebVtt => oxideav_core::Segment::Text(text),
        }],
    })
}

fn subtitle_to_frame(sub: &WSubtitle, kind: TextKind) -> Result<WFrame> {
    let text = match sub.rects.first() {
        Some(SubtitleRect::Text(text)) | Some(SubtitleRect::Ass(text)) => text.clone(),
        Some(SubtitleRect::Bitmap { .. }) => {
            return Err(Error::InvalidArgument);
        }
        None => String::new(),
    };
    let mut out = WFrame::new_video(0, 0, WPixel::None);
    out.pts = if sub.pts == NOPTS { 0 } else { sub.pts };
    out.duration = i64::from(sub.end_display_time) * 1000;
    out.time_base = us_time_base();
    out.metadata.set(CARRIER_KIND_KEY, kind.carrier_key());
    if let FrameData::Video(video) = &mut out.data {
        video.planes = vec![FramePlane {
            buffer: Buffer::from_slice(text.as_bytes()),
            offset: 0,
            linesize: text.len(),
        }];
    }
    Ok(out)
}

fn frame_to_subtitle(frame: &WFrame, kind: TextKind) -> Result<WSubtitle> {
    let text = carrier_text(frame)?;
    let start_us = if frame.pts == NOPTS { 0 } else { frame.pts };
    let end_us = start_us.saturating_add(frame.duration.max(0));
    let rect = match kind {
        TextKind::Srt | TextKind::WebVtt => SubtitleRect::Text(text),
    };
    Ok(WSubtitle {
        start_display_time: 0,
        end_display_time: ((end_us - start_us) / 1000) as u32,
        pts: start_us,
        rects: vec![rect],
    })
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct OxideavTextSubtitleDecoder {
    inner: Box<dyn ODecoder>,
    kind: TextKind,
    frames: VecDeque<WFrame>,
    draining: bool,
}

impl OxideavTextSubtitleDecoder {
    fn new(params: WParams, kind: TextKind) -> Result<Self> {
        let inner = oxideav_subtitle::codec::make_decoder(&decoder_params(&params, kind))
            .map_err(to_w_error)?;
        Ok(Self {
            inner,
            kind,
            frames: VecDeque::new(),
            draining: false,
        })
    }

    fn drain_frames(&mut self) -> Result<()> {
        loop {
            match self.inner.receive_frame() {
                Ok(OFrame::Subtitle(cue)) => self.frames.push_back(cue_to_frame(&cue, self.kind)),
                Ok(OFrame::StreamParams(_)) => continue,
                Ok(_) => return Err(Error::InvalidData),
                Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }
}

impl WDecoder for OxideavTextSubtitleDecoder {
    fn send_packet(&mut self, packet: Option<&WPacket>) -> Result<()> {
        self.drain_frames()?;
        match packet {
            Some(pkt) => {
                let mut op = OPacket::new(
                    0,
                    oxideav_core::TimeBase::new(1, 1_000_000),
                    pkt.data.data().to_vec(),
                );
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
        static SRT: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::SubSrt,
                media_type: MediaType::Subtitle,
                name: "srt_oxideav",
                long_name: "SubRip text subtitle [pure-Rust oxideav-subtitle]",
                properties: SUB_TEXT_LOSSLESS,
                profiles: &[],
            };
        static WEBVTT: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::SubWebvtt,
                media_type: MediaType::Subtitle,
                name: "webvtt_oxideav",
                long_name: "WebVTT text subtitle [pure-Rust oxideav-subtitle]",
                properties: SUB_TEXT_LOSSLESS,
                profiles: &[],
            };
        match self.kind {
            TextKind::Srt => &SRT,
            TextKind::WebVtt => &WEBVTT,
        }
    }
}

impl SubtitleDecoder for OxideavTextSubtitleDecoder {
    fn decode(&mut self, packet: &WPacket) -> Result<Option<WSubtitle>> {
        self.send_packet(Some(packet))?;
        match self.receive_frame() {
            Ok(frame) => Ok(Some(frame_to_subtitle(&frame, self.kind)?)),
            Err(Error::Again) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) {
        WDecoder::flush(self);
    }
}

struct TextDecoderFactory(TextKind);

impl DecoderFactory for TextDecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static SRT: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::SubSrt,
                name: "srt_oxideav",
                long_name: "SubRip text subtitle [pure-Rust oxideav-subtitle]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_TEXT_LOSSLESS,
                priority: 50,
            };
        static WEBVTT: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::SubWebvtt,
                name: "webvtt_oxideav",
                long_name: "WebVTT text subtitle [pure-Rust oxideav-subtitle]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_TEXT_LOSSLESS,
                priority: 50,
            };
        match self.0 {
            TextKind::Srt => &SRT,
            TextKind::WebVtt => &WEBVTT,
        }
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavTextSubtitleDecoder::new(params, self.0)?))
    }
}

static SRT_DECODER_FACTORY: TextDecoderFactory = TextDecoderFactory(TextKind::Srt);
static WEBVTT_DECODER_FACTORY: TextDecoderFactory = TextDecoderFactory(TextKind::WebVtt);
inventory::submit!(&SRT_DECODER_FACTORY as &dyn DecoderFactory);
inventory::submit!(&WEBVTT_DECODER_FACTORY as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

pub struct OxideavTextSubtitleEncoder {
    inner: Box<dyn OEncoder>,
    kind: TextKind,
    pending: VecDeque<WPacket>,
    draining: bool,
}

impl OxideavTextSubtitleEncoder {
    fn new(builder: &EncoderBuilder, kind: TextKind) -> Result<Self> {
        let inner = oxideav_subtitle::codec::make_encoder(&encoder_params(builder, kind))
            .map_err(to_w_error)?;
        Ok(Self {
            inner,
            kind,
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
                    out.dts = pkt.dts.unwrap_or(NOPTS);
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

impl WEncoder for OxideavTextSubtitleEncoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let cue = frame_to_cue(frame, self.kind)?;
                let of = OFrame::Subtitle(cue);
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

impl SubtitleEncoder for OxideavTextSubtitleEncoder {
    fn encode(&mut self, subtitle: &WSubtitle) -> Result<WPacket> {
        let frame = subtitle_to_frame(subtitle, self.kind)?;
        self.send_frame(Some(&frame))?;
        self.receive_packet()
    }
}

struct TextEncoderFactory(TextKind);

impl EncoderFactory for TextEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static SRT: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::SubSrt,
                name: "srt_oxideav",
                long_name: "SubRip text subtitle [pure-Rust oxideav-subtitle encoder]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_TEXT_LOSSLESS,
                supported_sample_formats: &[],
                supported_pixel_formats: &[],
                priority: 50,
            };
        static WEBVTT: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::SubWebvtt,
                name: "webvtt_oxideav",
                long_name: "WebVTT text subtitle [pure-Rust oxideav-subtitle encoder]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_TEXT_LOSSLESS,
                supported_sample_formats: &[],
                supported_pixel_formats: &[],
                priority: 50,
            };
        match self.0 {
            TextKind::Srt => &SRT,
            TextKind::WebVtt => &WEBVTT,
        }
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavTextSubtitleEncoder::new(&builder, self.0)?))
    }
}

static SRT_ENCODER_FACTORY: TextEncoderFactory = TextEncoderFactory(TextKind::Srt);
static WEBVTT_ENCODER_FACTORY: TextEncoderFactory = TextEncoderFactory(TextKind::WebVtt);
inventory::submit!(&SRT_ENCODER_FACTORY as &dyn EncoderFactory);
inventory::submit!(&WEBVTT_ENCODER_FACTORY as &dyn EncoderFactory);

// ---------------------------------------------------------------------------
// Adapter-level tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn srt_packet() -> WPacket {
        let bytes = b"1\n00:00:01,000 --> 00:00:02,500\nHello wedeo\n\n";
        WPacket::from_slice(bytes)
    }

    #[test]
    fn srt_decode_round_trip_through_registry() {
        let params = WParams::new(CodecId::SubSrt, MediaType::Subtitle);
        let mut dec = wedeo_codec::registry::find_decoder(CodecId::SubSrt)
            .expect("srt decoder factory")
            .create(params)
            .unwrap();
        dec.send_packet(Some(&srt_packet())).unwrap();
        let frame = dec.receive_frame().unwrap();
        assert_eq!(frame.pts, 1_000_000);
        assert_eq!(frame.duration, 1_500_000);
        assert_eq!(carrier_text(&frame).unwrap(), "Hello wedeo");

        let builder = EncoderBuilder::new(CodecId::SubSrt, MediaType::Subtitle);
        let mut enc = wedeo_codec::registry::find_encoder(CodecId::SubSrt)
            .expect("srt encoder factory")
            .create(builder)
            .unwrap();
        enc.send_frame(Some(&frame)).unwrap();
        let packet = enc.receive_packet().unwrap();
        let text = String::from_utf8_lossy(packet.data.data()).into_owned();
        assert!(text.contains("Hello wedeo"), "encoded SRT: {text}");
        assert!(text.contains("00:00:01,000"), "encoded SRT: {text}");
    }

    #[test]
    fn subtitle_trait_decode_and_encode() {
        let params = WParams::new(CodecId::SubSrt, MediaType::Subtitle);
        let mut dec = OxideavTextSubtitleDecoder::new(params, TextKind::Srt).unwrap();
        let sub = dec.decode(&srt_packet()).unwrap().unwrap();
        assert_eq!(sub.pts, 1_000_000);
        assert_eq!(sub.end_display_time, 1500);
        assert!(matches!(&sub.rects[0], SubtitleRect::Text(t) if t == "Hello wedeo"));

        let builder = EncoderBuilder::new(CodecId::SubSrt, MediaType::Subtitle);
        let mut enc = OxideavTextSubtitleEncoder::new(&builder, TextKind::Srt).unwrap();
        let pkt = enc.encode(&sub).unwrap();
        assert!(String::from_utf8_lossy(pkt.data.data()).contains("Hello wedeo"));
    }

    #[test]
    fn factories_are_discoverable_by_name() {
        assert!(wedeo_codec::registry::find_decoder_by_name("srt_oxideav").is_some());
        assert!(wedeo_codec::registry::find_decoder_by_name("webvtt_oxideav").is_some());
        assert!(wedeo_codec::registry::find_encoder_by_name("srt_oxideav").is_some());
        assert!(wedeo_codec::registry::find_encoder_by_name("webvtt_oxideav").is_some());
    }

    #[test]
    fn kind_helpers_are_consistent() {
        for kind in [TextKind::Srt, TextKind::WebVtt] {
            assert_eq!(
                CodecId::from_name(kind.o_codec_id()),
                Some(kind.w_codec_id())
            );
            assert!(!kind.name().is_empty());
            assert!(!kind.long_name().is_empty());
        }
    }
}
