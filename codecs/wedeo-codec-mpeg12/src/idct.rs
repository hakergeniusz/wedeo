//! FFmpeg's `simple_idct` (8-bit), copied from `simple_idct_template.c`.
//!
//! This is the IDCT FFmpeg selects by default for MPEG-1/2 and is the one
//! used by FATE. With `idct_permutation_type == FF_IDCT_PERM_NONE` the
//! permutation is the identity, so blocks are stored in natural order.
//!
//! All intermediate sums use wrapping i32 arithmetic: FFmpeg computes them in
//! `unsigned int`, whose modulo-2^32 overflow behaviour is reproduced by
//! casting the (wider) intermediate sums back to i32 before shifting.

const W1: i64 = 22725; // cos(i*M_PI/16)*sqrt(2)*(1<<14) + 0.5
const W2: i64 = 21407;
const W3: i64 = 19266;
const W4: i64 = 16383;
const W5: i64 = 12873;
const W6: i64 = 8867;
const W7: i64 = 4520;

const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;
const DC_SHIFT: u32 = 3;

fn clip_uint8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `idctRowCondDC` for BIT_DEPTH == 8, IN_IDCT_DEPTH == 16, extra_shift == 0.
fn idct_row_cond_dc(row: &mut [i32; 64], off: usize) {
    let r = &mut row[off..off + 8];

    // The row[0] (DC) coefficient is allowed to be nonzero in the fast path;
    // all other coefficients must be zero. This is FFmpeg's 64-bit ROW0_MASK
    // test with HAVE_BIGENDIAN == 0.
    if r[1] == 0 && r[2] == 0 && r[3] == 0 && r[4] == 0 && r[5] == 0 && r[6] == 0 && r[7] == 0 {
        // FFmpeg masks to 16 bits and writes through int16_t row slots, so the
        // stored value is sign-extended from the low 16 bits.
        let temp = ((((r[0] as i64) << DC_SHIFT) as u16) as i16) as i32;
        for x in r.iter_mut() {
            *x = temp;
        }
        return;
    }

    let mut a0 = (W4 * r[0] as i64 + (1i64 << (ROW_SHIFT - 1))) as i32;
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;

    a0 = a0.wrapping_add((W2 * r[2] as i64) as i32);
    a1 = a1.wrapping_add((W6 * r[2] as i64) as i32);
    a2 = a2.wrapping_sub((W6 * r[2] as i64) as i32);
    a3 = a3.wrapping_sub((W2 * r[2] as i64) as i32);

    let mut b0 = (W1 * r[1] as i64 + W3 * r[3] as i64) as i32;
    let mut b1 = (W3 * r[1] as i64 - W7 * r[3] as i64) as i32;
    let mut b2 = (W5 * r[1] as i64 - W1 * r[3] as i64) as i32;
    let mut b3 = (W7 * r[1] as i64 - W5 * r[3] as i64) as i32;

    if r[4] != 0 || r[5] != 0 || r[6] != 0 || r[7] != 0 {
        a0 = a0.wrapping_add((W4 * r[4] as i64 + W6 * r[6] as i64) as i32);
        a1 = a1.wrapping_add((-W4 * r[4] as i64 - W2 * r[6] as i64) as i32);
        a2 = a2.wrapping_add((-W4 * r[4] as i64 + W2 * r[6] as i64) as i32);
        a3 = a3.wrapping_add((W4 * r[4] as i64 - W6 * r[6] as i64) as i32);

        b0 = b0.wrapping_add((W5 * r[5] as i64 + W7 * r[7] as i64) as i32);
        b1 = b1.wrapping_add((-W1 * r[5] as i64 - W5 * r[7] as i64) as i32);
        b2 = b2.wrapping_add((W7 * r[5] as i64 + W3 * r[7] as i64) as i32);
        b3 = b3.wrapping_add((W3 * r[5] as i64 - W1 * r[7] as i64) as i32);
    }

    r[0] = a0.wrapping_add(b0) >> ROW_SHIFT;
    r[7] = a0.wrapping_sub(b0) >> ROW_SHIFT;
    r[1] = a1.wrapping_add(b1) >> ROW_SHIFT;
    r[6] = a1.wrapping_sub(b1) >> ROW_SHIFT;
    r[2] = a2.wrapping_add(b2) >> ROW_SHIFT;
    r[5] = a2.wrapping_sub(b2) >> ROW_SHIFT;
    r[3] = a3.wrapping_add(b3) >> ROW_SHIFT;
    r[4] = a3.wrapping_sub(b3) >> ROW_SHIFT;
}

/// `IDCT_COLS` + store, for one column of the transformed block.
fn idct_col(block: &[i32; 64], col: usize, dest: &mut [u8], stride: usize, add: bool) {
    let c = |k: usize| block[col + k * 8];

    // (1 << (COL_SHIFT - 1)) / W4 == 32.
    let round = (1i64 << (COL_SHIFT - 1)) / W4;
    let mut a0 = (W4 * (c(0) as i64 + round)) as i32;
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;

    a0 = a0.wrapping_add((W2 * c(2) as i64) as i32);
    a1 = a1.wrapping_add((W6 * c(2) as i64) as i32);
    a2 = a2.wrapping_sub((W6 * c(2) as i64) as i32);
    a3 = a3.wrapping_sub((W2 * c(2) as i64) as i32);

    let mut b0 = (W1 * c(1) as i64) as i32;
    let mut b1 = (W3 * c(1) as i64) as i32;
    let mut b2 = (W5 * c(1) as i64) as i32;
    let mut b3 = (W7 * c(1) as i64) as i32;

    b0 = b0.wrapping_add((W3 * c(3) as i64) as i32);
    b1 = b1.wrapping_sub((W7 * c(3) as i64) as i32);
    b2 = b2.wrapping_sub((W1 * c(3) as i64) as i32);
    b3 = b3.wrapping_sub((W5 * c(3) as i64) as i32);

    if c(4) != 0 {
        a0 = a0.wrapping_add((W4 * c(4) as i64) as i32);
        a1 = a1.wrapping_sub((W4 * c(4) as i64) as i32);
        a2 = a2.wrapping_sub((W4 * c(4) as i64) as i32);
        a3 = a3.wrapping_add((W4 * c(4) as i64) as i32);
    }
    if c(5) != 0 {
        b0 = b0.wrapping_add((W5 * c(5) as i64) as i32);
        b1 = b1.wrapping_sub((W1 * c(5) as i64) as i32);
        b2 = b2.wrapping_add((W7 * c(5) as i64) as i32);
        b3 = b3.wrapping_add((W3 * c(5) as i64) as i32);
    }
    if c(6) != 0 {
        a0 = a0.wrapping_add((W6 * c(6) as i64) as i32);
        a1 = a1.wrapping_sub((W2 * c(6) as i64) as i32);
        a2 = a2.wrapping_add((W2 * c(6) as i64) as i32);
        a3 = a3.wrapping_sub((W6 * c(6) as i64) as i32);
    }
    if c(7) != 0 {
        b0 = b0.wrapping_add((W7 * c(7) as i64) as i32);
        b1 = b1.wrapping_sub((W5 * c(7) as i64) as i32);
        b2 = b2.wrapping_add((W3 * c(7) as i64) as i32);
        b3 = b3.wrapping_sub((W1 * c(7) as i64) as i32);
    }

    let out = [
        a0.wrapping_add(b0) >> COL_SHIFT,
        a1.wrapping_add(b1) >> COL_SHIFT,
        a2.wrapping_add(b2) >> COL_SHIFT,
        a3.wrapping_add(b3) >> COL_SHIFT,
        a3.wrapping_sub(b3) >> COL_SHIFT,
        a2.wrapping_sub(b2) >> COL_SHIFT,
        a1.wrapping_sub(b1) >> COL_SHIFT,
        a0.wrapping_sub(b0) >> COL_SHIFT,
    ];

    for (y, &v) in out.iter().enumerate() {
        let p = &mut dest[y * stride + col];
        *p = if add {
            clip_uint8(*p as i32 + v)
        } else {
            clip_uint8(v)
        };
    }
}

/// `ff_simple_idct_put_int16_8bit`: transform `block` and store 8x8 into
/// `dest` (row-major, `stride` bytes per row), replacing existing samples.
pub fn idct_put(block: &[i16; 64], dest: &mut [u8], stride: usize) {
    let mut tmp = [0i32; 64];
    for (i, &v) in block.iter().enumerate() {
        tmp[i] = v as i32;
    }
    for i in 0..8 {
        idct_row_cond_dc(&mut tmp, i * 8);
    }
    for i in 0..8 {
        idct_col(&tmp, i, dest, stride, false);
    }
}

/// `ff_simple_idct_add_int16_8bit`: transform `block` and add to `dest`.
pub fn idct_add(block: &[i16; 64], dest: &mut [u8], stride: usize) {
    let mut tmp = [0i32; 64];
    for (i, &v) in block.iter().enumerate() {
        tmp[i] = v as i32;
    }
    for i in 0..8 {
        idct_row_cond_dc(&mut tmp, i * 8);
    }
    for i in 0..8 {
        idct_col(&tmp, i, dest, stride, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_dc(dc: i16) -> Vec<u8> {
        let mut block = [0i16; 64];
        block[0] = dc;
        let mut dest = [0u8; 64];
        idct_put(&block, &mut dest, 8);
        dest.to_vec()
    }

    #[test]
    fn dc_only_scaling() {
        // A DC-only block is divided by 8 by the 2-D IDCT.
        assert!(put_dc(456).iter().all(|&v| v == 57));
        assert!(put_dc(128).iter().all(|&v| v == 16));
        assert!(put_dc(0).iter().all(|&v| v == 0));
    }

    #[test]
    fn negative_dc_wraps_like_ffmpeg() {
        // FFmpeg writes `(row[0] << DC_SHIFT) & 0xffff` through int16_t slots.
        // -8 << 3 == -64, whose low 16 bits sign-extend back to -64.
        let mut block = [0i16; 64];
        block[0] = -8;
        let mut dest = [0u8; 64];
        idct_put(&block, &mut dest, 8);
        assert!(dest.iter().all(|&v| v == 0)); // -64 clipped to 0
    }

    #[test]
    fn mismatch_bit_does_not_change_dc_only_output() {
        let mut block = [0i16; 64];
        block[0] = 384;
        let mut a = [0u8; 64];
        idct_put(&block, &mut a, 8);
        block[63] = 1; // MPEG-2 mismatch-control trick
        let mut b = [0u8; 64];
        idct_put(&block, &mut b, 8);
        assert_eq!(a, b);
        assert!(a.iter().all(|&v| v == 48));
    }

    #[test]
    fn add_mode_clamps() {
        let mut block = [0i16; 64];
        block[0] = 1024; // DC-only output is 1024 / 8 == 128
        let mut dest = [10u8; 64];
        idct_add(&block, &mut dest, 8);
        assert!(dest.iter().all(|&v| v == 138));
    }
}
