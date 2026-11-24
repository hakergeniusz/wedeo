// VP8 in-loop deblocking filters.
//
// Translated from FFmpeg libavcodec/vp8dsp.c (filter_common,
// vp8_simple_limit, vp8_normal_limit, hev, filter_mbedge, LOOP_FILTER,
// LOOP_FILTER_SIMPLE) and libavcodec/vp8.c (filter_level_for_mb, filter_mb,
// filter_mb_simple, hev_thresh_lut).
//
// LGPL-2.1-or-later, same as FFmpeg.

use crate::header::HeaderInfo;
use crate::types::*;

#[inline]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline]
fn clip_int8(v: i32) -> i32 {
    v.clamp(-128, 127)
}

#[inline]
fn absdiff(a: u8, b: u8) -> i32 {
    (a as i32 - b as i32).abs()
}

/// Which kind of loop-filter edge is being processed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EdgeKind {
    /// Normal macroblock edge: uses `filter_mbedge` for low variance.
    MbEdge,
    /// Normal inner edge: uses the 6-tap filter for low variance.
    Inner,
    /// Simple filter: always the 4-tap filter, no HEV test.
    Simple,
}

/// `vp8_simple_limit`: true if this edge should be filtered.
#[inline]
fn simple_limit(p0: u8, p1: u8, q0: u8, q1: u8, flim: i32) -> bool {
    2 * absdiff(p0, q0) + (absdiff(p1, q1) >> 1) <= flim
}

/// `vp8_normal_limit`: extra interior limits.
/// Mirrors FFmpeg's 10-parameter helper signature.
#[allow(clippy::too_many_arguments)]
#[inline]
fn normal_limit(
    p3: u8,
    p2: u8,
    p1: u8,
    p0: u8,
    q0: u8,
    q1: u8,
    q2: u8,
    q3: u8,
    flim_e: i32,
    flim_i: i32,
) -> bool {
    simple_limit(p0, p1, q0, q1, flim_e)
        && absdiff(p3, p2) <= flim_i
        && absdiff(p2, p1) <= flim_i
        && absdiff(p1, p0) <= flim_i
        && absdiff(q3, q2) <= flim_i
        && absdiff(q2, q1) <= flim_i
        && absdiff(q1, q0) <= flim_i
}

#[inline]
fn high_edge_variance(p1: u8, p0: u8, q0: u8, q1: u8, thresh: i32) -> bool {
    absdiff(p1, p0) > thresh || absdiff(q1, q0) > thresh
}

/// `vp8_filter_common`: the 4-tap (HEV) or 6-tap (non-HEV inner) filter.
#[inline]
fn filter_common(plane: &mut [u8], idx: usize, step: usize, is4tap: bool) {
    let at = |n: isize| -> u8 {
        let i = (idx as isize + n * step as isize) as usize;
        plane[i]
    };
    let p1 = at(-2);
    let p0 = at(-1);
    let q0 = at(0);
    let q1 = at(1);

    let mut a = 3 * (q0 as i32 - p0 as i32);
    if is4tap {
        a += clip_int8(p1 as i32 - q1 as i32);
    }
    a = clip_int8(a);

    // We deviate from the spec here with c(a+3) >> 3 since that's what
    // libvpx does.
    let f1 = (a + 4).min(127) >> 3;
    let f2 = (a + 3).min(127) >> 3;

    // Despite what the spec says, we do need to clamp here to be bitexact
    // with libvpx.
    let i_m1 = (idx as isize - step as isize) as usize;
    let i_0 = idx;
    plane[i_m1] = clip_u8(p0 as i32 + f2);
    plane[i_0] = clip_u8(q0 as i32 - f1);

    if !is4tap {
        let a2 = (f1 + 1) >> 1;
        let i_m2 = (idx as isize - 2 * step as isize) as usize;
        let i_1 = (idx as isize + step as isize) as usize;
        plane[i_m2] = clip_u8(p1 as i32 + a2);
        plane[i_1] = clip_u8(q1 as i32 - a2);
    }
}

/// `filter_mbedge`: the special macroblock-edge filter for low-variance
/// (non-HEV) edges.
#[inline]
fn filter_mbedge(plane: &mut [u8], idx: usize, step: usize) {
    let at = |n: isize| -> u8 {
        let i = (idx as isize + n * step as isize) as usize;
        plane[i]
    };
    let p2 = at(-3);
    let p1 = at(-2);
    let p0 = at(-1);
    let q0 = at(0);
    let q1 = at(1);
    let q2 = at(2);

    let mut w = clip_int8(p1 as i32 - q1 as i32);
    w = clip_int8(w + 3 * (q0 as i32 - p0 as i32));

    let a0 = (27 * w + 63) >> 7;
    let a1 = (18 * w + 63) >> 7;
    let a2 = (9 * w + 63) >> 7;

    let mut put = |n: isize, v: u8| {
        let i = (idx as isize + n * step as isize) as usize;
        plane[i] = v;
    };
    put(-3, clip_u8(p2 as i32 + a2));
    put(-2, clip_u8(p1 as i32 + a1));
    put(-1, clip_u8(p0 as i32 + a0));
    put(0, clip_u8(q0 as i32 - a0));
    put(1, clip_u8(q1 as i32 - a1));
    put(2, clip_u8(q2 as i32 - a2));
}

/// Filter one entire line of edges.
/// `base`: index of q0 for the first edge. `step`: pixel step along the
/// filter direction. `line_step`: step to the next edge line. `count`:
/// number of edges.
#[allow(clippy::too_many_arguments)]
fn filter_line(
    plane: &mut [u8],
    base: usize,
    step: usize,
    line_step: usize,
    count: usize,
    flim_e: i32,
    flim_i: i32,
    hev_thresh: i32,
    kind: EdgeKind,
) {
    for i in 0..count {
        let idx = (base + i * line_step) as isize;
        let at = |n: isize| -> u8 { plane[(idx + n * step as isize) as usize] };
        let (p3, p2, p1, p0, q0, q1, q2, q3) =
            (at(-4), at(-3), at(-2), at(-1), at(0), at(1), at(2), at(3));

        let limit = match kind {
            EdgeKind::Simple => simple_limit(p0, p1, q0, q1, flim_e),
            EdgeKind::MbEdge | EdgeKind::Inner => {
                normal_limit(p3, p2, p1, p0, q0, q1, q2, q3, flim_e, flim_i)
            }
        };
        if !limit {
            continue;
        }

        match kind {
            EdgeKind::Simple => filter_common(plane, idx as usize, step, true),
            EdgeKind::MbEdge => {
                if high_edge_variance(p1, p0, q0, q1, hev_thresh) {
                    filter_common(plane, idx as usize, step, true);
                } else {
                    filter_mbedge(plane, idx as usize, step);
                }
            }
            EdgeKind::Inner => {
                // FFmpeg: high edge variance selects the 4-tap filter,
                // low variance uses the 6-tap filter.
                let hev = high_edge_variance(p1, p0, q0, q1, hev_thresh);
                filter_common(plane, idx as usize, step, hev);
            }
        }
    }
}

/// `hev_thresh_lut` from vp8.c (keyframe / non-keyframe variants).
const HEV_THRESH_LUT: [[u8; 64]; 2] = [
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
        3, 3, 3, 3,
    ],
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 2, 2,
    ],
];

/// Compute the loop filter strength for one macroblock (`filter_level_for_mb`).
pub fn filter_level_for_mb(header: &HeaderInfo, mb: &MacroblockState) -> FilterStrength {
    let mut filter_level: i32 = if header.segmentation.enabled {
        let mut v = header.segmentation.filter_level[mb.segment] as i32;
        if !header.segmentation.absolute_vals {
            v += header.filter_level as i32;
        }
        v
    } else {
        header.filter_level as i32
    };

    if header.lf_delta.enabled {
        filter_level += header.lf_delta.ref_delta[mb.ref_frame.index()] as i32;
        filter_level += header.lf_delta.mode[mb.mode as usize] as i32;
    }
    // av_clip_uintp2(filter_level, 6) clamps to [0, 63]; a bitmask would
    // wrap negative per-MB deltas into large filter strengths instead.
    filter_level = crate::header::av_clip_uintp2(filter_level, 6) as i32;

    let mut interior_limit = filter_level;
    if header.filter_sharpness != 0 {
        interior_limit >>= (header.filter_sharpness as i32 + 3) >> 2;
        interior_limit = interior_limit.min(9 - header.filter_sharpness as i32);
    }
    interior_limit = interior_limit.max(1);

    FilterStrength {
        filter_level: filter_level as u8,
        inner_limit: interior_limit as u8,
        inner_filter: !mb.skip || mb.mode == MODE_I4x4 || mb.mode == MVMODE_SPLIT,
    }
}

/// Per-macroblock state needed by the loop filter (subset of the full MB).
#[derive(Clone, Copy, Debug)]
pub struct MacroblockState {
    pub skip: bool,
    pub mode: u8,
    pub ref_frame: FrameType,
    pub segment: usize,
}

/// Apply the loop filter to one macroblock (`filter_mb` / `filter_mb_simple`).
/// `plane` slices are the full Y/U/V planes; `mb_x`/`mb_y` the MB position.
#[allow(clippy::too_many_arguments)]
pub fn filter_mb(
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
    y_stride: usize,
    uv_stride: usize,
    base_y: usize,
    base_uv: usize,
    mb_x: usize,
    mb_y: usize,
    f: FilterStrength,
    simple: bool,
    keyframe: bool,
) {
    let filter_level = f.filter_level as i32;
    if filter_level == 0 {
        return;
    }
    let inner_limit = f.inner_limit as i32;
    let inner_filter = f.inner_filter;

    let y_base = base_y + mb_y * 16 * y_stride + mb_x * 16;
    let u_base = base_uv + mb_y * 8 * uv_stride + mb_x * 8;
    let v_base = u_base;

    // FFmpeg's `vp8_h_loop_filter*` (vertical edges) walks rows: `step` is
    // the pixel step within one edge (1 for luma/chroma byte planes),
    // `line_step` moves to the next row. FFmpeg's `vp8_v_loop_filter*`
    // (horizontal edges) walks columns: `step` is the plane stride and
    // `line_step` is 1.
    if simple {
        let bedge_lim = 2 * filter_level + inner_limit;
        let mbedge_lim = bedge_lim + 4;
        // Left macroblock edge (vertical): filter along x.
        if mb_x != 0 {
            filter_line(
                y,
                y_base,
                1,
                y_stride,
                16,
                mbedge_lim,
                0,
                0,
                EdgeKind::Simple,
            );
        }
        if inner_filter {
            for off in [4, 8, 12] {
                filter_line(
                    y,
                    y_base + off,
                    1,
                    y_stride,
                    16,
                    bedge_lim,
                    0,
                    0,
                    EdgeKind::Simple,
                );
            }
        }
        // Top macroblock edge (horizontal): filter along y.
        if mb_y != 0 {
            filter_line(
                y,
                y_base,
                y_stride,
                1,
                16,
                mbedge_lim,
                0,
                0,
                EdgeKind::Simple,
            );
        }
        if inner_filter {
            for off in [4, 8, 12] {
                filter_line(
                    y,
                    y_base + off * y_stride,
                    y_stride,
                    1,
                    16,
                    bedge_lim,
                    0,
                    0,
                    EdgeKind::Simple,
                );
            }
        }
        return;
    }

    let bedge_lim = 2 * filter_level + inner_limit;
    let mbedge_lim = bedge_lim + 4;
    let hev_thresh = HEV_THRESH_LUT[usize::from(keyframe)][filter_level as usize] as i32;

    // Left macroblock edge (vertical): filter along x, walk rows.
    if mb_x != 0 {
        filter_line(
            y,
            y_base,
            1,
            y_stride,
            16,
            mbedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::MbEdge,
        );
        filter_line(
            u,
            u_base,
            1,
            uv_stride,
            8,
            mbedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::MbEdge,
        );
        filter_line(
            v,
            v_base,
            1,
            uv_stride,
            8,
            mbedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::MbEdge,
        );
    }
    // Inner vertical edges: filter along x, walk rows.
    if inner_filter {
        for off in [4, 8, 12] {
            filter_line(
                y,
                y_base + off,
                1,
                y_stride,
                16,
                bedge_lim,
                inner_limit,
                hev_thresh,
                EdgeKind::Inner,
            );
        }
        filter_line(
            u,
            u_base + 4,
            1,
            uv_stride,
            8,
            bedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::Inner,
        );
        filter_line(
            v,
            v_base + 4,
            1,
            uv_stride,
            8,
            bedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::Inner,
        );
    }
    // Top macroblock edge (horizontal): filter along y, walk columns.
    if mb_y != 0 {
        filter_line(
            y,
            y_base,
            y_stride,
            1,
            16,
            mbedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::MbEdge,
        );
        filter_line(
            u,
            u_base,
            uv_stride,
            1,
            8,
            mbedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::MbEdge,
        );
        filter_line(
            v,
            v_base,
            uv_stride,
            1,
            8,
            mbedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::MbEdge,
        );
    }
    // Inner horizontal edges: filter along y, walk columns.
    if inner_filter {
        for off in [4, 8, 12] {
            filter_line(
                y,
                y_base + off * y_stride,
                y_stride,
                1,
                16,
                bedge_lim,
                inner_limit,
                hev_thresh,
                EdgeKind::Inner,
            );
        }
        filter_line(
            u,
            u_base + 4 * uv_stride,
            uv_stride,
            1,
            8,
            bedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::Inner,
        );
        filter_line(
            v,
            v_base + 4 * uv_stride,
            uv_stride,
            1,
            8,
            bedge_lim,
            inner_limit,
            hev_thresh,
            EdgeKind::Inner,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_level_noop() {
        let mut y = vec![100u8; 32 * 32];
        let f = FilterStrength::default();
        filter_mb(&mut y, &mut [], &mut [], 32, 16, 0, 0, 1, 1, f, false, true);
        assert!(y.iter().all(|&v| v == 100));
    }
}
