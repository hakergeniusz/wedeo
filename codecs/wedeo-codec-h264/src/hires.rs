#![deny(unsafe_op_in_unsafe_fn)]
#![allow(unsafe_code)] // verified FFmpeg-derived SIMD/FFI/tile-threading kernels
//! High bit-depth (9..14) intra reconstruction.
//!
//! The 8-bit kernels in `mb.rs` write into the `u8` picture planes. FFmpeg
//! compiles the same reconstruction templates for `uint16_t` samples when
//! `BIT_DEPTH > 8`; this module mirrors that path for the `u16` planes of
//! `PictureBuffer`. Luma/chroma prediction and IDCT are shared with the 8-bit
//! path via the `Pixel` trait.
//!
//! Reference: FFmpeg `h264_mb_template.c` (`hl_decode_mb`), `h264pred_template.c`,
//! `h264idct_template.c`.

use crate::cavlc::Macroblock;
use crate::deblock::PictureBuffer;
use crate::dequant;
use crate::idct;
use crate::intra_pred;
use crate::mb::{
    BLOCK_8X8_OFFSET, BLOCK_INDEX_TO_XY, FrameDecodeContext, block_to_raster, luma_block_offset,
    luma_mb_offset,
};
use crate::mc;
use crate::pixel::Pixel;
use crate::shared_picture::RefPic;
use crate::slice::SliceHeader;
use std::sync::Arc;

#[inline]
fn gather_top<P: Pixel>(plane: &[P], offset: usize, stride: usize) -> [P; 16] {
    let mut top = [P::from_i32(P::MID); 16];
    if offset >= stride {
        let row = offset - stride;
        top.copy_from_slice(&plane[row..row + 16]);
    }
    top
}

#[inline]
fn gather_left<P: Pixel>(plane: &[P], offset: usize, stride: usize, has_left: bool) -> [P; 16] {
    let mut left = [P::from_i32(P::MID); 16];
    if has_left {
        for (i, lv) in left.iter_mut().enumerate() {
            *lv = plane[offset + i * stride - 1];
        }
    }
    left
}

#[inline]
fn gather_top_left<P: Pixel>(
    plane: &[P],
    offset: usize,
    stride: usize,
    has_top: bool,
    has_left: bool,
) -> P {
    if has_top && has_left && offset >= stride {
        plane[offset - stride - 1]
    } else {
        P::from_i32(P::MID)
    }
}

#[inline]
fn gather_left8<P: Pixel>(plane: &[P], offset: usize, stride: usize, has_left: bool) -> [P; 8] {
    let mut left = [P::from_i32(P::MID); 8];
    if has_left {
        for (i, lv) in left.iter_mut().enumerate() {
            *lv = plane[offset + i * stride - 1];
        }
    }
    left
}

/// Chroma top neighbor: 8 samples (chroma blocks are 8 pixels wide).
#[inline]
fn gather_top_chroma<P: Pixel>(plane: &[P], offset: usize, stride: usize) -> [P; 8] {
    let mut top = [P::from_i32(P::MID); 8];
    if offset >= stride {
        let row = offset - stride;
        top.copy_from_slice(&plane[row..row + 8]);
    }
    top
}

/// Chroma left neighbor: 4 rows for 4:2:0, 16 rows for 4:2:2.
fn gather_left_chroma<P: Pixel>(
    plane: &[P],
    offset: usize,
    stride: usize,
    has_left: bool,
    rows: usize,
) -> Vec<P> {
    let mut left = vec![P::from_i32(P::MID); rows];
    if has_left {
        for (i, lv) in left.iter_mut().enumerate() {
            *lv = plane[offset + i * stride - 1];
        }
    }
    left
}

/// Luma 4x4 block neighbor availability. Mirrors `decode_intra4x4` in mb.rs.
fn block_neighbors(
    ctx: &FrameDecodeContext,
    mb_x: u32,
    mb_y: u32,
    blk_x: u32,
    blk_y: u32,
    has_top: bool,
    has_left: bool,
) -> (bool, bool, bool) {
    let block_has_top = blk_y > 0 || has_top;
    let block_has_left = blk_x > 0 || has_left;
    let block_has_top_right = if blk_y > 0 {
        blk_x < 3
            && !(blk_x == 1 && blk_y == 1)
            && !(blk_x == 1 && blk_y == 3)
            && !(blk_x == 3 && blk_y == 1)
            && !(blk_x == 3 && blk_y == 3)
    } else if has_top {
        if blk_x < 3 {
            true
        } else {
            let tr_y_opt = if ctx.is_mbaff && ctx.mb_field {
                if mb_y >= 2 { Some(mb_y - 2) } else { None }
            } else if mb_y > 0 {
                Some(mb_y - 1)
            } else {
                None
            };
            if mb_x + 1 < ctx.mb_width
                && let Some(tr_y) = tr_y_opt
            {
                let tr_idx = (tr_y * ctx.mb_width + mb_x + 1) as usize;
                let decoded = ctx.slice_table[tr_idx] == ctx.current_slice;
                if decoded && ctx.constrained_intra_pred {
                    ctx.mb_info[tr_idx].is_intra
                } else {
                    decoded
                }
            } else {
                false
            }
        }
    } else {
        false
    };
    (block_has_top, block_has_left, block_has_top_right)
}

/// Gather the 4x4 top neighbors (with 4 top-right samples).
fn gather_top4<P: Pixel>(
    plane: &[P],
    offset: usize,
    stride: usize,
    has_top: bool,
    has_top_right: bool,
) -> [P; 8] {
    let mut top = [P::from_i32(P::MID); 8];
    if has_top && offset >= stride {
        let row = offset - stride;
        top[..4].copy_from_slice(&plane[row..row + 4]);
        if has_top_right {
            top[4..8].copy_from_slice(&plane[row + 4..row + 8]);
        } else {
            let last = top[3];
            top[4..8].fill(last);
        }
    }
    top
}

fn gather_left4<P: Pixel>(plane: &[P], offset: usize, stride: usize, has_left: bool) -> [P; 4] {
    let mut left = [P::from_i32(P::MID); 4];
    if has_left {
        for (i, lv) in left.iter_mut().enumerate() {
            *lv = plane[offset + i * stride - 1];
        }
    }
    left
}

/// 4:2:2/4:2:0 chroma geometry for a macroblock in a u16 plane.
///
/// In field mode the chroma stride is doubled and a bottom field starts one
/// original chroma line in, matching FFmpeg `hl_decode_mb`'s dest adjustment.
fn chroma_offset(pic: &PictureBuffer, mb_x: u32, mb_y: u32, mb_field: bool) -> (usize, usize) {
    let rows = if pic.chroma_format_idc == 2 { 16 } else { 8 };
    if mb_field {
        let pair_base = (mb_y & !1) as usize * rows;
        let field_offset = if mb_y & 1 == 1 { pic.uv_stride } else { 0 };
        (
            pair_base * pic.uv_stride + field_offset + mb_x as usize * 8,
            pic.uv_stride * 2,
        )
    } else {
        (
            mb_y as usize * rows * pic.uv_stride + mb_x as usize * 8,
            pic.uv_stride,
        )
    }
}

fn chroma_block_offset(
    pic: &PictureBuffer,
    mb_x: u32,
    mb_y: u32,
    blk_x: u32,
    blk_y: u32,
    mb_field: bool,
) -> (usize, usize) {
    let (base, stride) = chroma_offset(pic, mb_x, mb_y, mb_field);
    (
        base + blk_y as usize * 4 * stride + blk_x as usize * 4,
        stride,
    )
}

/// Destination chroma offset for the 2x2 (4:2:0) or 2x4 (4:2:2) chroma
/// block belonging to one 4x4 luma block, with field-mode stride.
fn inter_chroma_block_offset(
    pic: &PictureBuffer,
    mb_x: u32,
    mb_y: u32,
    blk_x: u32,
    blk_y: u32,
    mb_field: bool,
) -> (usize, usize) {
    let y_in_mb = if pic.chroma_format_idc == 2 {
        blk_y * 4
    } else {
        blk_y * 2
    };
    let (base, stride) = chroma_offset(pic, mb_x, mb_y, mb_field);
    (
        base + y_in_mb as usize * stride + blk_x as usize * 2,
        stride,
    )
}

#[allow(clippy::too_many_arguments)] // mirrors FFmpeg hl_decode_mb parameter set
fn decode_chroma_hires(
    ctx: &mut FrameDecodeContext,
    mb: &mut Macroblock,
    mb_x: u32,
    mb_y: u32,
    chroma_qp: [u8; 2],
    has_top: bool,
    has_left: bool,
) {
    let chroma422 = ctx.pic.chroma_format_idc == 2;
    let max_qp = 51 + ctx.pic.qp_bd_offset();
    let c_qps = chroma_qp.map(|qp| {
        if chroma422 {
            (qp as i32 + 3).min(max_qp) as u8
        } else {
            qp
        }
    });
    let cbp_chroma = (mb.cbp >> 4) & 3;

    for (plane_idx, &c_qp) in c_qps.iter().enumerate() {
        let (offset, stride) = chroma_offset(&ctx.pic, mb_x, mb_y, ctx.mb_field);
        let plane = if plane_idx == 0 {
            &mut ctx.pic.u16
        } else {
            &mut ctx.pic.v16
        };
        let top = gather_top_chroma::<u16>(plane, offset, stride);
        if chroma422 {
            let left = gather_left_chroma::<u16>(plane, offset, stride, has_left, 16);
            let top_left = gather_top_left::<u16>(plane, offset, stride, has_top, has_left);
            intra_pred::predict_chroma_8x16(
                &mut plane[offset..],
                stride,
                mb.chroma_pred_mode,
                &top,
                &left,
                top_left,
                has_top,
                has_left,
            );
        } else {
            let left = gather_left_chroma::<u16>(plane, offset, stride, has_left, 8);
            let top_left = gather_top_left::<u16>(plane, offset, stride, has_top, has_left);
            intra_pred::predict_chroma_8x8(
                &mut plane[offset..],
                stride,
                mb.chroma_pred_mode,
                &top,
                &left,
                top_left,
                has_top,
                has_left,
            );
        }

        if cbp_chroma > 0 {
            let dc_cqm = 1 + plane_idx;
            let qmul = dequant::dc_dequant_scale(&ctx.dequant4, dc_cqm, c_qp);
            let mut chroma_dc_out = [0i32; 8];
            let num_blocks = if chroma422 { 8 } else { 4 };
            if chroma422 {
                idct::chroma422_dc_dequant_idct(&mut chroma_dc_out, &mb.chroma_dc[plane_idx], qmul);
            } else {
                let dc_in: [i16; 4] = mb.chroma_dc[plane_idx][..4].try_into().unwrap();
                let mut dc420 = [0i32; 4];
                idct::chroma_dc_dequant_idct(&mut dc420, &dc_in, qmul);
                chroma_dc_out[..4].copy_from_slice(&dc420);
            }
            for (blk_idx, &dc_val) in chroma_dc_out.iter().enumerate().take(num_blocks) {
                let blk_x = (blk_idx & 1) as u32;
                let blk_y = (blk_idx >> 1) as u32;
                let (c_offset, c_stride) =
                    chroma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y, ctx.mb_field);
                let plane = if plane_idx == 0 {
                    &mut ctx.pic.u16
                } else {
                    &mut ctx.pic.v16
                };
                if cbp_chroma >= 2 {
                    mb.chroma_ac[plane_idx][blk_idx][0] = dc_val as i16;
                    // FFmpeg h264_cavlc.c:1138 uses plain chroma_qp for AC
                    // dequant; the 4:2:2 `+3` applies only to the DC Hadamard.
                    dequant::dequant_4x4(
                        &mut mb.chroma_ac[plane_idx][blk_idx],
                        &ctx.dequant4.coeffs[dc_cqm][chroma_qp[plane_idx] as usize],
                    );
                    mb.chroma_ac[plane_idx][blk_idx][0] = dc_val as i16;
                    idct::idct4x4_add(
                        &mut plane[c_offset..],
                        c_stride,
                        &mut mb.chroma_ac[plane_idx][blk_idx],
                    );
                } else {
                    let dc_add = (dc_val + 32) >> 6;
                    for j in 0..4 {
                        for i in 0..4 {
                            let idx = c_offset + j * c_stride + i;
                            plane[idx] = u16::from_i32(plane[idx].to_i32() + dc_add);
                        }
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // mirrors FFmpeg hl_decode_mb parameter set
fn decode_intra4x4_hires(
    ctx: &mut FrameDecodeContext,
    mb: &mut Macroblock,
    mb_x: u32,
    mb_y: u32,
    qp: u8,
    chroma_qp: [u8; 2],
    has_top: bool,
    has_left: bool,
) {
    let cbp_luma = mb.cbp & 0x0F;
    for (block, &(blk_x, blk_y)) in BLOCK_INDEX_TO_XY.iter().enumerate() {
        let (offset, stride) = luma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y, ctx.mb_field);
        let (bt, bl, btr) = block_neighbors(ctx, mb_x, mb_y, blk_x, blk_y, has_top, has_left);
        let top = gather_top4(&ctx.pic.y16, offset, stride, bt, btr);
        let left = gather_left4(&ctx.pic.y16, offset, stride, bl);
        let top_left = gather_top_left(&ctx.pic.y16, offset, stride, bt, bl);
        let mode = mb.intra4x4_pred_mode[block_to_raster(block)];
        intra_pred::predict_4x4(
            &mut ctx.pic.y16[offset..],
            stride,
            mode,
            &top,
            &left,
            top_left,
            bt,
            bl,
            btr,
        );
        let group_8x8 = (blk_y / 2) * 2 + (blk_x / 2);
        if cbp_luma & (1 << group_8x8) != 0 {
            let raster_idx = block_to_raster(block);
            // FFmpeg keeps dequantized coefficients in `int32_t` at high bit
            // depth (h264dec.h `sl->mb` reserves twice the space). Wedeo's
            // i16 coefficient storage wraps values such as DC 34496 at QP 33,
            // so dequantize into an i32 scratch block for the u16 plane.
            let mut coeffs = [0i32; 16];
            dequant::dequant_4x4_wide(
                &mut coeffs,
                &mb.luma_coeffs[raster_idx],
                &ctx.dequant4.coeffs[0][qp as usize],
            );
            idct::idct4x4_add_wide(&mut ctx.pic.y16[offset..], stride, &mut coeffs);
        }
    }
    decode_chroma_hires(ctx, mb, mb_x, mb_y, chroma_qp, has_top, has_left);
}

#[allow(clippy::too_many_arguments)] // mirrors FFmpeg hl_decode_mb parameter set
fn decode_intra8x8_hires(
    ctx: &mut FrameDecodeContext,
    mb: &mut Macroblock,
    mb_x: u32,
    mb_y: u32,
    qp: u8,
    chroma_qp: [u8; 2],
    has_top: bool,
    has_left: bool,
) {
    let cbp_luma = mb.cbp & 0x0F;
    for i8x8 in 0..4usize {
        let (blk_x, blk_y) = crate::mb::BLOCK_8X8_OFFSET[i8x8];
        // BLOCK_8X8_OFFSET holds pixel offsets; luma_block_offset takes 4x4 units.
        let (offset, stride) =
            luma_block_offset(&ctx.pic, mb_x, mb_y, blk_x / 4, blk_y / 4, ctx.mb_field);
        let (bt, bl, btr) =
            block_neighbors(ctx, mb_x, mb_y, blk_x / 4, blk_y / 4, has_top, has_left);
        let top = gather_top::<u16>(&ctx.pic.y16, offset, stride);
        let left = gather_left8::<u16>(&ctx.pic.y16, offset, stride, bl);
        let top_left = gather_top_left::<u16>(&ctx.pic.y16, offset, stride, bt, bl);
        // FFmpeg's `topright_samples_available` mask (0xEEEA) marks the
        // top-right sample unavailable for the bottom-right 8x8 block: its
        // top-right neighbor lies in the next MB, which is not decoded yet.
        // `btr` is derived from the 4x4-block cache; refine it for the 8x8
        // geometry used by `predict_8x8l`.
        let btr8 = if blk_y == 0 {
            btr && (blk_x == 0 || mb_x + 1 < ctx.mb_width)
        } else {
            btr && blk_x == 0
        };
        // 8x8DCT shares one prediction mode across the four 4x4 sub-blocks
        // of each 8x8 block. FFmpeg reads `intra4x4_pred_mode_cache[scan8[i]]`
        // where i is the 4x4 block index of the group's top-left sub-block.
        // Our `intra4x4_pred_mode` array is raster-ordered, so index it with
        // the raster position of that sub-block, not i8x8 * 4 (which points
        // into a different group for i8x8 >= 1).
        let mode_raster = (blk_y / 4 * 4 + blk_x / 4) as usize;
        let mode = mb.intra4x4_pred_mode[mode_raster];
        intra_pred::predict_8x8l(
            &mut ctx.pic.y16[offset..],
            stride,
            mode,
            &top,
            &left,
            top_left,
            bt,
            bl,
            bt && bl,
            btr8,
        );
        if cbp_luma & (1 << i8x8) != 0 {
            dequant::dequant_8x8(
                &mut mb.luma_8x8_coeffs[i8x8],
                &ctx.dequant8.coeffs[0][qp as usize],
            );
            idct::idct8x8_add(
                &mut ctx.pic.y16[offset..],
                stride,
                &mut mb.luma_8x8_coeffs[i8x8],
            );
        }
    }
    decode_chroma_hires(ctx, mb, mb_x, mb_y, chroma_qp, has_top, has_left);
}

#[allow(clippy::too_many_arguments)] // mirrors FFmpeg hl_decode_mb parameter set
fn decode_intra16x16_hires(
    ctx: &mut FrameDecodeContext,
    mb: &mut Macroblock,
    mb_x: u32,
    mb_y: u32,
    qp: u8,
    chroma_qp: [u8; 2],
    has_top: bool,
    has_left: bool,
) {
    let (offset, stride) = luma_mb_offset(&ctx.pic, mb_x, mb_y, ctx.mb_field);
    let top = gather_top::<u16>(&ctx.pic.y16, offset, stride);
    let left = gather_left::<u16>(&ctx.pic.y16, offset, stride, has_left);
    let top_left = gather_top_left::<u16>(&ctx.pic.y16, offset, stride, has_top, has_left);
    intra_pred::predict_16x16(
        &mut ctx.pic.y16[offset..],
        stride,
        mb.intra16x16_mode,
        &top,
        &left,
        top_left,
        has_top,
        has_left,
    );

    let qmul = dequant::dc_dequant_scale(&ctx.dequant4, 0, qp);
    let mut luma_dc_out = [0i32; 16];
    idct::luma_dc_dequant_idct(&mut luma_dc_out, &mb.luma_dc, qmul);

    let cbp_luma = mb.cbp & 0x0F;
    for (block, &(blk_x, blk_y)) in BLOCK_INDEX_TO_XY.iter().enumerate() {
        let raster_idx = block_to_raster(block);
        let group_8x8 = (blk_y / 2) * 2 + (blk_x / 2);
        let (off, stride) = luma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y, ctx.mb_field);
        if cbp_luma & (1 << group_8x8) != 0 {
            mb.luma_coeffs[raster_idx][0] = luma_dc_out[block] as i16;
            dequant::dequant_4x4(
                &mut mb.luma_coeffs[raster_idx],
                &ctx.dequant4.coeffs[0][qp as usize],
            );
            mb.luma_coeffs[raster_idx][0] = luma_dc_out[block] as i16;
            idct::idct4x4_add(
                &mut ctx.pic.y16[off..],
                stride,
                &mut mb.luma_coeffs[raster_idx],
            );
        } else {
            let dc_add = (luma_dc_out[block] + 32) >> 6;
            for j in 0..4 {
                for i in 0..4 {
                    let idx = off + j * stride + i;
                    ctx.pic.y16[idx] = u16::from_i32(ctx.pic.y16[idx].to_i32() + dc_add);
                }
            }
        }
    }
    decode_chroma_hires(ctx, mb, mb_x, mb_y, chroma_qp, has_top, has_left);
}

/// Reconstruct an intra macroblock into the 10-bit picture planes.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg hl_decode_mb parameter set
pub fn decode_intra_mb_hires(
    ctx: &mut FrameDecodeContext,
    mb: &mut Macroblock,
    mb_x: u32,
    mb_y: u32,
    qp: u8,
    chroma_qp: [u8; 2],
    has_top: bool,
    has_left: bool,
) {
    if mb.transform_size_8x8_flag && mb.is_intra4x4 {
        decode_intra8x8_hires(ctx, mb, mb_x, mb_y, qp, chroma_qp, has_top, has_left);
    } else if mb.is_intra4x4 {
        decode_intra4x4_hires(ctx, mb, mb_x, mb_y, qp, chroma_qp, has_top, has_left);
    } else if mb.is_intra16x16 {
        decode_intra16x16_hires(ctx, mb, mb_x, mb_y, qp, chroma_qp, has_top, has_left);
    } else {
        decode_pcm_mb_hires(ctx, mb, mb_x, mb_y);
    }
}

/// Reconstruct a high-depth I_PCM macroblock into the `u16` planes.
///
/// FFmpeg writes the raw 16-bit samples straight into the picture planes for
/// I_PCM (no prediction / transform). The 8-bit path in `mb.rs` casts the
/// samples down to `u8`; at BIT_DEPTH > 8 we must preserve the full value.
pub fn decode_pcm_mb_hires(
    ctx: &mut FrameDecodeContext,
    mb: &mut Macroblock,
    mb_x: u32,
    mb_y: u32,
) {
    let (luma_base, luma_stride) = luma_mb_offset(&ctx.pic, mb_x, mb_y, ctx.mb_field);
    for y in 0..16usize {
        for x in 0..16usize {
            let blk = (y / 4) * 4 + (x / 4);
            ctx.pic.y16[luma_base + y * luma_stride + x] =
                mb.luma_coeffs[blk][(y % 4) * 4 + (x % 4)] as u16;
        }
    }

    if !ctx.decode_chroma {
        return;
    }
    let rows = if ctx.pic.chroma_format_idc == 2 {
        16
    } else {
        8
    };
    let (base, uv_stride) = chroma_offset(&ctx.pic, mb_x, mb_y, ctx.mb_field);
    for plane_idx in 0..2usize {
        let plane = if plane_idx == 0 {
            &mut ctx.pic.u16
        } else {
            &mut ctx.pic.v16
        };
        for y in 0..rows {
            for x in 0..8usize {
                let blk = (y / 4) * 2 + (x / 4);
                plane[base + y * uv_stride + x] =
                    mb.chroma_ac[plane_idx][blk][(y % 4) * 4 + (x % 4)] as u16;
            }
        }
    }
}

/// Reconstruct one high-depth inter macroblock into the `u16` planes.
///
/// The 8-bit inter path in `mb.rs` writes only into the `u8` planes, which the
/// high-depth output path in `decoder.rs` never reads. FFmpeg instead runs the
/// qpel/chroma templates compiled for `uint16_t` samples. This uses the MVs
/// that `apply_macroblock` already stored in `ctx.mv_ctx` (one per 4x4 block,
/// including per-sub-partition MVs) and performs full-precision MC from the
/// u16 reference planes. Luma is reconstructed per 4x4 block; chroma is
/// reconstructed per matching 2x2 (4:2:0) or 2x4 (4:2:2) block, which is
/// exact because the bilinear chroma filter operates per sample.
///
/// Reference: FFmpeg `h264qpel_template.c`, `h264chroma_template.c`,
///            `h264_mb_template.c` (`hl_motion_*`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's inter reconstruction parameter set
pub fn reconstruct_inter_mb_hires(
    ctx: &mut FrameDecodeContext,
    slice_hdr: &SliceHeader,
    mb_x: u32,
    mb_y: u32,
    ref_pics: &[RefPic],
    ref_pics_l1: &[RefPic],
    mb: Option<&crate::cavlc::Macroblock>,
) {
    let max_pixel = ctx.pic.max_pixel() as i32;
    let chroma422 = ctx.pic.chroma_format_idc == 2;
    let ctx_field = ctx.mb_field;
    let self_pic = Arc::clone(ctx.pic.shared());
    let mut luma = [0u16; 16];
    let mut luma1 = [0u16; 16];
    let mut chroma = [0u16; 32];
    let mut chroma1 = [0u16; 32];

    for (blk, &(blk_x, blk_y)) in BLOCK_INDEX_TO_XY.iter().enumerate() {
        // MvContext stores MVs by raster index (bx + by*4), while `blk` is
        // the scan8-style BLOCK_INDEX_TO_XY index. Convert before lookup so
        // 8x8 partitions with distinct per-partition MVs resolve correctly.
        let raster = block_to_raster(blk);
        let (mv0, ref0) = ctx.mv_ctx.get(mb_x, mb_y, raster);
        let (mv1, ref1) = ctx.mv_ctx.get_l1(mb_x, mb_y, raster);
        let use_l0 = ref0 >= 0 && (ref0 as usize) < ref_pics.len();
        let use_l1 = ref1 >= 0 && (ref1 as usize) < ref_pics_l1.len();
        if !use_l0 && !use_l1 {
            continue; // intra / I_PCM block, already reconstructed by hires.rs
        }

        let dst_x = (mb_x * 16 + blk_x * 4) as i32;
        // A field reference has `pic_as_field()` applied in FFmpeg: its stride
        // is doubled, a bottom field starts one original line in, and the MV
        // is relative to field rows. Mirror `mb::apply_mc_partition`.
        let field_geometry = |ref_pic: &RefPic| {
            let field_view = ref_pic.is_field() && ctx_field;
            let ref_mb_y = if field_view { mb_y / 2 } else { mb_y };
            (field_view, ref_mb_y)
        };

        let mc_luma_from = |ref_pic: &RefPic, mv: [i16; 2], out: &mut [u16; 16]| {
            let (field_view, ref_mb_y) = field_geometry(ref_pic);
            let ref_y = (ref_mb_y * 16 + blk_y * 4) as i32;
            let mvy = mv[1] as i32;
            // Quarter-pel MV; the 6-tap filter reads 2 above and 3 below.
            let ref_y_bottom = ref_y + (mvy >> 2) + 4 + 3;
            let needed_row = (ref_y_bottom.max(0) as u32 / 16).min(ref_pic.mb_height() - 1);
            // A bottom field may reference the already-decoded top field of
            // the same picture; waiting there would deadlock (FFmpeg
            // h264_mb.c:60-70 skips the same check).
            let self_field_ref = Arc::ptr_eq(&ref_pic.pic, &self_pic) && ref_pic.is_field();
            if !self_field_ref {
                ref_pic.wait_for_row(needed_row as i32);
            }
            // SAFETY: self-field rows are complete before the bottom field
            // starts; all other rows are guaranteed by wait_for_row.
            let rp = unsafe { ref_pic.data() };
            let field_luma_offset = if field_view && ref_pic.parity == 2 {
                rp.y_stride
            } else {
                0
            };
            let ref_luma_stride = if field_view {
                rp.y_stride * 2
            } else {
                rp.y_stride
            };
            let ref_luma_height = if field_view { rp.height / 2 } else { rp.height };
            mc::mc_luma_u16(
                out,
                4,
                &rp.y16[field_luma_offset..],
                ref_luma_stride,
                dst_x + (mv[0] as i32 >> 2),
                ref_y + (mvy >> 2),
                (mv[0] as i32 & 3) as u8,
                (mvy & 3) as u8,
                4,
                4,
                rp.width,
                ref_luma_height,
                max_pixel,
            );
        };

        if use_l0 {
            mc_luma_from(&ref_pics[ref0 as usize], mv0, &mut luma);
        }
        if use_l1 {
            mc_luma_from(&ref_pics_l1[ref1 as usize], mv1, &mut luma1);
        }

        let cw = 2usize;
        let ch = if chroma422 { 4usize } else { 2usize };
        let mc_chroma_from = |ref_pic: &RefPic, mv: [i16; 2], plane: usize, out: &mut [u16; 32]| {
            let (field_view, ref_mb_y) = field_geometry(ref_pic);
            let c_dst_x = (mb_x * 8 + blk_x * 2) as i32;
            let c_dst_y = if chroma422 {
                (ref_mb_y * 16 + blk_y * 4) as i32
            } else {
                (ref_mb_y * 8 + blk_y * 2) as i32
            };
            let rp = unsafe { ref_pic.data() };
            let src = if plane == 0 { &rp.u16 } else { &rp.v16 };
            // 4:2:2 chroma is full-resolution vertically: reference row
            // uses `my >> 2` and the 1/8-pel phase is `(my << 1) & 7`
            // (FFmpeg h264_mb.c mc_dir_part, ysh = 3 - (chroma_idc == 2)).
            let chroma_ref_y = if chroma422 {
                c_dst_y + (mv[1] as i32 >> 2)
            } else {
                c_dst_y + (mv[1] as i32 >> 3)
            };
            let chroma_dy = if chroma422 {
                (mv[1] as i32) << 1 & 7
            } else {
                mv[1] as i32 & 7
            };
            let field_chroma_offset = if field_view && ref_pic.parity == 2 {
                rp.uv_stride
            } else {
                0
            };
            let ref_chroma_stride = if field_view {
                rp.uv_stride * 2
            } else {
                rp.uv_stride
            };
            let ref_chroma_height = if field_view && !chroma422 {
                rp.height / 4
            } else if chroma422 && !field_view {
                rp.height
            } else {
                rp.height / 2
            };
            mc::mc_chroma_u16(
                &mut out[..cw * ch],
                cw,
                &src[field_chroma_offset..],
                ref_chroma_stride,
                c_dst_x + (mv[0] as i32 >> 3),
                chroma_ref_y,
                (mv[0] as i32 & 7) as u8,
                chroma_dy as u8,
                cw,
                ch,
                rp.width / 2,
                ref_chroma_height,
                max_pixel,
            );
        };

        // Combine L0/L1 at full precision. Unweighted bi-prediction is the
        // rounded average; explicit weights use the slice's denom/offset.
        let combine = |a: u16, b: u16, implicit: Option<i32>| -> u16 {
            if !use_l1 {
                if use_l0 && slice_hdr.use_weight {
                    let (w0, o0) = slice_hdr.luma_weight_l0[ref0 as usize];
                    let denom = slice_hdr.luma_log2_weight_denom;
                    let round = 1 << denom.saturating_sub(1);
                    u16::from_i32((a.to_i32() * w0 + o0 + round) >> denom)
                } else {
                    a
                }
            } else if !use_l0 {
                b
            } else if let Some(w0) = implicit {
                let w1 = 64 - w0;
                u16::from_i32((a.to_i32() * w0 + b.to_i32() * w1 + 32) >> 6)
            } else if slice_hdr.use_weight {
                let (w0, o0) = slice_hdr.luma_weight_l0[ref0 as usize];
                let (w1, o1) = slice_hdr.luma_weight_l1[ref1 as usize];
                let denom = slice_hdr.luma_log2_weight_denom;
                let round = 1 << denom.saturating_sub(1);
                u16::from_i32(
                    (a.to_i32() * w0 + b.to_i32() * w1 + o0 + o1 + 2 * round + 1) >> (denom + 1),
                )
            } else {
                ((a.to_i32() + b.to_i32() + 1) >> 1) as u16
            }
        };

        let implicit_w0 = if use_l0 && use_l1 && slice_hdr.weighted_bipred_idc == 2 {
            ctx.implicit_weight
                .get(ref0 as usize)
                .and_then(|row| row.get(ref1 as usize))
                .copied()
        } else {
            None
        };

        let (luma_off, luma_stride) =
            luma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y, ctx.mb_field);
        for i in 0..16 {
            ctx.pic.y16[luma_off + (i / 4) * luma_stride + (i % 4)] =
                combine(luma[i], luma1[i], implicit_w0);
        }

        if !ctx.decode_chroma {
            continue;
        }
        let (chroma_off, uv_stride) =
            inter_chroma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y, ctx_field);
        for plane_idx in 0..2usize {
            if use_l0 {
                mc_chroma_from(&ref_pics[ref0 as usize], mv0, plane_idx, &mut chroma);
            }
            if use_l1 {
                mc_chroma_from(&ref_pics_l1[ref1 as usize], mv1, plane_idx, &mut chroma1);
            }
            let plane = if plane_idx == 0 {
                &mut ctx.pic.u16
            } else {
                &mut ctx.pic.v16
            };
            for i in 0..cw * ch {
                // Chroma uses the same implicit weights (log2_denom = 5), and
                // explicit chroma weights when signalled.
                let val = if use_l0 && use_l1 && slice_hdr.weighted_bipred_idc == 2 {
                    let w0 = implicit_w0.unwrap_or(32);
                    u16::from_i32(
                        (chroma[i].to_i32() * w0 + chroma1[i].to_i32() * (64 - w0) + 32) >> 6,
                    )
                } else if use_l1 && !use_l0 {
                    chroma1[i]
                } else if use_l1 && slice_hdr.use_weight_chroma {
                    let (w0, o0) = slice_hdr.chroma_weight_l0[ref0 as usize][plane_idx];
                    let (w1, o1) = slice_hdr.chroma_weight_l1[ref1 as usize][plane_idx];
                    let denom = slice_hdr.chroma_log2_weight_denom;
                    let round = 1 << denom.saturating_sub(1);
                    u16::from_i32(
                        (chroma[i].to_i32() * w0
                            + chroma1[i].to_i32() * w1
                            + o0
                            + o1
                            + 2 * round
                            + 1)
                            >> (denom + 1),
                    )
                } else if use_l0 && use_l1 {
                    ((chroma[i].to_i32() + chroma1[i].to_i32() + 1) >> 1) as u16
                } else {
                    chroma[i]
                };
                plane[chroma_off + i / cw * uv_stride + i % cw] = val;
            }
        }
    }

    // Apply residual to the high-depth planes. The generic (u8) pass has
    // dequantized the coefficients in `mb` and applied them via
    // `idct4x4_add_keep` (which preserves the coefficients), but that residual
    // went to the 8-bit planes only. Re-apply the same residual here, after
    // motion compensation, so the 10-bit planes get full-precision residual.
    let Some(mb) = mb else {
        return;
    };
    // Intra MB residual is applied by decode_intra_mb_hires; only inter MBs
    // need the deferred high-depth residual after motion compensation.
    if mb.is_intra || mb.is_pcm {
        return;
    }

    let cbp_luma = mb.cbp & 0x0F;
    if cbp_luma != 0 {
        if mb.transform_size_8x8_flag {
            // High-profile inter MBs can code luma residual with the 8x8
            // transform. Mirror FFmpeg's `idct8_add4` (h264_mb.c:773-777):
            // the dequantized coefficients were kept by the 8-bit pass
            // (`idct8x8_add_keep`), so apply the same IDCT to the u16 plane.
            for (i8x8, &(bx, by)) in BLOCK_8X8_OFFSET.iter().enumerate() {
                if cbp_luma & (1 << i8x8) == 0 {
                    continue;
                }
                let (off, stride) =
                    luma_block_offset(&ctx.pic, mb_x, mb_y, bx / 4, by / 4, ctx.mb_field);
                let mut c = mb.luma_8x8_coeffs[i8x8];
                idct::idct8x8_add(&mut ctx.pic.y16[off..], stride, &mut c);
            }
        } else {
            for (block, &(blk_x, blk_y)) in BLOCK_INDEX_TO_XY.iter().enumerate() {
                let group = (blk_y / 2) * 2 + (blk_x / 2);
                if cbp_luma & (1 << group) == 0 {
                    continue;
                }
                let raster = block_to_raster(block);
                let (off, stride) =
                    luma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y, ctx.mb_field);
                // The generic u8 pass dequantized into i16; the u16 plane
                // must use the full-precision copy to match FFmpeg's int32
                // dctcoef storage at BIT_DEPTH > 8.
                let mut c = mb.luma_coeffs32[raster];
                idct::idct4x4_add_wide(&mut ctx.pic.y16[off..], stride, &mut c);
            }
        }
    }

    let cbp_chroma = (mb.cbp >> 4) & 3;
    if cbp_chroma > 0 && ctx.decode_chroma {
        let max_qp = 51 + ctx.pic.qp_bd_offset();
        let chroma422 = ctx.pic.chroma_format_idc == 2;
        for plane_idx in 0..2usize {
            let c_qp = if chroma422 {
                (ctx.chroma_qp[plane_idx] as i32 + 3).min(max_qp) as u8
            } else {
                ctx.chroma_qp[plane_idx]
            };
            let dc_cqm = 4 + plane_idx;
            let qmul = dequant::dc_dequant_scale(&ctx.dequant4, dc_cqm, c_qp);
            let mut dc_out = [0i32; 8];
            let num_blocks = if chroma422 { 8 } else { 4 };
            if chroma422 {
                idct::chroma422_dc_dequant_idct(&mut dc_out, &mb.chroma_dc[plane_idx], qmul);
            } else {
                let dc_in: [i16; 4] = mb.chroma_dc[plane_idx][..4].try_into().unwrap();
                let mut dc420 = [0i32; 4];
                idct::chroma_dc_dequant_idct(&mut dc420, &dc_in, qmul);
                dc_out[..4].copy_from_slice(&dc420);
            }
            let max = ctx.pic.max_pixel() as i32;
            let offsets: Vec<(usize, usize)> = (0..num_blocks)
                .map(|b| {
                    let bx = (b & 1) as u32;
                    let by = (b >> 1) as u32;
                    chroma_block_offset(&ctx.pic, mb_x, mb_y, bx, by, ctx_field)
                })
                .collect();
            let plane = if plane_idx == 0 {
                &mut ctx.pic.u16
            } else {
                &mut ctx.pic.v16
            };
            for (blk_idx, &(off, stride)) in offsets.iter().enumerate() {
                if cbp_chroma >= 2 {
                    let mut c = mb.chroma_ac[plane_idx][blk_idx];
                    idct::idct4x4_add(&mut plane[off..], stride, &mut c);
                } else {
                    let dc_add = (dc_out[blk_idx] + 32) >> 6;
                    for j in 0..4 {
                        for i in 0..4 {
                            let idx = off + j * stride + i;
                            let v = plane[idx] as i32 + dc_add;
                            plane[idx] = v.clamp(0, max) as u16;
                        }
                    }
                }
            }
        }
    }
}
