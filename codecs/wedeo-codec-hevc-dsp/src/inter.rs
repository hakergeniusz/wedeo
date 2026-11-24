//! Inter prediction motion compensation.
//!
//! Translated from `libavcodec/h26x/h2656_inter_template.c` and the
//! `put_hevc_*` wrappers in `libavcodec/hevc/dsp_template.c`. Luma uses the
//! 8-tap 1/8-pel filters (`ff_hevc_qpel_filters`), chroma the 4-tap 1/8-pel
//! filters (`ff_hevc_epel_filters`). The `put_*` functions produce FFmpeg's
//! 14-bit intermediate; the `uni_*`/`bi_*` functions round and clip it to
//! picture samples, including weighted prediction.

use crate::{DspError, MAX_PB_SIZE, Result, check_bit_depth, clip_pixel};

/// `ff_hevc_qpel_filters` from `libavcodec/hevc/dsp.c` (first row is a dummy).
pub const QPEL_FILTERS: [[i8; 8]; 4] = [
    [0; 8],
    [-1, 4, -10, 58, 17, -5, 1, 0],
    [-1, 4, -11, 40, 40, -11, 4, -1],
    [0, 1, -5, 17, 58, -10, 4, -1],
];

/// `ff_hevc_epel_filters` from `libavcodec/hevc/dsp.c` (first row is a dummy).
pub const EPEL_FILTERS: [[i8; 4]; 8] = [
    [0; 4],
    [-2, 58, 10, -2],
    [-4, 54, 16, -2],
    [-6, 46, 28, -4],
    [-4, 36, 36, -4],
    [-4, 28, 46, -6],
    [-2, 16, 54, -4],
    [-2, 10, 58, -2],
];

/// Parameters shared by all motion-compensation calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McParams {
    pub bit_depth: u8,
    /// Horizontal fraction, 0..3 for luma (1/4-pel) or 0..7 for chroma.
    pub mx: usize,
    /// Vertical fraction, same range as `mx`.
    pub my: usize,
}

impl McParams {
    pub fn new(bit_depth: u8, mx: usize, my: usize) -> Result<Self> {
        check_bit_depth(bit_depth)?;
        if mx > 7 || my > 7 {
            return Err(DspError::InvalidFraction(mx.max(my)));
        }
        Ok(Self { bit_depth, mx, my })
    }
}

/// Weights for uni-directional weighted prediction (`put_hevc_*_uni_w_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniWeight {
    pub denom: u8,
    pub wx: i32,
    pub ox: i32,
}

/// Weights for bi-directional weighted prediction (`put_hevc_*_bi_w_*`).
///
/// `wx1` applies to the block being filtered, `wx0` to `src2`, matching
/// FFmpeg's argument order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BiWeight {
    pub denom: u8,
    pub wx0: i32,
    pub wx1: i32,
    pub ox: i32,
}

/// Padding around a decoded picture plane. Matches `wedeo-codec-hevc`'s
/// `picture::PAD`: FFmpeg calls `emulated_edge_mc()` whenever an interpolation
/// tap can fall outside the picture, replicating the edge samples into a
/// padded buffer. Our pictures already carry padding, so we replicate by
/// clamping the sample coordinates to the picture area instead.
const MC_PAD: usize = 16;

/// Infer the picture area of a reference plane from its allocated size.
/// Synthetic buffers used by the unit tests are smaller than a padded picture
/// and keep the old unchecked behaviour.
#[inline]
fn picture_bounds(src_len: usize, stride: usize) -> Option<(usize, usize, usize, usize)> {
    if stride <= 2 * MC_PAD || src_len <= stride * (2 * MC_PAD) {
        return None;
    }
    let width = stride - 2 * MC_PAD;
    let height = src_len / stride - 2 * MC_PAD;
    if width == 0 || height == 0 {
        return None;
    }
    Some((MC_PAD, MC_PAD + width - 1, MC_PAD, MC_PAD + height - 1))
}

#[inline]
fn sample(src: &[u16], stride: usize, x: i64, y: i64) -> Result<u16> {
    let (mut x, mut y) = (x, y);
    if let Some((x_min, x_max, y_min, y_max)) = picture_bounds(src.len(), stride) {
        // FFmpeg's `emulated_edge_mc()` replicates edge samples into the
        // padding whenever a filter tap crosses the picture boundary.
        x = x.clamp(x_min as i64, x_max as i64);
        y = y.clamp(y_min as i64, y_max as i64);
    }
    if x < 0 || y < 0 {
        return Err(DspError::OutOfBounds {
            what: "motion compensation source",
            index: 0,
            len: src.len(),
        });
    }
    let index = (y as usize)
        .checked_mul(stride)
        .and_then(|v| v.checked_add(x as usize))
        .ok_or(DspError::OutOfBounds {
            what: "motion compensation source",
            index: usize::MAX,
            len: src.len(),
        })?;
    src.get(index).copied().ok_or(DspError::OutOfBounds {
        what: "motion compensation source",
        index,
        len: src.len(),
    })
}

#[inline]
fn check_geometry(width: usize, height: usize, dst_stride: usize) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_PB_SIZE || height > MAX_PB_SIZE {
        return Err(DspError::BufferTooSmall {
            what: "motion compensation block",
            need: width * height,
            have: MAX_PB_SIZE * MAX_PB_SIZE,
        });
    }
    if dst_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "motion compensation destination stride",
            need: width,
            have: dst_stride,
        });
    }
    Ok(())
}

#[inline]
fn luma_filter(src: &[i32], x: usize, stride: usize, filter: &[i8; 8]) -> i32 {
    i32::from(filter[0]) * src[x - 3 * stride]
        + i32::from(filter[1]) * src[x - 2 * stride]
        + i32::from(filter[2]) * src[x - stride]
        + i32::from(filter[3]) * src[x]
        + i32::from(filter[4]) * src[x + stride]
        + i32::from(filter[5]) * src[x + 2 * stride]
        + i32::from(filter[6]) * src[x + 3 * stride]
        + i32::from(filter[7]) * src[x + 4 * stride]
}

#[inline]
fn chroma_filter(src: &[i32], x: usize, stride: usize, filter: &[i8; 4]) -> i32 {
    i32::from(filter[0]) * src[x - stride]
        + i32::from(filter[1]) * src[x]
        + i32::from(filter[2]) * src[x + stride]
        + i32::from(filter[3]) * src[x + 2 * stride]
}

/// 8-tap luma interpolation into FFmpeg's 14-bit intermediate domain
/// (`FUNC(put_hevc_qpel_pixels)` / `FUNC(put_luma_*)`).
///
/// `src` is the padded reference picture and `(x, y)` the block origin;
/// callers must supply at least 3 samples of padding on every side.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_qpel signature
pub fn put_luma(
    dst: &mut [i16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    if params.mx > 3 || params.my > 3 {
        return Err(DspError::InvalidFraction(params.mx.max(params.my)));
    }
    if dst.len() < height * dst_stride {
        return Err(DspError::BufferTooSmall {
            what: "luma interpolation destination",
            need: height * dst_stride,
            have: dst.len(),
        });
    }
    let scale_shift = 14 - i32::from(params.bit_depth);
    let half = params.mx != 0;
    let vertical = params.my != 0;

    if !half && !vertical {
        for row in 0..height {
            for col in 0..width {
                let v = i32::from(sample(src, src_stride, (x + col) as i64, (y + row) as i64)?)
                    << scale_shift;
                dst[col + row * dst_stride] = v as i16;
            }
        }
        return Ok(());
    }

    let hf = &QPEL_FILTERS[params.mx];
    let vf = &QPEL_FILTERS[params.my];
    let pixel_shift = i32::from(params.bit_depth) - 8;

    if !vertical {
        for row in 0..height {
            for col in 0..width {
                let mut line = [0i32; MAX_PB_SIZE + 8];
                for k in 0..8 {
                    line[k + 4] = i32::from(sample(
                        src,
                        src_stride,
                        x as i64 + col as i64 + k as i64 - 3,
                        y as i64 + row as i64,
                    )?);
                }
                let v = luma_filter(&line, 7, 1, hf) >> pixel_shift;
                dst[col + row * dst_stride] = v as i16;
            }
        }
        return Ok(());
    }

    if !half {
        for row in 0..height {
            for col in 0..width {
                let mut line = [0i32; 8];
                for k in 0..8 {
                    line[k] = i32::from(sample(
                        src,
                        src_stride,
                        x as i64 + col as i64,
                        y as i64 + row as i64 + k as i64 - 3,
                    )?);
                }
                let v = luma_filter(&line, 3, 1, vf) >> pixel_shift;
                dst[col + row * dst_stride] = v as i16;
            }
        }
        return Ok(());
    }

    // h then v, with FFmpeg's LUMA_EXTRA_BEFORE = 3 rows of context.
    let mut tmp = vec![0i32; (MAX_PB_SIZE + 7) * MAX_PB_SIZE];
    const EXTRA: usize = 3;
    for row in 0..height + 7 {
        for col in 0..width {
            let mut line = [0i32; MAX_PB_SIZE + 8];
            for k in 0..8 {
                line[k + 4] = i32::from(sample(
                    src,
                    src_stride,
                    x as i64 + col as i64 + k as i64 - 3,
                    y as i64 + row as i64 - EXTRA as i64,
                )?);
            }
            tmp[col + row * MAX_PB_SIZE] = luma_filter(&line, 7, 1, hf) >> pixel_shift;
        }
    }
    for row in 0..height {
        for col in 0..width {
            let base = (row + EXTRA) * MAX_PB_SIZE + col;
            let v = luma_filter(&tmp, base, MAX_PB_SIZE, vf) >> 6;
            dst[col + row * dst_stride] = v as i16;
        }
    }
    Ok(())
}

/// 4-tap chroma interpolation into FFmpeg's 14-bit intermediate domain
/// (`FUNC(put_hevc_epel_pixels)` / `FUNC(put_chroma_*)`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_epel signature
pub fn put_chroma(
    dst: &mut [i16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    if dst.len() < height * dst_stride {
        return Err(DspError::BufferTooSmall {
            what: "chroma interpolation destination",
            need: height * dst_stride,
            have: dst.len(),
        });
    }
    let scale_shift = 14 - i32::from(params.bit_depth);
    let half = params.mx != 0;
    let vertical = params.my != 0;

    if !half && !vertical {
        for row in 0..height {
            for col in 0..width {
                let v = i32::from(sample(src, src_stride, (x + col) as i64, (y + row) as i64)?)
                    << scale_shift;
                dst[col + row * dst_stride] = v as i16;
            }
        }
        return Ok(());
    }

    let hf = &EPEL_FILTERS[params.mx];
    let vf = &EPEL_FILTERS[params.my];
    let pixel_shift = i32::from(params.bit_depth) - 8;

    if !vertical {
        for row in 0..height {
            for col in 0..width {
                let mut line = [0i32; 6];
                for k in 0..4 {
                    line[k + 1] = i32::from(sample(
                        src,
                        src_stride,
                        x as i64 + col as i64 + k as i64 - 1,
                        y as i64 + row as i64,
                    )?);
                }
                let v = chroma_filter(&line, 2, 1, hf) >> pixel_shift;
                dst[col + row * dst_stride] = v as i16;
            }
        }
        return Ok(());
    }

    if !half {
        for row in 0..height {
            for col in 0..width {
                let mut line = [0i32; 4];
                for k in 0..4 {
                    line[k] = i32::from(sample(
                        src,
                        src_stride,
                        x as i64 + col as i64,
                        y as i64 + row as i64 + k as i64 - 1,
                    )?);
                }
                let v = chroma_filter(&line, 1, 1, vf) >> pixel_shift;
                dst[col + row * dst_stride] = v as i16;
            }
        }
        return Ok(());
    }

    let mut tmp = vec![0i32; (MAX_PB_SIZE + 3) * MAX_PB_SIZE];
    const EXTRA: usize = 1;
    for row in 0..height + 3 {
        for col in 0..width {
            let mut line = [0i32; 6];
            for k in 0..4 {
                line[k + 1] = i32::from(sample(
                    src,
                    src_stride,
                    x as i64 + col as i64 + k as i64 - 1,
                    y as i64 + row as i64 - EXTRA as i64,
                )?);
            }
            tmp[col + row * MAX_PB_SIZE] = chroma_filter(&line, 2, 1, hf) >> pixel_shift;
        }
    }
    for row in 0..height {
        for col in 0..width {
            let base = (row + EXTRA) * MAX_PB_SIZE + col;
            let v = chroma_filter(&tmp, base, MAX_PB_SIZE, vf) >> 6;
            dst[col + row * dst_stride] = v as i16;
        }
    }
    Ok(())
}

fn finish_uni(
    dst: &mut [u16],
    dst_stride: usize,
    tmp: &[i16],
    width: usize,
    height: usize,
    bit_depth: u8,
) -> Result<()> {
    let shift = 14 - i32::from(bit_depth);
    let offset = 1 << (shift - 1);
    for row in 0..height {
        for col in 0..width {
            dst[col + row * dst_stride] = clip_pixel(
                (i32::from(tmp[col + row * MAX_PB_SIZE]) + offset) >> shift,
                bit_depth,
            );
        }
    }
    Ok(())
}

/// Uni-prediction luma MC, rounded and clipped to picture samples
/// (`FUNC(put_hevc_qpel_uni_*)`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_qpel_uni signature
pub fn uni_luma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_luma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_uni(dst, dst_stride, &tmp, width, height, params.bit_depth)
}

/// Uni-prediction chroma MC, rounded and clipped to picture samples.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_epel_uni signature
pub fn uni_chroma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_chroma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_uni(dst, dst_stride, &tmp, width, height, params.bit_depth)
}

fn finish_uni_w(
    dst: &mut [u16],
    dst_stride: usize,
    tmp: &[i16],
    width: usize,
    height: usize,
    weight: UniWeight,
    bit_depth: u8,
) -> Result<()> {
    let shift = i32::from(weight.denom) + 14 - i32::from(bit_depth);
    let offset = 1 << (shift - 1);
    let ox = weight.ox << (bit_depth - 8);
    for row in 0..height {
        for col in 0..width {
            let v = ((i32::from(tmp[col + row * MAX_PB_SIZE]) * weight.wx + offset) >> shift) + ox;
            dst[col + row * dst_stride] = clip_pixel(v, bit_depth);
        }
    }
    Ok(())
}

/// Uni-prediction with weighted samples (`FUNC(put_hevc_*_uni_w_*)`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_qpel_uni_w signature
pub fn uni_w_luma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
    weight: UniWeight,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_luma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_uni_w(
        dst,
        dst_stride,
        &tmp,
        width,
        height,
        weight,
        params.bit_depth,
    )
}

/// Uni-prediction chroma with weighted samples.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_epel_uni_w signature
pub fn uni_w_chroma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
    weight: UniWeight,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_chroma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_uni_w(
        dst,
        dst_stride,
        &tmp,
        width,
        height,
        weight,
        params.bit_depth,
    )
}

// Mirrors FFmpeg's put_hevc_*_bi_* parameters.
#[allow(clippy::too_many_arguments)]
fn finish_bi(
    dst: &mut [u16],
    dst_stride: usize,
    tmp: &[i16],
    src2: &[i16],
    src2_stride: usize,
    width: usize,
    height: usize,
    bit_depth: u8,
) -> Result<()> {
    let shift = 14 + 1 - i32::from(bit_depth);
    let offset = 1 << (shift - 1);
    for row in 0..height {
        for col in 0..width {
            let v =
                i32::from(tmp[col + row * MAX_PB_SIZE]) + i32::from(src2[col + row * src2_stride]);
            dst[col + row * dst_stride] = clip_pixel((v + offset) >> shift, bit_depth);
        }
    }
    Ok(())
}

/// Bi-prediction luma: average the filtered block with `src2`, which holds
/// the second prediction in FFmpeg's 14-bit domain (`put_hevc_qpel_bi_*`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_qpel_bi signature
pub fn bi_luma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    src2: &[i16],
    src2_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    if src2.len() < height * src2_stride || src2_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "bi-prediction source 2",
            need: height * src2_stride,
            have: src2.len(),
        });
    }
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_luma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_bi(
        dst,
        dst_stride,
        &tmp,
        src2,
        src2_stride,
        width,
        height,
        params.bit_depth,
    )
}

/// Bi-prediction chroma (4-tap filter).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_epel_bi signature
pub fn bi_chroma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    src2: &[i16],
    src2_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    if src2.len() < height * src2_stride || src2_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "bi-prediction source 2",
            need: height * src2_stride,
            have: src2.len(),
        });
    }
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_chroma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_bi(
        dst,
        dst_stride,
        &tmp,
        src2,
        src2_stride,
        width,
        height,
        params.bit_depth,
    )
}

// Mirrors FFmpeg's put_hevc_*_bi_w_* parameters.
#[allow(clippy::too_many_arguments)]
fn finish_bi_w(
    dst: &mut [u16],
    dst_stride: usize,
    tmp: &[i16],
    src2: &[i16],
    src2_stride: usize,
    width: usize,
    height: usize,
    weight: BiWeight,
    bit_depth: u8,
) -> Result<()> {
    let shift = 14 + 1 - i32::from(bit_depth);
    let log2_wd = i32::from(weight.denom) + shift - 1;
    let ox = weight.ox << (bit_depth - 8);
    let ox = ox + 1;
    for row in 0..height {
        for col in 0..width {
            let v = i32::from(tmp[col + row * MAX_PB_SIZE]) * weight.wx1
                + i32::from(src2[col + row * src2_stride]) * weight.wx0
                + ox * (1 << log2_wd);
            dst[col + row * dst_stride] = clip_pixel(v >> (log2_wd + 1), bit_depth);
        }
    }
    Ok(())
}

/// Bi-prediction with weighted samples (`FUNC(put_hevc_qpel_bi_w_*)`).
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_qpel_bi_w signature
pub fn bi_w_luma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    src2: &[i16],
    src2_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
    weight: BiWeight,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    if src2.len() < height * src2_stride || src2_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "bi-prediction source 2",
            need: height * src2_stride,
            have: src2.len(),
        });
    }
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_luma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_bi_w(
        dst,
        dst_stride,
        &tmp,
        src2,
        src2_stride,
        width,
        height,
        weight,
        params.bit_depth,
    )
}

/// Bi-prediction chroma with weighted samples.
#[allow(clippy::too_many_arguments)] // mirrors FFmpeg's put_hevc_epel_bi_w signature
pub fn bi_w_chroma(
    dst: &mut [u16],
    dst_stride: usize,
    src: &[u16],
    src_stride: usize,
    src2: &[i16],
    src2_stride: usize,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    params: McParams,
    weight: BiWeight,
) -> Result<()> {
    check_geometry(width, height, dst_stride)?;
    if src2.len() < height * src2_stride || src2_stride < width {
        return Err(DspError::BufferTooSmall {
            what: "bi-prediction source 2",
            need: height * src2_stride,
            have: src2.len(),
        });
    }
    let mut tmp = vec![0i16; height * MAX_PB_SIZE];
    put_chroma(
        &mut tmp,
        MAX_PB_SIZE,
        src,
        src_stride,
        width,
        height,
        x,
        y,
        params,
    )?;
    finish_bi_w(
        dst,
        dst_stride,
        &tmp,
        src2,
        src2_stride,
        width,
        height,
        weight,
        params.bit_depth,
    )
}
