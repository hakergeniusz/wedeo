//! 8x8 block IDCT primitives.
//!
//! Port of FFmpeg's 8-bit `ff_simple_idct_put` / `ff_simple_idct_add` /
//! `ff_simple_idct` from `simple_idct.c` and `simple_idct_template.c`. The
//! row/column butterflies and fixed-point constants are reproduced exactly so
//! reconstruction stays bit-exact with FFmpeg's C path.

/// Fixed-point IDCT coefficients: `cos(i*pi/16) * sqrt(2) * (1<<14) + 0.5`.
const W1: i32 = 22725;
const W2: i32 = 21407;
const W3: i32 = 19266;
const W4: i32 = 16383;
const W5: i32 = 12873;
const W6: i32 = 8867;
const W7: i32 = 4520;
const ROW_SHIFT: i32 = 11;
const COL_SHIFT: i32 = 20;

#[inline]
fn idct_row(row: &mut [i16]) {
    let r = |i: usize| row[i] as i32;

    let mut a0 = W4 * r(0) + (1 << (ROW_SHIFT - 1));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;

    a0 += W2 * r(2);
    a1 += W6 * r(2);
    a2 -= W6 * r(2);
    a3 -= W2 * r(2);

    let mut b0 = W1 * r(1) + W3 * r(3);
    let mut b1 = W3 * r(1) - W7 * r(3);
    let mut b2 = W5 * r(1) - W1 * r(3);
    let mut b3 = W7 * r(1) - W5 * r(3);

    if r(4) != 0 || r(5) != 0 || r(6) != 0 || r(7) != 0 {
        a0 += W4 * r(4) + W6 * r(6);
        a1 += -W4 * r(4) - W2 * r(6);
        a2 += -W4 * r(4) + W2 * r(6);
        a3 += W4 * r(4) - W6 * r(6);

        b0 += W5 * r(5) + W7 * r(7);
        b1 += -W1 * r(5) - W5 * r(7);
        b2 += W7 * r(5) + W3 * r(7);
        b3 += W3 * r(5) - W1 * r(7);
    }

    row[0] = ((a0 + b0) >> ROW_SHIFT) as i16;
    row[7] = ((a0 - b0) >> ROW_SHIFT) as i16;
    row[1] = ((a1 + b1) >> ROW_SHIFT) as i16;
    row[6] = ((a1 - b1) >> ROW_SHIFT) as i16;
    row[2] = ((a2 + b2) >> ROW_SHIFT) as i16;
    row[5] = ((a2 - b2) >> ROW_SHIFT) as i16;
    row[3] = ((a3 + b3) >> ROW_SHIFT) as i16;
    row[4] = ((a3 - b3) >> ROW_SHIFT) as i16;
}

/// Column IDCT butterfly. Returns the 8 output samples in raster order.
#[inline]
fn idct_col(block: &[i16; 64], col: usize) -> [i32; 8] {
    let c = |k: usize| block[col + 8 * k] as i32;

    let mut a0 = W4 * (c(0) + (1 << (COL_SHIFT - 1)) / W4);
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;

    a0 += W2 * c(2);
    a1 += W6 * c(2);
    a2 -= W6 * c(2);
    a3 -= W2 * c(2);

    let mut b0 = W1 * c(1);
    let mut b1 = W3 * c(1);
    let mut b2 = W5 * c(1);
    let mut b3 = W7 * c(1);

    b0 += W3 * c(3);
    b1 -= W7 * c(3);
    b2 -= W1 * c(3);
    b3 -= W5 * c(3);

    if c(4) != 0 {
        a0 += W4 * c(4);
        a1 -= W4 * c(4);
        a2 -= W4 * c(4);
        a3 += W4 * c(4);
    }
    if c(5) != 0 {
        b0 += W5 * c(5);
        b1 -= W1 * c(5);
        b2 += W7 * c(5);
        b3 += W3 * c(5);
    }
    if c(6) != 0 {
        a0 += W6 * c(6);
        a1 -= W2 * c(6);
        a2 += W2 * c(6);
        a3 -= W6 * c(6);
    }
    if c(7) != 0 {
        b0 += W7 * c(7);
        b1 -= W5 * c(7);
        b2 += W3 * c(7);
        b3 -= W1 * c(7);
    }

    [
        (a0 + b0) >> COL_SHIFT,
        (a1 + b1) >> COL_SHIFT,
        (a2 + b2) >> COL_SHIFT,
        (a3 + b3) >> COL_SHIFT,
        (a3 - b3) >> COL_SHIFT,
        (a2 - b2) >> COL_SHIFT,
        (a1 - b1) >> COL_SHIFT,
        (a0 - b0) >> COL_SHIFT,
    ]
}

#[inline]
fn clip8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// 8x8 IDCT, writing clamped samples into `dst` (FFmpeg `ff_simple_idct_put`).
///
/// `dst[y * stride + x]` receives sample `(x, y)`. `block` is modified in
/// place by the row pass and is left holding intermediate values.
pub fn idct_put(block: &mut [i16; 64], dst: &mut [u8], stride: usize) {
    for i in 0..8 {
        idct_row(&mut block[i * 8..i * 8 + 8]);
    }
    for i in 0..8 {
        let out = idct_col(block, i);
        for (y, &v) in out.iter().enumerate() {
            dst[y * stride + i] = clip8(v);
        }
    }
}

/// 8x8 IDCT, adding samples to `dst` (FFmpeg `ff_simple_idct_add`).
pub fn idct_add(block: &mut [i16; 64], dst: &mut [u8], stride: usize) {
    for i in 0..8 {
        idct_row(&mut block[i * 8..i * 8 + 8]);
    }
    for i in 0..8 {
        let out = idct_col(block, i);
        for (y, &v) in out.iter().enumerate() {
            let idx = y * stride + i;
            dst[idx] = clip8(dst[idx] as i32 + v);
        }
    }
}

/// 8x8 IDCT performed entirely in the coefficient block
/// (FFmpeg `ff_simple_idct`). The result is stored as i16 samples.
pub fn idct(block: &mut [i16; 64]) {
    for i in 0..8 {
        idct_row(&mut block[i * 8..i * 8 + 8]);
    }
    for i in 0..8 {
        let out = idct_col(block, i);
        for (y, &v) in out.iter().enumerate() {
            block[y * 8 + i] = v as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_block_produces_constant() {
        // The IDCT itself has no +128 offset; it comes from the DC predictor.
        // A DC coefficient of 1024 (128 << 3) reconstructs to flat 128.
        let mut block = [0i16; 64];
        block[0] = 1024;
        let mut dst = [0u8; 64];
        idct_put(&mut block, &mut dst, 8);
        for &v in &dst {
            assert_eq!(v, 128);
        }
    }

    #[test]
    fn zero_block_is_zero() {
        let mut block = [0i16; 64];
        let mut dst = [0u8; 64];
        idct_put(&mut block, &mut dst, 8);
        assert!(dst.iter().all(|&v| v == 0));
    }

    #[test]
    fn idct_add_accumulates() {
        let mut block = [0i16; 64];
        block[0] = 1024;
        let mut dst = [16u8; 64];
        idct_add(&mut block, &mut dst, 8);
        assert!(dst.iter().all(|&v| v == 144));
    }
}
