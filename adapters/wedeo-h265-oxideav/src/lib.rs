#![forbid(unsafe_code)]
//! HEVC decoder and encoder adapters backed by the pure-Rust `oxideav-h265`
//! crate (MIT). No source from it is copied here: the adapter only converts
//! between wedeo's and oxideav's frame/packet/parameter types.
//!
//! The decoder is registered above wedeo's partial native HEVC port so real
//! streams use the conformance-tested implementation. The encoder is the only
//! HEVC encoder in wedeo.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId as OCodecId, CodecParameters as OParams, Decoder as ODecoder, Encoder as OEncoder,
    Frame as OFrame, Packet as OPacket, PixelFormat as OPixel, VideoFrame as OVideoFrame,
    VideoPlane as OVideoPlane,
};
use tracing::debug;

use wedeo_codec::decoder::{CodecParameters as WParams, Decoder as WDecoder};
use wedeo_codec::descriptor::{CodecCapabilities, CodecProperties};
use wedeo_codec::encoder::{CodecFlags, Encoder as WEncoder, EncoderBuilder};
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
        WPixel::Yuv420p10le => OPixel::Yuv420P10Le,
        WPixel::Yuv422p10le => OPixel::Yuv422P10Le,
        WPixel::Yuv444p10le => OPixel::Yuv444P10Le,
        WPixel::Gray8 => OPixel::Gray8,
        _ => OPixel::Yuv420P,
    }
}

fn to_wedeo_pixel(pf: OPixel) -> WPixel {
    match pf {
        OPixel::Yuv420P => WPixel::Yuv420p,
        OPixel::Yuv422P => WPixel::Yuv422p,
        OPixel::Yuv444P => WPixel::Yuv444p,
        OPixel::Yuv420P10Le => WPixel::Yuv420p10le,
        OPixel::Yuv422P10Le => WPixel::Yuv422p10le,
        OPixel::Yuv444P10Le => WPixel::Yuv444p10le,
        OPixel::Gray8 => WPixel::Gray8,
        _ => WPixel::Yuv420p,
    }
}

fn wedeo_params(p: &WParams) -> OParams {
    let mut out = OParams::video(OCodecId::new("h265"));
    out.width = Some(p.width);
    out.height = Some(p.height);
    out.pixel_format = Some(to_oxideav_pixel(p.pixel_format));
    out.extradata = p.extradata.clone();
    if p.bit_rate > 0 {
        out.bit_rate = Some(p.bit_rate as u64);
    }
    for (k, v) in p.options.iter() {
        out.options.insert(k, v);
    }
    out
}

fn time_base(tb: Rational) -> oxideav_core::TimeBase {
    oxideav_core::TimeBase::new(tb.num.max(1) as i64, tb.den.max(1) as i64)
}

/// Split an Annex B byte stream into NAL units (start codes removed, trailing
/// zero bytes trimmed). Three- and four-byte start codes are both accepted.
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = i + 3;
            let mut end = data.len();
            let mut j = start;
            while j + 3 <= data.len() {
                if data[j] == 0 && data[j + 1] == 0 && data[j + 2] == 1 {
                    end = j;
                    break;
                }
                j += 1;
            }
            while end > start && data[end - 1] == 0 {
                end -= 1;
            }
            if end > start {
                nals.push(&data[start..end]);
            }
            i = end.max(i + 3);
        } else {
            i += 1;
        }
    }
    nals
}

/// Convert an Annex B access unit into length-prefixed NAL units (4-byte
/// big-endian lengths), the sample format MP4/MOV expects.
fn annexb_to_length_prefixed(data: &[u8]) -> Vec<u8> {
    let nals = split_annex_b(data);
    if nals.is_empty() {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + nals.len() * 4);
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Build an ISO/IEC 14496-15 HEVCDecoderConfigurationRecord from the VPS, SPS
/// and PPS of an Annex B access unit. Profile/tier/level bytes are copied
/// verbatim from the SPS: after its 2-byte NAL header come the
/// profile_tier_level fields (1 + 4 + 6 + 1 bytes).
fn build_hvcc(nals: &[&[u8]]) -> Result<Vec<u8>> {
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for &nal in nals {
        if nal.len() < 2 {
            continue;
        }
        match (nal[0] >> 1) & 0x3f {
            32 => vps = Some(nal),
            33 => sps = Some(nal),
            34 => pps = Some(nal),
            _ => {}
        }
    }
    let (Some(vps), Some(sps), Some(pps)) = (vps, sps, pps) else {
        debug!(
            nals = nals.len(),
            vps = vps.is_some(),
            sps = sps.is_some(),
            pps = pps.is_some(),
            "hvcC probe: missing parameter sets"
        );
        return Err(Error::InvalidData);
    };

    if sps.len() < 14 {
        return Err(Error::InvalidData);
    }
    let mut out = Vec::with_capacity(23 + vps.len() + sps.len() + pps.len() + 18);
    out.push(1); // configurationVersion
    out.push(sps[2]); // general_profile_space / tier / profile_idc
    out.extend_from_slice(&sps[3..7]); // general_profile_compatibility_flags
    out.extend_from_slice(&sps[7..13]); // general_constraint_indicator_flags
    out.push(sps[13]); // general_level_idc
    out.extend_from_slice(&[0xff, 0xff]); // reserved + min_spatial_segmentation_idc = 0
    out.push(0xfc); // reserved + parallelismType = unknown
    out.push(0xfd); // reserved + chromaFormat = 1 (4:2:0)
    out.push(0xf8); // reserved + bitDepthLumaMinus8 = 0
    out.push(0xf8); // reserved + bitDepthChromaMinus8 = 0
    out.extend_from_slice(&[0, 0]); // avgFrameRate = unspecified
    out.push(3); // reserved + numTemporalLayers + temporalIdNested + lengthSizeMinusOne = 3
    out.push(3); // numOfArrays: VPS, SPS, PPS
    for (ty, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        out.push(0x80 | ty); // array_completeness = 1
        out.extend_from_slice(&1u16.to_be_bytes()); // numNalus
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// Produce the parameter sets needed for hvcC without consuming the real
/// encoder's state: encode one neutral grey frame on a throwaway instance.
/// oxideav-h265 emits VPS/SPS/PPS in band with every IDR and exposes no
/// standalone parameter-set API, so this is the only way to have them before
/// the muxer writes its header.
fn probe_hvcc(p: &OParams, width: u32, height: u32) -> Result<Vec<u8>> {
    let mut probe = oxideav_h265::encoder::make_encoder(p).map_err(to_w_error)?;
    let w = width.max(1) as usize;
    let h = height.max(1) as usize;
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let frame = OFrame::Video(OVideoFrame {
        pts: Some(0),
        planes: vec![
            OVideoPlane {
                stride: w,
                data: vec![128; w * h],
            },
            OVideoPlane {
                stride: cw,
                data: vec![128; cw * ch],
            },
            OVideoPlane {
                stride: cw,
                data: vec![128; cw * ch],
            },
        ],
    });
    probe.send_frame(&frame).map_err(to_w_error)?;
    let mut flushed = false;
    for i in 0..64 {
        match probe.receive_packet() {
            Ok(pkt) => return build_hvcc(&split_annex_b(&pkt.data)),
            Err(oxideav_core::Error::NeedMore) => {
                if !flushed {
                    probe.flush().map_err(to_w_error)?;
                    flushed = true;
                }
            }
            Err(oxideav_core::Error::Eof) => {
                debug!(i, flushed, "hvcC probe: encoder drained without a packet");
                break;
            }
            Err(e) => return Err(to_w_error(e)),
        }
    }
    Err(Error::InvalidData)
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

pub struct OxideavHevcDecoder {
    inner: Box<dyn ODecoder>,
    width: u32,
    height: u32,
    pixel_format: WPixel,
    draining: bool,
}

impl OxideavHevcDecoder {
    fn new(params: WParams) -> Result<Self> {
        let width = params.width;
        let height = params.height;
        let pixel_format = params.pixel_format;
        let p = wedeo_params(&params);
        let inner = oxideav_h265::decoder::make_decoder(&p).map_err(to_w_error)?;
        Ok(Self {
            inner,
            width,
            height,
            pixel_format,
            draining: false,
        })
    }

    fn convert_frame(&mut self, frame: OFrame) -> Result<WFrame> {
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
            let planes: Vec<OVideoPlane> = v
                .planes
                .into_iter()
                .filter(|p| p.stride > 0 && p.stride < (isize::MAX as usize))
                .collect();
            video.planes = planes
                .into_iter()
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

impl WDecoder for OxideavHevcDecoder {
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
                return self.convert_frame(frame);
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
                id: CodecId::Hevc,
                media_type: MediaType::Video,
                name: "hevc_oxideav",
                long_name: "HEVC Video [pure-Rust oxideav-h265]",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            };
        &DESC
    }
}

struct OxideavHevcDecoderFactory;

impl DecoderFactory for OxideavHevcDecoderFactory {
    fn descriptor(&self) -> &wedeo_codec::decoder::DecoderDescriptor {
        static DESC: wedeo_codec::decoder::DecoderDescriptor =
            wedeo_codec::decoder::DecoderDescriptor {
                codec_id: CodecId::Hevc,
                name: "hevc_oxideav",
                long_name: "HEVC Video [pure-Rust oxideav-h265]",
                media_type: MediaType::Video,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                priority: 110, // beat the partial native decoder (100)
            };
        &DESC
    }

    fn create(&self, params: WParams) -> Result<Box<dyn WDecoder>> {
        Ok(Box::new(OxideavHevcDecoder::new(params)?))
    }
}

inventory::submit!(&OxideavHevcDecoderFactory as &dyn DecoderFactory);

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

pub struct OxideavHevcEncoder {
    inner: Box<dyn OEncoder>,
    pending: VecDeque<WPacket>,
    extradata: Vec<u8>,
    /// FFmpeg's AV_CODEC_FLAG_GLOBAL_HEADER: the muxer needs configuration
    /// bytes and length-prefixed samples instead of in-band Annex B.
    global_header: bool,
    draining: bool,
}

impl OxideavHevcEncoder {
    fn new(builder: &EncoderBuilder) -> Result<Self> {
        if builder.pixel_format != WPixel::Yuv420p {
            return Err(Error::PatchwelcomeNotImplemented);
        }
        let mut p = OParams::video(OCodecId::new("h265"));
        p.width = Some(builder.width);
        p.height = Some(builder.height);
        p.pixel_format = Some(to_oxideav_pixel(builder.pixel_format));
        p.bit_rate = if builder.bit_rate > 0 {
            Some(builder.bit_rate as u64)
        } else {
            None
        };
        let mode = builder
            .options
            .get("mode")
            .map(str::to_string)
            .unwrap_or_else(|| "inter".to_string());
        p.options.insert("mode", &mode);
        if let Some(qp) = builder.options.get("qp") {
            p.options.insert("qp", qp);
        }
        if let Some(gop) = builder.options.get("gop") {
            p.options.insert("gop", gop);
        }
        if let Some(preset) = builder.options.get("preset") {
            p.options.insert("preset", preset);
        }
        let global_header = builder.flags.contains(CodecFlags::GLOBAL_HEADER);
        let inner = oxideav_h265::encoder::make_encoder(&p).map_err(to_w_error)?;
        let extradata = if global_header {
            probe_hvcc(&p, builder.width, builder.height)?
        } else {
            Vec::new()
        };
        debug!(width = builder.width, height = builder.height, %mode, global_header, extradata_len = extradata.len(), "oxideav HEVC encoder opened");
        Ok(Self {
            inner,
            pending: VecDeque::new(),
            extradata,
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
        let data = if self.global_header {
            annexb_to_length_prefixed(&pkt.data)
        } else {
            pkt.data.clone()
        };
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

impl WEncoder for OxideavHevcEncoder {
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
                let mut vf = OVideoFrame {
                    pts: if frame.pts == NOPTS {
                        None
                    } else {
                        Some(frame.pts)
                    },
                    planes,
                };
                // Drop side-channel entries (they are not image planes).
                vf.planes
                    .retain(|p| p.stride > 0 && p.stride < (isize::MAX as usize));
                let of = OFrame::Video(vf);
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

struct OxideavHevcEncoderFactory;

impl EncoderFactory for OxideavHevcEncoderFactory {
    fn descriptor(&self) -> &wedeo_codec::encoder::EncoderDescriptor {
        static DESC: wedeo_codec::encoder::EncoderDescriptor =
            wedeo_codec::encoder::EncoderDescriptor {
                codec_id: CodecId::Hevc,
                name: "hevc_oxideav",
                long_name: "HEVC Video [pure-Rust oxideav-h265 encoder]",
                media_type: MediaType::Video,
                capabilities: CodecCapabilities::empty(),
                properties: CodecProperties::LOSSY,
                supported_sample_formats: &[],
                supported_pixel_formats: &[WPixel::Yuv420p],
                priority: 50,
            };
        &DESC
    }

    fn create(&self, builder: EncoderBuilder) -> Result<Box<dyn WEncoder>> {
        Ok(Box::new(OxideavHevcEncoder::new(&builder)?))
    }
}

inventory::submit!(&OxideavHevcEncoderFactory as &dyn EncoderFactory);
