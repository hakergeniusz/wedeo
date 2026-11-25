#![forbid(unsafe_code)]
//! H.264 encoder adapter backed by the pure-Rust `rusty_h264-encoder` crate.
//!
//! The encoder itself is BSD-2-Clause and is used as an external dependency;
//! no source from it is copied here. This adapter (the wedeo `Encoder` glue,
//! Annex-B to AVCC packetization and avcC construction) is LGPL-2.1-or-later,
//! like the rest of wedeo. The upstream crate is built with `default-features
//! = false` so it does not install a process-wide allocator; the `asm`
//! feature enables the upstream SIMD fast path (BSD-2-Clause, credited in
//! `CREDITS.md`).
//!
//! FFmpeg has no native H.264 encoder, so there is no C source to port here.

use std::collections::VecDeque;

use rusty_h264_common::nal::{emulation_prevent_into, split_annex_b};
use rusty_h264_common::{NalUnit, NalUnitType, YuvFrame, YuvPlanes};
use rusty_h264_encoder::{Encoder as RustyEncoder, EncoderConfig, Pps, Preset, Sps};
use tracing::debug;

use wedeo_codec::descriptor::{CodecCapabilities, CodecProperties};
use wedeo_codec::encoder::{Encoder, EncoderBuilder, EncoderDescriptor};
use wedeo_codec::registry::EncoderFactory;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::pixel_format::PixelFormat as WPixel;

const DEFAULT_QP: u8 = 26;
const DEFAULT_GOP: u32 = 30;
const DEFAULT_FPS: f32 = 30.0;

pub struct RustyH264Encoder {
    encoder: RustyEncoder,
    pending: VecDeque<Packet>,
    /// `avcC` configuration record for MP4, built from the first SPS/PPS.
    extradata: Vec<u8>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    frame_duration: i64,
    next_pts: i64,
    /// With more than one thread, frames are encoded in parallel GOP batches
    /// through rusty_h264's `encode_all`.
    parallel: bool,
    batch: Vec<YuvFrame>,
    batch_meta: Vec<(i64, i64)>,
    draining: bool,
    ended: bool,
}

const PARALLEL_BATCH: usize = 256;

impl RustyH264Encoder {
    fn new(builder: &EncoderBuilder) -> Result<Self> {
        if !matches!(builder.pixel_format, WPixel::Yuv420p | WPixel::Yuv420p10le) {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        let width = builder.width as usize;
        let height = builder.height as usize;
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(Error::InvalidArgument);
        }

        let qp = builder
            .options
            .get("qp")
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(DEFAULT_QP)
            .min(51);
        let gop = builder
            .options
            .get("keyint")
            .or_else(|| builder.options.get("gop"))
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(DEFAULT_GOP);
        let preset = match builder.options.get("preset").map(str::to_ascii_lowercase) {
            Some(p) if p == "fast" || p == "ultrafast" => Preset::Fast,
            Some(p) if p == "quality" || p == "slow" => Preset::Quality,
            _ => Preset::Balanced,
        };

        let time_base = builder.time_base;
        let fps = if time_base.num > 0 && time_base.den > 0 {
            time_base.den as f32 / time_base.num as f32
        } else {
            DEFAULT_FPS
        };
        // With a 1/1000 time base a frame spans 33 ticks at 30 fps; with 1/30
        // it spans exactly one.  Clamp so a zero/odd time base stays valid.
        let frame_duration = if fps > 1.0 {
            ((fps / DEFAULT_FPS).round() as i64).max(1)
        } else {
            1
        };

        let mut cfg = EncoderConfig::new(width, height);
        // Zero added latency: one access unit out per frame, no lookahead or
        // reordering, so `dts == pts` always and `send_frame(None)` drains
        // nothing unexpected.
        let parallel = builder.thread_count.unwrap_or(1) > 1;
        cfg.mbtree = parallel;
        cfg.lookahead = if parallel { 40 } else { 0 };
        cfg.scenecut = 0;
        cfg.bframes = 0;
        cfg.gop_size = gop;
        cfg.qp = qp;
        cfg.preset = preset;
        cfg.framerate = if fps > 0.0 { fps } else { DEFAULT_FPS };
        if builder.bit_rate > 0 {
            cfg.bitrate = builder.bit_rate as u32;
        }
        // Precompute the parameter sets so avcC is available before the first
        // frame is encoded (the MP4 muxer needs it in write_header).
        let sps_nal = Sps::from_config(&cfg).to_nal();
        let pps_nal = Pps::from_config(&cfg).to_nal();
        fn nal_bytes(nal: &NalUnit) -> Vec<u8> {
            let mut out = Vec::with_capacity(nal.rbsp.len() + 8);
            out.push(nal.header_byte());
            emulation_prevent_into(&nal.rbsp, &mut out);
            out
        }
        let sps = nal_bytes(&sps_nal);
        let pps = nal_bytes(&pps_nal);

        let encoder = RustyEncoder::new(cfg).map_err(|e| Error::Other(e.to_string()))?;

        debug!(width, height, qp, gop, fps, "rusty_h264 encoder opened");

        let mut this = Self {
            encoder,
            pending: VecDeque::new(),
            extradata: Vec::new(),
            sps: Some(sps),
            pps: Some(pps),
            frame_duration,
            next_pts: 0,
            parallel,
            batch: Vec::new(),
            batch_meta: Vec::new(),
            draining: false,
            ended: false,
        };
        this.rebuild_extradata();
        Ok(this)
    }

    /// Turn one Annex-B access unit into one AVCC packet (4-byte NAL lengths,
    /// no start codes), capturing SPS/PPS into `avcC` extradata on the way.
    fn packetize(&mut self, stream: &[u8], pts: i64, duration: i64) {
        let mut data = Vec::with_capacity(stream.len());
        let mut key = false;
        for nal in split_annex_b(stream) {
            if nal.is_empty() {
                continue;
            }
            let ty = NalUnitType::from_id(nal[0] & 0x1f);
            match ty {
                NalUnitType::Sps => {
                    if self.sps.is_none() {
                        self.sps = Some(nal.to_vec());
                        self.rebuild_extradata();
                    }
                    continue;
                }
                NalUnitType::Pps => {
                    if self.pps.is_none() {
                        self.pps = Some(nal.to_vec());
                        self.rebuild_extradata();
                    }
                    continue;
                }
                NalUnitType::AccessUnitDelimiter => continue,
                NalUnitType::IdrSlice => key = true,
                _ => {}
            }
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }
        if data.is_empty() {
            return;
        }
        let mut pkt = Packet::from_slice(&data);
        pkt.stream_index = 0;
        pkt.pts = pts;
        pkt.dts = pts;
        pkt.duration = duration.max(1);
        if key {
            pkt.flags.insert(PacketFlags::KEY);
        }
        self.pending.push_back(pkt);
    }

    fn rebuild_extradata(&mut self) {
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else {
            return;
        };
        if sps.len() < 4 || pps.is_empty() {
            return;
        }
        let mut avcc = Vec::with_capacity(11 + sps.len() + pps.len());
        avcc.push(1); // configurationVersion
        avcc.push(sps[1]); // AVCProfileIndication
        avcc.push(sps[2]); // profile_compatibility
        avcc.push(sps[3]); // AVCLevelIndication
        avcc.push(0xff); // reserved + lengthSizeMinusOne = 3 (4-byte lengths)
        avcc.push(0xe1); // reserved + numOfSequenceParameterSets = 1
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1); // numOfPictureParameterSets
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
        self.extradata = avcc;
    }
}

impl RustyH264Encoder {
    /// Encode a batch of frames using rusty_h264's parallel GOP encoder.
    fn encode_batch(&mut self) -> Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let frames = std::mem::take(&mut self.batch);
        let meta = std::mem::take(&mut self.batch_meta);
        let streams = self
            .encoder
            .encode_all(&frames)
            .map_err(|e| Error::Other(e.to_string()))?;
        for (stream, (pts, duration)) in streams.into_iter().zip(meta) {
            self.packetize(&stream, pts, duration);
        }
        Ok(())
    }

    /// Copy a plane into a tightly packed buffer for `YuvFrame`.
    fn tight_plane(data: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height);
        for row in 0..height {
            let start = row * stride;
            if start + width > data.len() {
                break;
            }
            out.extend_from_slice(&data[start..start + width]);
        }
        out.resize(width * height, 0);
        out
    }
}

impl Encoder for RustyH264Encoder {
    fn send_frame(&mut self, frame: Option<&Frame>) -> Result<()> {
        if self.ended {
            return Err(Error::Eof);
        }
        match frame {
            Some(frame) => {
                let video = frame.video().ok_or(Error::InvalidData)?;
                if video.planes.len() < 3 || video.width % 2 != 0 || video.height % 2 != 0 {
                    return Err(Error::InvalidData);
                }
                let planes = [&video.planes[0], &video.planes[1], &video.planes[2]];
                // rusty_h264 only accepts 8-bit 4:2:0. Downshift 10-bit
                // little-endian words to 8-bit before handing planes over;
                // treating 16-bit words as bytes produces a green image.
                let is_10bit = video.format == WPixel::Yuv420p10le;
                let converted: Vec<Vec<u8>> = if is_10bit {
                    planes
                        .iter()
                        .map(|p| {
                            p.buffer.data()[p.offset..]
                                .chunks_exact(2)
                                .map(|w| (u16::from_le_bytes([w[0], w[1]]) >> 2) as u8)
                                .collect()
                        })
                        .collect()
                } else {
                    planes
                        .iter()
                        .map(|p| p.buffer.data()[p.offset..].to_vec())
                        .collect()
                };
                let (y_stride, u_stride, v_stride) = if is_10bit {
                    (
                        video.width as usize,
                        (video.width / 2) as usize,
                        (video.width / 2) as usize,
                    )
                } else {
                    (planes[0].linesize, planes[1].linesize, planes[2].linesize)
                };
                let data: Vec<&[u8]> = converted.iter().map(Vec::as_slice).collect();
                let Some(view) = YuvPlanes::new(
                    video.width as usize,
                    video.height as usize,
                    data[0],
                    data[1],
                    data[2],
                    y_stride,
                    u_stride,
                ) else {
                    return Err(Error::InvalidData);
                };
                let pts = if frame.pts == i64::MIN {
                    self.next_pts
                } else {
                    frame.pts
                };
                let duration = if frame.duration > 0 {
                    frame.duration
                } else {
                    self.frame_duration
                };
                if self.parallel {
                    let w = video.width as usize;
                    let h = video.height as usize;
                    self.batch.push(YuvFrame {
                        width: w,
                        height: h,
                        y: Self::tight_plane(&converted[0], y_stride, w, h),
                        u: Self::tight_plane(&converted[1], u_stride, w / 2, h / 2),
                        v: Self::tight_plane(&converted[2], v_stride, w / 2, h / 2),
                    });
                    self.batch_meta.push((pts, duration));
                    self.next_pts = pts + duration;
                    if self.batch.len() >= PARALLEL_BATCH {
                        self.encode_batch()?;
                    }
                    return Ok(());
                }
                let stream = self
                    .encoder
                    .encode_planes(&view)
                    .map_err(|e| Error::Other(e.to_string()))?;
                self.packetize(&stream, pts, duration);
                self.next_pts = pts + duration;
                Ok(())
            }
            None => {
                if !self.draining {
                    self.draining = true;
                    self.encode_batch()?;
                    let stream = self.encoder.flush();
                    self.packetize(&stream, self.next_pts, self.frame_duration);
                }
                self.ended = true;
                Ok(())
            }
        }
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.pending.pop_front() {
            return Ok(pkt);
        }
        if self.ended {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.pending.clear();
        self.ended = false;
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

struct RustyH264EncoderFactory;

impl EncoderFactory for RustyH264EncoderFactory {
    fn descriptor(&self) -> &EncoderDescriptor {
        static DESC: EncoderDescriptor = EncoderDescriptor {
            codec_id: CodecId::H264,
            name: "h264_rusty",
            long_name: "H.264 Video [pure-Rust rusty_h264 encoder]",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::empty(),
            properties: CodecProperties::LOSSY,
            supported_sample_formats: &[],
            supported_pixel_formats: &[WPixel::Yuv420p, WPixel::Yuv420p10le],
            priority: 50,
        };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn Encoder>> {
        Ok(Box::new(RustyH264Encoder::new(&builder)?))
    }
}

inventory::submit!(&RustyH264EncoderFactory as &dyn EncoderFactory);

/// Open an encoder and return `(encoder, avcC extradata)`. Useful for callers
/// that must set `CodecParameters::extradata` themselves (tests, custom muxers).
pub fn create_h264_encoder(builder: &EncoderBuilder) -> Result<(Box<dyn Encoder>, Vec<u8>)> {
    let enc = RustyH264Encoder::new(builder)?;
    let extradata = enc.extradata().unwrap_or_default();
    Ok((Box::new(enc), extradata))
}
