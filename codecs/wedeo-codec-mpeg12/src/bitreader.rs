//! MSB-first bit reader, the equivalent of FFmpeg's `GetBitContext`.
//!
//! FFmpeg's MPEG-1/2 decoder uses `UNCHECKED_BITSTREAM_READER`, so reads past
//! the end of the buffer yield zero bits (the input buffer is padded). We
//! mirror that behaviour here.

use wedeo_core::error::{Error, Result};

/// A canonical Huffman table built from `(code, length, symbol)` triples.
pub struct Vlc {
    by_len: Vec<Vec<(u32, i32)>>,
    max_len: u8,
}

impl Vlc {
    pub fn new(entries: &[(u32, u8, i32)]) -> Self {
        let max_len = entries.iter().map(|&(_, l, _)| l).max().unwrap_or(0);
        let mut by_len = vec![Vec::new(); max_len as usize + 1];
        for &(code, len, symbol) in entries {
            if len > 0 && (len as usize) < by_len.len() {
                by_len[len as usize].push((code, symbol));
            }
        }
        Self { by_len, max_len }
    }

    /// Decode one symbol, consuming the VLC code bits.
    pub fn decode(&self, br: &mut BitReader<'_>) -> Result<i32> {
        let mut code = 0u32;
        for len in 1..=self.max_len {
            code = (code << 1) | br.read_bit()?;
            if let Some(&(_, symbol)) = self.by_len[len as usize].iter().find(|&&(c, _)| c == code)
            {
                return Ok(symbol);
            }
        }
        Err(Error::InvalidData)
    }
}

/// Big-endian (MSB first) bit reader over a byte slice.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Current bit position from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    pub fn bits_left(&self) -> i64 {
        (self.data.len() * 8) as i64 - self.pos as i64
    }

    /// Read one bit. Past the end this returns 0, matching FFmpeg's padded
    /// unchecked bitstream reader.
    pub fn read_bit(&mut self) -> Result<u32> {
        let byte = self.pos >> 3;
        let bit = 7 - (self.pos & 7);
        self.pos += 1;
        Ok(if byte < self.data.len() {
            ((self.data[byte] >> bit) & 1) as u32
        } else {
            0
        })
    }

    /// Read up to 32 bits, MSB first. Missing bits are zero, matching FFmpeg.
    pub fn read_bits(&mut self, n: usize) -> Result<u32> {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()?;
        }
        Ok(v)
    }

    /// Read `n` bits and sign-extend as a two's complement value.
    pub fn read_sbits(&mut self, n: usize) -> Result<i32> {
        let v = self.read_bits(n)?;
        Ok(sign_extend(v, n))
    }

    /// `get_xbits` from FFmpeg's `get_bits.h`: the top bit is a sign flag with
    /// a `-(2^n - 1)` offset (there is no zero representation).
    pub fn read_xbits(&mut self, n: usize) -> Result<i32> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.read_bits(n)?;
        if v & (1 << (n - 1)) != 0 {
            Ok(v as i32)
        } else {
            Ok(v as i32 - ((1i32 << n) - 1))
        }
    }

    /// Peek at the next `n` bits without consuming them.
    pub fn show_bits(&self, n: usize) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            let p = self.pos + i;
            let bit = if p >> 3 < self.data.len() {
                (self.data[p >> 3] >> (7 - (p & 7))) & 1
            } else {
                0
            };
            v = (v << 1) | bit as u32;
        }
        v
    }

    pub fn skip_bits(&mut self, n: usize) {
        self.pos += n;
    }

    /// Align to the next byte boundary, like FFmpeg's `align_get_bits`.
    pub fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    pub fn decode_vlc(&mut self, vlc: &Vlc) -> Result<i32> {
        vlc.decode(self)
    }
}

/// FFmpeg's `sign_extend(v, n)`.
pub fn sign_extend(v: u32, n: usize) -> i32 {
    if n == 0 {
        return 0;
    }
    let shift = 32 - n;
    ((v << shift) as i32) >> shift
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        let mut br = BitReader::new(&[0xa5, 0x5a]);
        assert_eq!(br.read_bits(4).unwrap(), 0xa);
        assert_eq!(br.read_bits(4).unwrap(), 0x5);
        assert_eq!(br.read_bits(8).unwrap(), 0x5a);
        assert_eq!(br.bits_left(), 0);
    }

    #[test]
    fn xbits_uses_mpeg_offset_form() {
        let mut br = BitReader::new(&[0x40]); // 0100 0000
        assert_eq!(br.read_xbits(2).unwrap(), -2); // 01 -> 1 - 3
        let mut br = BitReader::new(&[0xc0]); // 1100 0000
        assert_eq!(br.read_xbits(2).unwrap(), 3); // 11 -> 3
    }

    #[test]
    fn vlc_decode() {
        // codes: 0 -> "0", 2 -> "10", 3 -> "11"
        let vlc = Vlc::new(&[(0, 1, 0), (2, 2, 1), (3, 2, 2)]);
        let mut br = BitReader::new(&[0b1100_0000]);
        assert_eq!(br.decode_vlc(&vlc).unwrap(), 2);
        assert_eq!(br.decode_vlc(&vlc).unwrap(), 0);
        assert_eq!(br.decode_vlc(&vlc).unwrap(), 0);
    }

    #[test]
    fn reads_past_end_as_zero() {
        let mut br = BitReader::new(&[0xff]);
        assert_eq!(br.read_bits(8).unwrap(), 0xff);
        assert_eq!(br.read_bits(4).unwrap(), 0);
    }
}
