//! DC and AC coefficient decoding helpers.
//!
//! Ports `decode_dc` (`mpeg12dec.h`), `ff_mpeg1_decode_block_intra`
//! (`mpeg12.c`), `mpeg1_decode_block_inter` and `mpeg2_decode_block_intra`
//! (`mpeg12dec.c`). These helpers use the VLC tables from [`crate::vlc`].

use wedeo_codec::bitstream::{BitRead, BitReadBE};
use wedeo_core::{Error, Result};

use crate::tables::{MPEG12_LEVEL, MPEG12_RL_NB_ELEMS, MPEG12_RUN};
use crate::vlc::{VlcTable, get_xbits};

const MAX_INDEX: i32 = 63;

/// Read an `n`-bit sign-extended value (FFmpeg `SHOW_SBITS` / `get_sbits`).
pub fn get_sbits(br: &mut BitReadBE<'_>, n: u32) -> Result<i32> {
    assert!(n > 0 && n <= 24, "get_sbits: n must be in 1..=24");
    let v = br.get_bits_32(n as usize) as i32;
    if v & (1 << (n - 1)) != 0 {
        Ok(v | !((1i32 << n) - 1))
    } else {
        Ok(v)
    }
}

/// Decode a DC difference (FFmpeg `decode_dc`).
///
/// `component` 0 selects the luminance VLC, anything else the chrominance VLC.
pub fn decode_dc(
    br: &mut BitReadBE<'_>,
    component: usize,
    luma_vlc: &VlcTable,
    chroma_vlc: &VlcTable,
) -> Result<i32> {
    let vlc = if component == 0 { luma_vlc } else { chroma_vlc };
    let n = vlc.decode(br)?;
    if n == 0 {
        Ok(0)
    } else {
        get_xbits(br, n as u32)
    }
}

/// Kind of an RL VLC symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlKind {
    /// A normal `(run, level)` entry.
    Normal { run: i32, level: i32 },
    /// Escape: the run/level follow as explicit bits.
    Escape,
    /// End of block.
    Eob,
}

/// Resolve an RL VLC symbol index into run/level (or escape/EOB).
#[inline]
pub fn rl_sym(index: i32) -> RlKind {
    let i = index as usize;
    if i == MPEG12_RL_NB_ELEMS + 1 {
        RlKind::Eob
    } else if i == MPEG12_RL_NB_ELEMS {
        RlKind::Escape
    } else if i < MPEG12_RL_NB_ELEMS {
        RlKind::Normal {
            run: MPEG12_RUN[i] as i32,
            level: MPEG12_LEVEL[i] as i32,
        }
    } else {
        RlKind::Eob
    }
}

/// MPEG-1 intra block decoding (FFmpeg `ff_mpeg1_decode_block_intra`).
///
/// `last_dc` is updated in place; `block` receives the dequantized
/// coefficients. The quant matrix and scantable are already permuted.
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn decode_block_intra_mpeg1(
    br: &mut BitReadBE<'_>,
    block: &mut [i16; 64],
    quant_matrix: &[u16; 64],
    scantable: &[u8; 64],
    last_dc: &mut [i32; 3],
    n: usize,
    qscale: i32,
    dc_luma: &VlcTable,
    dc_chroma: &VlcTable,
    rl: &VlcTable,
) -> Result<()> {
    let component = if n <= 3 { 0 } else { n - 4 + 1 };
    let diff = decode_dc(br, component, dc_luma, dc_chroma)?;
    let dc = last_dc[component] + diff;
    last_dc[component] = dc;
    block[0] = (dc * quant_matrix[0] as i32) as i16;

    let mut i = 0i32;
    loop {
        let sym = rl.decode(br)?;
        let (mut level, run) = match rl_sym(sym) {
            RlKind::Eob => break,
            RlKind::Escape => {
                let run = br.get_bits_32(6) as i32 + 1;
                let mut level = get_sbits(br, 8)?;
                if level == -128 {
                    level = br.get_bits_32(8) as i32 - 256;
                } else if level == 0 {
                    level = br.get_bits_32(8) as i32;
                }
                (level, run)
            }
            RlKind::Normal { level, run } => {
                let sign = br.get_bit();
                (if sign { -level } else { level }, run)
            }
        };

        i += run;
        if i > MAX_INDEX {
            break;
        }
        let j = scantable[i as usize] as usize;
        if level < 0 {
            level = ((-level) * qscale * quant_matrix[j] as i32) >> 4;
            level = ((level - 1) | 1).wrapping_neg();
        } else {
            level = (level * qscale * quant_matrix[j] as i32) >> 4;
            level = (level - 1) | 1;
        }
        block[j] = level as i16;
    }

    if i > MAX_INDEX {
        Err(Error::InvalidData)
    } else {
        Ok(())
    }
}

/// MPEG-1 inter block decoding (FFmpeg `mpeg1_decode_block_inter`).
pub fn decode_block_inter_mpeg1(
    br: &mut BitReadBE<'_>,
    block: &mut [i16; 64],
    quant_matrix: &[u16; 64],
    scantable: &[u8; 64],
    qscale: i32,
    rl: &VlcTable,
) -> Result<()> {
    let mut i = -1i32;

    // Special case for the first coefficient.
    if br.peek_bit() {
        let mut level = (3 * qscale * quant_matrix[0] as i32) >> 5;
        level = (level - 1) | 1;
        if br.peek_bits_32(2) & 0x2 != 0 {
            level = -level;
        }
        block[0] = level as i16;
        i += 1;
        br.skip_bits(2);
    }

    loop {
        let sym = rl.decode(br)?;
        let (level, run) = match rl_sym(sym) {
            RlKind::Eob => break,
            RlKind::Escape => {
                let run = br.get_bits_32(6) as i32 + 1;
                let mut level = get_sbits(br, 8)?;
                if level == -128 {
                    level = br.get_bits_32(8) as i32 - 256;
                } else if level == 0 {
                    level = br.get_bits_32(8) as i32;
                }
                (level, run)
            }
            RlKind::Normal { level, run } => {
                let sign = br.get_bit();
                (if sign { -level } else { level }, run)
            }
        };

        i += run;
        if i > MAX_INDEX {
            break;
        }
        let j = scantable[i as usize] as usize;
        let quantized = if level < 0 {
            ((((-level) << 1) + 1) * qscale * quant_matrix[j] as i32) >> 4
        } else {
            ((((level) << 1) + 1) * qscale * quant_matrix[j] as i32) >> 4
        };
        let quantized = ((quantized - 1) | 1) * if level < 0 { -1 } else { 1 };
        block[j] = quantized as i16;
    }
    Ok(())
}

/// MPEG-2 intra block decoding (FFmpeg `mpeg2_decode_block_intra`).
// Argument count mirrors the FFmpeg C function signature verbatim.
#[allow(clippy::too_many_arguments)]
pub fn decode_block_intra_mpeg2(
    br: &mut BitReadBE<'_>,
    block: &mut [i16; 64],
    quant_matrix: &[u16; 64],
    scantable: &[u8; 64],
    last_dc: &mut [i32; 3],
    n: usize,
    qscale: i32,
    intra_dc_precision: i32,
    dc_luma: &VlcTable,
    dc_chroma: &VlcTable,
    rl: &VlcTable,
) -> Result<()> {
    let component = if n < 4 { 0 } else { (n & 1) + 1 };
    let diff = decode_dc(br, component, dc_luma, dc_chroma)?;
    let dc = last_dc[component] + diff;
    last_dc[component] = dc;
    block[0] = (dc * (1 << (3 - intra_dc_precision))) as i16;

    let mut mismatch = block[0] as i32 ^ 1;
    let mut i = 0i32;
    loop {
        let sym = rl.decode(br)?;
        let (mut level, run) = match rl_sym(sym) {
            RlKind::Eob => break,
            RlKind::Escape => {
                let run = br.get_bits_32(6) as i32 + 1;
                let level = get_sbits(br, 12)?;
                (level, run)
            }
            RlKind::Normal { level, run } => {
                let sign = br.get_signed_bit();
                (if sign != 0 { -level } else { level }, run)
            }
        };

        i += run;
        if i > MAX_INDEX {
            break;
        }
        let j = scantable[i as usize] as usize;
        if level < 0 {
            level = ((-level) * qscale * quant_matrix[j] as i32) >> 4;
            level = -level;
        } else {
            level = (level * qscale * quant_matrix[j] as i32) >> 4;
        }
        mismatch ^= level;
        block[j] = level as i16;
    }
    block[63] ^= (mismatch & 1) as i16;
    if i > MAX_INDEX {
        Err(Error::InvalidData)
    } else {
        Ok(())
    }
}

/// Extension helper: read one bit as a signed `0` or `-1` (FFmpeg
/// `SHOW_SBITS(re, gb, 1)`). Kept as a small local trait to avoid depending on
/// a newer `av-bitstream` API.
trait SignedBit {
    fn get_signed_bit(&mut self) -> i32;
}

impl SignedBit for BitReadBE<'_> {
    fn get_signed_bit(&mut self) -> i32 {
        -(self.get_bit() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{MPEG1_DEFAULT_INTRA_MATRIX, ZIGZAG_DIRECT};
    use crate::vlc::{dc_chroma_vlc, dc_luma_vlc, rl_vlc};

    fn padded(data: &[u8]) -> Vec<u8> {
        let mut v = data.to_vec();
        v.resize(32, 0);
        v
    }

    #[test]
    fn decode_dc_zero_size() {
        let luma = dc_luma_vlc();
        let chroma = dc_chroma_vlc();
        // DC luma size index 0 has a 3-bit code of 0x4 (binary 100) and no
        // mantissa bits, so the difference is exactly zero.
        let buf = padded(&[0x80]);
        let mut br = BitReadBE::new(&buf);
        assert_eq!(decode_dc(&mut br, 0, &luma, &chroma).unwrap(), 0);
    }

    #[test]
    fn rl_sym_mapping() {
        assert_eq!(rl_sym(0), RlKind::Normal { run: 0, level: 1 });
        assert_eq!(rl_sym(MPEG12_RL_NB_ELEMS as i32), RlKind::Escape);
        assert_eq!(rl_sym(MPEG12_RL_NB_ELEMS as i32 + 1), RlKind::Eob);
    }

    #[test]
    fn decode_mpeg1_intra_simple() {
        let luma = dc_luma_vlc();
        let chroma = dc_chroma_vlc();
        let rl = rl_vlc(false);
        let identity: [u8; 64] = core::array::from_fn(|i| i as u8);
        let mut last_dc = [128, 128, 128];
        let mut block = [0i16; 64];

        // DC: size index 0 (code 100), no mantissa -> diff 0.
        // AC: EOB code for MPEG-1 is 0x2 with 2 bits -> sym 112 (EOB).
        let mut bits: u64 = 0;
        bits = (bits << 3) | 0b100; // DC size
        bits = (bits << 2) | 0b10; // EOB
        bits <<= 64 - 5;
        let data = [
            (bits >> 56) as u8,
            (bits >> 48) as u8,
            (bits >> 40) as u8,
            (bits >> 32) as u8,
            (bits >> 24) as u8,
            (bits >> 16) as u8,
            (bits >> 8) as u8,
            bits as u8,
        ];
        let buf = padded(&data);
        let mut br = BitReadBE::new(&buf);
        decode_block_intra_mpeg1(
            &mut br,
            &mut block,
            &MPEG1_DEFAULT_INTRA_MATRIX,
            &identity,
            &mut last_dc,
            0,
            1,
            &luma,
            &chroma,
            &rl,
        )
        .unwrap();
        assert_eq!(last_dc[0], 128);
        assert_eq!(block[0], (128 * 8) as i16);
        assert_eq!(block[ZIGZAG_DIRECT[1] as usize], 0);
    }
}
