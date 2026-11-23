//! Motion vector prediction primitives.
//!
//! Ports FFmpeg's `median3_c` / `mid_pred` (`libavcodec/mathops.h`),
//! `mpeg_decode_motion` (`mpeg12dec.c`) and `ff_h263_pred_motion`
//! (`h263.c`) into safe Rust.

use wedeo_codec::bitstream::{BitRead, BitReadBE};
use wedeo_core::Result;

use crate::vlc::{motion_vlc, sign_extend};

/// Median of three integers (FFmpeg `median3_c`, exposed as `mid_pred`).
#[inline]
pub fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    let (max2, min2) = if a >= b { (a, b) } else { (b, a) };
    let m = if c >= max2 { max2 } else { c };
    if m >= min2 { m } else { min2 }
}

/// Median of three 16-bit motion-vector components.
#[inline]
pub fn mid_pred_i16(a: i16, b: i16, c: i16) -> i16 {
    mid_pred(a as i32, b as i32, c as i32) as i16
}

/// Predict a motion vector as the component-wise median of three neighbors
/// (left A, top B, top-right C).
#[inline]
pub fn predict_motion_median(a: [i16; 2], b: [i16; 2], c: [i16; 2]) -> [i16; 2] {
    [
        mid_pred_i16(a[0], b[0], c[0]),
        mid_pred_i16(a[1], b[1], c[1]),
    ]
}

/// A motion-vector field for 8x8 blocks.
///
/// FFmpeg indexes this with `block_index[block]` and strides of
/// `b8_stride = 2 * mb_width + 1`. Real FFmpeg buffers contain edge padding,
/// so negative indices are legal; here [`MotionField::get`] returns `[0, 0]`
/// for out-of-range positions, which gives the same result for the
/// first-row/first-column special cases.
#[derive(Debug, Clone)]
pub struct MotionField {
    pub data: Vec<[i16; 2]>,
    pub stride: usize,
}

impl MotionField {
    /// Allocate a zeroed field for `mb_width` x `mb_height` macroblocks.
    pub fn new(mb_width: usize, mb_height: usize) -> Self {
        let stride = 2 * mb_width + 1;
        let height = 2 * mb_height + 1;
        Self {
            data: vec![[0, 0]; stride * height],
            stride,
        }
    }

    /// Read a motion vector at a (possibly negative) flat index.
    #[inline]
    pub fn get(&self, idx: isize) -> [i16; 2] {
        if idx < 0 {
            [0, 0]
        } else {
            self.data.get(idx as usize).copied().unwrap_or([0, 0])
        }
    }

    /// Write a motion vector at a (possibly negative) flat index.
    #[inline]
    pub fn set(&mut self, idx: isize, mv: [i16; 2]) {
        if idx >= 0
            && let Some(slot) = self.data.get_mut(idx as usize)
        {
            *slot = mv;
        }
    }
}

/// H.263 / MPEG-4 motion-vector prediction (FFmpeg `ff_h263_pred_motion`).
///
/// `block_index` is the current block's index in `field`
/// (`s->block_index[block]`), `block` is 0..3 within the macroblock, and
/// `stride` is `field.stride` (`s->b8_stride`). The special first-slice-line
/// and resync-marker cases from FFmpeg are reproduced.
///
/// Returns the predicted `[x, y]` and the flat index of the current block's
/// motion vector (the `mot_val` pointer), which the caller updates with the
/// decoded differential.
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn h263_predict_motion(
    field: &mut MotionField,
    block_index: isize,
    block: usize,
    first_slice_line: bool,
    mb_x: usize,
    resync_mb_x: usize,
    h263_pred: bool,
) -> ([i16; 2], isize) {
    const OFF: [isize; 4] = [2, 1, 1, -1];
    let wrap = field.stride as isize;
    let block = block.min(3);

    let a = field.get(block_index - 1);

    let mut pred = if first_slice_line && block < 3 {
        match block {
            0 => {
                if mb_x == resync_mb_x {
                    [0, 0]
                } else if mb_x + 1 == resync_mb_x && h263_pred {
                    let c = field.get(block_index + OFF[0] - wrap);
                    if mb_x == 0 {
                        c
                    } else {
                        [mid_pred_i16(a[0], 0, c[0]), mid_pred_i16(a[1], 0, c[1])]
                    }
                } else {
                    a
                }
            }
            1 => {
                if mb_x + 1 == resync_mb_x && h263_pred {
                    let c = field.get(block_index + OFF[1] - wrap);
                    [mid_pred_i16(a[0], 0, c[0]), mid_pred_i16(a[1], 0, c[1])]
                } else {
                    a
                }
            }
            _ => {
                let b = field.get(block_index - wrap);
                let c = field.get(block_index + OFF[2] - wrap);
                if mb_x == resync_mb_x {
                    // FFmpeg writes back a zeroed A: A[0] = A[1] = 0.
                    field.set(block_index - 1, [0, 0]);
                    predict_motion_median([0, 0], b, c)
                } else {
                    predict_motion_median(a, b, c)
                }
            }
        }
    } else {
        let b = field.get(block_index - wrap);
        let c = field.get(block_index + OFF[block] - wrap);
        predict_motion_median(a, b, c)
    };

    // FFmpeg returns a pointer to the current MV; normalize to [0, 0] here
    // as well for an out-of-range index.
    if block_index < 0 {
        pred = [0, 0];
    }
    (pred, block_index)
}

/// Decode one MPEG-1/2 motion-vector differential (FFmpeg
/// `mpeg_decode_motion`).
///
/// `fcode` is `s->mpeg_f_code[dir][xy]` and `pred` is the predicted component.
/// Returns `0xffff` on an invalid VLC code, matching FFmpeg's sentinel.
pub fn decode_motion(br: &mut BitReadBE<'_>, fcode: u32, pred: i32) -> Result<i32> {
    let vlc = motion_vlc();
    let code = vlc.decode(br)?;
    if code == 0 {
        return Ok(pred);
    }
    if code < 0 {
        return Ok(0xffff);
    }

    let sign = br.get_bit();
    let shift = fcode as i32 - 1;
    let mut val = code;
    if shift != 0 {
        val = (val - 1) << shift;
        val |= br.get_bits_32(shift as usize) as i32;
        val += 1;
    }
    if sign {
        val = -val;
    }
    val += pred;

    Ok(sign_extend(val, (5 + shift) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wedeo_codec::bitstream::BitRead;

    #[test]
    fn mid_pred_returns_median() {
        assert_eq!(mid_pred(1, 2, 3), 2);
        assert_eq!(mid_pred(3, 1, 2), 2);
        assert_eq!(mid_pred(2, 3, 1), 2);
        assert_eq!(mid_pred(5, 5, 1), 5);
        assert_eq!(mid_pred(-4, 7, 0), 0);
        assert_eq!(mid_pred_i16(-1, 3, 2), 2);
    }

    #[test]
    fn predict_motion_median_all_components() {
        let p = predict_motion_median([1, -2], [3, 4], [-5, 0]);
        assert_eq!(p, [1, 0]);
    }

    #[test]
    fn h263_prediction_normal_case() {
        // Field with stride = 2*2+1 = 5. Current block at index 7 (row 1,
        // col 2). A = index 6, B = index 2, C = index 3 (off[0]=2).
        let mut field = MotionField::new(2, 2);
        field.data[6] = [4, 5];
        field.data[2] = [-2, 8];
        field.data[4] = [10, -6];
        let (pred, idx) = h263_predict_motion(&mut field, 7, 0, false, 1, 0, true);
        assert_eq!(pred, [4, 5]);
        assert_eq!(idx, 7);
    }

    #[test]
    fn h263_prediction_first_slice_line_uses_left() {
        let mut field = MotionField::new(2, 2);
        field.data[1] = [7, 9]; // A for block_index 2
        let (pred, _) = h263_predict_motion(&mut field, 2, 0, true, 1, 0, true);
        assert_eq!(pred, [7, 9]);
    }

    #[test]
    fn h263_prediction_first_slice_line_block2_median() {
        let mut field = MotionField::new(2, 2);
        // block_index = 8, block = 2: A = 7, B = 3, C = 9.
        field.data[7] = [1, 1];
        field.data[3] = [5, 5];
        field.data[4] = [9, 9];
        let (pred, _) = h263_predict_motion(&mut field, 8, 2, true, 2, 0, true);
        assert_eq!(pred, [5, 5]);
    }

    #[test]
    fn decode_motion_zero_code_returns_prediction() {
        // Motion VLC code 0 is a single '1' bit (index 0 in the table).
        let data = [0x80, 0, 0, 0, 0, 0, 0, 0];
        let mut br = BitReadBE::new(&data);
        assert_eq!(decode_motion(&mut br, 1, 42).unwrap(), 42);
    }
}
