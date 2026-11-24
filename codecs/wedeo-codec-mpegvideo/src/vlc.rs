//! Variable-length code (VLC) decoding helpers.
//!
//! FFmpeg builds large, arch-specific lookup tables from the raw
//! `(code, bits)` pairs in `mpeg12data.c`. For a reusable pure-Rust library a
//! canonical decoder is sufficient: VLCs are prefix-free, so any correct
//! decoder produces the same symbols from the same bitstream. This module
//! provides a small table builder plus ready-made tables for the MPEG-1/2 DC,
//! motion-vector, macroblock-address, coded-block-pattern and run/level
//! codebooks.

use wedeo_codec::bitstream::{BitRead, BitReadBE};
use wedeo_core::{Error, Result};

use crate::tables::{
    DC_CHROMA_BITS, DC_CHROMA_CODE, DC_LUM_BITS, DC_LUM_CODE, MB_ADDR_INCR_TABLE,
    MB_MOTION_VECTOR_TABLE, MB_PAT_TABLE, MPEG1_VLC_TABLE, MPEG2_VLC_TABLE,
};

/// One VLC entry: a right-aligned code, its length in bits, and the decoded
/// symbol (normally an index into one of the raw tables).
#[derive(Debug, Clone, Copy)]
pub struct VlcEntry {
    pub code: u32,
    pub bits: u8,
    pub sym: i32,
}

/// A decoded VLC table.
#[derive(Debug, Clone)]
pub struct VlcTable {
    /// Lookup storage, indexed as `by_len[bits][code]`.
    by_len: Vec<Vec<i32>>,
    /// `-1` marks an unused slot.
    max_bits: u8,
}

impl VlcTable {
    /// Build a table from `(symbol, code, bits)` triples.
    ///
    /// `code` must be right-aligned (the standard representation in FFmpeg's
    /// `*_code[]` arrays). Codes must form a prefix-free set.
    pub fn new(entries: &[(i32, u32, u8)]) -> Self {
        let max_bits = entries.iter().map(|&(_, _, b)| b).max().unwrap_or(1);
        let mut by_len: Vec<Vec<i32>> = (0..=max_bits).map(|len| vec![-1; 1usize << len]).collect();
        for &(sym, code, bits) in entries {
            let len = bits as usize;
            if len == 0 || len > max_bits as usize {
                continue;
            }
            let idx = code as usize;
            if idx < by_len[len].len() {
                by_len[len][idx] = sym;
            }
        }
        Self { by_len, max_bits }
    }

    /// Decode one symbol from `br`, consuming exactly the code's bits.
    ///
    /// Returns `Error::InvalidData` when the incoming bits do not match any
    /// code in the table.
    pub fn decode(&self, br: &mut BitReadBE<'_>) -> Result<i32> {
        let peek = br.peek_bits_32(self.max_bits as usize);
        for len in 1..=self.max_bits {
            let candidate = (peek >> (self.max_bits - len)) as usize;
            let sym = self.by_len[len as usize][candidate];
            if sym >= 0 {
                br.skip_bits(len as usize);
                return Ok(sym);
            }
        }
        Err(Error::InvalidData)
    }

    /// Longest code length in this table.
    pub fn max_bits(&self) -> u8 {
        self.max_bits
    }
}

/// Build a VLC whose symbols are the indices of the `(code, bits)` pairs.
fn index_table(codes: &[(u32, u8)]) -> VlcTable {
    let entries: Vec<(i32, u32, u8)> = codes
        .iter()
        .enumerate()
        .map(|(i, &(c, b))| (i as i32, c, b))
        .collect();
    VlcTable::new(&entries)
}

/// Luminance DC size VLC (FFmpeg `ff_dc_lum_vlc`). The symbol is the DC size
/// index, i.e. the number of mantissa bits to pass to [`get_xbits`].
pub fn dc_luma_vlc() -> VlcTable {
    let codes: Vec<(u32, u8)> = DC_LUM_CODE
        .iter()
        .zip(DC_LUM_BITS.iter())
        .map(|(&c, &b)| (c as u32, b))
        .collect();
    index_table(&codes)
}

/// Chrominance DC size VLC (FFmpeg `ff_dc_chroma_vlc`).
pub fn dc_chroma_vlc() -> VlcTable {
    let codes: Vec<(u32, u8)> = DC_CHROMA_CODE
        .iter()
        .zip(DC_CHROMA_BITS.iter())
        .map(|(&c, &b)| (c as u32, b))
        .collect();
    index_table(&codes)
}

/// Motion-vector VLC (FFmpeg `ff_mv_vlc`). Symbol is an index into
/// [`crate::tables::MB_MOTION_VECTOR_TABLE`] (0..=16).
pub fn motion_vlc() -> VlcTable {
    let codes: Vec<(u32, u8)> = MB_MOTION_VECTOR_TABLE
        .iter()
        .map(|&[c, b]| (c as u32, b))
        .collect();
    index_table(&codes)
}

/// Macroblock address increment VLC (FFmpeg `ff_mbincr_vlc`).
pub fn mb_addr_incr_vlc() -> VlcTable {
    let codes: Vec<(u32, u8)> = MB_ADDR_INCR_TABLE
        .iter()
        .map(|&[c, b]| (c as u32, b))
        .collect();
    index_table(&codes)
}

/// Coded block pattern VLC (FFmpeg `ff_mb_pat_vlc`).
pub fn mb_pat_vlc() -> VlcTable {
    let codes: Vec<(u32, u8)> = MB_PAT_TABLE.iter().map(|&[c, b]| (c as u32, b)).collect();
    index_table(&codes)
}

/// Run/level VLC for MPEG-1 or MPEG-2 (FFmpeg `ff_mpeg1_rl_vlc` /
/// `ff_mpeg2_rl_vlc`). The symbol is an index into the corresponding
/// `*_VLC_TABLE`; indices `MPEG12_RL_NB_ELEMS` and `+1` are escape and EOB.
pub fn rl_vlc(mpeg2: bool) -> VlcTable {
    let table = if mpeg2 {
        &MPEG2_VLC_TABLE
    } else {
        &MPEG1_VLC_TABLE
    };
    let codes: Vec<(u32, u8)> = table.iter().map(|&[c, b]| (c as u32, b as u8)).collect();
    index_table(&codes)
}

/// Read an MPEG-1 "DC style" signed value of `n` bits (FFmpeg `get_xbits`).
///
/// The first bit is a sign flag: if set the value is the raw `n`-bit pattern,
/// otherwise the value is `raw - (1 << n) + 1`. `n` must be in `1..=25`.
pub fn get_xbits(br: &mut BitReadBE<'_>, n: u32) -> Result<i32> {
    assert!(n > 0 && n <= 25, "get_xbits: n must be in 1..=25");
    let raw = br.get_bits_32(n as usize) as i32;
    if raw & (1 << (n - 1)) != 0 {
        Ok(raw)
    } else {
        Ok(raw - (1i32 << n) + 1)
    }
}

/// Sign-extend a 5-bit (or wider) value (FFmpeg `sign_extend`).
#[inline]
pub fn sign_extend(v: i32, bits: u32) -> i32 {
    let shift = 32 - bits;
    (v << shift) >> shift
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        v.resize(data.len().max(8) + 8, 0);
        v
    }

    #[test]
    fn dc_luma_decodes_all_sizes() {
        // Each code is vlc_code followed by `index` mantissa bits.
        let vlc = dc_luma_vlc();
        for index in 0..12usize {
            let code = DC_LUM_CODE[index] as u32;
            let total_bits = DC_LUM_BITS[index] as usize + index;
            // Pack code (left-aligned in total_bits) followed by zero mantissa.
            let bits = ((code as u64) << index) << (64 - total_bits);
            let mut bytes = [0u8; 8];
            for i in 0..8 {
                bytes[i] = (bits >> (56 - 8 * i)) as u8;
            }
            let buf = padded(&bytes);
            let mut br = BitReadBE::new(&buf);
            assert_eq!(vlc.decode(&mut br).unwrap(), index as i32);
        }
    }

    #[test]
    fn get_xbits_matches_mpeg1_dc_semantics() {
        // n = 2: 00 -> -3, 01 -> -2, 10 -> 2, 11 -> 3.
        for &(bits, expected) in &[(0b00u32, -3), (0b01, -2), (0b10, 2), (0b11, 3)] {
            let bytes = [(bits << 6) as u8, 0, 0, 0, 0, 0, 0, 0];
            let buf = padded(&bytes);
            let mut br = BitReadBE::new(&buf);
            assert_eq!(get_xbits(&mut br, 2).unwrap(), expected);
        }
    }

    #[test]
    fn vlc_rejects_invalid_code() {
        // Use a deliberately incomplete table (only the code `0`) so that a
        // stream beginning with a `1` bit cannot be decoded.
        let vlc = VlcTable::new(&[(42, 0, 1)]);
        let buf = padded(&[0x80, 0, 0, 0]);
        let mut br = BitReadBE::new(&buf);
        assert_eq!(vlc.decode(&mut br), Err(Error::InvalidData));
    }
}
