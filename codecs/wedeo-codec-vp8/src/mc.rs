// VP8 motion compensation.
//
// Translated from FFmpeg libavcodec/vp8.c (`vp8_mc_luma`, `vp8_mc_chroma`,
// `vp8_mc_part`, `inter_predict`) and the sub-pixel filters in
// libavcodec/vp8dsp.c (`subpel_filters`, EPEL 4/6-tap and bilinear).
// Edge emulation is performed by clamping source coordinates to the
// reference plane, which is equivalent to `ff_emulated_edge_mc`.
//
// LGPL-2.1-or-later, same as FFmpeg.

use crate::types::*;

/// `subpel_idx[0]`: number of left extra pixels / filter function index.
const SUBPEL_IDX: [usize; 8] = [0, 1, 2, 1, 2, 1, 2, 1];
/// `subpel_idx[1]`: total number of extra pixels required.
const _SUBPEL_EXTRA: [usize; 8] = [0, 3, 5, 3, 5, 3, 5, 3];
/// `subpel_idx[2]`: number of right extra pixels required.
const _SUBPEL_RIGHT: [usize; 8] = [0, 2, 3, 2, 3, 2, 3, 2];

/// `subpel_filters` from vp8dsp.c.
const SUBPEL_FILTERS: [[u8; 6]; 7] = [
    [0, 6, 123, 12, 1, 0],
    [2, 11, 108, 36, 8, 1],
    [0, 9, 93, 50, 6, 0],
    [3, 16, 77, 77, 16, 3],
    [0, 6, 50, 93, 9, 0],
    [1, 8, 36, 108, 11, 2],
    [0, 1, 12, 123, 6, 0],
];

#[inline]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline]
fn get_px(plane: &[u8], stride: usize, w: usize, h: usize, x: i32, y: i32) -> u8 {
    let cx = x.clamp(0, w as i32 - 1) as usize;
    let cy = y.clamp(0, h as i32 - 1) as usize;
    plane[cy * stride + cx]
}

/// EPEL 4/6-tap horizontal filter at absolute source position (sx, sy).
/// Mirrors FFmpeg's 8-parameter `FILTER_6TAP` macro usage.
#[allow(clippy::too_many_arguments)]
#[inline]
fn filter_h(
    plane: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    sx: i32,
    sy: i32,
    f: &[u8; 6],
    taps: usize,
) -> u8 {
    macro_rules! p {
        ($dx:expr) => {
            get_px(plane, stride, w, h, sx + $dx, sy) as i32
        };
    }
    let v = if taps == 6 {
        f[2] as i32 * p!(0) - f[1] as i32 * p!(-1) + f[0] as i32 * p!(-2) + f[3] as i32 * p!(1)
            - f[4] as i32 * p!(2)
            + f[5] as i32 * p!(3)
            + 64
    } else {
        f[2] as i32 * p!(0) - f[1] as i32 * p!(-1) + f[3] as i32 * p!(1) - f[4] as i32 * p!(2) + 64
    };
    clip_u8(v >> 7)
}

/// EPEL 4/6-tap vertical filter over horizontally-filtered rows.
#[inline]
fn filter_v(mid: &[u8], mid_w: usize, row_base: usize, x: usize, f: &[u8; 6], taps: usize) -> u8 {
    let g = |k: i32| -> i32 {
        let idx = (row_base as i32 + k) as usize * mid_w + x;
        if idx < mid.len() { mid[idx] as i32 } else { 0 }
    };
    let v = if taps == 6 {
        f[2] as i32 * g(0) - f[1] as i32 * g(-1) + f[0] as i32 * g(-2) + f[3] as i32 * g(1)
            - f[4] as i32 * g(2)
            + f[5] as i32 * g(3)
            + 64
    } else {
        f[2] as i32 * g(0) - f[1] as i32 * g(-1) + f[3] as i32 * g(1) - f[4] as i32 * g(2) + 64
    };
    clip_u8(v >> 7)
}

/// Motion-compensate one block from a reference plane.
///
/// `x_off`/`y_off` are the full-pixel destination-relative offsets (already
/// including the integer part of the MV). `mx`/`my` are the sub-pixel phases
/// (0..=7). When `epel` is true the 6/4-tap EPEL filters are used; otherwise
/// the bilinear filters (VP8 profile > 0) are used.
#[allow(clippy::too_many_arguments)]
pub fn mc_block(
    dst: &mut [u8],
    dst_base: usize,
    dst_stride: usize,
    plane: &[u8],
    plane_stride: usize,
    pw: usize,
    ph: usize,
    x_off: i32,
    y_off: i32,
    w: usize,
    h: usize,
    mx: usize,
    my: usize,
    epel: bool,
) {
    if epel {
        let mx_idx = SUBPEL_IDX[mx];
        let my_idx = SUBPEL_IDX[my];
        let h_taps = if mx_idx == 1 {
            4
        } else if mx_idx == 2 {
            6
        } else {
            0
        };
        let v_taps = if my_idx == 1 {
            4
        } else if my_idx == 2 {
            6
        } else {
            0
        };

        // Build horizontally-filtered (or plain-copy) intermediate rows
        // when a vertical filter is needed. `y_start` is the source-row
        // offset of intermediate row 0 relative to `y_off`.
        let y_start: i32 = if v_taps > 0 {
            -(2 - i32::from(v_taps == 4))
        } else {
            0
        };
        let mut mid = Vec::<u8>::new();
        if h_taps > 0 || v_taps > 0 {
            let f = if h_taps > 0 {
                Some(&SUBPEL_FILTERS[mx - 1])
            } else {
                None
            };
            let count = if v_taps > 0 { h + v_taps - 1 } else { h };
            for iy in 0..count {
                let sy = y_off + y_start + iy as i32;
                for ix in 0..w {
                    let value = if let Some(f) = f {
                        filter_h(
                            plane,
                            plane_stride,
                            pw,
                            ph,
                            x_off + ix as i32,
                            sy,
                            f,
                            h_taps,
                        )
                    } else {
                        get_px(plane, plane_stride, pw, ph, x_off + ix as i32, sy)
                    };
                    mid.push(value);
                }
            }
        }

        for y in 0..h {
            let doff = dst_base + y * dst_stride;
            for x in 0..w {
                let value = if v_taps > 0 {
                    let f = &SUBPEL_FILTERS[my - 1];
                    // mid row 0 corresponds to sy = y_off + y_start.
                    let row_base = y as i32 - y_start;
                    filter_v(&mid, w, row_base as usize, x, f, v_taps)
                } else if h_taps > 0 {
                    let row_base = (y as i32 - y_start) as usize;
                    mid[row_base * w + x]
                } else {
                    get_px(
                        plane,
                        plane_stride,
                        pw,
                        ph,
                        x_off + x as i32,
                        y_off + y as i32,
                    )
                };
                dst[doff + x] = value;
            }
        }
    } else {
        // Bilinear MC (VP8 profile > 0). Chroma always uses this path too.
        let a = 8i32 - mx as i32;
        let b = mx as i32;
        let c = 8i32 - my as i32;
        let d = my as i32;

        let mut mid = Vec::<u8>::new();
        if mx != 0 || my != 0 {
            let rows = if my != 0 { h + 1 } else { h };
            for iy in 0..rows {
                let sy = y_off + iy as i32;
                for ix in 0..w {
                    let sx = x_off + ix as i32;
                    let v = if mx != 0 {
                        let p0 = get_px(plane, plane_stride, pw, ph, sx, sy) as i32;
                        let p1 = get_px(plane, plane_stride, pw, ph, sx + 1, sy) as i32;
                        (a * p0 + b * p1 + 4) >> 3
                    } else {
                        get_px(plane, plane_stride, pw, ph, sx, sy) as i32
                    };
                    mid.push(clip_u8(v));
                }
            }
        }

        for y in 0..h {
            let doff = dst_base + y * dst_stride;
            for x in 0..w {
                let value = if my != 0 {
                    let p0 = mid[y * w + x] as i32;
                    let p1 = mid[(y + 1) * w + x] as i32;
                    clip_u8((c * p0 + d * p1 + 4) >> 3)
                } else if mx != 0 {
                    mid[y * w + x]
                } else {
                    get_px(
                        plane,
                        plane_stride,
                        pw,
                        ph,
                        x_off + x as i32,
                        y_off + y as i32,
                    )
                };
                dst[doff + x] = value;
            }
        }
    }
}

/// `vp8_mc_part`: motion-compensate one rectangular partition (luma + chroma)
/// of a macroblock from a reference frame.
#[allow(clippy::too_many_arguments)]
pub fn mc_part(
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
    dst_stride_y: usize,
    dst_stride_uv: usize,
    ref_y: &[u8],
    ref_u: &[u8],
    ref_v: &[u8],
    ref_stride_y: usize,
    ref_stride_uv: usize,
    pw: usize,
    ph: usize,
    x_off: usize,
    y_off: usize,
    bx_off: usize,
    by_off: usize,
    block_w: usize,
    block_h: usize,
    mv: Mv,
    epel: bool,
) {
    // Luma: MV has quarter-pixel precision (mx = (mv.x*2)&7).
    let mx = ((mv.x as i32 * 2) & 7) as usize;
    let my = ((mv.y as i32 * 2) & 7) as usize;
    mc_block(
        dst_y,
        (y_off + by_off) * dst_stride_y + x_off + bx_off,
        dst_stride_y,
        ref_y,
        ref_stride_y,
        pw,
        ph,
        (x_off + bx_off) as i32 + (mv.x as i32 >> 2),
        (y_off + by_off) as i32 + (mv.y as i32 >> 2),
        block_w,
        block_h,
        mx,
        my,
        epel,
    );

    // Chroma: MV has 1/8-pixel precision (mx = mv.x & 7).
    let mut uvmv = mv;
    let cmx = (uvmv.x as i32 & 7) as usize;
    let cmy = (uvmv.y as i32 & 7) as usize;
    let _ = &mut uvmv;
    let cx_off = x_off >> 1;
    let cy_off = y_off >> 1;
    let cbx = bx_off >> 1;
    let cby = by_off >> 1;
    mc_block(
        dst_u,
        (cy_off + cby) * dst_stride_uv + cx_off + cbx,
        dst_stride_uv,
        ref_u,
        ref_stride_uv,
        pw >> 1,
        ph >> 1,
        (cx_off + cbx) as i32 + (mv.x as i32 >> 3),
        (cy_off + cby) as i32 + (mv.y as i32 >> 3),
        block_w >> 1,
        block_h >> 1,
        cmx,
        cmy,
        false,
    );
    mc_block(
        dst_v,
        (cy_off + cby) * dst_stride_uv + cx_off + cbx,
        dst_stride_uv,
        ref_v,
        ref_stride_uv,
        pw >> 1,
        ph >> 1,
        (cx_off + cbx) as i32 + (mv.x as i32 >> 3),
        (cy_off + cby) as i32 + (mv.y as i32 >> 3),
        block_w >> 1,
        block_h >> 1,
        cmx,
        cmy,
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_mc_identity() {
        let mut ref_y = vec![0u8; 32 * 32];
        for y in 0..32 {
            for x in 0..32 {
                ref_y[y * 32 + x] = (x * 7 + y * 13) as u8;
            }
        }
        let mut dst = vec![0u8; 16 * 16];
        mc_block(
            &mut dst, 0, 16, &ref_y, 32, 32, 32, 2, 2, 16, 16, 0, 0, true,
        );
        for y in 0..16 {
            for x in 0..16 {
                assert_eq!(dst[y * 16 + x], ref_y[(y + 2) * 32 + x + 2]);
            }
        }
    }
}
