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

use tracing::trace;

use crate::cavlc::Macroblock;
use crate::deblock::PictureBuffer;
use crate::dequant;
use crate::idct;
use crate::intra_pred;
use crate::mb::{
    BLOCK_INDEX_TO_XY, FrameDecodeContext, block_to_raster, luma_block_offset, luma_mb_offset,
};
use crate::pixel::Pixel;

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
fn chroma_offset(pic: &PictureBuffer, mb_x: u32, mb_y: u32) -> (usize, usize) {
    let rows = if pic.chroma_format_idc == 2 { 16 } else { 8 };
    (
        mb_y as usize * rows * pic.uv_stride + mb_x as usize * 8,
        pic.uv_stride,
    )
}

fn chroma_block_offset(
    pic: &PictureBuffer,
    mb_x: u32,
    mb_y: u32,
    blk_x: u32,
    blk_y: u32,
) -> (usize, usize) {
    let rows = if pic.chroma_format_idc == 2 { 16 } else { 8 };
    (
        (mb_y as usize * rows + blk_y as usize * 4) * pic.uv_stride
            + mb_x as usize * 8
            + blk_x as usize * 4,
        pic.uv_stride,
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
        let (offset, stride) = chroma_offset(&ctx.pic, mb_x, mb_y);
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
                let (c_offset, c_stride) = chroma_block_offset(&ctx.pic, mb_x, mb_y, blk_x, blk_y);
                let plane = if plane_idx == 0 {
                    &mut ctx.pic.u16
                } else {
                    &mut ctx.pic.v16
                };
                if cbp_chroma >= 2 {
                    mb.chroma_ac[plane_idx][blk_idx][0] = dc_val as i16;
                    dequant::dequant_4x4(
                        &mut mb.chroma_ac[plane_idx][blk_idx],
                        &ctx.dequant4.coeffs[dc_cqm][c_qp as usize],
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
            dequant::dequant_4x4(
                &mut mb.luma_coeffs[raster_idx],
                &ctx.dequant4.coeffs[0][qp as usize],
            );
            idct::idct4x4_add(
                &mut ctx.pic.y16[offset..],
                stride,
                &mut mb.luma_coeffs[raster_idx],
            );
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
        let _ = btr;
        let mode = mb.intra4x4_pred_mode[i8x8 * 4];
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
            btr,
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
        trace!(mb_x, mb_y, "high-depth I_PCM not yet handled");
    }
}
