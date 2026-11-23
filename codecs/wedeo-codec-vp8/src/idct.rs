// VP8 inverse transforms, translated from FFmpeg libavcodec/vp8dsp.c
// (vp8_luma_dc_wht_c, vp8_luma_dc_wht_dc_c, vp8_idct_add_c,
// vp8_idct_dc_add_c and MK_IDCT_DC_ADD4_C).
// LGPL-2.1-or-later, same as FFmpeg.

/// `vp8_luma_dc_wht_c`: 4x4 Walsh-Hadamard transform of the luma DC block.
/// Distributes the result into the DC (index 0) of the 16 luma 4x4 blocks.
pub fn luma_dc_wht(block: &mut [[[i16; 16]; 4]], dc: &mut [i16; 16]) {
    for i in 0..4 {
        let t0 = dc[i] + dc[12 + i];
        let t1 = dc[4 + i] + dc[8 + i];
        let t2 = dc[4 + i] - dc[8 + i];
        let t3 = dc[i] - dc[12 + i];
        dc[i] = t0 + t1;
        dc[4 + i] = t3 + t2;
        dc[8 + i] = t0 - t1;
        dc[12 + i] = t3 - t2;
    }
    for i in 0..4 {
        let b = i * 4;
        let t0 = dc[b] + dc[b + 3] + 3; // rounding
        let t1 = dc[b + 1] + dc[b + 2];
        let t2 = dc[b + 1] - dc[b + 2];
        let t3 = dc[b] - dc[b + 3] + 3; // rounding
        dc[b] = 0;
        dc[b + 1] = 0;
        dc[b + 2] = 0;
        dc[b + 3] = 0;
        block[i][0][0] = (t0 + t1) >> 3;
        block[i][1][0] = (t3 + t2) >> 3;
        block[i][2][0] = (t0 - t1) >> 3;
        block[i][3][0] = (t3 - t2) >> 3;
    }
}

/// `vp8_luma_dc_wht_dc_c`: DC-only variant of the luma DC transform.
pub fn luma_dc_wht_dc(block: &mut [[[i16; 16]; 4]], dc: &mut [i16; 16]) {
    let val = (dc[0] + 3) >> 3;
    dc[0] = 0;
    for i in 0..4 {
        for j in 0..4 {
            block[i][j][0] = val;
        }
    }
}

#[inline]
fn mul_20091(a: i32) -> i32 {
    ((a.wrapping_mul(20091)) >> 16).wrapping_add(a)
}

#[inline]
fn mul_35468(a: i32) -> i32 {
    (a.wrapping_mul(35468)) >> 16
}

#[inline]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `vp8_idct_add_c`: 4x4 inverse DCT with full transform, added to `dst`.
/// `dst` is a plane slice; the block is placed at (x, y) with row stride.
pub fn idct_add(dst: &mut [u8], x: usize, y: usize, stride: usize, block: &mut [i16; 16]) {
    let mut tmp = [0i32; 16];
    for i in 0..4 {
        let t0 = block[i] as i32 + block[8 + i] as i32;
        let t1 = block[i] as i32 - block[8 + i] as i32;
        let t2 = mul_35468(block[4 + i] as i32) - mul_20091(block[12 + i] as i32);
        let t3 = mul_20091(block[4 + i] as i32) + mul_35468(block[12 + i] as i32);
        block[i] = 0;
        block[4 + i] = 0;
        block[8 + i] = 0;
        block[12 + i] = 0;
        let r = i * 4;
        tmp[r] = t0 + t3;
        tmp[r + 1] = t1 + t2;
        tmp[r + 2] = t1 - t2;
        tmp[r + 3] = t0 - t3;
    }
    for i in 0..4 {
        let t0 = tmp[i] + tmp[8 + i];
        let t1 = tmp[i] - tmp[8 + i];
        let t2 = mul_35468(tmp[4 + i]) - mul_20091(tmp[12 + i]);
        let t3 = mul_20091(tmp[4 + i]) + mul_35468(tmp[12 + i]);
        let off = (y + i) * stride + x;
        dst[off] = clip_u8(dst[off] as i32 + ((t0 + t3 + 4) >> 3));
        dst[off + 1] = clip_u8(dst[off + 1] as i32 + ((t1 + t2 + 4) >> 3));
        dst[off + 2] = clip_u8(dst[off + 2] as i32 + ((t1 - t2 + 4) >> 3));
        dst[off + 3] = clip_u8(dst[off + 3] as i32 + ((t0 - t3 + 4) >> 3));
    }
}

/// `vp8_idct_dc_add_c`: DC-only inverse transform, added to `dst`.
pub fn idct_dc_add(dst: &mut [u8], x: usize, y: usize, stride: usize, block: &mut [i16; 16]) {
    let dc = (block[0] as i32 + 4) >> 3;
    block[0] = 0;
    for i in 0..4 {
        let off = (y + i) * stride + x;
        for j in 0..4 {
            dst[off + j] = clip_u8(dst[off + j] as i32 + dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_zero_block_noop() {
        let mut plane = vec![10u8; 16 * 16];
        let mut block = [0i16; 16];
        idct_dc_add(&mut plane, 0, 0, 16, &mut block);
        assert!(plane.iter().all(|&v| v == 10));
    }

    #[test]
    fn dc_add_clamps() {
        let mut plane = vec![255u8; 16 * 16];
        let mut block = [0i16; 16];
        block[0] = 64; // dc = (64+4)>>3 = 8
        idct_dc_add(&mut plane, 0, 0, 16, &mut block);
        assert_eq!(plane[0], 255);
    }
}
