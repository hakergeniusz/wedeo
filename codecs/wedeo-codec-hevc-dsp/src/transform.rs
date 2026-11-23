//! HEVC inverse transforms (`libavcodec/hevc/dsp_template.c`,
//! `libavcodec/hevc/dsp.c`).
//!
//! DCT-II sizes 4x4, 8x8, 16x16 and 32x32 use FFmpeg's integer matrix and
//! exactly the same intermediate clipping as the C code. Transform-skip
//! (`transform_rdpcm`) and the DC-only IDCT are included as well.

use crate::{Result, check_bit_depth, check_size, clip_i16};

/// `static const int8_t transform[32][32]` from `libavcodec/hevc/dsp.c`.
pub const TRANSFORM: [[i16; 32]; 32] = [
    [
        64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64, 64,
        64, 64, 64, 64, 64, 64, 64, 64, 64,
    ],
    [
        90, 90, 88, 85, 82, 78, 73, 67, 61, 54, 46, 38, 31, 22, 13, 4, -4, -13, -22, -31, -38, -46,
        -54, -61, -67, -73, -78, -82, -85, -88, -90, -90,
    ],
    [
        90, 87, 80, 70, 57, 43, 25, 9, -9, -25, -43, -57, -70, -80, -87, -90, -90, -87, -80, -70,
        -57, -43, -25, -9, 9, 25, 43, 57, 70, 80, 87, 90,
    ],
    [
        90, 82, 67, 46, 22, -4, -31, -54, -73, -85, -90, -88, -78, -61, -38, -13, 13, 38, 61, 78,
        88, 90, 85, 73, 54, 31, 4, -22, -46, -67, -82, -90,
    ],
    [
        89, 75, 50, 18, -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89, 89, 75, 50, 18,
        -18, -50, -75, -89, -89, -75, -50, -18, 18, 50, 75, 89,
    ],
    [
        88, 67, 31, -13, -54, -82, -90, -78, -46, -4, 38, 73, 90, 85, 61, 22, -22, -61, -85, -90,
        -73, -38, 4, 46, 78, 90, 82, 54, 13, -31, -67, -88,
    ],
    [
        87, 57, 9, -43, -80, -90, -70, -25, 25, 70, 90, 80, 43, -9, -57, -87, -87, -57, -9, 43, 80,
        90, 70, 25, -25, -70, -90, -80, -43, 9, 57, 87,
    ],
    [
        85, 46, -13, -67, -90, -73, -22, 38, 82, 88, 54, -4, -61, -90, -78, -31, 31, 78, 90, 61, 4,
        -54, -88, -82, -38, 22, 73, 90, 67, 13, -46, -85,
    ],
    [
        83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83, 83, 36, -36, -83,
        -83, -36, 36, 83, 83, 36, -36, -83, -83, -36, 36, 83,
    ],
    [
        82, 22, -54, -90, -61, 13, 78, 85, 31, -46, -90, -67, 4, 73, 88, 38, -38, -88, -73, -4, 67,
        90, 46, -31, -85, -78, -13, 61, 90, 54, -22, -82,
    ],
    [
        80, 9, -70, -87, -25, 57, 90, 43, -43, -90, -57, 25, 87, 70, -9, -80, -80, -9, 70, 87, 25,
        -57, -90, -43, 43, 90, 57, -25, -87, -70, 9, 80,
    ],
    [
        78, -4, -82, -73, 13, 85, 67, -22, -88, -61, 31, 90, 54, -38, -90, -46, 46, 90, 38, -54,
        -90, -31, 61, 88, 22, -67, -85, -13, 73, 82, 4, -78,
    ],
    [
        75, -18, -89, -50, 50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75, 75, -18, -89, -50,
        50, 89, 18, -75, -75, 18, 89, 50, -50, -89, -18, 75,
    ],
    [
        73, -31, -90, -22, 78, 67, -38, -90, -13, 82, 61, -46, -88, -4, 85, 54, -54, -85, 4, 88,
        46, -61, -82, 13, 90, 38, -67, -78, 22, 90, 31, -73,
    ],
    [
        70, -43, -87, 9, 90, 25, -80, -57, 57, 80, -25, -90, -9, 87, 43, -70, -70, 43, 87, -9, -90,
        -25, 80, 57, -57, -80, 25, 90, 9, -87, -43, 70,
    ],
    [
        67, -54, -78, 38, 85, -22, -90, 4, 90, 13, -88, -31, 82, 46, -73, -61, 61, 73, -46, -82,
        31, 88, -13, -90, -4, 90, 22, -85, -38, 78, 54, -67,
    ],
    [
        64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64,
        64, -64, -64, 64, 64, -64, -64, 64, 64, -64, -64, 64,
    ],
    [
        61, -73, -46, 82, 31, -88, -13, 90, -4, -90, 22, 85, -38, -78, 54, 67, -67, -54, 78, 38,
        -85, -22, 90, 4, -90, 13, 88, -31, -82, 46, 73, -61,
    ],
    [
        57, -80, -25, 90, -9, -87, 43, 70, -70, -43, 87, 9, -90, 25, 80, -57, -57, 80, 25, -90, 9,
        87, -43, -70, 70, 43, -87, -9, 90, -25, -80, 57,
    ],
    [
        54, -85, -4, 88, -46, -61, 82, 13, -90, 38, 67, -78, -22, 90, -31, -73, 73, 31, -90, 22,
        78, -67, -38, 90, -13, -82, 61, 46, -88, 4, 85, -54,
    ],
    [
        50, -89, 18, 75, -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50, 50, -89, 18, 75,
        -75, -18, 89, -50, -50, 89, -18, -75, 75, 18, -89, 50,
    ],
    [
        46, -90, 38, 54, -90, 31, 61, -88, 22, 67, -85, 13, 73, -82, 4, 78, -78, -4, 82, -73, -13,
        85, -67, -22, 88, -61, -31, 90, -54, -38, 90, -46,
    ],
    [
        43, -90, 57, 25, -87, 70, 9, -80, 80, -9, -70, 87, -25, -57, 90, -43, -43, 90, -57, -25,
        87, -70, -9, 80, -80, 9, 70, -87, 25, 57, -90, 43,
    ],
    [
        38, -88, 73, -4, -67, 90, -46, -31, 85, -78, 13, 61, -90, 54, 22, -82, 82, -22, -54, 90,
        -61, -13, 78, -85, 31, 46, -90, 67, 4, -73, 88, -38,
    ],
    [
        36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36, 36, -83, 83, -36,
        -36, 83, -83, 36, 36, -83, 83, -36, -36, 83, -83, 36,
    ],
    [
        31, -78, 90, -61, 4, 54, -88, 82, -38, -22, 73, -90, 67, -13, -46, 85, -85, 46, 13, -67,
        90, -73, 22, 38, -82, 88, -54, -4, 61, -90, 78, -31,
    ],
    [
        25, -70, 90, -80, 43, 9, -57, 87, -87, 57, -9, -43, 80, -90, 70, -25, -25, 70, -90, 80,
        -43, -9, 57, -87, 87, -57, 9, 43, -80, 90, -70, 25,
    ],
    [
        22, -61, 85, -90, 73, -38, -4, 46, -78, 90, -82, 54, -13, -31, 67, -88, 88, -67, 31, 13,
        -54, 82, -90, 78, -46, 4, 38, -73, 90, -85, 61, -22,
    ],
    [
        18, -50, 75, -89, 89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18, 18, -50, 75, -89,
        89, -75, 50, -18, -18, 50, -75, 89, -89, 75, -50, 18,
    ],
    [
        13, -38, 61, -78, 88, -90, 85, -73, 54, -31, 4, 22, -46, 67, -82, 90, -90, 82, -67, 46,
        -22, -4, 31, -54, 73, -85, 90, -88, 78, -61, 38, -13,
    ],
    [
        9, -25, 43, -57, 70, -80, 87, -90, 90, -87, 80, -70, 57, -43, 25, -9, -9, 25, -43, 57, -70,
        80, -87, 90, -90, 87, -80, 70, -57, 43, -25, 9,
    ],
    [
        4, -13, 22, -31, 38, -46, 54, -61, 67, -73, 78, -82, 85, -88, 90, -90, 90, -90, 88, -85,
        82, -78, 73, -67, 61, -54, 46, -38, 31, -22, 13, -4,
    ],
];

macro_rules! set_v {
    ($dst:expr, $val:expr, $shift:expr, $add:expr) => {{
        let _ = ($shift, $add);
        $dst = $val;
    }};
}

macro_rules! scale_v {
    ($dst:expr, $val:expr, $shift:expr, $add:expr) => {{
        $dst = clip_i16(($val + $add) >> $shift) as i32;
    }};
}

/// `TR_4` from `libavcodec/hevc/dsp_template.c`.
macro_rules! tr4 {
    ($dst:expr, $dst_off:expr, $dstep:expr, $src:expr, $src_off:expr, $sstep:expr,
     $assign:ident, $shift:expr, $add:expr) => {{
        let s0 = $src[$src_off];
        let s1 = $src[$src_off + $sstep];
        let s2 = $src[$src_off + 2 * $sstep];
        let s3 = $src[$src_off + 3 * $sstep];
        let e0 = 64 * s0 + 64 * s2;
        let e1 = 64 * s0 - 64 * s2;
        let o0 = 83 * s1 + 36 * s3;
        let o1 = 36 * s1 - 83 * s3;
        $assign!($dst[$dst_off], e0 + o0, $shift, $add);
        $assign!($dst[$dst_off + $dstep], e1 + o1, $shift, $add);
        $assign!($dst[$dst_off + 2 * $dstep], e1 - o1, $shift, $add);
        $assign!($dst[$dst_off + 3 * $dstep], e0 - o0, $shift, $add);
    }};
}

/// `TR_8` from `libavcodec/hevc/dsp_template.c`.
macro_rules! tr8 {
    ($dst:expr, $dst_off:expr, $dstep:expr, $src:expr, $src_off:expr, $sstep:expr,
     $assign:ident, $shift:expr, $add:expr, $end:expr) => {{
        let mut e_8 = [0i32; 4];
        let mut o_8 = [0i32; 4];
        for i in 0..4 {
            for j in (1..$end).step_by(2) {
                o_8[i] += i32::from(TRANSFORM[4 * j][i]) * $src[$src_off + j * $sstep];
            }
        }
        tr4!(e_8, 0, 1, $src, $src_off, 2 * $sstep, set_v, 0, 0);
        for i in 0..4 {
            $assign!($dst[$dst_off + i * $dstep], e_8[i] + o_8[i], $shift, $add);
            $assign!(
                $dst[$dst_off + (7 - i) * $dstep],
                e_8[i] - o_8[i],
                $shift,
                $add
            );
        }
    }};
}

/// `TR_16` from `libavcodec/hevc/dsp_template.c`.
macro_rules! tr16 {
    ($dst:expr, $dst_off:expr, $dstep:expr, $src:expr, $src_off:expr, $sstep:expr,
     $assign:ident, $shift:expr, $add:expr, $end:expr) => {{
        let mut e_16 = [0i32; 8];
        let mut o_16 = [0i32; 8];
        for i in 0..8 {
            for j in (1..$end).step_by(2) {
                o_16[i] += i32::from(TRANSFORM[2 * j][i]) * $src[$src_off + j * $sstep];
            }
        }
        // FFmpeg hardcodes `end = 8` here (not `$end / 2`).
        tr8!(e_16, 0, 1, $src, $src_off, 2 * $sstep, set_v, 0, 0, 8);
        for i in 0..8 {
            $assign!($dst[$dst_off + i * $dstep], e_16[i] + o_16[i], $shift, $add);
            $assign!(
                $dst[$dst_off + (15 - i) * $dstep],
                e_16[i] - o_16[i],
                $shift,
                $add
            );
        }
    }};
}

/// `TR_32` from `libavcodec/hevc/dsp_template.c`.
macro_rules! tr32 {
    ($dst:expr, $dst_off:expr, $dstep:expr, $src:expr, $src_off:expr, $sstep:expr,
     $assign:ident, $shift:expr, $add:expr, $end:expr) => {{
        let mut e_32 = [0i32; 16];
        let mut o_32 = [0i32; 16];
        for i in 0..16 {
            for j in (1..$end).step_by(2) {
                o_32[i] += i32::from(TRANSFORM[j][i]) * $src[$src_off + j * $sstep];
            }
        }
        tr16!(
            e_32,
            0,
            1,
            $src,
            $src_off,
            2 * $sstep,
            set_v,
            0,
            0,
            $end / 2
        );
        for i in 0..16 {
            $assign!($dst[$dst_off + i * $dstep], e_32[i] + o_32[i], $shift, $add);
            $assign!(
                $dst[$dst_off + (31 - i) * $dstep],
                e_32[i] - o_32[i],
                $shift,
                $add
            );
        }
    }};
}

macro_rules! tr_dispatch {
    ($log2:expr, $dst:expr, $dst_off:expr, $dstep:expr, $src:expr, $src_off:expr,
     $sstep:expr, $assign:ident, $shift:expr, $add:expr, $end:expr) => {{
        match $log2 {
            2 => tr4!(
                $dst, $dst_off, $dstep, $src, $src_off, $sstep, $assign, $shift, $add
            ),
            3 => tr8!(
                $dst, $dst_off, $dstep, $src, $src_off, $sstep, $assign, $shift, $add, $end
            ),
            4 => tr16!(
                $dst, $dst_off, $dstep, $src, $src_off, $sstep, $assign, $shift, $add, $end
            ),
            5 => tr32!(
                $dst, $dst_off, $dstep, $src, $src_off, $sstep, $assign, $shift, $add, $end
            ),
            _ => unreachable!("validated by check_size"),
        }
    }};
}

/// `FUNC(dequant)`, ITU-T H.265 8.6.3.
pub fn dequant(coeffs: &mut [i16], log2_size: u8, bit_depth: u8) -> Result<()> {
    let size = check_size(log2_size)?;
    check_bit_depth(bit_depth)?;
    if coeffs.len() < size * size {
        return Err(crate::DspError::BufferTooSmall {
            what: "dequant coefficients",
            need: size * size,
            have: coeffs.len(),
        });
    }
    let shift = 15 - i32::from(bit_depth) - i32::from(log2_size);
    if bit_depth <= 9 || shift > 0 {
        let offset = 1 << (shift - 1);
        for c in coeffs.iter_mut().take(size * size) {
            *c = ((i32::from(*c) + offset) >> shift) as i16;
        }
    } else if bit_depth > 10 && shift < 0 {
        for c in coeffs.iter_mut().take(size * size) {
            *c = ((*c as u16) << (-shift)) as i16;
        }
    }
    Ok(())
}

/// `FUNC(transform_rdpcm)`: transform-skip horizontal (`mode == 0`) or
/// vertical (`mode == 1`) reconstruction.
pub fn transform_rdpcm(coeffs: &mut [i16], log2_size: u8, mode: u8) -> Result<()> {
    let size = check_size(log2_size)?;
    if coeffs.len() < size * size {
        return Err(crate::DspError::BufferTooSmall {
            what: "rdpcm coefficients",
            need: size * size,
            have: coeffs.len(),
        });
    }
    if mode > 1 {
        return Err(crate::DspError::InvalidIntraMode(mode));
    }
    if mode == 1 {
        for y in 1..size {
            for x in 0..size {
                let v = coeffs[y * size + x] + coeffs[(y - 1) * size + x];
                coeffs[y * size + x] = v;
            }
        }
    } else {
        for y in 0..size {
            for x in 1..size {
                let v = coeffs[y * size + x] + coeffs[y * size + x - 1];
                coeffs[y * size + x] = v;
            }
        }
    }
    Ok(())
}

/// `FUNC(transform_4x4_luma)`: the dedicated 4x4 luma DCT-II.
pub fn transform_4x4_luma(coeffs: &mut [i16], bit_depth: u8) -> Result<()> {
    check_bit_depth(bit_depth)?;
    if coeffs.len() < 16 {
        return Err(crate::DspError::BufferTooSmall {
            what: "4x4 luma coefficients",
            need: 16,
            have: coeffs.len(),
        });
    }
    let mut tmp = [0i32; 16];
    for i in 0..16 {
        tmp[i] = i32::from(coeffs[i]);
    }

    for i in 0..4 {
        let s = i;
        let c0 = tmp[s] + tmp[s + 8];
        let c1 = tmp[s + 8] + tmp[s + 12];
        let c2 = tmp[s] - tmp[s + 12];
        let c3 = 74 * tmp[s + 4];
        let shift = 7;
        let add = 1 << (shift - 1);
        tmp[s + 8] = clip_i16((74 * (tmp[s] - tmp[s + 8] + tmp[s + 12]) + add) >> shift) as i32;
        tmp[s] = clip_i16((29 * c0 + 55 * c1 + c3 + add) >> shift) as i32;
        tmp[s + 4] = clip_i16((55 * c2 - 29 * c1 + c3 + add) >> shift) as i32;
        tmp[s + 12] = clip_i16((55 * c0 + 29 * c2 - c3 + add) >> shift) as i32;
    }

    let shift = 20 - i32::from(bit_depth);
    let add = 1 << (shift - 1);
    for i in 0..4 {
        let s = 4 * i;
        let c0 = tmp[s] + tmp[s + 2];
        let c1 = tmp[s + 2] + tmp[s + 3];
        let c2 = tmp[s] - tmp[s + 3];
        let c3 = 74 * tmp[s + 1];
        tmp[s + 2] = clip_i16((74 * (tmp[s] - tmp[s + 2] + tmp[s + 3]) + add) >> shift) as i32;
        tmp[s] = clip_i16((29 * c0 + 55 * c1 + c3 + add) >> shift) as i32;
        tmp[s + 1] = clip_i16((55 * c2 - 29 * c1 + c3 + add) >> shift) as i32;
        tmp[s + 3] = clip_i16((55 * c0 + 29 * c2 - c3 + add) >> shift) as i32;
    }
    for i in 0..16 {
        coeffs[i] = tmp[i] as i16;
    }
    Ok(())
}

/// `FUNC(idct_HxH)`: DCT-II inverse transform with an optional column limit
/// (`col_limit` is the number of significant coefficient columns, passed by
/// CABAC; `0` means all columns).
pub fn idct(coeffs: &mut [i16], log2_size: u8, col_limit: usize, bit_depth: u8) -> Result<()> {
    let size = check_size(log2_size)?;
    check_bit_depth(bit_depth)?;
    if coeffs.len() < size * size {
        return Err(crate::DspError::BufferTooSmall {
            what: "IDCT coefficients",
            need: size * size,
            have: coeffs.len(),
        });
    }
    let mut tmp = vec![0i32; size * size];
    for i in 0..size * size {
        tmp[i] = i32::from(coeffs[i]);
    }

    // FFmpeg keeps this limit in a signed `int`; it may become negative after
    // several column groups, at which point the odd-coefficient loops are empty.
    let mut limit2 = col_limit.saturating_add(4).min(size) as i32;
    let shift1 = 7;
    let add1 = 1 << (shift1 - 1);
    for i in 0..size {
        tr_dispatch!(
            log2_size,
            tmp,
            i,
            size,
            tmp,
            i,
            size,
            scale_v,
            shift1,
            add1,
            limit2.max(0) as usize
        );
        if limit2 < size as i32 && i % 4 == 0 && i != 0 {
            limit2 -= 4;
        }
    }

    let limit = col_limit.min(size);
    let shift2 = 20 - i32::from(bit_depth);
    let add2 = 1 << (shift2 - 1);
    for i in 0..size {
        tr_dispatch!(
            log2_size,
            tmp,
            i * size,
            1,
            tmp,
            i * size,
            1,
            scale_v,
            shift2,
            add2,
            limit
        );
    }

    for i in 0..size * size {
        coeffs[i] = tmp[i] as i16;
    }
    Ok(())
}

/// `FUNC(idct_HxH_dc)`: DC-only inverse transform.
pub fn idct_dc(coeffs: &mut [i16], log2_size: u8, bit_depth: u8) -> Result<()> {
    let size = check_size(log2_size)?;
    check_bit_depth(bit_depth)?;
    if coeffs.is_empty() {
        return Err(crate::DspError::BufferTooSmall {
            what: "DC IDCT coefficients",
            need: 1,
            have: 0,
        });
    }
    let shift = 14 - i32::from(bit_depth);
    let add = 1 << (shift - 1);
    let coeff = (((i32::from(coeffs[0]) + 1) >> 1) + add) >> shift;
    for c in coeffs.iter_mut().take(size * size) {
        *c = coeff as i16;
    }
    Ok(())
}
