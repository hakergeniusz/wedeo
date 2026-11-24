//! CRC-32/MPEG-2 used by MPEG-TS PSI sections.
//!
//! FFmpeg computes the section CRC with
//! `av_bswap32(av_crc(av_crc_get_table(AV_CRC_32_IEEE), -1, buf, len - 4))`.
//! `AV_CRC_32_IEEE` is a big-endian (`le = 0`) table for the polynomial
//! 0x04C11DB7, so `av_crc` returns the byte-swapped value and `av_bswap32`
//! turns it back into the plain big-endian CRC-32/MPEG-2 (init 0xFFFFFFFF,
//! no final XOR). We compute that value directly.

/// Compute CRC-32/MPEG-2 (polynomial 0x04C11DB7, init 0xFFFFFFFF, no final
/// XOR) over `data`. The result is written big-endian into PSI sections.
pub(crate) fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_mpeg2_matches_ffmpeg_section() {
        // CRC bytes of the PAT section produced by ffmpeg n9.1 for a single
        // program (service id 1, PMT pid 0x1000, version 0).
        let pat = [
            0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xf0, 0x00,
        ];
        assert_eq!(crc32_mpeg2(&pat), 0x2ab1_04b2);
    }

    #[test]
    fn crc32_mpeg2_empty() {
        assert_eq!(crc32_mpeg2(&[]), 0xFFFF_FFFF);
    }
}
