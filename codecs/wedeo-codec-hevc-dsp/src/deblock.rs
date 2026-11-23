//! HEVC deblocking filter.
//!
//! Kernels from `libavcodec/h26x/h2656_deblock_template.c`; the boundary
//! decision from `FUNC(hevc_loop_filter_luma)` in
//! `libavcodec/hevc/dsp_template.c`; tables from
//! `libavcodec/hevc/filter.c`. `beta` / `tc` are the 8-bit values from
//! `betatable` / `tctable` and are scaled internally by `1 << (bit_depth - 8)`,
//! exactly as FFmpeg does.

use crate::{DspError, Result, check_bit_depth, clip, clip_pixel};

/// `tctable[54]` from `libavcodec/hevc/filter.c`.
pub const TC_TABLE: [u8; 54] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24,
];

/// `betatable[52]` from `libavcodec/hevc/filter.c`.
pub const BETA_TABLE: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64,
];

/// `DEFAULT_INTRA_TC_OFFSET` from `hevc/ps.h`.
pub const DEFAULT_INTRA_TC_OFFSET: i32 = 2;

/// `TC_CALC(qp, bs)` from `filter.c` (`tc_offset` is rounded down to even).
pub fn tc_from_qp(qp: i32, bs: i32, tc_offset: i32) -> i32 {
    let index = clip(
        qp + DEFAULT_INTRA_TC_OFFSET * (bs - 1) + (tc_offset & !1),
        0,
        53,
    );
    i32::from(TC_TABLE[index as usize])
}

/// `betatable[av_clip(qp + beta_offset, 0, MAX_QP)]`.
pub fn beta_from_qp(qp: i32, beta_offset: i32) -> i32 {
    let index = clip(qp + beta_offset, 0, 51);
    i32::from(BETA_TABLE[index as usize])
}

/// Chroma `tc` (`chroma_tc()` in `filter.c`).
///
/// `cb_qp_offset` / `cr_qp_offset` come from the PPS; slice QP offsets are
/// deliberately not used for deblocking.
pub fn chroma_tc(
    qp_y: i32,
    c_idx: usize,
    cb_qp_offset: i32,
    cr_qp_offset: i32,
    tc_offset: i32,
    chroma_format_idc: u8,
) -> i32 {
    const QP_C: [i32; 14] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37];
    let offset = if c_idx == 1 {
        cb_qp_offset
    } else {
        cr_qp_offset
    };
    let qp_i = clip(qp_y + offset, 0, 57);
    let qp = if chroma_format_idc == 1 {
        if qp_i < 30 {
            qp_i
        } else if qp_i > 43 {
            qp_i - 6
        } else {
            QP_C[(qp_i - 30) as usize]
        }
    } else {
        clip(qp_i, 0, 51)
    };
    let idxt = clip(qp + DEFAULT_INTRA_TC_OFFSET + tc_offset, 0, 53);
    i32::from(TC_TABLE[idxt as usize])
}

#[inline]
fn offset(
    base: usize,
    xoff: isize,
    yoff: isize,
    xstride: isize,
    ystride: isize,
    len: usize,
) -> Result<usize> {
    let value = base as isize + xoff * xstride + yoff * ystride;
    if value < 0 {
        return Err(DspError::OutOfBounds {
            what: "deblocking boundary",
            index: 0,
            len,
        });
    }
    let index = value as usize;
    if index >= len {
        return Err(DspError::OutOfBounds {
            what: "deblocking boundary",
            index,
            len,
        });
    }
    Ok(index)
}

#[inline]
fn get(
    buf: &[u16],
    base: usize,
    xoff: isize,
    yoff: isize,
    xstride: isize,
    ystride: isize,
) -> Result<i32> {
    Ok(i32::from(
        buf[offset(base, xoff, yoff, xstride, ystride, buf.len())?],
    ))
}

// FFmpeg assigns a pixel with five coordinates plus strides and bit depth.
#[allow(clippy::too_many_arguments)]
#[inline]
fn set(
    buf: &mut [u16],
    base: usize,
    xoff: isize,
    yoff: isize,
    xstride: isize,
    ystride: isize,
    value: i32,
    bit_depth: u8,
) -> Result<()> {
    let index = offset(base, xoff, yoff, xstride, ystride, buf.len())?;
    buf[index] = clip_pixel(value, bit_depth);
    Ok(())
}

/// `FUNC(loop_filter_luma_strong)`: four lines of strong luma filtering.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's loop_filter_luma_strong
pub fn loop_filter_luma_strong(
    buf: &mut [u16],
    base: usize,
    xstride: isize,
    ystride: isize,
    tc: i32,
    tc2: i32,
    tc3: i32,
    no_p: bool,
    no_q: bool,
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    for d in 0isize..4 {
        let p3 = get(buf, base, -4, d, xstride, ystride)?;
        let p2 = get(buf, base, -3, d, xstride, ystride)?;
        let p1 = get(buf, base, -2, d, xstride, ystride)?;
        let p0 = get(buf, base, -1, d, xstride, ystride)?;
        let q0 = get(buf, base, 0, d, xstride, ystride)?;
        let q1 = get(buf, base, 1, d, xstride, ystride)?;
        let q2 = get(buf, base, 2, d, xstride, ystride)?;
        let q3 = get(buf, base, 3, d, xstride, ystride)?;
        if !no_p {
            set(
                buf,
                base,
                -1,
                d,
                xstride,
                ystride,
                p0 + clip(
                    ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) - p0,
                    -tc3,
                    tc3,
                ),
                bit_depth,
            )?;
            set(
                buf,
                base,
                -2,
                d,
                xstride,
                ystride,
                p1 + clip(((p2 + p1 + p0 + q0 + 2) >> 2) - p1, -tc2, tc2),
                bit_depth,
            )?;
            set(
                buf,
                base,
                -3,
                d,
                xstride,
                ystride,
                p2 + clip(((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) - p2, -tc, tc),
                bit_depth,
            )?;
        }
        if !no_q {
            set(
                buf,
                base,
                0,
                d,
                xstride,
                ystride,
                q0 + clip(
                    ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) - q0,
                    -tc3,
                    tc3,
                ),
                bit_depth,
            )?;
            set(
                buf,
                base,
                1,
                d,
                xstride,
                ystride,
                q1 + clip(((p0 + q0 + q1 + q2 + 2) >> 2) - q1, -tc2, tc2),
                bit_depth,
            )?;
            set(
                buf,
                base,
                2,
                d,
                xstride,
                ystride,
                q2 + clip(((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) - q2, -tc, tc),
                bit_depth,
            )?;
        }
    }
    Ok(())
}

/// `FUNC(loop_filter_luma_weak)`.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's loop_filter_luma_weak
pub fn loop_filter_luma_weak(
    buf: &mut [u16],
    base: usize,
    xstride: isize,
    ystride: isize,
    tc: i32,
    no_p: bool,
    no_q: bool,
    nd_p: usize,
    nd_q: usize,
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    let tc_2 = tc >> 1;
    for d in 0isize..4 {
        let p2 = get(buf, base, -3, d, xstride, ystride)?;
        let p1 = get(buf, base, -2, d, xstride, ystride)?;
        let p0 = get(buf, base, -1, d, xstride, ystride)?;
        let q0 = get(buf, base, 0, d, xstride, ystride)?;
        let q1 = get(buf, base, 1, d, xstride, ystride)?;
        let q2 = get(buf, base, 2, d, xstride, ystride)?;
        let mut delta0 = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
        if delta0.abs() < 10 * tc {
            delta0 = clip(delta0, -tc, tc);
            if !no_p {
                set(buf, base, -1, d, xstride, ystride, p0 + delta0, bit_depth)?;
            }
            if !no_q {
                set(buf, base, 0, d, xstride, ystride, q0 - delta0, bit_depth)?;
            }
            if !no_p && nd_p > 1 {
                let deltap1 = clip((((p2 + p0 + 1) >> 1) - p1 + delta0) >> 1, -tc_2, tc_2);
                set(buf, base, -2, d, xstride, ystride, p1 + deltap1, bit_depth)?;
            }
            if !no_q && nd_q > 1 {
                let deltaq1 = clip((((q2 + q0 + 1) >> 1) - q1 - delta0) >> 1, -tc_2, tc_2);
                set(buf, base, 1, d, xstride, ystride, q1 + deltaq1, bit_depth)?;
            }
        }
    }
    Ok(())
}

/// `FUNC(loop_filter_chroma_weak)` for `size` lines (4 for chroma).
// Mirrors FFmpeg's loop_filter_chroma_weak argument list.
#[allow(clippy::too_many_arguments)]
pub fn loop_filter_chroma_weak(
    buf: &mut [u16],
    base: usize,
    xstride: isize,
    ystride: isize,
    size: usize,
    tc: i32,
    no_p: bool,
    no_q: bool,
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    for d in 0..size {
        let d = d as isize;
        let p1 = get(buf, base, -2, d, xstride, ystride)?;
        let p0 = get(buf, base, -1, d, xstride, ystride)?;
        let q0 = get(buf, base, 0, d, xstride, ystride)?;
        let q1 = get(buf, base, 1, d, xstride, ystride)?;
        let delta0 = clip((((q0 - p0) * 4) + p1 - q1 + 4) >> 3, -tc, tc);
        if !no_p {
            set(buf, base, -1, d, xstride, ystride, p0 + delta0, bit_depth)?;
        }
        if !no_q {
            set(buf, base, 0, d, xstride, ystride, q0 - delta0, bit_depth)?;
        }
    }
    Ok(())
}

/// `FUNC(hevc_loop_filter_luma)`: full boundary decision for two 4-line
/// groups, with `tc[0]` / `tc[1]` for the first / second group.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's hevc_loop_filter_luma
pub fn loop_filter_luma(
    buf: &mut [u16],
    base: usize,
    xstride: isize,
    ystride: isize,
    beta: i32,
    tc: &[i32; 2],
    no_p: &[bool; 2],
    no_q: &[bool; 2],
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    let shift = i32::from(bit_depth) - 8;
    let beta = beta << shift;
    for j in 0..2 {
        let group_base = (base as isize + 4 * j as isize * ystride) as usize;
        let abs2 = |a: isize, b: isize, c: isize| -> Result<i32> {
            let v = get(buf, group_base, a, 0, xstride, ystride)?
                - 2 * get(buf, group_base, b, 0, xstride, ystride)?
                + get(buf, group_base, c, 0, xstride, ystride)?;
            Ok(v.abs())
        };
        let abs3 = |a: isize, b: isize, c: isize| -> Result<i32> {
            let v = get(buf, group_base, a, 3, xstride, ystride)?
                - 2 * get(buf, group_base, b, 3, xstride, ystride)?
                + get(buf, group_base, c, 3, xstride, ystride)?;
            Ok(v.abs())
        };
        let dp0 = abs2(-3, -2, -1)?;
        let dq0 = abs2(0, 1, 2)?;
        let dp3 = abs3(-3, -2, -1)?;
        let dq3 = abs3(0, 1, 2)?;
        let d0 = dp0 + dq0;
        let d3 = dp3 + dq3;
        let tc_j = tc[j] << shift;
        if d0 + d3 >= beta {
            continue;
        }
        let beta_3 = beta >> 3;
        let beta_2 = beta >> 2;
        let tc25 = (tc_j * 5 + 1) >> 1;
        let p3p0 = (get(buf, group_base, -4, 0, xstride, ystride)?
            - get(buf, group_base, -1, 0, xstride, ystride)?)
        .abs()
            + (get(buf, group_base, 3, 0, xstride, ystride)?
                - get(buf, group_base, 0, 0, xstride, ystride)?)
            .abs();
        let t3t0 = (get(buf, group_base, -4, 3, xstride, ystride)?
            - get(buf, group_base, -1, 3, xstride, ystride)?)
        .abs()
            + (get(buf, group_base, 3, 3, xstride, ystride)?
                - get(buf, group_base, 0, 3, xstride, ystride)?)
            .abs();
        let p0q0 = (get(buf, group_base, -1, 0, xstride, ystride)?
            - get(buf, group_base, 0, 0, xstride, ystride)?)
        .abs();
        let tp0tq0 = (get(buf, group_base, -1, 3, xstride, ystride)?
            - get(buf, group_base, 0, 3, xstride, ystride)?)
        .abs();
        if p3p0 < beta_3
            && p0q0 < tc25
            && t3t0 < beta_3
            && tp0tq0 < tc25
            && (d0 << 1) < beta_2
            && (d3 << 1) < beta_2
        {
            let tc2 = tc_j << 1;
            loop_filter_luma_strong(
                buf, group_base, xstride, ystride, tc2, tc2, tc2, no_p[j], no_q[j], bit_depth,
            )?;
        } else {
            let threshold = (beta + (beta >> 1)) >> 3;
            let nd_p = if dp0 + dp3 < threshold { 2 } else { 1 };
            let nd_q = if dq0 + dq3 < threshold { 2 } else { 1 };
            loop_filter_luma_weak(
                buf, group_base, xstride, ystride, tc_j, no_p[j], no_q[j], nd_p, nd_q, bit_depth,
            )?;
        }
    }
    Ok(())
}

/// `FUNC(hevc_loop_filter_chroma)` for two 4-line groups.
// Mirrors FFmpeg's hevc_loop_filter_chroma argument list.
#[allow(clippy::too_many_arguments)]
pub fn loop_filter_chroma(
    buf: &mut [u16],
    base: usize,
    xstride: isize,
    ystride: isize,
    tc: &[i32; 2],
    no_p: &[bool; 2],
    no_q: &[bool; 2],
    bit_depth: u8,
) -> Result<()> {
    check_bit_depth(bit_depth)?;
    for j in 0..2 {
        let group_base = (base as isize + 4 * j as isize * ystride) as usize;
        let tc_j = tc[j] << (i32::from(bit_depth) - 8);
        if tc_j > 0 {
            loop_filter_chroma_weak(
                buf, group_base, xstride, ystride, 4, tc_j, no_p[j], no_q[j], bit_depth,
            )?;
        }
    }
    Ok(())
}
