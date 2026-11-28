// VP9 motion compensation.
//
// 8-tap subpel interpolation, edge emulation, and inter prediction dispatch.
// Translated from FFmpeg's vp9recon.c (mc_luma_unscaled, mc_chroma_unscaled),
// vp9_mc_template.c (inter_pred), and vp9dsp_template.c (8-tap filter kernels).
// The kernels are generic over the pixel container: the 8-bit path keeps the
// runtime-dispatched SIMD kernels via `crate::simd` (SSE2/AVX2/NEON, scalar
// fallback) so its output stays bit-exact; the 10/12-bit path uses scalar i32
// sums clipped by `av_clip_pixel(max = (1 << bit_depth) - 1)`, matching the
// `pixel = uint16_t` instantiation of the same FFmpeg template.
//
// Only unscaled prediction is implemented. Scaled prediction (when ref
// dimensions differ from the current frame) is not implemented. The generic
// `inter_pred` entry point now dispatches on the frame's sample container, so
// both 8-bit and high-depth frames use the same prediction structure.
//
// LGPL-2.1-or-later - same licence as FFmpeg.

use std::sync::Arc;

use crate::block::BlockInfo;
use crate::data::{BWH_TAB, VP9_SUBPEL_FILTERS};
use crate::intra_pred::Sample;
use crate::recon::PlaneSample;
use crate::refs::RefFrame;
use crate::types::BlockSize;

// ---------------------------------------------------------------------------
// Pixel container dispatch
// ---------------------------------------------------------------------------

/// Selects the MC filter implementation for a pixel container.
///
/// FFmpeg instantiates `vp9dsp_template.c` once per bit depth with `pixel` =
/// `uint8_t` (8-bit) or `uint16_t` (10/12-bit). Both compute the 8-tap sum in
/// `int` and clip via `av_clip_pixel(max = (1 << bit_depth) - 1)`. The 8-bit
/// container also has hand-written SIMD kernels; those are preserved here for
/// bit-exactness, while 16-bit samples use the shared scalar kernels below.
pub trait McSample: Sample {
    /// Horizontal 8-tap put filter.
    // Mirrors FFmpeg's DSP ABI: dst/src pointers, two strides, w/h, taps, depth.
    #[allow(clippy::too_many_arguments)]
    fn filter_h(
        dst: &mut [Self],
        dst_stride: usize,
        src: &[Self],
        src_stride: usize,
        w: usize,
        h: usize,
        filter: &[i16; 8],
        bit_depth: u8,
    );

    /// Vertical 8-tap put filter.
    // Mirrors FFmpeg's DSP ABI; see filter_h.
    #[allow(clippy::too_many_arguments)]
    fn filter_v(
        dst: &mut [Self],
        dst_stride: usize,
        src: &[Self],
        src_stride: usize,
        w: usize,
        h: usize,
        filter: &[i16; 8],
        bit_depth: u8,
    );

    /// 2D 8-tap put filter (horizontal pass then vertical pass).
    // Mirrors FFmpeg's DSP ABI plus a second filter array.
    #[allow(clippy::too_many_arguments)]
    fn filter_hv(
        dst: &mut [Self],
        dst_stride: usize,
        src: &[Self],
        src_stride: usize,
        w: usize,
        h: usize,
        h_filter: &[i16; 8],
        v_filter: &[i16; 8],
        bit_depth: u8,
    );

    /// Compound average: `dst[i] = (dst[i] + src[i] + 1) >> 1`.
    fn avg(
        dst: &mut [Self],
        dst_stride: usize,
        src: &[Self],
        src_stride: usize,
        w: usize,
        h: usize,
    );
}

impl McSample for u8 {
    // bit_depth is always 8 for this container; keep the SIMD-dispatched
    // kernels so 8-bit output remains identical to the pre-u16 path.
    fn filter_h(
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        w: usize,
        h: usize,
        filter: &[i16; 8],
        _bit_depth: u8,
    ) {
        crate::simd::filter_h_8tap(dst, dst_stride, src, src_stride, w, h, filter);
    }

    fn filter_v(
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        w: usize,
        h: usize,
        filter: &[i16; 8],
        _bit_depth: u8,
    ) {
        crate::simd::filter_v_8tap(dst, dst_stride, src, src_stride, w, h, filter);
    }

    fn filter_hv(
        dst: &mut [u8],
        dst_stride: usize,
        src: &[u8],
        src_stride: usize,
        w: usize,
        h: usize,
        h_filter: &[i16; 8],
        v_filter: &[i16; 8],
        _bit_depth: u8,
    ) {
        // FFmpeg's do_8tap_2d_c uses `pixel tmp[64 * 71]`: the horizontal pass
        // writes h+7 rows (max 71), then the vertical pass reads 8 rows with
        // stride 64. SIMD loads touch 8 columns per lane, so use 64*72 zeroed
        // bytes of scratch to stay in bounds.
        let mut tmp = [0u8; 64 * 72];
        crate::simd::filter_hv_8tap(
            dst, dst_stride, src, src_stride, w, h, h_filter, v_filter, &mut tmp,
        );
    }

    fn avg(dst: &mut [u8], dst_stride: usize, src: &[u8], src_stride: usize, w: usize, h: usize) {
        crate::simd::avg_block(dst, dst_stride, src, src_stride, w, h);
    }
}

impl McSample for u16 {
    fn filter_h(
        dst: &mut [u16],
        dst_stride: usize,
        src: &[u16],
        src_stride: usize,
        w: usize,
        h: usize,
        filter: &[i16; 8],
        bit_depth: u8,
    ) {
        filter_h_scalar(dst, dst_stride, src, src_stride, w, h, filter, bit_depth);
    }

    fn filter_v(
        dst: &mut [u16],
        dst_stride: usize,
        src: &[u16],
        src_stride: usize,
        w: usize,
        h: usize,
        filter: &[i16; 8],
        bit_depth: u8,
    ) {
        filter_v_scalar(dst, dst_stride, src, src_stride, w, h, filter, bit_depth);
    }

    fn filter_hv(
        dst: &mut [u16],
        dst_stride: usize,
        src: &[u16],
        src_stride: usize,
        w: usize,
        h: usize,
        h_filter: &[i16; 8],
        v_filter: &[i16; 8],
        bit_depth: u8,
    ) {
        let mut tmp = [0u16; 64 * 72];
        filter_hv_scalar(
            dst, dst_stride, src, src_stride, w, h, h_filter, v_filter, &mut tmp, bit_depth,
        );
    }

    fn avg(dst: &mut [u16], dst_stride: usize, src: &[u16], src_stride: usize, w: usize, h: usize) {
        avg_scalar(dst, dst_stride, src, src_stride, w, h);
    }
}

// ---------------------------------------------------------------------------
// Scalar 8-tap kernels (u16 high depth)
// ---------------------------------------------------------------------------

/// FFmpeg's `FILTER_8TAP`: `(sum(tap[i] * src[x + i*stride]) + 64) >> 7`,
/// clipped to `[0, (1 << bit_depth) - 1]` by `av_clip_pixel`.
///
/// `src` is already offset by the -3 margin so tap 3 is the destination pixel.
#[inline]
fn filter_8tap<P: Sample>(
    src: &[P],
    x: usize,
    stride: usize,
    filter: &[i16; 8],
    max_val: i32,
) -> P {
    let mut sum = 64i32;
    for i in 0..8 {
        sum = sum.wrapping_add((filter[i] as i32).wrapping_mul(src[x + i * stride].to_i32()));
    }
    P::from_i32((sum >> 7).clamp(0, max_val))
}

#[allow(clippy::too_many_arguments)] // FFmpeg DSP ABI: dst/src, strides, w/h, taps, depth.
fn filter_h_scalar<P: Sample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    filter: &[i16; 8],
    bit_depth: u8,
) {
    let max_val = (1_i32 << bit_depth) - 1;
    for y in 0..h {
        let src_row = &src[y * src_stride..];
        let dst_row = &mut dst[y * dst_stride..];
        for x in 0..w {
            dst_row[x] = filter_8tap(src_row, x, 1, filter, max_val);
        }
    }
}

#[allow(clippy::too_many_arguments)] // FFmpeg DSP ABI: dst/src, strides, w/h, taps, depth.
fn filter_v_scalar<P: Sample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    filter: &[i16; 8],
    bit_depth: u8,
) {
    let max_val = (1_i32 << bit_depth) - 1;
    for y in 0..h {
        let src_row = &src[y * src_stride..];
        let dst_row = &mut dst[y * dst_stride..];
        for x in 0..w {
            dst_row[x] = filter_8tap(src_row, x, src_stride, filter, max_val);
        }
    }
}

#[allow(clippy::too_many_arguments)] // FFmpeg DSP ABI plus two tap arrays and a depth.
fn filter_hv_scalar<P: Sample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    h_filter: &[i16; 8],
    v_filter: &[i16; 8],
    tmp: &mut [P],
    bit_depth: u8,
) {
    let max_val = (1_i32 << bit_depth) - 1;
    // Horizontal pass: h+7 rows of up to 64 clipped samples, row stride 64.
    for y in 0..h + 7 {
        let src_row = &src[y * src_stride..];
        let tmp_row = &mut tmp[y * 64..];
        for x in 0..w {
            tmp_row[x] = filter_8tap(src_row, x, 1, h_filter, max_val);
        }
    }
    // Vertical pass: tmp row 3 is the block's first row (the horizontal pass
    // started at source row -3), so tapping tmp[y .. y+7] centres on block
    // row y. This matches FFmpeg's do_8tap_2d_c and the u8 SIMD path.
    for y in 0..h {
        let tmp_row = &tmp[y * 64..];
        let dst_row = &mut dst[y * dst_stride..];
        for x in 0..w {
            dst_row[x] = filter_8tap(tmp_row, x, 64, v_filter, max_val);
        }
    }
}

/// Scalar compound average: `(dst + src + 1) >> 1`.
fn avg_scalar<P: Sample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
) {
    for y in 0..h {
        for x in 0..w {
            let d = dst[y * dst_stride + x].to_i32();
            let s = src[y * src_stride + x].to_i32();
            dst[y * dst_stride + x] = P::from_i32(d.wrapping_add(s).wrapping_add(1) >> 1);
        }
    }
}

// ---------------------------------------------------------------------------
// 8-tap subpel filter application
// ---------------------------------------------------------------------------

/// Apply an 8-tap filter horizontally (put, not avg).
///
/// `src` must have at least `w + 7` accessible columns (3 before, 4 after).
/// The filter operates on `src[x - 3 .. x + 4]` for each output pixel.
#[allow(clippy::too_many_arguments)] // FFmpeg DSP ABI: dst/src, strides, w/h, taps, depth.
fn filter_h_8tap<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    filter: &[i16; 8],
    bit_depth: u8,
) {
    P::filter_h(dst, dst_stride, src, src_stride, w, h, filter, bit_depth);
}

/// Apply an 8-tap filter vertically (put, not avg).
///
/// `src` must have at least `h + 7` accessible rows (3 before, 4 after).
#[allow(clippy::too_many_arguments)] // FFmpeg DSP ABI: dst/src, strides, w/h, taps, depth.
fn filter_v_8tap<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    filter: &[i16; 8],
    bit_depth: u8,
) {
    P::filter_v(dst, dst_stride, src, src_stride, w, h, filter, bit_depth);
}

/// Apply an 8-tap filter in both dimensions (2-pass: horizontal into the
/// pixel container with stride 64, then vertical to the destination).
///
/// `src` must have `(h + 7)` rows and `(w + 7)` cols of accessible data
/// (3 pixels of margin on each side).
#[allow(clippy::too_many_arguments)] // FFmpeg DSP ABI plus two tap arrays and a depth.
fn filter_hv_8tap<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    h_filter: &[i16; 8],
    v_filter: &[i16; 8],
    bit_depth: u8,
) {
    P::filter_hv(
        dst, dst_stride, src, src_stride, w, h, h_filter, v_filter, bit_depth,
    );
}

/// Simple pixel copy (integer-pel, no filtering).
fn copy_block<P: Sample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
) {
    for y in 0..h {
        dst[y * dst_stride..y * dst_stride + w]
            .copy_from_slice(&src[y * src_stride..y * src_stride + w]);
    }
}

/// Average dst with src: `dst[i] = (dst[i] + src[i] + 1) >> 1`.
/// The 8-bit path is SIMD-dispatched; 16-bit uses the scalar i32 kernel.
fn avg_block<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
) {
    P::avg(dst, dst_stride, src, src_stride, w, h);
}

// ---------------------------------------------------------------------------
// Edge emulation
// ---------------------------------------------------------------------------

/// Emulated edge MC: copy a block from `src` with clamped border extension.
///
/// Translated from `ff_emulated_edge_mc_template` in videodsp_template.c.
/// `src_x`, `src_y` may be negative (the copy window extends before the frame).
#[allow(clippy::too_many_arguments)] // FFmpeg videodsp ABI: dst/src, strides, block/src geometry.
fn emu_edge<P: Sample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    block_w: usize,
    block_h: usize,
    src_x: isize,
    src_y: isize,
    frame_w: usize,
    frame_h: usize,
) {
    for y in 0..block_h {
        let sy = (src_y + y as isize).clamp(0, frame_h as isize - 1) as usize;
        for x in 0..block_w {
            let sx = (src_x + x as isize).clamp(0, frame_w as isize - 1) as usize;
            dst[y * dst_stride + x] = src[sy * src_stride + sx];
        }
    }
}

// ---------------------------------------------------------------------------
// Per-block MC
// ---------------------------------------------------------------------------

/// MC for one luma block (unscaled).
///
/// Translated from `mc_luma_unscaled` in vp9recon.c.
///
/// * `row_px`, `col_px` - pixel position of the block's top-left corner.
/// * `mv` - `[x, y]` in 1/8-pel units.
/// * `bw`, `bh` - block dimensions in pixels.
/// * `filter_idx` - 0=Smooth, 1=Regular, 2=Sharp.
/// * `bit_depth` - frame bit depth; only used by the 16-bit sample path.
#[allow(clippy::too_many_arguments)] // FFmpeg mc_luma_unscaled ABI plus sample depth.
pub fn mc_luma<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    ref_plane: &[P],
    ref_stride: usize,
    ref_w: usize,
    ref_h: usize,
    row_px: usize,
    col_px: usize,
    mv: &[i16; 2],
    bw: usize,
    bh: usize,
    filter_idx: u8,
    bit_depth: u8,
    emu_buf: &mut [P],
) {
    let mx = mv[0] as isize;
    let my = mv[1] as isize;

    let x = col_px as isize + (mx >> 3);
    let y = row_px as isize + (my >> 3);
    let mx_frac = (mx & 7) as usize;
    let my_frac = (my & 7) as usize;

    // The 8-tap filter needs 3 pixels before and 4 after the block in each
    // dimension.  Use emu_edge when any filter tap would fall outside the
    // reference frame (matches FFmpeg mc_luma_unscaled / mc_chroma_unscaled).
    // FFmpeg uses !!my * 5 for the vertical bound check (not 4).
    let need_emu = x < (mx_frac != 0) as isize * 3
        || y < (my_frac != 0) as isize * 3
        || x + (mx_frac != 0) as isize * 4 + bw as isize > ref_w as isize
        || y + (my_frac != 0) as isize * 5 + bh as isize > ref_h as isize;

    if need_emu {
        let emu_w = bw + if mx_frac != 0 { 7 } else { 0 };
        let emu_h = bh + if my_frac != 0 { 7 } else { 0 };
        let emu_x = x - if mx_frac != 0 { 3 } else { 0 };
        let emu_y = y - if my_frac != 0 { 3 } else { 0 };
        emu_edge(
            emu_buf, 160, ref_plane, ref_stride, emu_w, emu_h, emu_x, emu_y, ref_w, ref_h,
        );

        // The emu buffer starts at (emu_x, emu_y) = (integer - 3, y - 3),
        // which is already the position our filter functions expect (they read
        // src[0..7] with center at tap[3] = integer position). No offset needed.
        // (FFmpeg adds +3 here because its FILTER_8TAP reads src[x-3..x+4].)
        mc_apply(
            dst, dst_stride, emu_buf, 160, bw, bh, mx_frac, my_frac, filter_idx, bit_depth, true,
        );
    } else {
        // Position src 3 rows above and 3 pixels left of the block start
        // so the 8-tap filter center (tap[3]) lands on the block.
        let offset_y = y as usize - if my_frac != 0 { 3 } else { 0 };
        let offset_x = x as usize - if mx_frac != 0 { 3 } else { 0 };
        let src_off = offset_y * ref_stride + offset_x;
        let src = &ref_plane[src_off..];
        mc_apply(
            dst, dst_stride, src, ref_stride, bw, bh, mx_frac, my_frac, filter_idx, bit_depth, true,
        );
    }
}

/// MC for one chroma block pair (U+V, unscaled).
///
/// Translated from `mc_chroma_unscaled` in vp9recon.c.
/// Chroma MV uses 4-bit subpel: `mx = mv.x * (1 << !ss_h) & 15`.
#[allow(clippy::too_many_arguments)] // FFmpeg mc_chroma_unscaled ABI plus sample depth.
pub fn mc_chroma<P: McSample + PlaneSample>(
    dst_u: &mut [P],
    dst_v: &mut [P],
    dst_stride: usize,
    ref_u: &[P],
    ref_v: &[P],
    ref_stride: usize,
    ref_w: usize,
    ref_h: usize,
    row_px: usize,
    col_px: usize,
    mv: &[i16; 2],
    bw: usize,
    bh: usize,
    filter_idx: u8,
    bit_depth: u8,
    ss_h: bool,
    ss_v: bool,
    emu_buf: &mut [P],
) {
    // Chroma MV: multiply by 2 if not subsampled in that dimension.
    let mx = mv[0] as isize * (1 << (!ss_h as isize));
    let my = mv[1] as isize * (1 << (!ss_v as isize));

    let x = col_px as isize + (mx >> 4);
    let y = row_px as isize + (my >> 4);
    let mx_frac = (mx & 15) as usize;
    let my_frac = (my & 15) as usize;

    // FFmpeg uses !!my * 5 for the vertical bound check (not 4).
    let need_emu = x < (mx_frac != 0) as isize * 3
        || y < (my_frac != 0) as isize * 3
        || x + (mx_frac != 0) as isize * 4 + bw as isize > ref_w as isize
        || y + (my_frac != 0) as isize * 5 + bh as isize > ref_h as isize;

    if need_emu {
        let emu_w = bw + if mx_frac != 0 { 7 } else { 0 };
        let emu_h = bh + if my_frac != 0 { 7 } else { 0 };
        let emu_x = x - if mx_frac != 0 { 3 } else { 0 };
        let emu_y = y - if my_frac != 0 { 3 } else { 0 };

        // U plane — emu buffer starts at (x-3, y-3), no offset needed
        // (see mc_luma comment for why).
        emu_edge(
            emu_buf, 160, ref_u, ref_stride, emu_w, emu_h, emu_x, emu_y, ref_w, ref_h,
        );
        mc_apply(
            dst_u, dst_stride, emu_buf, 160, bw, bh, mx_frac, my_frac, filter_idx, bit_depth, false,
        );

        // V plane
        emu_edge(
            emu_buf, 160, ref_v, ref_stride, emu_w, emu_h, emu_x, emu_y, ref_w, ref_h,
        );
        mc_apply(
            dst_v, dst_stride, emu_buf, 160, bw, bh, mx_frac, my_frac, filter_idx, bit_depth, false,
        );
    } else {
        let offset_y = y as usize - if my_frac != 0 { 3 } else { 0 };
        let offset_x = x as usize - if mx_frac != 0 { 3 } else { 0 };
        let src_off = offset_y * ref_stride + offset_x;
        mc_apply(
            dst_u,
            dst_stride,
            &ref_u[src_off..],
            ref_stride,
            bw,
            bh,
            mx_frac,
            my_frac,
            filter_idx,
            bit_depth,
            false,
        );
        mc_apply(
            dst_v,
            dst_stride,
            &ref_v[src_off..],
            ref_stride,
            bw,
            bh,
            mx_frac,
            my_frac,
            filter_idx,
            bit_depth,
            false,
        );
    }
}

/// Dispatch the appropriate MC kernel based on subpel fractions.
///
/// For luma: `mx`/`my` are in [0, 7] (3-bit). Filter lookup uses `mx << 1`.
/// For chroma: `mx`/`my` are in [0, 15] (4-bit). Filter lookup uses `mx` directly.
///
/// The `luma` flag distinguishes luma (3-bit, shift left) from chroma (4-bit,
/// use directly). This avoids the ambiguity of guessing from the value.
#[allow(clippy::too_many_arguments)] // FFmpeg mc dispatch ABI plus sample depth.
fn mc_apply<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    src: &[P],
    src_stride: usize,
    w: usize,
    h: usize,
    mx: usize,
    my: usize,
    filter_idx: u8,
    bit_depth: u8,
    luma: bool,
) {
    let mx_pos = if luma { mx << 1 } else { mx };
    let my_pos = if luma { my << 1 } else { my };

    let fi = filter_idx as usize;
    let h_filter = &VP9_SUBPEL_FILTERS[fi][mx_pos];
    let v_filter = &VP9_SUBPEL_FILTERS[fi][my_pos];

    if mx_pos == 0 && my_pos == 0 {
        copy_block(dst, dst_stride, src, src_stride, w, h);
    } else if my_pos == 0 {
        filter_h_8tap(dst, dst_stride, src, src_stride, w, h, h_filter, bit_depth);
    } else if mx_pos == 0 {
        filter_v_8tap(dst, dst_stride, src, src_stride, w, h, v_filter, bit_depth);
    } else {
        filter_hv_8tap(
            dst, dst_stride, src, src_stride, w, h, h_filter, v_filter, bit_depth,
        );
    }
}

// ---------------------------------------------------------------------------
// Chroma MV averaging for sub-8x8 blocks
// ---------------------------------------------------------------------------

/// FFmpeg's ROUNDED_DIV: round away from zero.
/// `(a >= 0 ? a + (b >> 1) : a - (b >> 1)) / b`
#[inline]
fn rounded_div(a: i32, b: i32) -> i16 {
    ((if a >= 0 { a + (b >> 1) } else { a - (b >> 1) }) / b) as i16
}

/// Average two MVs using ROUNDED_DIV (for 4:2:0 chroma).
fn rounded_div_mv_x2(a: [i16; 2], b: [i16; 2]) -> [i16; 2] {
    [
        rounded_div(a[0] as i32 + b[0] as i32, 2),
        rounded_div(a[1] as i32 + b[1] as i32, 2),
    ]
}

/// Average four MVs using ROUNDED_DIV (for 4:2:0 chroma with 4x4 sub-blocks).
fn rounded_div_mv_x4(a: [i16; 2], b: [i16; 2], c: [i16; 2], d: [i16; 2]) -> [i16; 2] {
    [
        rounded_div(a[0] as i32 + b[0] as i32 + c[0] as i32 + d[0] as i32, 4),
        rounded_div(a[1] as i32 + b[1] as i32 + c[1] as i32 + d[1] as i32, 4),
    ]
}

// ---------------------------------------------------------------------------
// Top-level inter prediction dispatch
// ---------------------------------------------------------------------------

/// Perform inter prediction for one decoded block.
///
/// Translated from `inter_pred` in vp9_mc_template.c (unscaled 8bpp path).
/// Handles all block sizes including sub-8x8 with chroma MV averaging.
///
/// For compound prediction: predicts ref0 into `fb`, predicts ref1 into a temp
/// buffer, then averages.
#[allow(clippy::too_many_arguments)]
pub fn inter_pred<P: McSample + PlaneSample>(
    y: &mut [P],
    u: &mut [P],
    v: &mut [P],
    y_stride: usize,
    uv_stride: usize,
    block: &BlockInfo,
    ref_slots: &[Option<Arc<RefFrame>>; 8],
    ref_idx: &[u8; 3],
    ss_h: bool,
    ss_v: bool,
    bit_depth: u8,
) {
    let bs = block.bs as usize;
    let bw4 = BWH_TAB[0][bs][0] as usize;
    let bh4 = BWH_TAB[0][bs][1] as usize;
    let bw = bw4 * 4;
    let bh = bh4 * 4;
    let col_px = block.col * 4;
    let row_px = block.row * 4;

    // Resolve the reference frame for ref[0].
    let ref0_slot = ref_idx[block.ref_frame[0] as usize] as usize;
    let ref0 = match &ref_slots[ref0_slot] {
        Some(r) => r,
        None => return, // Missing ref frame — skip silently.
    };

    let filter = block.filter;

    // Allocate an edge-emulation buffer with the frame's sample width
    // (160-sample stride, enough for 64+7 rows). A `u8` buffer would
    // truncate high-depth references before the 8-tap filters run.
    let mut emu_buf = vec![P::from_i32(0); 160 * 80];

    // --- Luma MC for ref[0] ---
    if bs > BlockSize::Bs8x8 as usize {
        // Sub-8x8: per-sub-block MC.
        sub8x8_luma_mc(
            y,
            y_stride,
            block,
            ref0,
            col_px,
            row_px,
            filter,
            bit_depth,
            &mut emu_buf,
        );
    } else {
        // Normal block: single MV.
        let dst_off = row_px * y_stride + col_px;
        mc_luma(
            &mut y[dst_off..],
            y_stride,
            P::plane_slice(&ref0.fb.y).expect("reference luma plane sample type"),
            ref0.fb.y_stride,
            ref0.width as usize,
            ref0.height as usize,
            row_px,
            col_px,
            &block.mv[0][0],
            bw,
            bh,
            filter,
            bit_depth,
            &mut emu_buf,
        );
    }

    // --- Chroma MC for ref[0] ---
    // FFmpeg vp9_mc_template.c:408-410 derives chroma dimensions from the
    // subsampling flags: `uvbw = ff_vp9_bwh_tab[s->ss_h][bs][0] * 4` and
    // `uvbh = ff_vp9_bwh_tab[s->ss_v][bs][1] * 4`. For 4:4:4 (no subsampling)
    // that is table[0] (luma-sized); table[1] is only correct for 4:2:0.
    let uv_bw4 = BWH_TAB[ss_h as usize][bs][0] as usize;
    let uv_bh4 = BWH_TAB[ss_v as usize][bs][1] as usize;
    let uv_bw = uv_bw4 * 4;
    let uv_bh = uv_bh4 * 4;
    let uv_col = col_px >> ss_h as usize;
    let uv_row = row_px >> ss_v as usize;
    let ref_uv_w = (ref0.width as usize).div_ceil(1 << ss_h as usize);
    let ref_uv_h = (ref0.height as usize).div_ceil(1 << ss_v as usize);

    if bs > BlockSize::Bs8x8 as usize {
        sub8x8_chroma_mc(
            u,
            v,
            uv_stride,
            block,
            ref0,
            uv_col,
            uv_row,
            uv_bw,
            uv_bh,
            ref_uv_w,
            ref_uv_h,
            filter,
            ss_h,
            ss_v,
            bit_depth,
            &mut emu_buf,
        );
    } else {
        let u_off = uv_row * uv_stride + uv_col;
        let v_off = u_off;
        mc_chroma(
            &mut u[u_off..],
            &mut v[v_off..],
            uv_stride,
            P::plane_slice(&ref0.fb.u).expect("reference U plane sample type"),
            P::plane_slice(&ref0.fb.v).expect("reference V plane sample type"),
            ref0.fb.uv_stride,
            ref_uv_w,
            ref_uv_h,
            uv_row,
            uv_col,
            &block.mv[0][0],
            uv_bw,
            uv_bh,
            filter,
            bit_depth,
            ss_h,
            ss_v,
            &mut emu_buf,
        );
    }

    // --- Compound prediction: average with ref[1] ---
    if block.comp {
        let ref1_slot = ref_idx[block.ref_frame[1] as usize] as usize;
        let ref1 = match &ref_slots[ref1_slot] {
            Some(r) => r,
            None => return,
        };

        // MC ref1 into temp buffers, then average with fb. For sub-8x8
        // blocks the MC helpers write each sub-block at a relative offset
        // inside the temp buffer, so the buffer (and the subsequent
        // averaging) must cover the full 8x8 luma / 4x4 chroma region.
        // Using the single-sub-block bw/bh here caused OOB writes and
        // left all but the first sub-block un-averaged.
        let (avg_bw, avg_bh) = if bs > BlockSize::Bs8x8 as usize {
            (8, 8)
        } else {
            (bw, bh)
        };
        let (avg_uv_bw, avg_uv_bh) = if bs > BlockSize::Bs8x8 as usize {
            // Sub-8x8 blocks cover an 8x8 luma region; in 4:4:4 chroma
            // covers the same 8x8 region (FFmpeg vp9_mc_template.c).
            if !ss_h && !ss_v {
                (8, 8)
            } else if !ss_h && ss_v {
                // 4:4:0 chroma covers the whole 8x8 luma region: 8x4.
                (8, 4)
            } else {
                (4, 4)
            }
        } else {
            (uv_bw, uv_bh)
        };
        let mut tmp_y = vec![P::from_i32(0); y_stride * avg_bh];
        let mut tmp_u = vec![P::from_i32(0); uv_stride * avg_uv_bh];
        let mut tmp_v = vec![P::from_i32(0); uv_stride * avg_uv_bh];

        // Luma ref1 — per-sub-block for sub-8x8, single MC for >=8x8.
        if bs > BlockSize::Bs8x8 as usize {
            sub8x8_luma_mc_buf(
                &mut tmp_y,
                y_stride,
                block,
                ref1,
                col_px,
                row_px,
                filter,
                &mut emu_buf,
                1, // ref_idx=1
            );
        } else {
            mc_luma(
                &mut tmp_y,
                y_stride,
                P::plane_slice(&ref1.fb.y).expect("reference luma plane sample type"),
                ref1.fb.y_stride,
                ref1.width as usize,
                ref1.height as usize,
                row_px,
                col_px,
                &block.mv[0][1],
                bw,
                bh,
                filter,
                bit_depth,
                &mut emu_buf,
            );
        }
        // Average luma.
        let dst_off = row_px * y_stride + col_px;
        avg_block(
            &mut y[dst_off..],
            y_stride,
            &tmp_y,
            y_stride,
            avg_bw,
            avg_bh,
        );

        // Chroma ref1 — per-sub-block averaging for sub-8x8.
        let ref1_uv_w = (ref1.width as usize).div_ceil(1 << ss_h as usize);
        let ref1_uv_h = (ref1.height as usize).div_ceil(1 << ss_v as usize);
        if bs > BlockSize::Bs8x8 as usize {
            sub8x8_chroma_mc_buf(
                &mut tmp_u,
                &mut tmp_v,
                uv_stride,
                block,
                ref1,
                uv_col,
                uv_row,
                uv_bw,
                uv_bh,
                ref1_uv_w,
                ref1_uv_h,
                filter,
                ss_h,
                ss_v,
                &mut emu_buf,
                1, // ref_idx=1
            );
        } else {
            mc_chroma(
                &mut tmp_u,
                &mut tmp_v,
                uv_stride,
                P::plane_slice(&ref1.fb.u).expect("reference U plane sample type"),
                P::plane_slice(&ref1.fb.v).expect("reference V plane sample type"),
                ref1.fb.uv_stride,
                ref1_uv_w,
                ref1_uv_h,
                uv_row,
                uv_col,
                &block.mv[0][1],
                uv_bw,
                uv_bh,
                filter,
                bit_depth,
                ss_h,
                ss_v,
                &mut emu_buf,
            );
        }
        let u_off = uv_row * uv_stride + uv_col;
        avg_block(
            &mut u[u_off..],
            uv_stride,
            &tmp_u,
            uv_stride,
            avg_uv_bw,
            avg_uv_bh,
        );
        avg_block(
            &mut v[u_off..],
            uv_stride,
            &tmp_v,
            uv_stride,
            avg_uv_bw,
            avg_uv_bh,
        );
    }
}

// ---------------------------------------------------------------------------
// Sub-8x8 block MC helpers
// ---------------------------------------------------------------------------

/// MC luma for sub-8x8 blocks into an arbitrary buffer (for compound ref[1]).
///
/// `ri` selects which reference MV to use: 0 or 1.
#[allow(clippy::too_many_arguments)]
fn sub8x8_luma_mc_buf<P: McSample + PlaneSample>(
    dst: &mut [P],
    dst_stride: usize,
    block: &BlockInfo,
    ref_frame: &RefFrame,
    col_px: usize,
    row_px: usize,
    filter: u8,
    emu_buf: &mut [P],
    ri: usize,
) {
    let bit_depth = ref_frame.fb.bit_depth;
    let ref_y = P::plane_slice(&ref_frame.fb.y).expect("reference luma plane sample type");
    let bs = block.bs;
    let ref_w = ref_frame.width as usize;
    let ref_h = ref_frame.height as usize;

    match bs {
        BlockSize::Bs8x4 => {
            for sub in 0..2 {
                let sub_row = row_px + sub * 4;
                let dst_off = sub * 4 * dst_stride;
                mc_luma(
                    &mut dst[dst_off..],
                    dst_stride,
                    ref_y,
                    ref_frame.fb.y_stride,
                    ref_w,
                    ref_h,
                    sub_row,
                    col_px,
                    &block.mv[sub * 2][ri],
                    8,
                    4,
                    filter,
                    bit_depth,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x8 => {
            for sub in 0..2 {
                let sub_col = col_px + sub * 4;
                let dst_off = sub * 4;
                mc_luma(
                    &mut dst[dst_off..],
                    dst_stride,
                    ref_y,
                    ref_frame.fb.y_stride,
                    ref_w,
                    ref_h,
                    row_px,
                    sub_col,
                    &block.mv[sub][ri],
                    4,
                    8,
                    filter,
                    bit_depth,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x4 => {
            for sub in 0..4 {
                let sub_row = (sub >> 1) * 4;
                let sub_col = (sub & 1) * 4;
                let dst_off = sub_row * dst_stride + sub_col;
                mc_luma(
                    &mut dst[dst_off..],
                    dst_stride,
                    ref_y,
                    ref_frame.fb.y_stride,
                    ref_w,
                    ref_h,
                    row_px + sub_row,
                    col_px + sub_col,
                    &block.mv[sub][ri],
                    4,
                    4,
                    filter,
                    bit_depth,
                    emu_buf,
                );
            }
        }
        _ => {}
    }
}

/// MC chroma for sub-8x8 blocks when neither dimension is subsampled.
///
/// In 4:4:4 the chroma geometry matches luma, so FFmpeg issues one
/// `mc_chroma_dir` per luma sub-block, each with its own MV (see the
/// `BS_8x4`, `BS_4x8` and `BS_4x4` branches of `vp9_mc_template.c` with
/// `ss_h == ss_v == 0`). A single averaged MV would leave most of the
/// chroma block unwritten.
#[allow(clippy::too_many_arguments)]
fn sub8x8_chroma_mc_444<P: McSample + PlaneSample>(
    dst_u: &mut [P],
    dst_v: &mut [P],
    dst_stride: usize,
    block: &BlockInfo,
    ref_frame: &RefFrame,
    uv_col: usize,
    uv_row: usize,
    ref_uv_w: usize,
    ref_uv_h: usize,
    filter: u8,
    emu_buf: &mut [P],
    ri: usize,
) {
    let bit_depth = ref_frame.fb.bit_depth;
    let ref_u = P::plane_slice(&ref_frame.fb.u).expect("reference U plane sample type");
    let ref_v = P::plane_slice(&ref_frame.fb.v).expect("reference V plane sample type");
    let ref_stride = ref_frame.fb.uv_stride;
    match block.bs {
        BlockSize::Bs8x4 => {
            for sub in 0..2 {
                let y = uv_row + sub * 4;
                // Destination offsets are relative to the caller's block
                // origin; the compound-prediction scratch buffers start at 0.
                let off = sub * 4 * dst_stride;
                mc_chroma(
                    &mut dst_u[off..],
                    &mut dst_v[off..],
                    dst_stride,
                    ref_u,
                    ref_v,
                    ref_stride,
                    ref_uv_w,
                    ref_uv_h,
                    y,
                    uv_col,
                    &block.mv[sub * 2][ri],
                    8,
                    4,
                    filter,
                    bit_depth,
                    false,
                    false,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x8 => {
            for sub in 0..2 {
                let x = uv_col + sub * 4;
                let off = sub * 4;
                mc_chroma(
                    &mut dst_u[off..],
                    &mut dst_v[off..],
                    dst_stride,
                    ref_u,
                    ref_v,
                    ref_stride,
                    ref_uv_w,
                    ref_uv_h,
                    uv_row,
                    x,
                    &block.mv[sub][ri],
                    4,
                    8,
                    filter,
                    bit_depth,
                    false,
                    false,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x4 => {
            for sub in 0..4 {
                let x = uv_col + (sub & 1) * 4;
                let y = uv_row + (sub >> 1) * 4;
                let off = (sub >> 1) * 4 * dst_stride + (sub & 1) * 4;
                mc_chroma(
                    &mut dst_u[off..],
                    &mut dst_v[off..],
                    dst_stride,
                    ref_u,
                    ref_v,
                    ref_stride,
                    ref_uv_w,
                    ref_uv_h,
                    y,
                    x,
                    &block.mv[sub][ri],
                    4,
                    4,
                    filter,
                    bit_depth,
                    false,
                    false,
                    emu_buf,
                );
            }
        }
        _ => {}
    }
}

/// MC chroma for sub-8x8 blocks in 4:4:0 (`ss_h == false`, `ss_v == true`).
///
/// FFmpeg's `vp9_mc_template.c` writes one 8x4 chroma region spanning the
/// whole 8x8 luma block. Vertical pairs of luma MVs are averaged, while the
/// left and right 4x4 chroma halves keep their own MVs for 4x8/4x4 blocks.
#[allow(clippy::too_many_arguments)]
fn sub8x8_chroma_mc_440<P: McSample + PlaneSample>(
    dst_u: &mut [P],
    dst_v: &mut [P],
    dst_stride: usize,
    block: &BlockInfo,
    ref_frame: &RefFrame,
    uv_col: usize,
    uv_row: usize,
    ref_uv_w: usize,
    ref_uv_h: usize,
    filter: u8,
    emu_buf: &mut [P],
    ri: usize,
) {
    let bit_depth = ref_frame.fb.bit_depth;
    let ref_u = P::plane_slice(&ref_frame.fb.u).expect("reference U plane sample type");
    let ref_v = P::plane_slice(&ref_frame.fb.v).expect("reference V plane sample type");
    let ref_stride = ref_frame.fb.uv_stride;
    match block.bs {
        BlockSize::Bs8x4 => {
            let mv = rounded_div_mv_x2(block.mv[0][ri], block.mv[2][ri]);
            mc_chroma(
                dst_u, dst_v, dst_stride, ref_u, ref_v, ref_stride, ref_uv_w, ref_uv_h, uv_row,
                uv_col, &mv, 8, 4, filter, bit_depth, false, true, emu_buf,
            );
        }
        BlockSize::Bs4x8 => {
            for sub in 0..2 {
                let off = sub * 4;
                mc_chroma(
                    &mut dst_u[off..],
                    &mut dst_v[off..],
                    dst_stride,
                    ref_u,
                    ref_v,
                    ref_stride,
                    ref_uv_w,
                    ref_uv_h,
                    uv_row,
                    uv_col + off,
                    &block.mv[sub][ri],
                    4,
                    4,
                    filter,
                    bit_depth,
                    false,
                    true,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x4 => {
            for sub in 0..2 {
                let off = sub * 4;
                let mv = rounded_div_mv_x2(block.mv[sub][ri], block.mv[sub + 2][ri]);
                mc_chroma(
                    &mut dst_u[off..],
                    &mut dst_v[off..],
                    dst_stride,
                    ref_u,
                    ref_v,
                    ref_stride,
                    ref_uv_w,
                    ref_uv_h,
                    uv_row,
                    uv_col + off,
                    &mv,
                    4,
                    4,
                    filter,
                    bit_depth,
                    false,
                    true,
                    emu_buf,
                );
            }
        }
        _ => {}
    }
}

/// MC chroma for sub-8x8 blocks into arbitrary buffers (for compound ref[1]).
///
/// `ri` selects which reference MV to use: 0 or 1.
#[allow(clippy::too_many_arguments)]
fn sub8x8_chroma_mc_buf<P: McSample + PlaneSample>(
    dst_u: &mut [P],
    dst_v: &mut [P],
    dst_stride: usize,
    block: &BlockInfo,
    ref_frame: &RefFrame,
    uv_col: usize,
    uv_row: usize,
    uv_bw: usize,
    uv_bh: usize,
    ref_uv_w: usize,
    ref_uv_h: usize,
    filter: u8,
    ss_h: bool,
    ss_v: bool,
    emu_buf: &mut [P],
    ri: usize,
) {
    let bit_depth = ref_frame.fb.bit_depth;
    let ref_u = P::plane_slice(&ref_frame.fb.u).expect("reference U plane sample type");
    let ref_v = P::plane_slice(&ref_frame.fb.v).expect("reference V plane sample type");
    if !ss_h && !ss_v {
        sub8x8_chroma_mc_444(
            dst_u, dst_v, dst_stride, block, ref_frame, uv_col, uv_row, ref_uv_w, ref_uv_h, filter,
            emu_buf, ri,
        );
        return;
    }
    if !ss_h && ss_v {
        sub8x8_chroma_mc_440(
            dst_u, dst_v, dst_stride, block, ref_frame, uv_col, uv_row, ref_uv_w, ref_uv_h, filter,
            emu_buf, ri,
        );
        return;
    }
    let bs = block.bs;
    let chroma_mv = match bs {
        BlockSize::Bs8x4 if ss_v => rounded_div_mv_x2(block.mv[0][ri], block.mv[2][ri]),
        BlockSize::Bs4x8 if ss_h => rounded_div_mv_x2(block.mv[0][ri], block.mv[1][ri]),
        BlockSize::Bs4x4 if ss_h && ss_v => rounded_div_mv_x4(
            block.mv[0][ri],
            block.mv[1][ri],
            block.mv[2][ri],
            block.mv[3][ri],
        ),
        _ => block.mv[0][ri],
    };
    mc_chroma(
        dst_u,
        dst_v,
        dst_stride,
        ref_u,
        ref_v,
        ref_frame.fb.uv_stride,
        ref_uv_w,
        ref_uv_h,
        uv_row,
        uv_col,
        &chroma_mv,
        uv_bw,
        uv_bh,
        filter,
        bit_depth,
        ss_h,
        ss_v,
        emu_buf,
    );
}

/// MC luma for sub-8x8 blocks (per-sub-block MVs).
#[allow(clippy::too_many_arguments)] // FFmpeg mc_luma ABI plus the sample-plane slices.
fn sub8x8_luma_mc<P: McSample + PlaneSample>(
    y: &mut [P],
    y_stride: usize,
    block: &BlockInfo,
    ref_frame: &RefFrame,
    col_px: usize,
    row_px: usize,
    filter: u8,
    bit_depth: u8,
    emu_buf: &mut [P],
) {
    let ref_y = P::plane_slice(&ref_frame.fb.y).expect("reference luma plane sample type");
    let bs = block.bs;
    let ref_w = ref_frame.width as usize;
    let ref_h = ref_frame.height as usize;

    match bs {
        BlockSize::Bs8x4 => {
            // Two 8x4 sub-blocks stacked vertically.
            for sub in 0..2 {
                let sub_row = row_px + sub * 4;
                let dst_off = sub_row * y_stride + col_px;
                mc_luma(
                    &mut y[dst_off..],
                    y_stride,
                    ref_y,
                    ref_frame.fb.y_stride,
                    ref_w,
                    ref_h,
                    sub_row,
                    col_px,
                    &block.mv[sub * 2][0],
                    8,
                    4,
                    filter,
                    bit_depth,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x8 => {
            // Two 4x8 sub-blocks side by side.
            for sub in 0..2 {
                let sub_col = col_px + sub * 4;
                let dst_off = row_px * y_stride + sub_col;
                mc_luma(
                    &mut y[dst_off..],
                    y_stride,
                    ref_y,
                    ref_frame.fb.y_stride,
                    ref_w,
                    ref_h,
                    row_px,
                    sub_col,
                    &block.mv[sub][0],
                    4,
                    8,
                    filter,
                    bit_depth,
                    emu_buf,
                );
            }
        }
        BlockSize::Bs4x4 => {
            // Four 4x4 sub-blocks.
            for sub in 0..4 {
                let sub_row = row_px + (sub >> 1) * 4;
                let sub_col = col_px + (sub & 1) * 4;
                let dst_off = sub_row * y_stride + sub_col;
                mc_luma(
                    &mut y[dst_off..],
                    y_stride,
                    ref_y,
                    ref_frame.fb.y_stride,
                    ref_w,
                    ref_h,
                    sub_row,
                    sub_col,
                    &block.mv[sub][0],
                    4,
                    4,
                    filter,
                    bit_depth,
                    emu_buf,
                );
            }
        }
        _ => {}
    }
}

/// MC chroma for sub-8x8 blocks (averaged MVs for 4:2:0).
#[allow(clippy::too_many_arguments)]
fn sub8x8_chroma_mc<P: McSample + PlaneSample>(
    u: &mut [P],
    v: &mut [P],
    uv_stride: usize,
    block: &BlockInfo,
    ref_frame: &RefFrame,
    uv_col: usize,
    uv_row: usize,
    uv_bw: usize,
    uv_bh: usize,
    ref_uv_w: usize,
    ref_uv_h: usize,
    filter: u8,
    ss_h: bool,
    ss_v: bool,
    bit_depth: u8,
    emu_buf: &mut [P],
) {
    let ref_u = P::plane_slice(&ref_frame.fb.u).expect("reference U plane sample type");
    let ref_v = P::plane_slice(&ref_frame.fb.v).expect("reference V plane sample type");
    if !ss_h && !ss_v {
        let off = uv_row * uv_stride + uv_col;
        sub8x8_chroma_mc_444(
            &mut u[off..],
            &mut v[off..],
            uv_stride,
            block,
            ref_frame,
            uv_col,
            uv_row,
            ref_uv_w,
            ref_uv_h,
            filter,
            emu_buf,
            0,
        );
        return;
    }
    if !ss_h && ss_v {
        let off = uv_row * uv_stride + uv_col;
        sub8x8_chroma_mc_440(
            &mut u[off..],
            &mut v[off..],
            uv_stride,
            block,
            ref_frame,
            uv_col,
            uv_row,
            ref_uv_w,
            ref_uv_h,
            filter,
            emu_buf,
            0,
        );
        return;
    }
    let bs = block.bs;

    // For 4:2:0, sub-8x8 chroma uses averaged MVs from the luma sub-blocks.
    let chroma_mv = match bs {
        BlockSize::Bs8x4 if ss_v => {
            // Two 8x4 luma sub-blocks → one chroma block. Average MVs.
            rounded_div_mv_x2(block.mv[0][0], block.mv[2][0])
        }
        BlockSize::Bs4x8 if ss_h => {
            // Two 4x8 luma sub-blocks → one chroma block. Average MVs.
            rounded_div_mv_x2(block.mv[0][0], block.mv[1][0])
        }
        BlockSize::Bs4x4 if ss_h && ss_v => {
            // Four 4x4 luma sub-blocks → one chroma block. Average all 4 MVs.
            rounded_div_mv_x4(
                block.mv[0][0],
                block.mv[1][0],
                block.mv[2][0],
                block.mv[3][0],
            )
        }
        _ => {
            // No averaging needed — use the first sub-block MV.
            block.mv[0][0]
        }
    };

    let u_off = uv_row * uv_stride + uv_col;
    mc_chroma(
        &mut u[u_off..],
        &mut v[u_off..],
        uv_stride,
        ref_u,
        ref_v,
        ref_frame.fb.uv_stride,
        ref_uv_w,
        ref_uv_h,
        uv_row,
        uv_col,
        &chroma_mv,
        uv_bw,
        uv_bh,
        filter,
        bit_depth,
        ss_h,
        ss_v,
        emu_buf,
    );
}

// ---------------------------------------------------------------------------
// Tests: 10/12-bit kernels against FFmpeg-derived golden values
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 12x12 ramp spanning the full sample range, stored in a 16-wide plane.
    /// This is the same pattern used by the FFmpeg `vp9dsp_template.c`
    /// reference program that produced the golden arrays below.
    fn ramp(bit_depth: u8) -> Vec<u16> {
        let mut v = vec![0u16; 12 * 16];
        for y in 0..12 {
            for x in 0..12 {
                v[y * 16 + x] = ((y * 173 + x * 337 + 511) % (1usize << bit_depth)) as u16;
            }
        }
        v
    }

    fn assert_block(dst: &[u16], stride: usize, expected: &[[u16; 4]; 4]) {
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(
                    dst[y * stride + x],
                    expected[y][x],
                    "mismatch at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn filter_h_8tap_u16_10bit_matches_ffmpeg() {
        // FFmpeg: regular filter, subpel position 8, 10-bit pixel width.
        let src = ramp(10);
        let mut dst = vec![0u16; 8 * 16];
        filter_h_8tap(
            &mut dst,
            16,
            &src[3 * 16..],
            16,
            4,
            4,
            &VP9_SUBPEL_FILTERS[1][8],
            10,
        );
        assert_block(
            &dst,
            16,
            &[
                [682, 347, 988, 661],
                [183, 824, 497, 170],
                [508, 949, 678, 343],
                [833, 506, 179, 820],
            ],
        );
    }

    #[test]
    fn filter_v_8tap_u16_12bit_matches_ffmpeg() {
        // FFmpeg: sharp filter, subpel position 6, 12-bit pixel width.
        let src = ramp(12);
        let mut dst = vec![0u16; 8 * 16];
        filter_v_8tap(
            &mut dst,
            16,
            &src[3..],
            16,
            4,
            4,
            &VP9_SUBPEL_FILTERS[2][6],
            12,
        );
        assert_block(
            &dst,
            16,
            &[
                [2111, 2448, 2785, 3122],
                [2284, 2621, 2958, 3295],
                [2457, 2794, 3131, 3468],
                [2630, 2967, 3304, 3737],
            ],
        );
    }

    #[test]
    fn filter_hv_8tap_u16_10bit_matches_ffmpeg() {
        // FFmpeg: smooth horizontal (pos 5) + regular vertical (pos 11).
        let src = ramp(10);
        let mut dst = vec![0u16; 8 * 16];
        filter_hv_8tap(
            &mut dst,
            16,
            &src,
            16,
            4,
            4,
            &VP9_SUBPEL_FILTERS[0][5],
            &VP9_SUBPEL_FILTERS[1][11],
            10,
        );
        assert_block(
            &dst,
            16,
            &[
                [540, 557, 562, 498],
                [406, 712, 668, 532],
                [507, 581, 513, 557],
                [720, 668, 540, 688],
            ],
        );
    }

    #[test]
    fn avg_block_u16_matches_ffmpeg() {
        // Both depths share FFmpeg's round-up-on-tie average.
        for (bit_depth, expected) in [
            (10u8, [512u16, 513, 514, 515]),
            (12u8, [2048u16, 2049, 2050, 2051]),
        ] {
            let max = (1u16 << bit_depth) - 1;
            let mut dst = vec![0u16; 2 * 16];
            let mut src = vec![0u16; 2 * 16];
            for y in 0..2 {
                for x in 0..4 {
                    dst[y * 16 + x] = max - x as u16;
                    src[y * 16 + x] = (x * 3 + y) as u16;
                }
            }
            avg_block(&mut dst, 16, &src, 16, 4, 2);
            for y in 0..2 {
                for x in 0..4 {
                    assert_eq!(dst[y * 16 + x], expected[x], "bd={bit_depth}");
                }
            }
        }
    }

    #[test]
    fn filter_8tap_u16_clips_to_bit_depth() {
        // FFmpeg av_clip_pixel keeps an all-max input at (1 << depth) - 1.
        for bit_depth in [10u8, 12u8] {
            let max = (1u16 << bit_depth) - 1;
            let src = vec![max; 12 * 16];
            let mut dst = vec![0u16; 16];
            filter_h_8tap(
                &mut dst,
                16,
                &src[3 * 16..],
                16,
                4,
                1,
                &VP9_SUBPEL_FILTERS[0][8],
                bit_depth,
            );
            assert_eq!(&dst[..4], &[max; 4]);
        }
    }

    #[test]
    fn mc_luma_u16_emu_edge_matches_ffmpeg() {
        // Block (0,0), MV=(5,3): subpel 2D filter at regular positions
        // 10/6, with edge emulation at the top and left frame borders.
        for (bit_depth, expected) in [
            (
                10u8,
                [
                    [926u16, 515, 267, 984],
                    [814, 480, 565, 787],
                    [244, 260, 1008, 420],
                    [223, 751, 824, 481],
                ],
            ),
            (
                12u8,
                [
                    [745u16, 1114, 1440, 1780],
                    [938, 1307, 1633, 1973],
                    [1106, 1475, 1801, 2141],
                    [1280, 1649, 1975, 2315],
                ],
            ),
        ] {
            let src = ramp(bit_depth);
            let mut dst = vec![0u16; 8 * 16];
            let mut emu = vec![0u16; 160 * 80];
            mc_luma(
                &mut dst,
                16,
                &src,
                16,
                12,
                12,
                0,
                0,
                &[5, 3],
                4,
                4,
                1,
                bit_depth,
                &mut emu,
            );
            assert_block(&dst, 16, &expected);
        }
    }

    #[test]
    fn u8_kernels_keep_the_simd_path() {
        // The generic u8 wrappers must hand off to the existing SIMD
        // kernels unchanged, preserving 8-bit bit-exactness.
        let src: Vec<u8> = (0..(12 * 16)).map(|i| (i * 17 % 256) as u8).collect();
        let mut a = vec![0u8; 8 * 16];
        let mut b = vec![0u8; 8 * 16];
        let f = &VP9_SUBPEL_FILTERS[1][8];
        filter_h_8tap(&mut a, 16, &src[3 * 16..], 16, 4, 4, f, 8);
        crate::simd::filter_h_8tap(&mut b, 16, &src[3 * 16..], 16, 4, 4, f);
        assert_eq!(a, b);
    }
}
