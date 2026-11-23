//! Big-endian RBSP bit reader, the equivalent of FFmpeg's `GetBitContext`.
//!
//! `av-bitstream`'s `BitReadBE` silently returns zeros for buffers shorter than
//! eight bytes (it cannot refill its 64-bit cache), which is unsuitable for
//! the short RBSPs parsed here. This reader tracks an explicit bit position so
//! every read is bounds-checked and `bits_left()` can go negative after an
//! intentional over-read, just like FFmpeg's `get_bits_left()`.

use wedeo_core::{Error, Result};

/// A bounds-checked, MSB-first bit reader over an RBSP byte slice.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Bit position measured from the MSB of `data[0]`.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    /// Create a reader positioned at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Total number of bits in the underlying buffer.
    pub fn size_in_bits(&self) -> usize {
        self.data.len() * 8
    }

    /// Number of bits already consumed.
    pub fn bit_position(&self) -> usize {
        self.bit_pos
    }

    /// Number of bits still available. Mirrors FFmpeg's `get_bits_left()`.
    pub fn bits_left(&self) -> isize {
        self.size_in_bits() as isize - self.bit_pos as isize
    }

    /// Byte offset of the current bit position (rounded up), used for
    /// `slice_header.data_offset` (FFmpeg: `align_get_bits(gb) - gb->buffer`).
    pub fn byte_position(&self) -> usize {
        self.bit_pos.div_ceil(8)
    }

    /// Peek `n` bits (MSB first) without advancing.
    pub fn show_bits(&self, n: usize) -> Result<u32> {
        if n > 32 || self.bit_pos + n > self.size_in_bits() {
            return Err(Error::InvalidData);
        }
        let mut v = 0u32;
        for i in 0..n {
            let p = self.bit_pos + i;
            v = (v << 1) | u32::from((self.data[p / 8] >> (7 - (p % 8))) & 1);
        }
        Ok(v)
    }

    /// Read a single bit.
    pub fn read_bit(&mut self) -> Result<bool> {
        if self.bit_pos >= self.data.len() * 8 {
            return Err(Error::InvalidData);
        }
        let p = self.bit_pos;
        self.bit_pos += 1;
        Ok(((self.data[p / 8] >> (7 - (p % 8))) & 1) != 0)
    }

    /// Read a single-bit flag (`u(1)`).
    pub fn read_flag(&mut self) -> Result<bool> {
        self.read_bit()
    }

    /// Read `n` bits as an unsigned big-endian integer (`u(n)`), `n <= 32`.
    pub fn read_bits(&mut self, n: usize) -> Result<u32> {
        if n > 32 || self.bit_pos + n > self.data.len() * 8 {
            return Err(Error::InvalidData);
        }
        let mut v = 0u32;
        for _ in 0..n {
            let p = self.bit_pos;
            self.bit_pos += 1;
            v = (v << 1) | u32::from((self.data[p / 8] >> (7 - (p % 8))) & 1);
        }
        Ok(v)
    }

    /// Read `n` bits as an unsigned big-endian integer (`u(n)`), `n <= 64`.
    pub fn read_bits64(&mut self, n: usize) -> Result<u64> {
        if n > 64 || self.bit_pos + n > self.data.len() * 8 {
            return Err(Error::InvalidData);
        }
        let mut v = 0u64;
        for _ in 0..n {
            let p = self.bit_pos;
            self.bit_pos += 1;
            v = (v << 1) | u64::from((self.data[p / 8] >> (7 - (p % 8))) & 1);
        }
        Ok(v)
    }

    /// Skip `n` bits. Tolerant of over-reads (clamped at the end of the
    /// buffer), matching FFmpeg's use of `skip_bits_long()` for trailing
    /// reserved bits.
    pub fn skip_bits(&mut self, n: usize) {
        self.bit_pos = self.bit_pos.saturating_add(n).min(self.data.len() * 8);
    }

    /// Align to the next byte boundary.
    pub fn align(&mut self) {
        let rem = self.bit_pos % 8;
        if rem != 0 {
            self.skip_bits(8 - rem);
        }
    }

    /// Read an unsigned exp-Golomb code (`ue(v)`), FFmpeg's
    /// `get_ue_golomb_long`: up to 31 leading zeros are allowed.
    pub fn read_ue(&mut self) -> Result<u32> {
        let mut leading_zeros = 0u32;
        while !self.read_bit()? {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err(Error::InvalidData);
            }
        }
        if leading_zeros == 0 {
            return Ok(0);
        }
        let suffix = self.read_bits(leading_zeros as usize)?;
        Ok((1u32 << leading_zeros) - 1 + suffix)
    }

    /// Read an unsigned exp-Golomb code that fits in 31 bits
    /// (FFmpeg's `get_ue_golomb_31`). Both use the same 32-bit window.
    pub fn read_ue31(&mut self) -> Result<u32> {
        self.read_ue()
    }

    /// Read a signed exp-Golomb code (`se(v)`), FFmpeg's `get_se_golomb`:
    /// `ue=0 -> 0`, `ue=1 -> 1`, `ue=2 -> -1`, `ue=3 -> 2`, ...
    pub fn read_se(&mut self) -> Result<i32> {
        let ue = self.read_ue()?;
        let sign = (ue & 1) as i32 - 1; // 0 for odd (positive), -1 for even
        Ok(((ue >> 1) as i32 ^ sign) + 1)
    }

    /// H.265 `more_rbsp_data()`: true if the remaining bits contain data other
    /// than just the `rbsp_trailing_bits`. The last set bit in an RBSP is its
    /// stop bit, so any set bit strictly before it is real data.
    pub fn more_rbsp_data(&self) -> bool {
        let total = self.data.len() * 8;
        if self.bit_pos >= total {
            return false;
        }
        // Locate the final set bit: the rbsp_stop_one_bit.
        let Some(last) = (self.bit_pos..total)
            .rev()
            .find(|&p| (self.data[p / 8] >> (7 - (p % 8))) & 1 != 0)
        else {
            return false;
        };
        // Any set bit strictly between the current position and the stop bit
        // is real data.
        (self.bit_pos..last).any(|p| (self.data[p / 8] >> (7 - (p % 8))) & 1 != 0)
    }

    /// Return the remaining bytes starting at the current position, aligned to
    /// a byte boundary. Used by SEI parsing (`ff_hevc_decode_nal_sei` consumes
    /// the payload as raw bytes).
    pub fn remaining_bytes(&self) -> &'a [u8] {
        let start = self.byte_position().min(self.data.len());
        &self.data[start..]
    }
}

#[cfg(test)]
mod bitreader_tests {
    use super::*;

    #[test]
    fn reads_bits_and_flags() {
        // 1011_0010 1100_0000
        let data = [0xB2, 0xC0];
        let mut br = BitReader::new(&data);
        assert!(br.read_flag().unwrap());
        assert!(!br.read_flag().unwrap());
        assert_eq!(br.read_bits(2).unwrap(), 3);
        assert_eq!(br.read_bits(4).unwrap(), 0b0010);
        assert_eq!(br.read_bits(8).unwrap(), 0b1100_0000);
        assert_eq!(br.bits_left(), 0);
    }

    #[test]
    fn reads_ue_se() {
        // ue 0 (1), ue 1 (010), ue 2 (011), se 0 (1), se -1 (011), se 1 (010)
        let data = [0b1010_0111, 0b0110_1000, 0, 0, 0, 0, 0, 0];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_ue().unwrap(), 0);
        assert_eq!(br.read_ue().unwrap(), 1);
        assert_eq!(br.read_ue().unwrap(), 2);
        assert_eq!(br.read_se().unwrap(), 0);
        assert_eq!(br.read_se().unwrap(), -1);
        assert_eq!(br.read_se().unwrap(), 1);
    }

    #[test]
    fn overread_returns_error() {
        let data = [0x80];
        let mut br = BitReader::new(&data);
        assert_eq!(br.read_bits(9), Err(Error::InvalidData));
    }

    #[test]
    fn more_rbsp_data() {
        // Byte 0x81: bit0 = 1 (data), then stop bit at bit 7 = 1.
        let data = [0x81];
        let mut br = BitReader::new(&data);
        assert!(br.more_rbsp_data()); // before the data bit
        br.skip_bits(1);
        assert!(!br.more_rbsp_data()); // only trailing bits remain
    }
}
