//! MSB-first bit reader, equivalent to FFmpeg's `GetBitContext`.
//!
//! FLAC is a big-endian bitstream format: bits are consumed most-significant
//! bit first from each byte.  FFmpeg's input buffers carry
//! `AV_INPUT_BUFFER_PADDING_SIZE` zero bytes after the end of the packet, so
//! reads past the end return zero bits instead of failing.  `BitReader`
//! reproduces that behaviour.

/// Read up to 32 bits at `n` bits from `v`, sign extended.
fn sign_extend_u32(v: u32, n: u32) -> i32 {
    if n == 0 {
        0
    } else if n >= 32 {
        v as i32
    } else {
        (v.wrapping_shl(32 - n) as i32) >> (32 - n)
    }
}

/// Read up to 64 bits at `n` bits from `v`, sign extended.
fn sign_extend_u64(v: u64, n: u32) -> i64 {
    if n == 0 {
        0
    } else if n >= 64 {
        v as i64
    } else {
        (v.wrapping_shl(64 - n) as i64) >> (64 - n)
    }
}

/// MSB-first bit reader over a byte slice.
#[derive(Clone, Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Number of bits consumed from the start of `data`.
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    /// Create a reader positioned at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Number of unread bits (FFmpeg `get_bits_left`).
    pub fn get_bits_left(&self) -> i32 {
        (self.data.len().saturating_mul(8) as i64 - self.bit_pos as i64) as i32
    }

    /// Consumed whole bytes (used for CRC coverage).
    pub fn byte_position(&self) -> usize {
        self.bit_pos / 8
    }

    /// Align the read position to the next byte boundary (FFmpeg `align_get_bits`).
    pub fn align_to_byte(&mut self) {
        self.bit_pos = (self.bit_pos + 7) & !7;
    }

    /// Skip `n` bits (FFmpeg `skip_bits`).
    pub fn skip_bits(&mut self, n: u32) {
        self.bit_pos += n as usize;
    }

    /// Read `n` (<= 32) bits, MSB first. Bits past the end of the buffer are
    /// zero, matching FFmpeg's padded input buffer.
    pub fn get_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        let mut result = 0u32;
        let mut remaining = n as usize;
        while remaining > 0 {
            let byte_index = self.bit_pos >> 3;
            let bit_offset = self.bit_pos & 7;
            let available = 8 - bit_offset;
            let take = remaining.min(available);
            let byte = if byte_index < self.data.len() {
                self.data[byte_index]
            } else {
                0
            };
            let shift = available - take;
            let mask = ((1u16 << take) - 1) as u8;
            let chunk = ((byte >> shift) & mask) as u32;
            result = (result << take) | chunk;
            self.bit_pos += take;
            remaining -= take;
        }
        result
    }

    /// Read a single bit (FFmpeg `get_bits1`).
    pub fn get_bits1(&mut self) -> u32 {
        self.get_bits(1)
    }

    /// Read `n` (<= 32) bits and sign extend (FFmpeg `get_sbits`).
    pub fn get_sbits(&mut self, n: u32) -> i32 {
        sign_extend_u32(self.get_bits(n), n)
    }

    /// Read `n` (<= 32) bits, sign-extended long variant
    /// (FFmpeg `get_sbits_long`).
    pub fn get_sbits_long(&mut self, n: u32) -> i32 {
        sign_extend_u32(self.get_bits(n), n)
    }

    /// Read `n` (<= 64) bits (FFmpeg `get_bits64`).
    pub fn get_bits64(&mut self, n: u32) -> u64 {
        debug_assert!(n <= 64);
        if n <= 32 {
            self.get_bits(n) as u64
        } else {
            let high = self.get_bits(n - 32) as u64;
            let low = self.get_bits(32) as u64;
            (high << 32) | low
        }
    }

    /// Read `n` (<= 64) bits and sign extend (FFmpeg `get_sbits64`).
    pub fn get_sbits64(&mut self, n: u32) -> i64 {
        sign_extend_u64(self.get_bits64(n), n)
    }

    /// Peek at `n` (<= 64) bits without consuming them
    /// (FFmpeg `show_bits_long`).
    pub fn show_bits_long(&self, n: u32) -> u64 {
        let mut copy = self.clone();
        copy.get_bits64(n)
    }

    /// Get a unary code terminated by `stop`, returning the number of bits
    /// before the stop bit (FFmpeg `get_unary`).
    pub fn get_unary(&mut self, stop: u32, len: usize) -> usize {
        let mut i = 0;
        while i < len && self.get_bits1() != stop {
            i += 1;
        }
        i
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_msb_first_bits() {
        let data = [0b1011_0010, 0b1100_0000];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get_bits(1), 1);
        assert_eq!(br.get_bits(3), 0b011);
        assert_eq!(br.get_bits(4), 0b0010);
        assert_eq!(br.get_bits(8), 0b1100_0000);
        assert_eq!(br.get_bits_left(), 0);
    }

    #[test]
    fn sign_extension() {
        let data = [0xFF, 0x80, 0x7F];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get_sbits(8), -1);
        assert_eq!(br.get_sbits(8), -128);
        assert_eq!(br.get_sbits(8), 127);
    }

    #[test]
    fn unary_and_peek() {
        // 0,0,1 then 0xAB
        let data = [0b0010_1011];
        let mut br = BitReader::new(&data);
        assert_eq!(br.show_bits_long(3), 0b001);
        assert_eq!(br.get_unary(1, 8), 2);
        assert_eq!(br.get_bits(5), 0b01011);
    }

    #[test]
    fn reads_past_end_return_zero() {
        let data = [0x80];
        let mut br = BitReader::new(&data);
        assert_eq!(br.get_bits(24), 0x800000);
        assert_eq!(br.get_bits_left(), -16);
    }
}
