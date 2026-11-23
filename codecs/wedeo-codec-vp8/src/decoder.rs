// VP8 decoder: orchestration, `Decoder` trait implementation and registry.
//
// Translated from FFmpeg libavcodec/vp8.c (vp78_decode_frame,
// decode_mb_row_no_filter, intra_predict, inter_predict, idct_mb,
// filter_mb_row, reference-frame bookkeeping).
// LGPL-2.1-or-later, same as FFmpeg.

use std::collections::VecDeque;

use tracing::debug;
use wedeo_codec::decoder::{CodecParameters, Decoder, DecoderDescriptor};
use wedeo_codec::descriptor::{CodecCapabilities, CodecDescriptor, CodecProperties};
use wedeo_codec::registry::DecoderFactory;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::Frame;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;

use crate::bool_decoder::BoolDecoder;
use crate::coeff::{CoeffState, MbState, decode_mb_coeffs};
use crate::header::{HeaderInfo, ParsedHeader, decode_frame_header};
use crate::idct;
use crate::intra_pred::{self, gather_neighbors4};
use crate::loopfilter::{self, MacroblockState};
use crate::mb::decode_mb_mode;
use crate::mc::mc_part;
use crate::recon::{FrameBuffer, References};
use crate::types::*;

const MARGIN: i32 = 64; // (16 << 2) in FFmpeg

/// Native VP8 decoder.
pub struct Vp8Decoder {
    #[allow(dead_code)]
    params: CodecParameters,
    output_queue: VecDeque<Frame>,
    draining: bool,
    codec_descriptor: CodecDescriptor,
    /// Probabilities from the previous frame (for interframe header updates).
    probs: Probabilities,
    /// Loop-filter deltas persist across frames when `update` is not signalled.
    lf_delta: LfDelta,
    refs: References,
    width: u32,
    height: u32,
    frame_count: u64,
}

impl Vp8Decoder {
    pub fn new(params: CodecParameters) -> Result<Self> {
        Ok(Self {
            params,
            output_queue: VecDeque::new(),
            draining: false,
            codec_descriptor: CodecDescriptor {
                id: CodecId::Vp8,
                media_type: MediaType::Video,
                name: "vp8",
                long_name: "VP8 Video",
                properties: CodecProperties::LOSSY,
                profiles: &[],
            },
            probs: crate::header::initial_probabilities(),
            lf_delta: LfDelta::default(),
            refs: References::new(),
            width: 0,
            height: 0,
            frame_count: 0,
        })
    }

    /// Decode one VP8 frame.
    fn decode_frame(&mut self, data: &[u8], pts: i64) -> Result<()> {
        let ParsedHeader {
            info,
            coder,
            coeff_partitions,
        } = decode_frame_header(data, &self.probs, &self.lf_delta, self.width, self.height)?;

        debug!(
            keyframe = info.keyframe,
            width = info.width,
            height = info.height,
            invisible = info.invisible,
            "VP8 frame header decoded"
        );

        self.width = info.width;
        self.height = info.height;

        // Interframes require all three reference buffers.
        if !info.keyframe
            && (self.refs.get(FrameType::Previous.index()).is_none()
                || self.refs.get(FrameType::Golden.index()).is_none()
                || self.refs.get(FrameType::AltRef.index()).is_none())
        {
            return Err(Error::InvalidData);
        }

        let prev_frame = self.refs.get(FrameType::Current.index()).cloned();
        let mut cur = FrameBuffer::new(info.width as usize, info.height as usize);
        cur.is_keyframe = info.keyframe;

        let deblock = info.filter_level != 0;
        let mb_w = info.mb_width;
        let mb_h = info.mb_height;

        // Per-frame predictor state.
        let mut top_nnz = vec![[0u8; 9]; mb_w];
        let mut seg_map = vec![0u8; mb_w * mb_h];
        let mut intra4x4_top = vec![DC_PRED; 4 * mb_w];
        let mut intra4x4_left: [u8; 4];
        let mut left_nnz: [u8; 9];

        // Coefficient partition range decoders.
        let mut coeff_coders: Vec<BoolDecoder<'_>> = Vec::with_capacity(info.num_coeff_partitions);
        for part in &coeff_partitions {
            coeff_coders.push(BoolDecoder::new(part).ok_or(Error::InvalidData)?);
        }

        let mut header_coder = coder;
        let mut mb_grid = vec![MbState::default(); mb_w * mb_h];
        let dummy = MbState::default();

        let mut min_y = -MARGIN;
        let mut max_y = ((mb_h as i32 - 1) << 6) + MARGIN;

        for mb_y in 0..mb_h {
            intra4x4_left = [DC_PRED; 4];
            left_nnz = [0u8; 9];
            let bounds = MvBounds {
                min_x: -MARGIN,
                max_x: ((mb_w as i32 - 1) << 6) + MARGIN,
                min_y,
                max_y,
            };

            for mb_x in 0..mb_w {
                if header_coder.is_end() {
                    return Err(Error::InvalidData);
                }
                let mb_xy = mb_y * mb_w + mb_x;

                let left = if mb_x > 0 {
                    &mb_grid[mb_xy - 1]
                } else {
                    &dummy
                };
                let top = if mb_y > 0 {
                    &mb_grid[mb_xy - mb_w]
                } else {
                    &dummy
                };
                let topleft = if mb_x > 0 && mb_y > 0 {
                    &mb_grid[mb_xy - mb_w - 1]
                } else {
                    &dummy
                };

                let mut mb = MbState::default();
                decode_mb_mode(
                    &mut header_coder,
                    &info,
                    &mut mb,
                    mb_x,
                    mb_y,
                    prev_frame.as_ref().map(|f| f.seg_map.as_slice()),
                    &mut seg_map,
                    left,
                    top,
                    topleft,
                    &bounds,
                    &mut intra4x4_top,
                    &mut intra4x4_left,
                );

                let mut coeffs = CoeffState::default();
                if !mb.skip {
                    let part = mb_y & (info.num_coeff_partitions - 1);
                    if coeff_coders[part].is_end() {
                        return Err(Error::InvalidData);
                    }
                    decode_mb_coeffs(
                        &mut coeff_coders[part],
                        &info,
                        &mut mb,
                        &mut coeffs,
                        &mut top_nnz,
                        &mut left_nnz,
                        mb_x,
                    );
                } else {
                    // FFmpeg's skip path (vp8.c decode_mb_row_no_filter):
                    // AV_ZERO64(left_nnz) / AV_WN64(top_nnz[mb_x], 0) clear
                    // only indices 0..8. Index 8 (the separate luma DC block
                    // predictor) is reset only if this MB would have had a DC
                    // block, i.e. not I4x4 and not SPLIT. Clearing it
                    // unconditionally shifts the DC token context of the next
                    // MB and desynchronises the coefficient bitstream.
                    left_nnz[0..8].fill(0);
                    top_nnz[mb_x][0..8].fill(0);
                    if mb.mode != MODE_I4x4 && mb.mode != MVMODE_SPLIT {
                        left_nnz[8] = 0;
                        top_nnz[mb_x][8] = 0;
                    }
                }

                let by = cur.base_y();
                let buv = cur.base_uv();
                if mb.mode <= MODE_I4x4 {
                    predict_intra(&info, &mb, &coeffs, &mut cur, mb_x, mb_y, by, buv);
                } else {
                    predict_inter(&info, &mb, &self.refs, &mut cur, mb_x, mb_y, by, buv);
                }
                if !mb.skip {
                    idct_mb(&mb, &coeffs, &mut cur, mb_x, mb_y, by, buv);
                }

                mb_grid[mb_xy] = mb;
            }
            min_y -= 64;
            max_y -= 64;
        }

        // Loop filter (after all reconstruction, in raster order).
        if deblock {
            for mb_y in 0..mb_h {
                for mb_x in 0..mb_w {
                    let mb = &mb_grid[mb_y * mb_w + mb_x];
                    let f = loopfilter::filter_level_for_mb(
                        &info,
                        &MacroblockState {
                            skip: mb.skip,
                            mode: mb.mode,
                            ref_frame: mb.ref_frame,
                            segment: mb.segment,
                        },
                    );
                    let cur_base_y = cur.base_y();
                    let cur_base_uv = cur.base_uv();
                    loopfilter::filter_mb(
                        &mut cur.y,
                        &mut cur.u,
                        &mut cur.v,
                        cur.stride_y,
                        cur.stride_uv,
                        cur_base_y,
                        cur_base_uv,
                        mb_x,
                        mb_y,
                        f,
                        info.filter_simple,
                        info.keyframe,
                    );
                }
            }
        }

        cur.seg_map = seg_map;

        // Reference-frame bookkeeping (mirrors vp78_decode_frame).
        let next_altref = match info.update_altref {
            FRAME_NONE => self.refs.slots[FrameType::AltRef.index()].clone(),
            idx if idx == FrameType::Current as i32 => Some(cur.clone()),
            idx => self.refs.slots[idx as usize].clone(),
        };
        let next_golden = match info.update_golden {
            FRAME_NONE => self.refs.slots[FrameType::Golden.index()].clone(),
            idx if idx == FrameType::Current as i32 => Some(cur.clone()),
            idx => self.refs.slots[idx as usize].clone(),
        };
        let next_previous = if info.update_last {
            Some(cur.clone())
        } else {
            self.refs.slots[FrameType::Previous.index()].clone()
        };
        self.refs.slots[FrameType::Current.index()] = Some(cur.clone());
        self.refs.slots[FrameType::Previous.index()] = next_previous;
        self.refs.slots[FrameType::Golden.index()] = next_golden;
        self.refs.slots[FrameType::AltRef.index()] = next_altref;

        // Probability persistence.
        if info.update_probabilities {
            self.probs = info.probs.clone();
        }
        self.lf_delta = info.lf_delta;

        if !info.invisible {
            let frame = cur.to_frame(pts, self.params.time_base);
            self.output_queue.push_back(frame);
        }
        Ok(())
    }
}

/// Predict one intra macroblock (`intra_predict` from vp8.c).
#[allow(clippy::too_many_arguments)]
fn predict_intra(
    info: &HeaderInfo,
    mb: &MbState,
    coeffs: &CoeffState,
    cur: &mut FrameBuffer,
    mb_x: usize,
    mb_y: usize,
    by: usize,
    buv: usize,
) {
    let y_stride = cur.stride_y;
    let uv_stride = cur.stride_uv;

    if mb.mode < MODE_I4x4 {
        let mode = intra_pred::check_intra_pred8x8(mb.mode, mb_x, mb_y);
        intra_pred::pred_block(
            &mut cur.y,
            by + mb_y * 16 * y_stride + mb_x * 16,
            y_stride,
            16,
            mode,
            false,
        );
    } else {
        let y_base = by + mb_y * 16 * y_stride + mb_x * 16;
        for y in 0..4 {
            for x in 0..4 {
                let base = y_base + y * 4 * y_stride + x * 4;
                let n = gather_neighbors4(&cur.y, base, y_stride, x, y, mb_x, mb_y, info.mb_width);
                let mode = intra_pred::check_intra_pred4x4(
                    mb.intra4x4_pred_mode[4 * y + x],
                    mb_x + x,
                    mb_y + y,
                );
                intra_pred::pred4x4(&mut cur.y, base, y_stride, mode, &n);
                let nz = coeffs.nz[y][x];
                if nz == 1 {
                    let mut block = coeffs.blocks[y][x];
                    idct::idct_dc_add(
                        &mut cur.y,
                        base % y_stride,
                        base / y_stride,
                        y_stride,
                        &mut block,
                    );
                } else if nz > 1 {
                    let mut block = coeffs.blocks[y][x];
                    idct::idct_add(
                        &mut cur.y,
                        base % y_stride,
                        base / y_stride,
                        y_stride,
                        &mut block,
                    );
                }
            }
        }
    }

    let cmode = intra_pred::check_intra_pred8x8(mb.chroma_pred_mode, mb_x, mb_y);
    let u_base = buv + mb_y * 8 * uv_stride + mb_x * 8;
    intra_pred::pred_block(&mut cur.u, u_base, uv_stride, 8, cmode, true);
    intra_pred::pred_block(&mut cur.v, u_base, uv_stride, 8, cmode, true);
}

/// Predict one inter macroblock (`inter_predict` / `vp8_mc_part` from vp8.c).
#[allow(clippy::too_many_arguments)]
fn predict_inter(
    info: &HeaderInfo,
    mb: &MbState,
    refs: &References,
    cur: &mut FrameBuffer,
    mb_x: usize,
    mb_y: usize,
    by: usize,
    buv: usize,
) {
    let ref_fb = match refs.get(mb.ref_frame.index()) {
        Some(f) => f,
        None => return,
    };
    let x_off = mb_x << 4;
    let y_off = mb_y << 4;
    let epel = info.profile == 0;

    // Split the plane borrows once so they can be reused for each partition.
    let dy = &mut cur.y[by..];
    let du = &mut cur.u[buv..];
    let dv = &mut cur.v[buv..];
    let ry = &ref_fb.y[ref_fb.base_y()..];
    let ru = &ref_fb.u[ref_fb.base_uv()..];
    let rv = &ref_fb.v[ref_fb.base_uv()..];
    let pw = ref_fb.width;
    let ph = ref_fb.height;

    let mut part = |bx: usize, byoff: usize, bw: usize, bh: usize, mv: Mv| {
        mc_part(
            dy,
            du,
            dv,
            cur.stride_y,
            cur.stride_uv,
            ry,
            ru,
            rv,
            ref_fb.stride_y,
            ref_fb.stride_uv,
            pw,
            ph,
            x_off,
            y_off,
            bx,
            byoff,
            bw,
            bh,
            mv,
            epel,
        );
    };

    match mb.partitioning {
        SPLITMVMODE_4x4 => {
            for y in 0..4 {
                for x in 0..4 {
                    part(4 * x, 4 * y, 4, 4, mb.bmv[4 * y + x]);
                }
            }
            // Chroma MV is the rounded average of the four luma MVs under it.
            for y in 0..2 {
                for x in 0..2 {
                    let mut sx = 0i32;
                    let mut sy = 0i32;
                    for dy4 in 0..2 {
                        for dx4 in 0..2 {
                            let mv = mb.bmv[(2 * y + dy4) * 4 + 2 * x + dx4];
                            sx += mv.x as i32;
                            sy += mv.y as i32;
                        }
                    }
                    let round = |v: i32| -> i32 { (v + 2 + (v >> 31)) >> 2 };
                    let uvmv = Mv::new(round(sx) as i16, round(sy) as i16);
                    let cmx = (uvmv.x as i32 & 7) as usize;
                    let cmy = (uvmv.y as i32 & 7) as usize;
                    let cx = ((x_off >> 1) + 4 * x) as i32 + (uvmv.x as i32 >> 3);
                    let cy = ((y_off >> 1) + 4 * y) as i32 + (uvmv.y as i32 >> 3);
                    let cobase = ((y_off >> 1) + 4 * y) * cur.stride_uv + (x_off >> 1) + 4 * x;
                    crate::mc::mc_block(
                        du,
                        cobase,
                        cur.stride_uv,
                        ru,
                        ref_fb.stride_uv,
                        pw >> 1,
                        ph >> 1,
                        cx,
                        cy,
                        4,
                        4,
                        cmx,
                        cmy,
                        false,
                    );
                    crate::mc::mc_block(
                        dv,
                        cobase,
                        cur.stride_uv,
                        rv,
                        ref_fb.stride_uv,
                        pw >> 1,
                        ph >> 1,
                        cx,
                        cy,
                        4,
                        4,
                        cmx,
                        cmy,
                        false,
                    );
                }
            }
        }
        SPLITMVMODE_16x8 => {
            part(0, 0, 16, 8, mb.bmv[0]);
            part(0, 8, 16, 8, mb.bmv[1]);
        }
        SPLITMVMODE_8x16 => {
            part(0, 0, 8, 16, mb.bmv[0]);
            part(8, 0, 8, 16, mb.bmv[1]);
        }
        SPLITMVMODE_8x8 => {
            part(0, 0, 8, 8, mb.bmv[0]);
            part(8, 0, 8, 8, mb.bmv[1]);
            part(0, 8, 8, 8, mb.bmv[2]);
            part(8, 8, 8, 8, mb.bmv[3]);
        }
        _ => part(0, 0, 16, 16, mb.mv),
    }
}

/// Add residuals for a macroblock (`idct_mb` from vp8.c).
#[allow(clippy::too_many_arguments)]
fn idct_mb(
    mb: &MbState,
    coeffs: &CoeffState,
    cur: &mut FrameBuffer,
    mb_x: usize,
    mb_y: usize,
    by: usize,
    buv: usize,
) {
    let y_stride = cur.stride_y;
    let uv_stride = cur.stride_uv;

    if mb.mode != MODE_I4x4 {
        for y in 0..4 {
            for x in 0..4 {
                let nz = coeffs.nz[y][x];
                if nz == 0 {
                    continue;
                }
                let base = by + (mb_y * 16 + y * 4) * y_stride + mb_x * 16 + x * 4;
                let mut block = coeffs.blocks[y][x];
                if nz == 1 {
                    idct::idct_dc_add(
                        &mut cur.y,
                        base % y_stride,
                        base / y_stride,
                        y_stride,
                        &mut block,
                    );
                } else {
                    idct::idct_add(
                        &mut cur.y,
                        base % y_stride,
                        base / y_stride,
                        y_stride,
                        &mut block,
                    );
                }
            }
        }
    }

    for ch in 0..2 {
        for y in 0..2 {
            for x in 0..2 {
                let nz = coeffs.nz[4 + ch][(y << 1) + x];
                if nz == 0 {
                    continue;
                }
                let base = buv + (mb_y * 8 + y * 4) * uv_stride + mb_x * 8 + x * 4;
                let mut block = coeffs.blocks[4 + ch][(y << 1) + x];
                if nz == 1 {
                    idct::idct_dc_add(
                        cur.u_or_v(ch),
                        base % uv_stride,
                        base / uv_stride,
                        uv_stride,
                        &mut block,
                    );
                } else {
                    idct::idct_add(
                        cur.u_or_v(ch),
                        base % uv_stride,
                        base / uv_stride,
                        uv_stride,
                        &mut block,
                    );
                }
            }
        }
    }
}

impl Decoder for Vp8Decoder {
    fn send_packet(&mut self, packet: Option<&Packet>) -> Result<()> {
        match packet {
            None => {
                self.draining = true;
                Ok(())
            }
            Some(pkt) => {
                let frame_count = self.frame_count;
                self.frame_count += 1;
                match self.decode_frame(pkt.data.data(), pkt.pts) {
                    Ok(()) => Ok(()),
                    Err(Error::PatchwelcomeNotImplemented) => {
                        debug!(frame = frame_count, "VP8: skipping unsupported frame");
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.output_queue.pop_front() {
            return Ok(frame);
        }
        if self.draining {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.output_queue.clear();
        self.draining = false;
        self.refs.clear();
        self.probs = crate::header::initial_probabilities();
        self.lf_delta = LfDelta::default();
        self.width = 0;
        self.height = 0;
    }

    fn descriptor(&self) -> &CodecDescriptor {
        &self.codec_descriptor
    }
}

struct Vp8DecoderFactory;

impl DecoderFactory for Vp8DecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Vp8,
            name: "vp8",
            long_name: "VP8 Video",
            media_type: MediaType::Video,
            capabilities: CodecCapabilities::DR1,
            properties: CodecProperties::LOSSY,
            priority: 100,
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(Vp8Decoder::new(params)?))
    }
}

inventory::submit!(&Vp8DecoderFactory as &dyn DecoderFactory);
