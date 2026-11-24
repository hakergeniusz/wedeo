// Intra prediction for VP8.
//
// Direct translation of the prediction functions in FFmpeg's
// libavcodec/h264pred.c and h264pred_template.c that VP8 installs in its
// H264PredContext (see ff_h264_pred_init, AV_CODEC_ID_VP8 case), plus the
// edge-mode substitution and 4x4 copy-buffer logic from `intra_predict` in
// libavcodec/vp8.c.
//
// LGPL-2.1-or-later, same as FFmpeg.

use crate::types::*;

#[inline]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Neighbour samples for one 4x4 luma block. FFmpeg reads these directly
/// from the frame with the `copy` edge-emulation buffer; we materialise the
/// same values here.
#[derive(Clone, Copy)]
pub struct Neighbors4 {
    pub top: [u8; 4],
    pub left: [u8; 4],
    pub topleft: u8,
    pub topright: [u8; 4],
}

/// Gather the four neighbour groups for a 4x4 block at `base` in `plane`.
/// `mb_x`/`mb_y` are absolute macroblock coordinates (needed for edge
/// substitution), `bx`/`by` are the 4x4 block indices inside the MB.
#[allow(clippy::too_many_arguments)]
pub fn gather_neighbors4(
    plane: &[u8],
    base: usize,
    stride: usize,
    bx: usize,
    by: usize,
    mb_x: usize,
    mb_y: usize,
    mb_width: usize,
) -> Neighbors4 {
    let lo = 127u8;
    let hi = 129u8;

    let top = if mb_y == 0 && by == 0 {
        [lo; 4]
    } else {
        [
            plane[base - stride],
            plane[base - stride + 1],
            plane[base - stride + 2],
            plane[base - stride + 3],
        ]
    };

    let left = if mb_x == 0 && bx == 0 {
        [hi; 4]
    } else {
        [
            plane[base - 1],
            plane[base + stride - 1],
            plane[base + 2 * stride - 1],
            plane[base + 3 * stride - 1],
        ]
    };

    let topleft = if mb_y == 0 && by == 0 {
        lo
    } else if mb_x == 0 && bx == 0 {
        hi
    } else {
        plane[base - stride - 1]
    };

    // FFmpeg computes `tr_right = ptr - linesize + 16` ONCE before the y
    // loop, i.e. from the row ABOVE the macroblock (already decoded). It
    // is reused for every right-edge (x==3) 4x4 block, even when y>0.
    let mb_base = base - by * 4 * stride - bx * 4;
    let tr_right = mb_base - stride + 16;
    let topright = if (by == 0 || bx == 3) && mb_y == 0 {
        [lo; 4]
    } else if bx == 3 {
        if mb_x == mb_width - 1 {
            // FFmpeg: tr = tr_right[-1] * 0x01010101u (edge extension).
            [plane[tr_right - 1]; 4]
        } else {
            [
                plane[tr_right],
                plane[tr_right + 1],
                plane[tr_right + 2],
                plane[tr_right + 3],
            ]
        }
    } else {
        [
            plane[base + 4 - stride],
            plane[base + 5 - stride],
            plane[base + 6 - stride],
            plane[base + 7 - stride],
        ]
    };

    Neighbors4 {
        top,
        left,
        topleft,
        topright,
    }
}

/// Write one 4-byte row into `plane` at `base + row*stride`.
#[inline]
fn write_row(plane: &mut [u8], base: usize, stride: usize, row: usize, v: [u8; 4]) {
    let off = base + row * stride;
    plane[off..off + 4].copy_from_slice(&v);
}

/// Predict one 4x4 luma block using the already-substituted mode and
/// neighbour samples.
pub fn pred4x4(plane: &mut [u8], base: usize, stride: usize, mode: u8, n: &Neighbors4) {
    let t = n.top;
    let l = n.left;
    let lt = n.topleft;
    let tr = n.topright;

    match mode {
        VERT_PRED => {
            // pred4x4_vertical_vp8
            let v = [
                ((lt as i32 + 2 * t[0] as i32 + t[1] as i32 + 2) >> 2) as u8,
                ((t[0] as i32 + 2 * t[1] as i32 + t[2] as i32 + 2) >> 2) as u8,
                ((t[1] as i32 + 2 * t[2] as i32 + t[3] as i32 + 2) >> 2) as u8,
                ((t[2] as i32 + 2 * t[3] as i32 + tr[0] as i32 + 2) >> 2) as u8,
            ];
            for y in 0..4 {
                write_row(plane, base, stride, y, v);
            }
        }
        HOR_PRED => {
            // pred4x4_horizontal_vp8
            let v = [
                ((lt as i32 + 2 * l[0] as i32 + l[1] as i32 + 2) >> 2) as u8,
                ((l[0] as i32 + 2 * l[1] as i32 + l[2] as i32 + 2) >> 2) as u8,
                ((l[1] as i32 + 2 * l[2] as i32 + l[3] as i32 + 2) >> 2) as u8,
                ((l[2] as i32 + 2 * l[3] as i32 + l[3] as i32 + 2) >> 2) as u8,
            ];
            for y in 0..4 {
                write_row(plane, base, stride, y, [v[y]; 4]);
            }
        }
        DC_PRED => {
            // pred4x4_dc
            let dc = ((t[0] as i32
                + t[1] as i32
                + t[2] as i32
                + t[3] as i32
                + l[0] as i32
                + l[1] as i32
                + l[2] as i32
                + l[3] as i32
                + 4)
                >> 3) as u8;
            for y in 0..4 {
                write_row(plane, base, stride, y, [dc; 4]);
            }
        }
        DIAG_DOWN_LEFT_PRED => {
            // pred4x4_down_left
            let mut d = [[0u8; 4]; 4];
            d[0][0] = ((t[0] as i32 + t[2] as i32 + 2 * t[1] as i32 + 2) >> 2) as u8;
            d[1][0] = ((t[1] as i32 + t[3] as i32 + 2 * t[2] as i32 + 2) >> 2) as u8;
            d[0][1] = ((t[1] as i32 + t[3] as i32 + 2 * t[2] as i32 + 2) >> 2) as u8;
            d[2][0] = ((t[2] as i32 + tr[0] as i32 + 2 * t[3] as i32 + 2) >> 2) as u8;
            d[1][1] = ((t[2] as i32 + tr[0] as i32 + 2 * t[3] as i32 + 2) >> 2) as u8;
            d[0][2] = ((t[2] as i32 + tr[0] as i32 + 2 * t[3] as i32 + 2) >> 2) as u8;
            d[3][0] = ((t[3] as i32 + tr[1] as i32 + 2 * tr[0] as i32 + 2) >> 2) as u8;
            d[2][1] = ((t[3] as i32 + tr[1] as i32 + 2 * tr[0] as i32 + 2) >> 2) as u8;
            d[1][2] = ((t[3] as i32 + tr[1] as i32 + 2 * tr[0] as i32 + 2) >> 2) as u8;
            d[0][3] = ((t[3] as i32 + tr[1] as i32 + 2 * tr[0] as i32 + 2) >> 2) as u8;
            d[3][1] = ((tr[0] as i32 + tr[2] as i32 + 2 * tr[1] as i32 + 2) >> 2) as u8;
            d[2][2] = ((tr[0] as i32 + tr[2] as i32 + 2 * tr[1] as i32 + 2) >> 2) as u8;
            d[1][3] = ((tr[0] as i32 + tr[2] as i32 + 2 * tr[1] as i32 + 2) >> 2) as u8;
            d[3][2] = ((tr[1] as i32 + tr[3] as i32 + 2 * tr[2] as i32 + 2) >> 2) as u8;
            d[2][3] = ((tr[1] as i32 + tr[3] as i32 + 2 * tr[2] as i32 + 2) >> 2) as u8;
            d[3][3] = ((tr[2] as i32 + 3 * tr[3] as i32 + 2) >> 2) as u8;
            for y in 0..4 {
                write_row(plane, base, stride, y, d[y]);
            }
        }
        DIAG_DOWN_RIGHT_PRED => {
            // pred4x4_down_right
            let mut d = [[0u8; 4]; 4];
            d[3][0] = ((l[3] as i32 + 2 * l[2] as i32 + l[1] as i32 + 2) >> 2) as u8;
            d[2][0] = ((l[2] as i32 + 2 * l[1] as i32 + l[0] as i32 + 2) >> 2) as u8;
            d[3][1] = ((l[2] as i32 + 2 * l[1] as i32 + l[0] as i32 + 2) >> 2) as u8;
            d[1][0] = ((l[1] as i32 + 2 * l[0] as i32 + lt as i32 + 2) >> 2) as u8;
            d[2][1] = ((l[1] as i32 + 2 * l[0] as i32 + lt as i32 + 2) >> 2) as u8;
            d[3][2] = ((l[1] as i32 + 2 * l[0] as i32 + lt as i32 + 2) >> 2) as u8;
            d[0][0] = ((l[0] as i32 + 2 * lt as i32 + t[0] as i32 + 2) >> 2) as u8;
            d[1][1] = ((l[0] as i32 + 2 * lt as i32 + t[0] as i32 + 2) >> 2) as u8;
            d[2][2] = ((l[0] as i32 + 2 * lt as i32 + t[0] as i32 + 2) >> 2) as u8;
            d[3][3] = ((l[0] as i32 + 2 * lt as i32 + t[0] as i32 + 2) >> 2) as u8;
            d[0][1] = ((lt as i32 + 2 * t[0] as i32 + t[1] as i32 + 2) >> 2) as u8;
            d[1][2] = ((lt as i32 + 2 * t[0] as i32 + t[1] as i32 + 2) >> 2) as u8;
            d[2][3] = ((lt as i32 + 2 * t[0] as i32 + t[1] as i32 + 2) >> 2) as u8;
            d[0][2] = ((t[0] as i32 + 2 * t[1] as i32 + t[2] as i32 + 2) >> 2) as u8;
            d[1][3] = ((t[0] as i32 + 2 * t[1] as i32 + t[2] as i32 + 2) >> 2) as u8;
            d[0][3] = ((t[1] as i32 + 2 * t[2] as i32 + t[3] as i32 + 2) >> 2) as u8;
            for y in 0..4 {
                write_row(plane, base, stride, y, d[y]);
            }
        }
        VERT_RIGHT_PRED => {
            // pred4x4_vertical_right (h264pred_template.c)
            let mut d = [[0u8; 4]; 4];
            let (lt, t, l) = (lt as i32, &t, &l);
            let v = [
                ((lt + t[0] as i32 + 1) >> 1) as u8,
                ((t[0] as i32 + t[1] as i32 + 1) >> 1) as u8,
                ((t[1] as i32 + t[2] as i32 + 1) >> 1) as u8,
                ((t[2] as i32 + t[3] as i32 + 1) >> 1) as u8,
            ];
            d[0][0] = v[0];
            d[2][1] = v[0];
            d[0][1] = v[1];
            d[2][2] = v[1];
            d[0][2] = v[2];
            d[2][3] = v[2];
            d[0][3] = v[3];
            d[1][0] = ((l[0] as i32 + 2 * lt + t[0] as i32 + 2) >> 2) as u8;
            d[3][1] = d[1][0];
            d[1][1] = ((lt + 2 * t[0] as i32 + t[1] as i32 + 2) >> 2) as u8;
            d[3][2] = d[1][1];
            d[1][2] = ((t[0] as i32 + 2 * t[1] as i32 + t[2] as i32 + 2) >> 2) as u8;
            d[3][3] = d[1][2];
            d[1][3] = ((t[1] as i32 + 2 * t[2] as i32 + t[3] as i32 + 2) >> 2) as u8;
            d[2][0] = ((lt + 2 * l[0] as i32 + l[1] as i32 + 2) >> 2) as u8;
            d[3][0] = ((l[0] as i32 + 2 * l[1] as i32 + l[2] as i32 + 2) >> 2) as u8;
            for y in 0..4 {
                write_row(plane, base, stride, y, d[y]);
            }
        }
        HOR_DOWN_PRED => {
            // pred4x4_horizontal_down (h264pred_template.c)
            let mut d = [[0u8; 4]; 4];
            let (lt, t, l) = (lt as i32, &t, &l);
            let h = |a: i32, b: i32| ((a + b + 1) >> 1) as u8;
            let q = |a: i32, b: i32, c: i32| ((a + 2 * b + c + 2) >> 2) as u8;
            d[0][0] = h(lt, l[0] as i32);
            d[1][2] = d[0][0];
            d[0][1] = q(l[0] as i32, lt, t[0] as i32);
            d[1][3] = d[0][1];
            d[0][2] = q(lt, t[0] as i32, t[1] as i32);
            d[0][3] = q(t[0] as i32, t[1] as i32, t[2] as i32);
            d[1][0] = h(l[0] as i32, l[1] as i32);
            d[2][2] = d[1][0];
            d[1][1] = q(lt, l[0] as i32, l[1] as i32);
            d[2][3] = d[1][1];
            d[2][0] = h(l[1] as i32, l[2] as i32);
            d[3][2] = d[2][0];
            d[2][1] = q(l[0] as i32, l[1] as i32, l[2] as i32);
            d[3][3] = d[2][1];
            d[3][0] = h(l[2] as i32, l[3] as i32);
            d[3][1] = q(l[1] as i32, l[2] as i32, l[3] as i32);
            for y in 0..4 {
                write_row(plane, base, stride, y, d[y]);
            }
        }
        VERT_LEFT_PRED => {
            // pred4x4_vertical_left_vp8 (h264pred.c)
            let tt = [
                t[0] as i32,
                t[1] as i32,
                t[2] as i32,
                t[3] as i32,
                tr[0] as i32,
                tr[1] as i32,
                tr[2] as i32,
                tr[3] as i32,
            ];
            let h = |i: usize| ((tt[i] + tt[i + 1] + 1) >> 1) as u8;
            let q = |i: usize| ((tt[i] + 2 * tt[i + 1] + tt[i + 2] + 2) >> 2) as u8;
            // FFmpeg pred4x4_vertical_left_vp8_c fills the grid as:
            //   row0: h0 h1 h2 h3
            //   row1: q0 q1 q2 q3
            //   row2: h1 h2 h3 q4
            //   row3: q1 q2 q3 q5
            let d = [
                [h(0), h(1), h(2), h(3)],
                [q(0), q(1), q(2), q(3)],
                [h(1), h(2), h(3), q(4)],
                [q(1), q(2), q(3), q(5)],
            ];
            for y in 0..4 {
                write_row(plane, base, stride, y, d[y]);
            }
        }
        HOR_UP_PRED => {
            // pred4x4_horizontal_up (h264pred_template.c)
            let mut d = [[0u8; 4]; 4];
            let (l0, l1, l2, l3) = (l[0] as i32, l[1] as i32, l[2] as i32, l[3] as i32);
            d[0][0] = ((l0 + l1 + 1) >> 1) as u8;
            d[0][1] = ((l0 + 2 * l1 + l2 + 2) >> 2) as u8;
            d[0][2] = ((l1 + l2 + 1) >> 1) as u8;
            d[1][0] = d[0][2];
            d[0][3] = ((l1 + 2 * l2 + l3 + 2) >> 2) as u8;
            d[1][1] = d[0][3];
            d[1][2] = ((l2 + l3 + 1) >> 1) as u8;
            d[2][0] = d[1][2];
            d[1][3] = ((l2 + 2 * l3 + l3 + 2) >> 2) as u8;
            d[2][1] = d[1][3];
            // FFmpeg pred4x4_horizontal_up_c: only row 3 (and row 2 cols 2-3)
            // are filled with l3. Row 2 cols 0-1 were just computed above.
            for x in 2..4 {
                d[2][x] = l3 as u8;
            }
            for x in 0..4 {
                d[3][x] = l3 as u8;
            }
            for y in 0..4 {
                write_row(plane, base, stride, y, d[y]);
            }
        }
        TM_VP8_PRED => {
            // pred4x4_tm_vp8
            for y in 0..4 {
                let row = [
                    clip_u8(t[0] as i32 + l[y] as i32 - lt as i32),
                    clip_u8(t[1] as i32 + l[y] as i32 - lt as i32),
                    clip_u8(t[2] as i32 + l[y] as i32 - lt as i32),
                    clip_u8(t[3] as i32 + l[y] as i32 - lt as i32),
                ];
                write_row(plane, base, stride, y, row);
            }
        }
        VERT_VP8_PRED => {
            // pred4x4_vertical (unaveraged)
            for y in 0..4 {
                write_row(plane, base, stride, y, t);
            }
        }
        DC_127_PRED => {
            for y in 0..4 {
                write_row(plane, base, stride, y, [127; 4]);
            }
        }
        DC_129_PRED => {
            for y in 0..4 {
                write_row(plane, base, stride, y, [129; 4]);
            }
        }
        HOR_VP8_PRED => {
            // pred4x4_horizontal (unaveraged)
            for y in 0..4 {
                write_row(plane, base, stride, y, [l[y]; 4]);
            }
        }
        _ => {
            // Defensive: DC as fallback. Should not happen for valid streams.
            let dc = ((t[0] as i32 + t[1] as i32 + t[2] as i32 + t[3] as i32 + 2) >> 2) as u8;
            for y in 0..4 {
                write_row(plane, base, stride, y, [dc; 4]);
            }
        }
    }
}

/// `check_intra_pred8x8_mode_emuedge` from vp8.c (VP8 path).
pub fn check_intra_pred8x8(mode: u8, mb_x: usize, mb_y: usize) -> u8 {
    match mode {
        DC_PRED8x8 => {
            if mb_x == 0 {
                if mb_y != 0 {
                    TOP_DC_PRED8x8
                } else {
                    DC_128_PRED8x8
                }
            } else if mb_y != 0 {
                mode
            } else {
                LEFT_DC_PRED8x8
            }
        }
        VERT_PRED8x8 => {
            if mb_y == 0 {
                DC_127_PRED8x8
            } else {
                mode
            }
        }
        HOR_PRED8x8 => {
            if mb_x == 0 {
                DC_129_PRED8x8
            } else {
                mode
            }
        }
        PLANE_PRED8x8 => {
            if mb_x == 0 {
                if mb_y != 0 {
                    VERT_PRED8x8
                } else {
                    DC_129_PRED8x8
                }
            } else if mb_y != 0 {
                mode
            } else {
                HOR_PRED8x8
            }
        }
        _ => mode,
    }
}

/// `check_intra_pred4x4_mode_emuedge` from vp8.c (VP8 path). Returns the
/// substituted mode. The `copy` flag is implicit in the neighbour values
/// gathered by `gather_neighbors4`.
pub fn check_intra_pred4x4(mode: u8, mb_x: usize, mb_y: usize) -> u8 {
    match mode {
        VERT_PRED => {
            if mb_y == 0 {
                DC_127_PRED
            } else {
                // mb_x == 0 uses copy_buf=1; neighbours are substituted in
                // gather_neighbors4, so the mode itself is unchanged.
                VERT_PRED
            }
        }
        DIAG_DOWN_LEFT_PRED | VERT_LEFT_PRED => {
            if mb_y == 0 {
                DC_127_PRED
            } else {
                mode
            }
        }
        HOR_PRED => {
            if mb_x == 0 && mb_y != 0 {
                DC_129_PRED
            } else {
                // mb_y == 0 uses copy_buf=1; mode unchanged.
                HOR_PRED
            }
        }
        HOR_UP_PRED => {
            if mb_x == 0 {
                DC_129_PRED
            } else {
                mode
            }
        }
        TM_VP8_PRED => {
            if mb_x == 0 {
                if mb_y != 0 {
                    VERT_VP8_PRED
                } else {
                    DC_129_PRED
                }
            } else if mb_y != 0 {
                mode
            } else {
                HOR_VP8_PRED
            }
        }
        DC_PRED | DIAG_DOWN_RIGHT_PRED | VERT_RIGHT_PRED | HOR_DOWN_PRED => mode,
        _ => mode,
    }
}

/// Predict a 16x16 or 8x8 block (luma 16x16 or chroma 8x8).
/// `size` is 16 for luma, 8 for chroma; `is_chroma` selects the rv40-style
/// DC variants used by VP8 for chroma.
pub fn pred_block(
    plane: &mut [u8],
    base: usize,
    stride: usize,
    size: usize,
    mode: u8,
    is_chroma: bool,
) {
    const FILL: [u8; 3] = [128, 127, 129];
    match mode {
        DC_128_PRED8x8 | DC_127_PRED8x8 | DC_129_PRED8x8 => {
            let v = FILL[(mode - DC_128_PRED8x8) as usize];
            for y in 0..size {
                let off = base + y * stride;
                for x in 0..size {
                    plane[off + x] = v;
                }
            }
        }
        VERT_PRED8x8 => {
            for y in 0..size {
                let off = base + y * stride;
                let src = base - stride;
                for x in 0..size {
                    let v = plane[src + x];
                    plane[off + x] = v;
                }
            }
        }
        HOR_PRED8x8 => {
            for y in 0..size {
                let v = plane[base + y * stride - 1];
                let off = base + y * stride;
                for x in 0..size {
                    plane[off + x] = v;
                }
            }
        }
        LEFT_DC_PRED8x8 | TOP_DC_PRED8x8 | DC_PRED8x8 => {
            let mut sum = 0i32;
            if is_chroma {
                // pred8x8_*_rv40 use rounded shifts of +4>>3 and +8>>4.
                match mode {
                    LEFT_DC_PRED8x8 => {
                        for i in 0..8 {
                            sum += plane[base + i * stride - 1] as i32;
                        }
                        fill(plane, base, stride, 8, ((sum + 4) >> 3) as u8);
                    }
                    TOP_DC_PRED8x8 => {
                        for i in 0..8 {
                            sum += plane[base + i - stride] as i32;
                        }
                        fill(plane, base, stride, 8, ((sum + 4) >> 3) as u8);
                    }
                    _ => {
                        // FFmpeg's H264_PRED macro installs pred8x8_dc_rv40
                        // as the default DC predictor for VP8 as well.
                        for i in 0..4 {
                            sum += plane[base + i * stride - 1] as i32;
                            sum += plane[base + i - stride] as i32;
                            sum += plane[base + 4 + i - stride] as i32;
                            sum += plane[base - 1 + (i + 4) * stride] as i32;
                        }
                        fill(plane, base, stride, 8, ((sum + 8) >> 4) as u8);
                    }
                }
            } else {
                match mode {
                    LEFT_DC_PRED8x8 => {
                        for i in 0..16 {
                            sum += plane[base + i * stride - 1] as i32;
                        }
                        fill(plane, base, stride, 16, ((sum + 8) >> 4) as u8);
                    }
                    TOP_DC_PRED8x8 => {
                        for i in 0..16 {
                            sum += plane[base + i - stride] as i32;
                        }
                        fill(plane, base, stride, 16, ((sum + 8) >> 4) as u8);
                    }
                    _ => {
                        for i in 0..16 {
                            sum += plane[base + i * stride - 1] as i32;
                            sum += plane[base + i - stride] as i32;
                        }
                        fill(plane, base, stride, 16, ((sum + 16) >> 5) as u8);
                    }
                }
            }
        }
        PLANE_PRED8x8 => {
            // TM_VP8 prediction (TrueMotion).
            let lt = plane[base - stride - 1] as i32;
            for y in 0..size {
                let left = plane[base + y * stride - 1] as i32;
                let off = base + y * stride;
                let src = base - stride;
                for x in 0..size {
                    let top = plane[src + x] as i32;
                    plane[off + x] = clip_u8(top + left - lt);
                }
            }
        }
        _ => {
            fill(plane, base, stride, size, 128);
        }
    }
}

#[inline]
fn fill(plane: &mut [u8], base: usize, stride: usize, size: usize, v: u8) {
    for y in 0..size {
        let off = base + y * stride;
        for x in 0..size {
            plane[off + x] = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tm_vp8_basic() {
        let mut plane = vec![0u8; 32 * 32];
        // top row above the block (base = 32+2)
        for x in 0..4 {
            plane[32 + 2 - 32 + x] = 10 + x as u8;
        }
        for y in 0..4 {
            plane[32 + 2 + y * 32 - 1] = 20 + y as u8;
        }
        plane[32 + 2 - 32 - 1] = 5; // topleft
        let base = 32 + 2;
        let n = Neighbors4 {
            top: [10, 11, 12, 13],
            left: [20, 21, 22, 23],
            topleft: 5,
            topright: [0; 4],
        };
        pred4x4(&mut plane, base, 32, TM_VP8_PRED, &n);
        assert_eq!(plane[base], clip_u8(10 + 20 - 5));
        assert_eq!(plane[base + 1], clip_u8(11 + 20 - 5));
    }

    #[test]
    fn check_modes_at_edges() {
        assert_eq!(check_intra_pred8x8(DC_PRED8x8, 0, 0), DC_128_PRED8x8);
        assert_eq!(check_intra_pred8x8(DC_PRED8x8, 0, 1), TOP_DC_PRED8x8);
        assert_eq!(check_intra_pred8x8(PLANE_PRED8x8, 1, 0), HOR_PRED8x8);
        assert_eq!(check_intra_pred4x4(TM_VP8_PRED, 0, 1), VERT_VP8_PRED);
    }
}
