// H.264 intra prediction modes.
//
// Implements all 9 Intra_4x4, 4 Intra_16x16, and 4 chroma prediction modes
// as defined in ITU-T H.264 section 8.3.1.
//
// Reference: FFmpeg libavcodec/h264pred_template.c

use crate::pixel::Pixel;

// -- Mode constants --

/// Intra 4x4 prediction mode indices (Table 7-11).
pub const INTRA_4X4_VERTICAL: u8 = 0;
pub const INTRA_4X4_HORIZONTAL: u8 = 1;
pub const INTRA_4X4_DC: u8 = 2;
pub const INTRA_4X4_DIAG_DOWN_LEFT: u8 = 3;
pub const INTRA_4X4_DIAG_DOWN_RIGHT: u8 = 4;
pub const INTRA_4X4_VERT_RIGHT: u8 = 5;
pub const INTRA_4X4_HOR_DOWN: u8 = 6;
pub const INTRA_4X4_VERT_LEFT: u8 = 7;
pub const INTRA_4X4_HOR_UP: u8 = 8;

/// Intra 16x16 prediction mode indices (Table 7-13).
pub const INTRA_16X16_VERTICAL: u8 = 0;
pub const INTRA_16X16_HORIZONTAL: u8 = 1;
pub const INTRA_16X16_DC: u8 = 2;
pub const INTRA_16X16_PLANE: u8 = 3;

/// Chroma intra prediction mode indices (Table 7-14).
/// Note: numbering differs from luma 16x16.
pub const INTRA_CHROMA_DC: u8 = 0;
pub const INTRA_CHROMA_HORIZONTAL: u8 = 1;
pub const INTRA_CHROMA_VERTICAL: u8 = 2;
pub const INTRA_CHROMA_PLANE: u8 = 3;

// -- Helper functions --

/// Weighted average of three samples: (a + 2*b + c + 2) >> 2
#[inline(always)]
fn avg3<P: Pixel>(a: P, b: P, c: P) -> P {
    P::from_i32((a.to_i32() + 2 * b.to_i32() + c.to_i32() + 2) >> 2)
}

/// Simple average of two samples: (a + b + 1) >> 1
#[inline(always)]
fn avg2<P: Pixel>(a: P, b: P) -> P {
    P::from_i32((a.to_i32() + b.to_i32() + 1) >> 1)
}

/// Clamp an i32 to the sample range and return a pixel.
#[inline(always)]
fn clip<P: Pixel>(val: i32) -> P {
    P::from_i32(val)
}

/// Compute dst index from (x, y) coordinates and stride.
#[inline(always)]
fn at(x: usize, y: usize, stride: usize) -> usize {
    x + y * stride
}

// ============================================================================
// Intra 4x4 prediction (9 modes)
// ============================================================================

/// Predict a 4x4 block using the given intra prediction mode.
///
/// `dst` is the output buffer (stride = `stride`).
/// `top` is at least 8 bytes (top\[0..3\] required, top\[4..7\] used by some modes).
/// `left` is at least 4 bytes.
/// `top_left` is the corner sample.
/// `has_top`, `has_left`, `has_top_right` indicate neighbor availability.
#[allow(clippy::too_many_arguments)] // Parameters match H.264 spec requirements
pub fn predict_4x4<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    mode: u8,
    top: &[P],
    left: &[P],
    top_left: P,
    has_top: bool,
    has_left: bool,
    has_top_right: bool,
) {
    match mode {
        INTRA_4X4_VERTICAL => pred_4x4_vertical(dst, stride, top),
        INTRA_4X4_HORIZONTAL => pred_4x4_horizontal(dst, stride, left),
        INTRA_4X4_DC => pred_4x4_dc(dst, stride, top, left, has_top, has_left),
        INTRA_4X4_DIAG_DOWN_LEFT => pred_4x4_diag_down_left(dst, stride, top, has_top_right),
        INTRA_4X4_DIAG_DOWN_RIGHT => pred_4x4_diag_down_right(dst, stride, top, left, top_left),
        INTRA_4X4_VERT_RIGHT => pred_4x4_vertical_right(dst, stride, top, left, top_left),
        INTRA_4X4_HOR_DOWN => pred_4x4_horizontal_down(dst, stride, top, left, top_left),
        INTRA_4X4_VERT_LEFT => pred_4x4_vertical_left(dst, stride, top),
        INTRA_4X4_HOR_UP => pred_4x4_horizontal_up(dst, stride, left),
        _ => {} // Unknown mode, leave dst unchanged
    }
}

/// Mode 0: Vertical -- each row copies top[0..3].
fn pred_4x4_vertical<P: Pixel>(dst: &mut [P], stride: usize, top: &[P]) {
    fill_vertical::<P, 4>(dst, stride, top);
}

/// Mode 1: Horizontal -- each row is filled with left[y].
fn pred_4x4_horizontal<P: Pixel>(dst: &mut [P], stride: usize, left: &[P]) {
    fill_horizontal::<P, 4>(dst, stride, left);
}

/// Mode 2: DC -- average of available neighbors, or P::MID if none available.
fn pred_4x4_dc<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    has_top: bool,
    has_left: bool,
) {
    let dc = compute_dc_value::<P, 4>(has_top, has_left, top, left);
    fill_block::<P, 4>(dst, stride, dc);
}

/// Mode 3: Diagonal Down-Left.
///
/// Predicts toward the bottom-right using top samples.
/// Uses top[0..7] (top-right samples top[4..7] are needed).
/// When has_top_right is false, top[4..7] should already be filled with top[3]
/// by the caller (as per H.264 spec 8.3.1.2.3).
fn pred_4x4_diag_down_left<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    _has_top_right: bool,
) {
    let t0 = top[0].to_i32();
    let t1 = top[1].to_i32();
    let t2 = top[2].to_i32();
    let t3 = top[3].to_i32();
    let t4 = top[4].to_i32();
    let t5 = top[5].to_i32();
    let t6 = top[6].to_i32();
    let t7 = top[7].to_i32();

    let s = stride;

    // Matches FFmpeg pred4x4_down_left exactly.
    // Each anti-diagonal (x+y = const) shares the same value.
    dst[at(0, 0, s)] = P::from_i32((t0 + 2 * t1 + t2 + 2) >> 2);
    dst[at(1, 0, s)] = P::from_i32((t1 + 2 * t2 + t3 + 2) >> 2);
    dst[at(0, 1, s)] = dst[at(1, 0, s)];
    dst[at(2, 0, s)] = P::from_i32((t2 + 2 * t3 + t4 + 2) >> 2);
    dst[at(1, 1, s)] = dst[at(2, 0, s)];
    dst[at(0, 2, s)] = dst[at(2, 0, s)];
    dst[at(3, 0, s)] = P::from_i32((t3 + 2 * t4 + t5 + 2) >> 2);
    dst[at(2, 1, s)] = dst[at(3, 0, s)];
    dst[at(1, 2, s)] = dst[at(3, 0, s)];
    dst[at(0, 3, s)] = dst[at(3, 0, s)];
    dst[at(3, 1, s)] = P::from_i32((t4 + 2 * t5 + t6 + 2) >> 2);
    dst[at(2, 2, s)] = dst[at(3, 1, s)];
    dst[at(1, 3, s)] = dst[at(3, 1, s)];
    dst[at(3, 2, s)] = P::from_i32((t5 + 2 * t6 + t7 + 2) >> 2);
    dst[at(2, 3, s)] = dst[at(3, 2, s)];
    dst[at(3, 3, s)] = P::from_i32((t6 + 3 * t7 + 2) >> 2);
}

/// Mode 4: Diagonal Down-Right.
///
/// Predicts toward the bottom-left using top, left, and top-left samples.
fn pred_4x4_diag_down_right<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    top_left: P,
) {
    let lt = top_left;
    let t0 = top[0];
    let t1 = top[1];
    let t2 = top[2];
    let t3 = top[3];
    let l0 = left[0];
    let l1 = left[1];
    let l2 = left[2];
    let l3 = left[3];
    let s = stride;

    // Matches FFmpeg pred4x4_down_right exactly.
    // Each diagonal (x-y = const) shares the same value.
    dst[at(0, 3, s)] = avg3(l3, l2, l1);
    dst[at(0, 2, s)] = avg3(l2, l1, l0);
    dst[at(1, 3, s)] = dst[at(0, 2, s)];
    dst[at(0, 1, s)] = avg3(l1, l0, lt);
    dst[at(1, 2, s)] = dst[at(0, 1, s)];
    dst[at(2, 3, s)] = dst[at(0, 1, s)];
    dst[at(0, 0, s)] = avg3(l0, lt, t0);
    dst[at(1, 1, s)] = dst[at(0, 0, s)];
    dst[at(2, 2, s)] = dst[at(0, 0, s)];
    dst[at(3, 3, s)] = dst[at(0, 0, s)];
    dst[at(1, 0, s)] = avg3(lt, t0, t1);
    dst[at(2, 1, s)] = dst[at(1, 0, s)];
    dst[at(3, 2, s)] = dst[at(1, 0, s)];
    dst[at(2, 0, s)] = avg3(t0, t1, t2);
    dst[at(3, 1, s)] = dst[at(2, 0, s)];
    dst[at(3, 0, s)] = avg3(t1, t2, t3);
}

/// Mode 5: Vertical-Right.
///
/// Approximately 26.6 degrees from vertical.
fn pred_4x4_vertical_right<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    top_left: P,
) {
    let lt = top_left;
    let t0 = top[0];
    let t1 = top[1];
    let t2 = top[2];
    let t3 = top[3];
    let l0 = left[0];
    let l1 = left[1];
    let l2 = left[2];
    let s = stride;

    // Matches FFmpeg pred4x4_vertical_right exactly
    dst[at(0, 0, s)] = avg2(lt, t0);
    dst[at(1, 2, s)] = dst[at(0, 0, s)];
    dst[at(1, 0, s)] = avg2(t0, t1);
    dst[at(2, 2, s)] = dst[at(1, 0, s)];
    dst[at(2, 0, s)] = avg2(t1, t2);
    dst[at(3, 2, s)] = dst[at(2, 0, s)];
    dst[at(3, 0, s)] = avg2(t2, t3);
    dst[at(0, 1, s)] = avg3(l0, lt, t0);
    dst[at(1, 3, s)] = dst[at(0, 1, s)];
    dst[at(1, 1, s)] = avg3(lt, t0, t1);
    dst[at(2, 3, s)] = dst[at(1, 1, s)];
    dst[at(2, 1, s)] = avg3(t0, t1, t2);
    dst[at(3, 3, s)] = dst[at(2, 1, s)];
    dst[at(3, 1, s)] = avg3(t1, t2, t3);
    dst[at(0, 2, s)] = avg3(lt, l0, l1);
    dst[at(0, 3, s)] = avg3(l0, l1, l2);
}

/// Mode 6: Horizontal-Down.
///
/// Approximately 26.6 degrees from horizontal.
fn pred_4x4_horizontal_down<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    top_left: P,
) {
    let lt = top_left;
    let t0 = top[0];
    let t1 = top[1];
    let t2 = top[2];
    let l0 = left[0];
    let l1 = left[1];
    let l2 = left[2];
    let l3 = left[3];
    let s = stride;

    // Matches FFmpeg pred4x4_horizontal_down exactly
    dst[at(0, 0, s)] = avg2(lt, l0);
    dst[at(2, 1, s)] = dst[at(0, 0, s)];
    dst[at(1, 0, s)] = avg3(l0, lt, t0);
    dst[at(3, 1, s)] = dst[at(1, 0, s)];
    dst[at(2, 0, s)] = avg3(lt, t0, t1);
    dst[at(3, 0, s)] = avg3(t0, t1, t2);
    dst[at(0, 1, s)] = avg2(l0, l1);
    dst[at(2, 2, s)] = dst[at(0, 1, s)];
    dst[at(1, 1, s)] = avg3(lt, l0, l1);
    dst[at(3, 2, s)] = dst[at(1, 1, s)];
    dst[at(0, 2, s)] = avg2(l1, l2);
    dst[at(2, 3, s)] = dst[at(0, 2, s)];
    dst[at(1, 2, s)] = avg3(l0, l1, l2);
    dst[at(3, 3, s)] = dst[at(1, 2, s)];
    dst[at(0, 3, s)] = avg2(l2, l3);
    dst[at(1, 3, s)] = avg3(l1, l2, l3);
}

/// Mode 7: Vertical-Left.
///
/// Mirror of diagonal down-left, uses top[0..7].
fn pred_4x4_vertical_left<P: Pixel>(dst: &mut [P], stride: usize, top: &[P]) {
    let t0 = top[0];
    let t1 = top[1];
    let t2 = top[2];
    let t3 = top[3];
    let t4 = top[4];
    let t5 = top[5];
    let t6 = top[6];
    let s = stride;

    // Matches FFmpeg pred4x4_vertical_left exactly
    dst[at(0, 0, s)] = avg2(t0, t1);
    dst[at(1, 0, s)] = avg2(t1, t2);
    dst[at(0, 2, s)] = dst[at(1, 0, s)];
    dst[at(2, 0, s)] = avg2(t2, t3);
    dst[at(1, 2, s)] = dst[at(2, 0, s)];
    dst[at(3, 0, s)] = avg2(t3, t4);
    dst[at(2, 2, s)] = dst[at(3, 0, s)];
    dst[at(3, 2, s)] = avg2(t4, t5);
    dst[at(0, 1, s)] = avg3(t0, t1, t2);
    dst[at(1, 1, s)] = avg3(t1, t2, t3);
    dst[at(0, 3, s)] = dst[at(1, 1, s)];
    dst[at(2, 1, s)] = avg3(t2, t3, t4);
    dst[at(1, 3, s)] = dst[at(2, 1, s)];
    dst[at(3, 1, s)] = avg3(t3, t4, t5);
    dst[at(2, 3, s)] = dst[at(3, 1, s)];
    dst[at(3, 3, s)] = avg3(t4, t5, t6);
}

/// Mode 8: Horizontal-Up.
///
/// Uses left[0..3] only.
fn pred_4x4_horizontal_up<P: Pixel>(dst: &mut [P], stride: usize, left: &[P]) {
    let l0 = left[0];
    let l1 = left[1];
    let l2 = left[2];
    let l3 = left[3];
    let s = stride;

    // Matches FFmpeg pred4x4_horizontal_up exactly
    dst[at(0, 0, s)] = avg2(l0, l1);
    dst[at(1, 0, s)] = avg3(l0, l1, l2);
    dst[at(2, 0, s)] = avg2(l1, l2);
    dst[at(0, 1, s)] = dst[at(2, 0, s)];
    dst[at(3, 0, s)] = avg3(l1, l2, l3);
    dst[at(1, 1, s)] = dst[at(3, 0, s)];
    dst[at(2, 1, s)] = avg2(l2, l3);
    dst[at(0, 2, s)] = dst[at(2, 1, s)];
    dst[at(3, 1, s)] = avg3(l2, l3, l3);
    dst[at(1, 2, s)] = dst[at(3, 1, s)];
    // Bottom-right region is all l3
    dst[at(3, 2, s)] = l3;
    dst[at(1, 3, s)] = l3;
    dst[at(0, 3, s)] = l3;
    dst[at(2, 2, s)] = l3;
    dst[at(2, 3, s)] = l3;
    dst[at(3, 3, s)] = l3;
}

// ============================================================================
// Intra 8x8 luma prediction (9 modes, High profile)
// ============================================================================

/// Predict an 8x8 luma block using the given intra prediction mode.
///
/// Reference samples must be raw (unfiltered) pixels. This function applies the
/// mandatory 3-tap lowpass filter before dispatching to the mode function.
///
/// `top` must have at least 16 entries (8 top + 8 top-right, replicated if unavail).
/// `left` must have at least 8 entries.
/// `top_left` is the corner sample.
///
/// Reference: FFmpeg h264pred_template.c pred8x8l_* functions.
#[allow(clippy::too_many_arguments)]
pub fn predict_8x8l<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    mode: u8,
    top: &[P],
    left: &[P],
    top_left: P,
    has_top: bool,
    has_left: bool,
    has_top_left: bool,
    has_top_right: bool,
) {
    // Filter reference samples with 3-tap lowpass: (a + 2*b + c + 2) >> 2
    // Reference: FFmpeg h264pred_template.c PREDICT_8x8_LOAD macros

    // Filtered left samples (l0..l7)
    let mut fl = [P::from_i32(0); 8];
    if has_left {
        let tl = if has_top_left { top_left } else { left[0] };
        fl[0] = avg3(tl, left[0], left[1]);
        for i in 1..7 {
            fl[i] = avg3(left[i - 1], left[i], left[i + 1]);
        }
        fl[7] = P::from_i32((left[6].to_i32() + 3 * left[7].to_i32() + 2) >> 2);
    }

    // Filtered top-left sample
    let flt = if has_top_left {
        avg3(left[0], top_left, top[0])
    } else if has_top {
        top[0]
    } else if has_left {
        left[0]
    } else {
        P::from_i32(P::MID)
    };

    // Filtered top samples (t0..t7) and top-right extension (t8..t15)
    let mut ft = [P::from_i32(0); 16];
    if has_top {
        let tl = if has_top_left { top_left } else { top[0] };
        ft[0] = avg3(tl, top[0], top[1]);
        for i in 1..7 {
            ft[i] = avg3(top[i - 1], top[i], top[i + 1]);
        }
        if has_top_right {
            ft[7] = avg3(top[6], top[7], top[8]);
            for i in 8..15 {
                ft[i] = avg3(top[i - 1], top[i], top[i + 1]);
            }
            ft[15] = P::from_i32((top[14].to_i32() + 3 * top[15].to_i32() + 2) >> 2);
        } else {
            // No top-right: filter t7, then replicate the RAW p[7,-1].
            // Spec 8.3.2.2: p[8..15,-1] = p[7,-1] (unfiltered), then
            // filtering in 8.3.2.2.1 yields p'[8..15] = p[7,-1] since
            // avg3(v,v,v) = v. FFmpeg: t8=t9=...=t15=SRC(7,-1) (raw).
            ft[7] = avg3(top[6], top[7], top[7]);
            ft[8..16].fill(top[7]);
        }
    }

    match mode {
        INTRA_4X4_VERTICAL => pred_8x8l_vertical(dst, stride, &ft),
        INTRA_4X4_HORIZONTAL => pred_8x8l_horizontal(dst, stride, &fl),
        INTRA_4X4_DC => pred_8x8l_dc(dst, stride, &ft, &fl, has_top, has_left),
        INTRA_4X4_DIAG_DOWN_LEFT => pred_8x8l_diag_down_left(dst, stride, &ft),
        INTRA_4X4_DIAG_DOWN_RIGHT => pred_8x8l_diag_down_right(dst, stride, &ft, &fl, flt),
        INTRA_4X4_VERT_RIGHT => pred_8x8l_vert_right(dst, stride, &ft, &fl, flt),
        INTRA_4X4_HOR_DOWN => pred_8x8l_hor_down(dst, stride, &ft, &fl, flt),
        INTRA_4X4_VERT_LEFT => pred_8x8l_vert_left(dst, stride, &ft),
        INTRA_4X4_HOR_UP => pred_8x8l_hor_up(dst, stride, &fl),
        _ => {}
    }
}

/// 8x8 mode 0: Vertical — each row copies filtered top[0..7].
fn pred_8x8l_vertical<P: Pixel>(dst: &mut [P], stride: usize, t: &[P]) {
    for y in 0..8 {
        dst[y * stride..y * stride + 8].copy_from_slice(&t[..8]);
    }
}

/// 8x8 mode 1: Horizontal — each column copies filtered left[y].
fn pred_8x8l_horizontal<P: Pixel>(dst: &mut [P], stride: usize, l: &[P]) {
    for (y, &lv) in l[..8].iter().enumerate() {
        let row = y * stride;
        dst[row..row + 8].fill(lv);
    }
}

/// 8x8 mode 2: DC — average of available filtered neighbors.
fn pred_8x8l_dc<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    t: &[P],
    l: &[P],
    has_top: bool,
    has_left: bool,
) {
    let dc = match (has_top, has_left) {
        (true, true) => {
            let sum: u32 = t[..8].iter().map(|&v| v.to_i32() as u32).sum::<u32>()
                + l[..8].iter().map(|&v| v.to_i32() as u32).sum::<u32>();
            P::from_i32(((sum + 8) >> 4) as i32)
        }
        (true, false) => {
            let sum: u32 = t[..8].iter().map(|&v| v.to_i32() as u32).sum();
            P::from_i32(((sum + 4) >> 3) as i32)
        }
        (false, true) => {
            let sum: u32 = l[..8].iter().map(|&v| v.to_i32() as u32).sum();
            P::from_i32(((sum + 4) >> 3) as i32)
        }
        (false, false) => P::from_i32(P::MID),
    };
    for y in 0..8 {
        let row = y * stride;
        dst[row..row + 8].fill(dc);
    }
}

/// 8x8 mode 3: Diagonal down-left.
/// Uses t[0..15] (needs top-right extension).
fn pred_8x8l_diag_down_left<P: Pixel>(dst: &mut [P], stride: usize, t: &[P]) {
    let s = stride;
    for y in 0..8 {
        for x in 0..8 {
            let i = x + y;
            if i < 14 {
                dst[at(x, y, s)] = avg3(t[i], t[i + 1], t[i + 2]);
            } else {
                // i == 14: last position uses edge-biased formula
                dst[at(x, y, s)] = P::from_i32((t[14].to_i32() + 3 * t[15].to_i32() + 2) >> 2);
            }
        }
    }
}

/// 8x8 mode 4: Diagonal down-right.
/// Uses t[0..7], l[0..7], and lt (filtered top-left).
fn pred_8x8l_diag_down_right<P: Pixel>(dst: &mut [P], stride: usize, t: &[P], l: &[P], lt: P) {
    let s = stride;
    for y in 0..8 {
        for x in 0..8 {
            let d = (x as i32) - (y as i32);
            dst[at(x, y, s)] = if d > 1 {
                // Upper-right: uses top samples
                let i = (d - 2) as usize;
                avg3(t[i], t[i + 1], t[i + 2])
            } else if d == 1 {
                avg3(lt, t[0], t[1])
            } else if d == 0 {
                // Main diagonal
                avg3(l[0], lt, t[0])
            } else if d == -1 {
                avg3(lt, l[0], l[1])
            } else {
                // Lower-left: uses left samples
                let i = (-d - 2) as usize;
                avg3(l[i], l[i + 1], l[i + 2])
            };
        }
    }
}

/// 8x8 mode 5: Vertical-right (~26.6° from vertical).
/// Uses t[0..7], l[0..6], and lt.
/// Reference: FFmpeg h264pred_template.c pred8x8l_vertical_right.
fn pred_8x8l_vert_right<P: Pixel>(dst: &mut [P], stride: usize, t: &[P], l: &[P], lt: P) {
    let s = stride;
    for y in 0..8 {
        for x in 0..8 {
            // zVR = 2*x - y
            let zvr = 2 * (x as i32) - (y as i32);
            dst[at(x, y, s)] = if zvr >= 2 {
                let i = (zvr / 2 - 1) as usize;
                if zvr & 1 == 0 {
                    avg2(t[i], t[i + 1])
                } else {
                    avg3(t[i], t[i + 1], t[i + 2])
                }
            } else if zvr == 1 {
                avg3(lt, t[0], t[1])
            } else if zvr == 0 {
                avg2(lt, t[0])
            } else if zvr == -1 {
                avg3(l[0], lt, t[0])
            } else {
                // zvr < -1: uses left samples.
                // Pattern: avg3(l[d-3], l[d-2], l[d-1]) where d = y-2*x, l[-1] = lt
                let li = ((y as i32) - 2 * (x as i32) - 2) as usize;
                if li == 0 {
                    avg3(lt, l[0], l[1])
                } else {
                    avg3(l[li - 1], l[li], l[li + 1])
                }
            };
        }
    }
}

/// 8x8 mode 6: Horizontal-down (~26.6° from horizontal).
/// Uses t[0..6], l[0..7], and lt.
/// Reference: FFmpeg h264pred_template.c pred8x8l_horizontal_down.
fn pred_8x8l_hor_down<P: Pixel>(dst: &mut [P], stride: usize, t: &[P], l: &[P], lt: P) {
    let s = stride;
    for y in 0..8 {
        for x in 0..8 {
            // zHD = 2*y - x
            let zhd = 2 * (y as i32) - (x as i32);
            dst[at(x, y, s)] = if zhd >= 2 {
                let i = (zhd / 2 - 1) as usize;
                if zhd & 1 == 0 {
                    avg2(l[i], l[i + 1])
                } else {
                    avg3(l[i], l[i + 1], l[i + 2])
                }
            } else if zhd == 1 {
                avg3(lt, l[0], l[1])
            } else if zhd == 0 {
                avg2(lt, l[0])
            } else if zhd == -1 {
                avg3(t[0], lt, l[0])
            } else {
                // zhd < -1: uses top samples.
                // Pattern: avg3(t[d-3], t[d-2], t[d-1]) where d = x-2*y, t[-1] = lt
                let ti = ((x as i32) - 2 * (y as i32) - 2) as usize;
                if ti == 0 {
                    avg3(lt, t[0], t[1])
                } else {
                    avg3(t[ti - 1], t[ti], t[ti + 1])
                }
            };
        }
    }
}

/// 8x8 mode 7: Vertical-left (mirror of diag down-left from top).
/// Uses t[0..14].
fn pred_8x8l_vert_left<P: Pixel>(dst: &mut [P], stride: usize, t: &[P]) {
    let s = stride;
    for y in 0..8 {
        for x in 0..8 {
            let i = x + y / 2;
            if y & 1 == 0 {
                dst[at(x, y, s)] = avg2(t[i], t[i + 1]);
            } else {
                dst[at(x, y, s)] = avg3(t[i], t[i + 1], t[i + 2]);
            }
        }
    }
}

/// 8x8 mode 8: Horizontal-up (mirror of diag down-left from left).
/// Uses l[0..7]. Replicates l[7] at the bottom boundary.
/// Reference: FFmpeg h264pred_template.c pred8x8l_horizontal_up.
fn pred_8x8l_hor_up<P: Pixel>(dst: &mut [P], stride: usize, l: &[P]) {
    let s = stride;
    for y in 0..8 {
        for x in 0..8 {
            let i = y + x / 2;
            if i >= 7 {
                dst[at(x, y, s)] = l[7];
            } else if x & 1 == 0 {
                dst[at(x, y, s)] = avg2(l[i], l[i + 1]);
            } else {
                let c = if i + 2 >= 8 { l[7] } else { l[i + 2] };
                dst[at(x, y, s)] = avg3(l[i], l[i + 1], c);
            }
        }
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Compute the DC value for an N×N intra block.
///
/// Averages the available top and/or left neighbors of length N.
/// Falls back to P::MID when neither is available.
#[inline]
fn compute_dc_value<P: Pixel, const N: usize>(
    has_top: bool,
    has_left: bool,
    top: &[P],
    left: &[P],
) -> P {
    match (has_top, has_left) {
        (true, true) => {
            let sum: u32 = top[..N].iter().map(|&v| v.to_i32() as u32).sum::<u32>()
                + left[..N].iter().map(|&v| v.to_i32() as u32).sum::<u32>();
            P::from_i32(((sum + N as u32) >> (N.trailing_zeros() + 1)) as i32)
        }
        (true, false) => {
            let sum: u32 = top[..N].iter().map(|&v| v.to_i32() as u32).sum();
            P::from_i32(((sum + (N / 2) as u32) >> N.trailing_zeros()) as i32)
        }
        (false, true) => {
            let sum: u32 = left[..N].iter().map(|&v| v.to_i32() as u32).sum();
            P::from_i32(((sum + (N / 2) as u32) >> N.trailing_zeros()) as i32)
        }
        (false, false) => P::from_i32(P::MID),
    }
}

/// Fill every cell of an N×N block with `val`.
#[inline]
fn fill_block<P: Pixel, const N: usize>(dst: &mut [P], stride: usize, val: P) {
    for y in 0..N {
        let row = y * stride;
        dst[row..row + N].fill(val);
    }
}

/// Plane prediction for an N×N block (luma 16x16 or chroma 8x8).
///
/// The gradient multiplier, bias and shift differ between block sizes and
/// are supplied as const parameters so the compiler can fold them away:
///
/// | N  | GRAD_MULT | GRAD_ADD | GRAD_SHIFT |
/// |----|-----------|----------|------------|
/// | 16 |     5     |    32    |      6     |
/// |  8 |    17     |    16    |      5     |
///
/// All other constants (pivot = N/2-1, extra_mult = N/2, coef = N/2-1)
/// are derived from N at compile time.
#[inline]
fn plane_pred<
    P: Pixel,
    const N: usize,
    const GRAD_MULT: i32,
    const GRAD_ADD: i32,
    const GRAD_SHIFT: i32,
>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    top_left: P,
) {
    let pivot = (N / 2 - 1) as i32;
    let extra_mult = (N / 2) as i32;
    let last = N - 1;

    let mut h_val: i32 = 0;
    for x in 1..=pivot {
        h_val += x * (top[last / 2 + x as usize].to_i32() - top[last / 2 - x as usize].to_i32());
    }
    h_val += extra_mult * (top[last].to_i32() - top_left.to_i32());

    let mut v_val: i32 = 0;
    for y in 1..=pivot {
        v_val += y * (left[last / 2 + y as usize].to_i32() - left[last / 2 - y as usize].to_i32());
    }
    v_val += extra_mult * (left[last].to_i32() - top_left.to_i32());

    let b = (GRAD_MULT * h_val + GRAD_ADD) >> GRAD_SHIFT;
    let c = (GRAD_MULT * v_val + GRAD_ADD) >> GRAD_SHIFT;
    let a = 16 * (left[last].to_i32() + top[last].to_i32() + 1) - pivot * (b + c);

    for j in 0..N {
        let mut acc = a + c * (j as i32);
        let row = j * stride;
        for i in 0..N {
            dst[row + i] = clip(acc >> 5);
            acc += b;
        }
    }
}

/// Fill an N×N block by repeating `top[0..N]` into every row (vertical pred).
#[inline]
fn fill_vertical<P: Pixel, const N: usize>(dst: &mut [P], stride: usize, top: &[P]) {
    for y in 0..N {
        let row = y * stride;
        dst[row..row + N].copy_from_slice(&top[..N]);
    }
}

/// Fill an N×N block so that row y is entirely `left[y]` (horizontal pred).
#[inline]
fn fill_horizontal<P: Pixel, const N: usize>(dst: &mut [P], stride: usize, left: &[P]) {
    for (y, &val) in left[..N].iter().enumerate() {
        let row = y * stride;
        dst[row..row + N].fill(val);
    }
}

// ============================================================================
// Intra 16x16 prediction (4 modes)
// ============================================================================

/// Predict a 16x16 luma block using the given intra prediction mode.
///
/// `dst` is the output buffer (stride = `stride`).
/// `top` is 16 samples above the block.
/// `left` is 16 samples to the left.
/// `top_left` is the corner sample.
/// `has_top`, `has_left` indicate neighbor availability.
#[allow(clippy::too_many_arguments)] // Parameters match H.264 spec requirements
pub fn predict_16x16<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    mode: u8,
    top: &[P],
    left: &[P],
    top_left: P,
    has_top: bool,
    has_left: bool,
) {
    match mode {
        INTRA_16X16_VERTICAL => pred_16x16_vertical(dst, stride, top),
        INTRA_16X16_HORIZONTAL => pred_16x16_horizontal(dst, stride, left),
        INTRA_16X16_DC => pred_16x16_dc(dst, stride, top, left, has_top, has_left),
        INTRA_16X16_PLANE => pred_16x16_plane(dst, stride, top, left, top_left),
        _ => {}
    }
}

/// Mode 0: Vertical -- each row copies top[0..15].
fn pred_16x16_vertical<P: Pixel>(dst: &mut [P], stride: usize, top: &[P]) {
    fill_vertical::<P, 16>(dst, stride, top);
}

/// Mode 1: Horizontal -- each row filled with left[y].
fn pred_16x16_horizontal<P: Pixel>(dst: &mut [P], stride: usize, left: &[P]) {
    fill_horizontal::<P, 16>(dst, stride, left);
}

/// Mode 2: DC -- average of available top + left samples.
fn pred_16x16_dc<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    has_top: bool,
    has_left: bool,
) {
    let dc = compute_dc_value::<P, 16>(has_top, has_left, top, left);
    fill_block::<P, 16>(dst, stride, dc);
}

/// Mode 3: Plane -- linear interpolation using H and V gradients.
///
/// H = sum(x=1..8) x * (top[7+x] - top[7-x])
/// V = sum(y=1..8) y * (left[7+y] - left[7-y])
/// a = 16 * (top[15] + left[15] + 1) - 7 * (b + c)
/// b = (5*H + 32) >> 6
/// c = (5*V + 32) >> 6
/// pred[y][x] = clip((a + b*x + c*y) >> 5, 0, P::MAX)
fn pred_16x16_plane<P: Pixel>(dst: &mut [P], stride: usize, top: &[P], left: &[P], top_left: P) {
    plane_pred::<P, 16, 5, 32, 6>(dst, stride, top, left, top_left);
}

// ============================================================================
// Chroma intra prediction (4 modes, 8x8 for 4:2:0)
// ============================================================================

/// Predict an 8x8 chroma block using the given intra prediction mode.
///
/// `dst` is the output buffer (stride = `stride`).
/// `top` is 8 samples above the block.
/// `left` is 8 samples to the left.
/// `top_left` is the corner sample.
/// `has_top`, `has_left` indicate neighbor availability.
#[allow(clippy::too_many_arguments)] // Parameters match H.264 spec requirements
pub fn predict_chroma_8x8<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    mode: u8,
    top: &[P],
    left: &[P],
    top_left: P,
    has_top: bool,
    has_left: bool,
) {
    match mode {
        INTRA_CHROMA_DC => pred_chroma_8x8_dc(dst, stride, top, left, has_top, has_left),
        INTRA_CHROMA_HORIZONTAL => pred_chroma_8x8_horizontal(dst, stride, left),
        INTRA_CHROMA_VERTICAL => pred_chroma_8x8_vertical(dst, stride, top),
        INTRA_CHROMA_PLANE => pred_chroma_8x8_plane(dst, stride, top, left, top_left),
        _ => {}
    }
}

/// Chroma Mode 2: Vertical -- each row copies top[0..7].
fn pred_chroma_8x8_vertical<P: Pixel>(dst: &mut [P], stride: usize, top: &[P]) {
    fill_vertical::<P, 8>(dst, stride, top);
}

/// Chroma Mode 1: Horizontal -- each row filled with left[y].
fn pred_chroma_8x8_horizontal<P: Pixel>(dst: &mut [P], stride: usize, left: &[P]) {
    fill_horizontal::<P, 8>(dst, stride, left);
}

/// Chroma Mode 0: DC -- splits the 8x8 block into 4 quadrants (4x4 each).
///
/// The DC value for each quadrant depends on which neighbors are available.
/// This matches FFmpeg's pred8x8_dc / pred8x8_left_dc / pred8x8_top_dc /
/// pred8x8_128_dc behavior:
///
/// When both top and left are available:
///   top-left quadrant (0..3, 0..3):  dc0 = avg(top\[0..3\] + left\[0..3\])
///   top-right quadrant (4..7, 0..3): dc1 = avg(top\[4..7\])
///   bot-left quadrant (0..3, 4..7):  dc2 = avg(left\[4..7\])
///   bot-right quadrant (4..7, 4..7): dc3 = avg(dc1_raw + dc2_raw)
///
/// When only left available:
///   top half uses avg(left\[0..3\]), bottom half uses avg(left\[4..7\]),
///   applied uniformly across the full width.
///
/// When only top available:
///   left half uses avg(top\[0..3\]), right half uses avg(top\[4..7\]),
///   applied uniformly across the full height.
///
/// When neither available: all 128.
fn pred_chroma_8x8_dc<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    has_top: bool,
    has_left: bool,
) {
    match (has_top, has_left) {
        (true, true) => {
            let mut dc0: u32 = 0;
            let mut dc1: u32 = 0;
            let mut dc2: u32 = 0;
            for i in 0..4 {
                dc0 += (top[i].to_i32() as u32) + (left[i].to_i32() as u32);
                dc1 += top[4 + i].to_i32() as u32;
                dc2 += left[4 + i].to_i32() as u32;
            }
            let dc0_val = P::from_i32(((dc0 + 4) >> 3) as i32);
            let dc1_val = P::from_i32(((dc1 + 2) >> 2) as i32);
            let dc2_val = P::from_i32(((dc2 + 2) >> 2) as i32);
            let dc3_val = P::from_i32(((dc1 + dc2 + 4) >> 3) as i32);

            fill_quadrants(dst, stride, dc0_val, dc1_val, dc2_val, dc3_val);
        }
        (false, true) => {
            let mut dc0: u32 = 0;
            let mut dc2: u32 = 0;
            for i in 0..4 {
                dc0 += left[i].to_i32() as u32;
                dc2 += left[4 + i].to_i32() as u32;
            }
            let dc0_val = P::from_i32(((dc0 + 2) >> 2) as i32);
            let dc2_val = P::from_i32(((dc2 + 2) >> 2) as i32);

            fill_quadrants(dst, stride, dc0_val, dc0_val, dc2_val, dc2_val);
        }
        (true, false) => {
            let mut dc0: u32 = 0;
            let mut dc1: u32 = 0;
            for i in 0..4 {
                dc0 += top[i].to_i32() as u32;
                dc1 += top[4 + i].to_i32() as u32;
            }
            let dc0_val = P::from_i32(((dc0 + 2) >> 2) as i32);
            let dc1_val = P::from_i32(((dc1 + 2) >> 2) as i32);

            fill_quadrants(dst, stride, dc0_val, dc1_val, dc0_val, dc1_val);
        }
        (false, false) => {
            for y in 0..8 {
                let row = y * stride;
                for x in 0..8 {
                    dst[row + x] = P::from_i32(P::MID);
                }
            }
        }
    }
}

/// Fill the 8x8 block as four 4x4 quadrants with the given DC values.
fn fill_quadrants<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top_left_dc: P,
    top_right_dc: P,
    bot_left_dc: P,
    bot_right_dc: P,
) {
    for y in 0..4 {
        let row = y * stride;
        for x in 0..4 {
            dst[row + x] = top_left_dc;
        }
        for x in 4..8 {
            dst[row + x] = top_right_dc;
        }
    }
    for y in 4..8 {
        let row = y * stride;
        for x in 0..4 {
            dst[row + x] = bot_left_dc;
        }
        for x in 4..8 {
            dst[row + x] = bot_right_dc;
        }
    }
}

/// Chroma Mode 3: Plane -- linear interpolation for 8x8 chroma block.
///
/// H = sum(x=1..4) x * (top[3+x] - top[3-x])
/// V = sum(y=1..4) y * (left[3+y] - left[3-y])
/// a = 16 * (top[7] + left[7] + 1) - 3 * (b + c)
/// b = (17*H + 16) >> 5
/// c = (17*V + 16) >> 5
/// pred[y][x] = clip((a + b*x + c*y) >> 5, 0, P::MAX)
fn pred_chroma_8x8_plane<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    top: &[P],
    left: &[P],
    top_left: P,
) {
    plane_pred::<P, 8, 17, 16, 5>(dst, stride, top, left, top_left);
}

// ============================================================================
// Chroma 8x16 prediction (4:2:2)
// ============================================================================

/// Predict an 8x16 chroma block (4:2:2 chroma has full vertical resolution).
///
/// `top` holds 8 samples above the block, `left` holds 16 samples to its left.
/// FFmpeg selects these via `hpc.pred8x8[]` with `chroma_y_shift == 0`
/// (`FUNCC(pred8x16_*)` in h264pred_template.c).
#[allow(clippy::too_many_arguments)] // Parameters match H.264 spec requirements
pub fn predict_chroma_8x16<P: Pixel>(
    dst: &mut [P],
    stride: usize,
    mode: u8,
    top: &[P],
    left: &[P],
    top_left: P,
    has_top: bool,
    has_left: bool,
) {
    match mode {
        INTRA_CHROMA_DC => {
            let mut dc0 = 0i32;
            let mut dc1 = 0i32;
            let mut dc2 = 0i32;
            let mut dc3 = 0i32;
            let mut dc4 = 0i32;
            if has_top {
                for i in 0..4 {
                    dc0 += top[i].to_i32() + if has_left { left[i].to_i32() } else { 0 };
                    dc1 += top[4 + i].to_i32();
                }
            }
            if has_left {
                for i in 0..8 {
                    match i {
                        0..=3 => dc2 += left[4 + i].to_i32(),
                        4..=7 => dc3 += left[i].to_i32(),
                        _ => {}
                    }
                }
                for i in 0..4 {
                    dc4 += left[12 + i].to_i32();
                }
            }
            for y in 0..16usize {
                let row = y * stride;
                let (lv, rv) = match y {
                    0..=3 => (
                        (dc0 + 4) >> 3,
                        if has_top { (dc1 + 2) >> 2 } else { P::MID },
                    ),
                    4..=7 => (
                        if has_left { (dc2 + 2) >> 2 } else { P::MID },
                        if has_top && has_left {
                            (dc1 + dc2 + 4) >> 3
                        } else if has_top {
                            (dc1 + 2) >> 2
                        } else {
                            P::MID
                        },
                    ),
                    8..=11 => (
                        if has_left {
                            ((if has_top { dc3 } else { dc0 }) + 2) >> 2
                        } else {
                            P::MID
                        },
                        if has_left {
                            (dc1 + (if has_top { dc3 } else { dc0 }) + 4) >> 3
                        } else {
                            P::MID
                        },
                    ),
                    _ => (
                        if has_left {
                            ((if has_top { dc4 } else { dc2 }) + 2) >> 2
                        } else {
                            P::MID
                        },
                        if has_left {
                            (dc1 + (if has_top { dc4 } else { dc2 }) + 4) >> 3
                        } else {
                            P::MID
                        },
                    ),
                };
                dst[row..row + 4].fill(P::from_i32(lv));
                dst[row + 4..row + 8].fill(P::from_i32(rv));
            }
        }
        INTRA_CHROMA_HORIZONTAL => {
            for y in 0..16usize {
                dst[y * stride..y * stride + 8].fill(left[y]);
            }
        }
        INTRA_CHROMA_VERTICAL => {
            for y in 0..16usize {
                dst[y * stride..y * stride + 8].copy_from_slice(&top[..8]);
            }
        }
        INTRA_CHROMA_PLANE => {
            // FFmpeg FUNCC(pred8x16_plane).
            let mut h = top[4].to_i32() - top[2].to_i32();
            let mut v = left[7].to_i32() - left[5].to_i32();
            for k in 2..=4usize {
                let left_top = if k == 4 {
                    top_left.to_i32()
                } else {
                    top[3 - k].to_i32()
                };
                h += k as i32 * (top[3 + k].to_i32() - left_top);
                v += k as i32 * (left[7 + k].to_i32() - left[7 - k].to_i32());
            }
            for k in 5..=8usize {
                let above = if k == 8 {
                    top_left.to_i32()
                } else {
                    left[7 - k].to_i32()
                };
                v += k as i32 * (left[7 + k].to_i32() - above);
            }
            let h = (17 * h + 16) >> 5;
            let v = (5 * v + 32) >> 6;
            let mut a = 16 * (left[15].to_i32() + top[7].to_i32() + 1) - 7 * v - 3 * h;
            for y in 0..16usize {
                let mut b = a;
                a += v;
                let row = y * stride;
                for x in 0..8usize {
                    dst[row + x] = P::from_i32(b >> 5);
                    b += h;
                }
            }
        }
        _ => {}
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dst(stride: usize, rows: usize) -> Vec<u8> {
        vec![0u8; stride * rows]
    }

    fn extract_block(dst: &[u8], stride: usize, w: usize, h: usize) -> Vec<Vec<u8>> {
        (0..h)
            .map(|y| dst[y * stride..y * stride + w].to_vec())
            .collect()
    }

    // ----- 4x4 tests -----

    #[test]
    fn test_4x4_vertical() {
        let mut dst = make_dst(8, 4);
        let top = [10, 20, 30, 40, 50, 60, 70, 80];
        let left = [0; 4];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_VERTICAL,
            &top,
            &left,
            0,
            true,
            false,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        for row in &block {
            assert_eq!(row, &[10, 20, 30, 40]);
        }
    }

    #[test]
    fn test_4x4_horizontal() {
        let mut dst = make_dst(8, 4);
        let top = [0; 8];
        let left = [11, 22, 33, 44];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_HORIZONTAL,
            &top,
            &left,
            0,
            false,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        assert_eq!(block[0], [11, 11, 11, 11]);
        assert_eq!(block[1], [22, 22, 22, 22]);
        assert_eq!(block[2], [33, 33, 33, 33]);
        assert_eq!(block[3], [44, 44, 44, 44]);
    }

    #[test]
    fn test_4x4_dc_both() {
        let mut dst = make_dst(8, 4);
        let top = [8, 8, 8, 8, 0, 0, 0, 0];
        let left = [8, 8, 8, 8];

        predict_4x4(&mut dst, 8, INTRA_4X4_DC, &top, &left, 0, true, true, false);

        let block = extract_block(&dst, 8, 4, 4);
        for row in &block {
            assert_eq!(row, &[8, 8, 8, 8]);
        }
    }

    #[test]
    fn test_4x4_dc_top_only() {
        let mut dst = make_dst(8, 4);
        let top = [12, 16, 20, 24, 0, 0, 0, 0];
        let left = [0; 4];
        // sum = 72, dc = (72+2)>>2 = 18

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DC,
            &top,
            &left,
            0,
            true,
            false,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        for row in &block {
            assert_eq!(row, &[18, 18, 18, 18]);
        }
    }

    #[test]
    fn test_4x4_dc_left_only() {
        let mut dst = make_dst(8, 4);
        let top = [0; 8];
        let left = [4, 8, 12, 16];
        // sum = 40, dc = (40+2)>>2 = 10

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DC,
            &top,
            &left,
            0,
            false,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        for row in &block {
            assert_eq!(row, &[10, 10, 10, 10]);
        }
    }

    #[test]
    fn test_4x4_dc_none() {
        let mut dst = make_dst(8, 4);
        let top = [0; 8];
        let left = [0; 4];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DC,
            &top,
            &left,
            0,
            false,
            false,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        for row in &block {
            assert_eq!(row, &[128, 128, 128, 128]);
        }
    }

    #[test]
    fn test_4x4_diag_down_left() {
        let mut dst = make_dst(8, 4);
        let top = [0, 4, 8, 12, 16, 20, 24, 28];
        let left = [0; 4];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DIAG_DOWN_LEFT,
            &top,
            &left,
            0,
            true,
            false,
            true,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // For evenly spaced values (step 4): avg3(a, b, c) = b
        assert_eq!(block[0][0], 4);
        assert_eq!(block[0][1], 8);
        assert_eq!(block[0][2], 12);
        assert_eq!(block[0][3], 16);
        // Diagonal property: block[y][x] == block[y-1][x+1]
        assert_eq!(block[1][0], block[0][1]);
        assert_eq!(block[2][0], block[0][2]);
        assert_eq!(block[3][0], block[0][3]);
    }

    #[test]
    fn test_4x4_diag_down_right() {
        let mut dst = make_dst(8, 4);
        let top = [100, 104, 108, 112, 0, 0, 0, 0];
        let left = [96, 92, 88, 84];
        let top_left = 98u8;

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DIAG_DOWN_RIGHT,
            &top,
            &left,
            top_left,
            true,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // Main diagonal: all values equal
        assert_eq!(block[0][0], block[1][1]);
        assert_eq!(block[1][1], block[2][2]);
        assert_eq!(block[2][2], block[3][3]);
    }

    #[test]
    fn test_4x4_horizontal_up() {
        let mut dst = make_dst(8, 4);
        let top = [0; 8];
        let left = [10, 20, 30, 40];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_HOR_UP,
            &top,
            &left,
            0,
            false,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // row 0: avg2(10,20)=15, avg3(10,20,30)=20, avg2(20,30)=25, avg3(20,30,40)=30
        assert_eq!(block[0][0], 15);
        assert_eq!(block[0][1], 20);
        assert_eq!(block[0][2], 25);
        assert_eq!(block[0][3], 30);
        // Bottom row: all l3=40
        assert_eq!(block[3], [40, 40, 40, 40]);
    }

    #[test]
    fn test_4x4_vertical_right() {
        let mut dst = make_dst(8, 4);
        let top = [40, 50, 60, 70, 0, 0, 0, 0];
        let left = [30, 20, 10, 0];
        let top_left = 35u8;

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_VERT_RIGHT,
            &top,
            &left,
            top_left,
            true,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // row 0, col 0: avg2(lt, t0) = avg2(35, 40) = 38
        assert_eq!(block[0][0], 38);
        // row 0, col 3: avg2(t2, t3) = avg2(60, 70) = 65
        assert_eq!(block[0][3], 65);
    }

    #[test]
    fn test_4x4_horizontal_down() {
        let mut dst = make_dst(8, 4);
        let top = [50, 60, 70, 80, 0, 0, 0, 0];
        let left = [40, 30, 20, 10];
        let top_left = 45u8;

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_HOR_DOWN,
            &top,
            &left,
            top_left,
            true,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // row 0, col 0: avg2(lt, l0) = avg2(45, 40) = 43
        assert_eq!(block[0][0], 43);
        // row 3, col 0: avg2(l2, l3) = avg2(20, 10) = 15
        assert_eq!(block[3][0], 15);
    }

    #[test]
    fn test_4x4_vertical_left() {
        let mut dst = make_dst(8, 4);
        let top = [10, 20, 30, 40, 50, 60, 70, 80];
        let left = [0; 4];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_VERT_LEFT,
            &top,
            &left,
            0,
            true,
            false,
            true,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // row 0: avg2(10,20)=15, avg2(20,30)=25, avg2(30,40)=35, avg2(40,50)=45
        assert_eq!(block[0], [15, 25, 35, 45]);
        // row 1: avg3(10,20,30)=20, avg3(20,30,40)=30, avg3(30,40,50)=40, avg3(40,50,60)=50
        assert_eq!(block[1], [20, 30, 40, 50]);
    }

    // ----- 16x16 tests -----

    #[test]
    fn test_16x16_vertical() {
        let mut dst = make_dst(32, 16);
        let top: Vec<u8> = (0..16).map(|i| (i * 10) as u8).collect();
        let left = [0u8; 16];

        predict_16x16(
            &mut dst,
            32,
            INTRA_16X16_VERTICAL,
            &top,
            &left,
            0,
            true,
            false,
        );

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 32 + x], top[x], "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn test_16x16_horizontal() {
        let mut dst = make_dst(32, 16);
        let top = [0u8; 16];
        let left: Vec<u8> = (0..16).map(|i| (i * 15) as u8).collect();

        predict_16x16(
            &mut dst,
            32,
            INTRA_16X16_HORIZONTAL,
            &top,
            &left,
            0,
            false,
            true,
        );

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 32 + x], left[y], "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn test_16x16_dc_both() {
        let mut dst = make_dst(32, 16);
        let top = [100u8; 16];
        let left = [100u8; 16];

        predict_16x16(&mut dst, 32, INTRA_16X16_DC, &top, &left, 0, true, true);

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 32 + x], 100);
            }
        }
    }

    #[test]
    fn test_16x16_dc_none() {
        let mut dst = make_dst(32, 16);
        let top = [0u8; 16];
        let left = [0u8; 16];

        predict_16x16(&mut dst, 32, INTRA_16X16_DC, &top, &left, 0, false, false);

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 32 + x], 128);
            }
        }
    }

    #[test]
    fn test_16x16_dc_top_only() {
        let mut dst = make_dst(32, 16);
        let top = [64u8; 16];
        let left = [0u8; 16];

        predict_16x16(&mut dst, 32, INTRA_16X16_DC, &top, &left, 0, true, false);

        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 32 + x], 64);
            }
        }
    }

    #[test]
    fn test_16x16_plane_uniform() {
        let mut dst = make_dst(32, 16);
        let top = [128u8; 16];
        let left = [128u8; 16];
        let top_left = 128u8;

        predict_16x16(
            &mut dst,
            32,
            INTRA_16X16_PLANE,
            &top,
            &left,
            top_left,
            true,
            true,
        );

        // H = 0, V = 0, b = 0, c = 0
        // a = 16*(128 + 128 + 1) = 4112
        // pred = 4112 >> 5 = 128
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 32 + x], 128, "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn test_16x16_plane_gradient() {
        let mut dst = make_dst(32, 16);
        let top: Vec<u8> = (0..16).collect();
        let left: Vec<u8> = (0..16).collect();
        let top_left = 0u8;

        predict_16x16(
            &mut dst,
            32,
            INTRA_16X16_PLANE,
            &top,
            &left,
            top_left,
            true,
            true,
        );

        // H = sum(x=1..7) x*2x + 8*(15-0) = 2*140 + 120 = 400
        // V = 400, b = (5*400+32)>>6 = 31, c = 31
        // a = 16*(15+15+1) - 7*(31+31) = 496 - 434 = 62
        // pred[0][0] = 62 >> 5 = 1
        assert_eq!(dst[0], 1);
        // pred[0][1] = (62 + 31) >> 5 = 93 >> 5 = 2
        assert_eq!(dst[1], 2);
        // pred[7][7] = (62 + 31*7 + 31*7) >> 5 = 496 >> 5 = 15
        assert_eq!(dst[7 * 32 + 7], 15);
    }

    // ----- Chroma 8x8 tests -----

    #[test]
    fn test_chroma_vertical() {
        let mut dst = make_dst(16, 8);
        let top = [10, 20, 30, 40, 50, 60, 70, 80];
        let left = [0u8; 8];

        predict_chroma_8x8(
            &mut dst,
            16,
            INTRA_CHROMA_VERTICAL,
            &top,
            &left,
            0,
            true,
            false,
        );

        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(dst[y * 16 + x], top[x], "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn test_chroma_horizontal() {
        let mut dst = make_dst(16, 8);
        let top = [0u8; 8];
        let left = [11, 22, 33, 44, 55, 66, 77, 88];

        predict_chroma_8x8(
            &mut dst,
            16,
            INTRA_CHROMA_HORIZONTAL,
            &top,
            &left,
            0,
            false,
            true,
        );

        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(dst[y * 16 + x], left[y], "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn test_chroma_dc_both() {
        let mut dst = make_dst(16, 8);
        let top = [8, 8, 8, 8, 16, 16, 16, 16];
        let left = [8, 8, 8, 8, 16, 16, 16, 16];

        predict_chroma_8x8(&mut dst, 16, INTRA_CHROMA_DC, &top, &left, 0, true, true);

        let block = extract_block(&dst, 16, 8, 8);
        // Top-left: dc0 = (32+32+4)>>3 = 8
        assert_eq!(block[0][0], 8);
        assert_eq!(block[3][3], 8);
        // Top-right: dc1 = (64+2)>>2 = 16
        assert_eq!(block[0][4], 16);
        // Bottom-left: dc2 = (64+2)>>2 = 16
        assert_eq!(block[4][0], 16);
        // Bottom-right: dc3 = (64+64+4)>>3 = 16
        assert_eq!(block[7][7], 16);
    }

    #[test]
    fn test_chroma_dc_none() {
        let mut dst = make_dst(16, 8);
        let top = [0u8; 8];
        let left = [0u8; 8];

        predict_chroma_8x8(&mut dst, 16, INTRA_CHROMA_DC, &top, &left, 0, false, false);

        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(dst[y * 16 + x], 128);
            }
        }
    }

    #[test]
    fn test_chroma_dc_top_only() {
        let mut dst = make_dst(16, 8);
        let top = [20, 20, 20, 20, 40, 40, 40, 40];
        let left = [0u8; 8];

        predict_chroma_8x8(&mut dst, 16, INTRA_CHROMA_DC, &top, &left, 0, true, false);

        let block = extract_block(&dst, 16, 8, 8);
        // All rows: left half = 20, right half = 40
        for (y, row) in block.iter().enumerate() {
            for (x, &val) in row[..4].iter().enumerate() {
                assert_eq!(val, 20, "mismatch at ({x}, {y})");
            }
            for (x, &val) in row[4..8].iter().enumerate() {
                assert_eq!(val, 40, "mismatch at ({}, {y})", x + 4);
            }
        }
    }

    #[test]
    fn test_chroma_dc_left_only() {
        let mut dst = make_dst(16, 8);
        let top = [0u8; 8];
        let left = [30, 30, 30, 30, 60, 60, 60, 60];

        predict_chroma_8x8(&mut dst, 16, INTRA_CHROMA_DC, &top, &left, 0, false, true);

        let block = extract_block(&dst, 16, 8, 8);
        // Top 4 rows: all 30, bottom 4 rows: all 60
        for (y, row) in block[..4].iter().enumerate() {
            for (x, &val) in row.iter().enumerate() {
                assert_eq!(val, 30, "mismatch at ({x}, {y})");
            }
        }
        for (y, row) in block[4..8].iter().enumerate() {
            for (x, &val) in row.iter().enumerate() {
                assert_eq!(val, 60, "mismatch at ({}, {})", x, y + 4);
            }
        }
    }

    #[test]
    fn test_chroma_plane_uniform() {
        let mut dst = make_dst(16, 8);
        let top = [128u8; 8];
        let left = [128u8; 8];
        let top_left = 128u8;

        predict_chroma_8x8(
            &mut dst,
            16,
            INTRA_CHROMA_PLANE,
            &top,
            &left,
            top_left,
            true,
            true,
        );

        // H = 0, V = 0, b = 0, c = 0
        // a = 16*(128+128+1) = 4112
        // pred = 4112 >> 5 = 128
        for y in 0..8 {
            for x in 0..8 {
                assert_eq!(dst[y * 16 + x], 128, "mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn test_chroma_plane_gradient() {
        let mut dst = make_dst(16, 8);
        let top: Vec<u8> = (0..8).collect();
        let left: Vec<u8> = (0..8).collect();
        let top_left = 0u8;

        predict_chroma_8x8(
            &mut dst,
            16,
            INTRA_CHROMA_PLANE,
            &top,
            &left,
            top_left,
            true,
            true,
        );

        // H = 1*(4-2) + 2*(5-1) + 3*(6-0) + 4*(7-0) = 2+8+18+28 = 56
        // V = 56, b = (17*56+16)>>5 = 30, c = 30
        // a = 16*(7+7+1) - 3*(30+30) = 240 - 180 = 60
        // pred[0][0] = 60>>5 = 1
        assert_eq!(dst[0], 1);
        // pred[0][1] = (60+30)>>5 = 2
        assert_eq!(dst[1], 2);
    }

    #[test]
    fn test_avg3() {
        assert_eq!(avg3(0u8, 0u8, 0u8), 0);
        assert_eq!(avg3(0u8, 128u8, 0u8), 64);
        assert_eq!(avg3(100u8, 100u8, 100u8), 100);
        assert_eq!(avg3(255u8, 255u8, 255u8), 255);
        assert_eq!(avg3(10u8, 20u8, 30u8), 20);
    }

    #[test]
    fn test_avg2() {
        assert_eq!(avg2(0u8, 0u8), 0);
        assert_eq!(avg2(0u8, 1u8), 1);
        assert_eq!(avg2(1u8, 0u8), 1);
        assert_eq!(avg2(254u8, 255u8), 255);
        assert_eq!(avg2(10u8, 20u8), 15);
    }

    #[test]
    fn test_clip_function() {
        assert_eq!(clip::<u8>(0), 0);
        assert_eq!(clip::<u8>(128), 128);
        assert_eq!(clip::<u8>(255), 255);
        assert_eq!(clip::<u8>(256), 255);
        assert_eq!(clip::<u8>(-1), 0);
        assert_eq!(clip::<u8>(1000), 255);
        assert_eq!(clip::<u8>(-1000), 0);
    }

    // ----- Cross-validation: verify diagonal symmetry properties -----

    #[test]
    fn test_4x4_diag_down_left_anti_diagonal() {
        let mut dst = make_dst(8, 4);
        let top = [10, 30, 50, 70, 90, 110, 130, 150];
        let left = [0; 4];

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DIAG_DOWN_LEFT,
            &top,
            &left,
            0,
            true,
            false,
            true,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // Anti-diagonal: block[0][1] == block[1][0]
        assert_eq!(block[0][1], block[1][0]);
        assert_eq!(block[0][2], block[1][1]);
        assert_eq!(block[1][1], block[2][0]);
        assert_eq!(block[0][3], block[1][2]);
        assert_eq!(block[1][2], block[2][1]);
        assert_eq!(block[2][1], block[3][0]);
    }

    #[test]
    fn test_4x4_diag_down_right_diagonal() {
        let mut dst = make_dst(8, 4);
        let top = [100, 110, 120, 130, 0, 0, 0, 0];
        let left = [90, 80, 70, 60];
        let top_left = 95u8;

        predict_4x4(
            &mut dst,
            8,
            INTRA_4X4_DIAG_DOWN_RIGHT,
            &top,
            &left,
            top_left,
            true,
            true,
            false,
        );

        let block = extract_block(&dst, 8, 4, 4);
        // Main diagonal: all equal
        assert_eq!(block[0][0], block[1][1]);
        assert_eq!(block[1][1], block[2][2]);
        assert_eq!(block[2][2], block[3][3]);
        // Sub-diagonal
        assert_eq!(block[1][0], block[2][1]);
        assert_eq!(block[2][1], block[3][2]);
    }
}
