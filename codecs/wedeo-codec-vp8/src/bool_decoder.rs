// VP8 (VPX) boolean arithmetic (range) decoder.
//
// Translated from FFmpeg's libavcodec/vpx_rac.h, vpx_rac.c and vp89_rac.h.
// The range coder keeps a probability interval [0, high) encoded in a
// `code_word` register. Renormalization shifts bits in from the input byte
// stream to keep `high` in [128, 256).
//
// Invariants (from vpx_rac.h):
//   - `high` is stored in [0, 255].
//   - `bits` is stored negated: a negative value means that many bits are
//     still available in `code_word` before the next refill.
//   - `code_word` holds at most 32 bits of lookahead.
//   - Initialisation: high=255, bits=-16, code_word = first 3 bytes BE.
//
// LGPL-2.1-or-later, same as FFmpeg.

/// `ff_vpx_norm_shift[x]`: number of leading zero bits in `x` (for x in
/// 0..=255), i.e. how much to left-shift to move the MSB into bit 7.
/// Verbatim copy of `ff_vpx_norm_shift` from vpx_rac.c.
const NORM_SHIFT: [u8; 256] = [
    8, 7, 6, 6, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// VP8/VPX range decoder (FFmpeg `VPXRangeCoder`).
#[derive(Clone)]
pub struct BoolDecoder<'a> {
    /// Upper end of the current probability interval (stored in [0, 255]).
    high: u32,
    /// Negated count of valid bits remaining in `code_word` before the next
    /// 16-bit refill. A value of -16 means 16 bits are available.
    bits: i32,
    /// Remaining input bytes.
    buffer: &'a [u8],
    /// Current byte position in `buffer`.
    pos: usize,
    /// Prefetched code word (up to 32 bits of lookahead).
    code_word: u32,
    /// How many times the end of the stream has been reached.
    end_reached: u32,
}

impl<'a> BoolDecoder<'a> {
    /// Initialise a range decoder from `data`. Returns `None` if empty.
    /// Mirrors `ff_vpx_init_range_decoder`: high=255, bits=-16, code_word=BE24.
    pub fn new(data: &'a [u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        let (code_word, pos) = if data.len() >= 3 {
            (
                ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | (data[2] as u32),
                3,
            )
        } else if data.len() == 2 {
            (((data[0] as u32) << 16) | ((data[1] as u32) << 8), 2)
        } else {
            ((data[0] as u32) << 16, 1)
        };
        Some(Self {
            high: 255,
            bits: -16,
            buffer: data,
            pos,
            code_word,
            end_reached: 0,
        })
    }

    /// `vpx_rac_is_end`: true once the stream end has been reached repeatedly.
    #[inline]
    pub fn is_end(&mut self) -> bool {
        if self.pos >= self.buffer.len() && self.bits >= 0 {
            self.end_reached = self.end_reached.saturating_add(1);
        }
        self.end_reached > 10
    }

    /// `vpx_rac_renorm`: renormalise the interval and refill bits as needed.
    #[inline]
    pub fn renorm(&mut self) -> u32 {
        let shift = NORM_SHIFT[self.high as usize] as i32;
        self.high <<= shift;
        let mut code_word = self.code_word << shift;
        self.bits += shift;
        if self.bits >= 0 && self.pos < self.buffer.len() {
            let b0 = self.buffer[self.pos] as u32;
            self.pos += 1;
            let b1 = if self.pos < self.buffer.len() {
                let v = self.buffer[self.pos] as u32;
                self.pos += 1;
                v
            } else {
                0
            };
            code_word |= ((b0 << 8) | b1) << self.bits as u32;
            self.bits -= 16;
        }
        self.code_word = code_word;
        code_word
    }

    /// `vpx_rac_get_prob`: decode one bit with probability `prob` (1..=255).
    #[inline]
    pub fn get_prob(&mut self, prob: u8) -> bool {
        let code_word = self.renorm();
        let low = 1 + (((self.high - 1).wrapping_mul(prob as u32)) >> 8);
        let low_shift = low << 16;
        let bit = code_word >= low_shift;
        if bit {
            self.high -= low;
            self.code_word = code_word - low_shift;
        } else {
            self.high = low;
            self.code_word = code_word;
        }
        bit
    }

    /// `vp89_rac_get`: an equiprobable bit implemented as `get_prob(128)`
    /// (FFmpeg uses a different rounding than `vpx_rac_get`).
    #[inline]
    pub fn get(&mut self) -> bool {
        self.get_prob(128)
    }

    /// `vp89_rac_get_uint`: read `bits` equiprobable bits, MSB first.
    #[inline]
    pub fn get_uint(&mut self, bits: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..bits {
            value = (value << 1) | u32::from(self.get());
        }
        value
    }

    /// `vp8_rac_get_sint`: signed value with variable-magnitude coding.
    pub fn get_sint(&mut self, bits: u32) -> i32 {
        if !self.get() {
            return 0;
        }
        let mut v = self.get_uint(bits) as i32;
        if self.get() {
            v = -v;
        }
        v
    }

    /// `vp8_rac_get_nn`: positive non-zero integer.
    pub fn get_nn(&mut self) -> i32 {
        let v = (self.get_uint(7) << 1) as i32;
        v + i32::from(v == 0)
    }

    /// `vp8_rac_get_coeff`: read extra bits using a 0-terminated probability
    /// array (`DCTextra` in the spec).
    pub fn get_coeff(&mut self, probs: &[u8]) -> i32 {
        let mut v = 0i32;
        for &p in probs {
            if p == 0 {
                break;
            }
            v = (v << 1) | i32::from(self.get_prob(p));
        }
        v
    }

    /// `vp89_rac_get_tree`: decode a symbol from a binary decision tree.
    /// Negative tree entries are leaves (`-symbol`).
    pub fn get_tree(&mut self, tree: &[[i8; 2]], probs: &[u8]) -> i32 {
        let mut i: usize = 0;
        loop {
            let branch = usize::from(self.get_prob(probs[i]));
            let next = tree[i][branch];
            if next <= 0 {
                return -i32::from(next);
            }
            i = next as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_data_returns_none() {
        assert!(BoolDecoder::new(&[]).is_none());
    }

    #[test]
    fn initial_state_matches_ffmpeg() {
        let bd = BoolDecoder::new(&[0x12, 0x34, 0x56]).unwrap();
        assert_eq!(bd.high, 255);
        assert_eq!(bd.bits, -16);
        assert_eq!(bd.code_word, 0x123456);
        assert_eq!(bd.pos, 3);
    }

    #[test]
    fn norm_shift_spot_checks() {
        assert_eq!(NORM_SHIFT[0], 8);
        assert_eq!(NORM_SHIFT[1], 7);
        assert_eq!(NORM_SHIFT[128], 0);
        assert_eq!(NORM_SHIFT[255], 0);
    }

    #[test]
    fn all_ones_stream() {
        let data = [0xff; 16];
        let mut bd = BoolDecoder::new(&data).unwrap();
        assert_eq!(bd.get_uint(8), 0xff);
    }
}
