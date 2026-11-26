#![forbid(unsafe_code)]
//! ASS/SSA subtitle decoder and encoder adapters backed by the pure-Rust
//! `oxideav-ass` crate (MIT). No upstream source is copied here: the adapter
//! converts between wedeo's packet/frame/subtitle types and oxideav's
//! packet/frame/cue types.
//!
//! # CREDITS.md provenance note
//!
//! `oxideav-ass` 0.0.11 (MIT, https://github.com/OxideAV/oxideav-ass, author
//! Mark Karpeles) is used as a Cargo dependency only. It also depends on
//! `oxideav-subtitle` (MIT) for the shared `SubtitleCue` IR and the
//! `segments_to_ass_text` writer used here. Both are built with default
//! features off; this adapter crate is LGPL-2.1-or-later.
//!
//! # Frame carrier
//!
//! `wedeo_core::Frame` has no subtitle variant. The inventory-registered
//! `Decoder`/`Encoder` facade carries one cue per zero-sized video frame:
//! plane 0 holds the ASS dialogue text (override tags re-emitted verbatim),
//! `pts`/`duration` are microseconds, and metadata keeps the style name.
//! The semantically correct API is exposed through
//! `wedeo_codec::subtitle::{SubtitleDecoder, SubtitleEncoder}` using
//! `SubtitleRect::Ass`. See the handoff report for the wedeo-core gap.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId as OCodecId, CodecParameters as OParams, Decoder as ODecoder, Encoder as OEncoder,
    Frame as OFrame, Packet as OPacket, SubtitleCue as OCue,
};
use oxideav_subtitle::ass_script::segments_to_ass_text;

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
const STYLE_KEY: &str = "wedeo.subtitle.style";

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

fn decoder_params(p: &WParams) -> OParams {
    let mut out = OParams::subtitle(OCodecId::new(oxideav_ass::codec::ASS_CODEC_ID));
    out.extradata = p.extradata.clone();
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn encoder_params(b: &EncoderBuilder) -> OParams {
    let mut out = OParams::subtitle(OCodecId::new(oxideav_ass::codec::ASS_CODEC_ID));
    for (k, v) in b.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn first_text(sub: &WSubtitle) -> Result<String> {
    match sub.rects.first() {
        Some(SubtitleRect::Text(text)) | Some(SubtitleRect::Ass(text)) => Ok(text.clone()),
        Some(SubtitleRect::Bitmap { .. }) => Err(Error::InvalidArgument),
        None => Ok(String::new()),
    }
}

fn carrier_text(frame: &WFrame) -> Result<String> {
    let video = frame.video().ok_or(Error::InvalidData)?;
    let plane = video.planes.first().ok_or(Error::InvalidData)?;
    let bytes = plane.buffer.data();
    let start = plane.offset.min(bytes.len());
    let end = start.saturating_add(plane.linesize).min(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[start..end]).into_owned())
}

fn cue_to_frame(cue: &OCue) -> WFrame {
    let text = segments_to_ass_text(&cue.segments);
    let mut out = WFrame::new_video(0, 0, WPixel::None);
    out.pts = cue.start_us;
    out.duration = cue.end_us.saturating_sub(cue.start_us).max(0);
    out.time_base = us_time_base();
    if let Some(style) = &cue.style_ref {
        out.metadata.set(STYLE_KEY, style);
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

fn frame_to_cue(frame: &WFrame) -> Result<OCue> {
    let text = carrier_text(frame)?;
    let start_us = if frame.pts == NOPTS { 0 } else { frame.pts };
    let end_us = start_us.saturating_add(frame.duration.max(0));
    Ok(OCue {
        start_us,
        end_us,
        style_ref: frame.metadata.get(STYLE_KEY).map(str::to_owned),
        positioning: None,
        // Raw passes through oxideav-ass's Dialogue writer verbatim, so ASS
        // override tags survive the wedeo -> oxideav hop unchanged.
        segments: vec![oxideav_core::Segment::Raw(text)],
    })
}

fn subtitle_to_frame(sub: &WSubtitle) -> Result<WFrame> {
    let text = first_text(sub)?;
    let mut out = WFrame::new_video(0, 0, WPixel::None);
    out.pts = if sub.pts == NOPTS { 0 } else { sub.pts };
    out.duration = i64::from(sub.end_display_time) * 1000;
    out.time_base = us_time_base();
    if let FrameData::Video(video) = &mut out.data {
        video.planes = vec![FramePlane {
            buffer: Buffer::from_slice(text.as_bytes()),
            offset: 0,
            linesize: text.len(),
        }];
    }
    Ok(out)
}

fn frame_to_subtitle(frame: &WFrame) -> Result<WSubtitle> {
    let text = carrier_text(frame)?;
    let start_us = if frame.pts == NOPTS { 0 } else { frame.pts };
    let end_us = start_us.saturating_add(frame.duration.max(0));
    Ok(WSubtitle {
        start_display_time: 0,
        end_display_time: ((end_us - start_us) / 1000) as u32,
        pts: start_us,
        rects: vec![SubtitleRect::Ass(text)],
    })
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct OxideavAssDecoder {
    inner: Box<dyn ODecoder>,
    frames: VecDeque<WFrame>,
    draining: bool,
}

impl OxideavAssDecoder {
    pub fn new(params: WParams) -> Result<Self> {
        let inner =
            oxideav_ass::codec::make_decoder(&decoder_params(&params)).map_err(to_w_error)?;
        Ok(Self {
            inner,
            frames: VecDeque::new(),
            draining: false,
        })
    }

    fn drain_frames(&mut self) -> Result<()> {
        loop {
            match self.inner.receive_frame() {
                Ok(OFrame::Subtitle(cue)) => self.frames.push_back(cue_to_frame(&cue)),
                Ok(OFrame::StreamParams(_)) => continue,
                Ok(_) => return Err(Error::InvalidData),
                Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
                Err(e) => return Err(to_w_error(e)),
            }
        }
        Ok(())
    }
}

impl WDecoder for OxideavAssDecoder {
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
        static DESC: wedeo_codec::descriptor::CodecDescriptor =
            wedeo_codec::descriptor::CodecDescriptor {
                id: CodecId::SubSsa,
                media_type: MediaType::Subtitle,
                name: "ass_oxideav",
                long_name: "ASS/SSA subtitle [pure-Rust oxideav-ass]",
                properties: SUB_TEXT_LOSSLESS,
                profiles: &[],
            };
        &DESC
    }
}

impl SubtitleDecoder for OxideavAssDecoder {
    fn decode(&mut self, packet: &WPacket) -> Result<Option<WSubtitle>> {
        self.send_packet(Some(packet))?;
        match self.receive_frame() {
            Ok(frame) => Ok(Some(frame_to_subtitle(&frame)?)),
            Err(Error::Again) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn flush(&mut self) {
        WDecoder::flush(self);
    }
}

struct AssDecoderFactory;

impl DecoderFactory for AssDecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static DESC: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::SubSsa,
                name: "ass_oxideav",
                long_name: "ASS/SSA subtitle [pure-Rust oxideav-ass]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_TEXT_LOSSLESS,
                priority: 50,
            };
        &DESC
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavAssDecoder::new(params)?))
    }
}

inventory::submit!(&AssDecoderFactory as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

pub struct OxideavAssEncoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    draining: bool,
}

impl OxideavAssEncoder {
    pub fn new(builder: &EncoderBuilder) -> Result<Self> {
        let inner =
            oxideav_ass::codec::make_encoder(&encoder_params(builder)).map_err(to_w_error)?;
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

impl WEncoder for OxideavAssEncoder {
    fn send_frame(&mut self, frame: Option<&WFrame>) -> Result<()> {
        match frame {
            Some(frame) => {
                let cue = frame_to_cue(frame)?;
                self.inner
                    .send_frame(&OFrame::Subtitle(cue))
                    .map_err(to_w_error)?;
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

impl SubtitleEncoder for OxideavAssEncoder {
    fn encode(&mut self, subtitle: &WSubtitle) -> Result<WPacket> {
        let frame = subtitle_to_frame(subtitle)?;
        self.send_frame(Some(&frame))?;
        self.receive_packet()
    }
}

struct AssEncoderFactory;

impl EncoderFactory for AssEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static DESC: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::SubSsa,
                name: "ass_oxideav",
                long_name: "ASS/SSA subtitle [pure-Rust oxideav-ass encoder]",
                media_type: MediaType::Subtitle,
                capabilities: CodecCapabilities::empty(),
                properties: SUB_TEXT_LOSSLESS,
                supported_sample_formats: &[],
                supported_pixel_formats: &[],
                priority: 50,
            };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavAssEncoder::new(&builder)?))
    }
}

inventory::submit!(&AssEncoderFactory as &dyn EncoderFactory);

// ---------------------------------------------------------------------------
// Adapter-level tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dialogue_packet() -> WPacket {
        let line = b"Dialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,{\\b1}Hello{\\b0} ASS\n";
        WPacket::from_slice(line)
    }

    #[test]
    fn ass_decode_round_trip_through_registry() {
        let params = WParams::new(CodecId::SubSsa, MediaType::Subtitle);
        let mut dec = wedeo_codec::registry::find_decoder(CodecId::SubSsa)
            .expect("ass decoder factory")
            .create(params)
            .unwrap();
        dec.send_packet(Some(&dialogue_packet())).unwrap();
        let frame = dec.receive_frame().unwrap();
        assert_eq!(frame.pts, 1_000_000);
        assert_eq!(frame.duration, 2_000_000);
        assert_eq!(frame.metadata.get(STYLE_KEY), Some("Default"));
        let text = carrier_text(&frame).unwrap();
        // The carrier keeps ASS override tags verbatim (FFmpeg's ass rects do
        // too), so the plain words are split by the tag block.
        assert!(text.contains("{\\b1}Hello{\\b0} ASS"), "carrier: {text}");

        let builder = EncoderBuilder::new(CodecId::SubSsa, MediaType::Subtitle);
        let mut enc = wedeo_codec::registry::find_encoder(CodecId::SubSsa)
            .expect("ass encoder factory")
            .create(builder)
            .unwrap();
        enc.send_frame(Some(&frame)).unwrap();
        let packet = enc.receive_packet().unwrap();
        let out = String::from_utf8_lossy(packet.data.data()).into_owned();
        assert!(out.starts_with("Dialogue:"), "encoded ASS: {out}");
        assert!(out.contains("{\\b1}Hello{\\b0}"), "encoded ASS: {out}");
    }

    #[test]
    fn subtitle_trait_uses_ass_rect() {
        let params = WParams::new(CodecId::SubSsa, MediaType::Subtitle);
        let mut dec = OxideavAssDecoder::new(params).unwrap();
        let sub = dec.decode(&dialogue_packet()).unwrap().unwrap();
        match &sub.rects[0] {
            SubtitleRect::Ass(text) => assert!(text.contains("{\\b1}Hello{\\b0} ASS")),
            other => panic!("expected ASS rect, got {other:?}"),
        }
        let builder = EncoderBuilder::new(CodecId::SubSsa, MediaType::Subtitle);
        let mut enc = OxideavAssEncoder::new(&builder).unwrap();
        let pkt = enc.encode(&sub).unwrap();
        assert!(String::from_utf8_lossy(pkt.data.data()).contains("{\\b1}Hello{\\b0}"));
    }

    #[test]
    fn registry_names_are_unique() {
        assert!(wedeo_codec::registry::find_decoder_by_name("ass_oxideav").is_some());
        assert!(wedeo_codec::registry::find_encoder_by_name("ass_oxideav").is_some());
    }
}
