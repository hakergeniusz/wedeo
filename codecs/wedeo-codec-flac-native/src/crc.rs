//! CRCs used by the FLAC frame header and frame footer.
//!
//! FFmpeg uses `AV_CRC_8_ATM` (polynomial 0x07, non-reflected, init 0, xorout 0)
//! for the frame header and `AV_CRC_16_ANSI` (polynomial 0x8005) for the frame
//! footer.  The footer is only checked when `AV_EF_CRCCHECK` is set, which is
//! not the default, so only CRC-8 is mandatory for bit-exact decoding.

/// CRC-8/ATM (`AV_CRC_8_ATM`), polynomial x^8 + x^2 + x + 1.
pub fn crc8_atm(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
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
    fn crc8_appended_yields_zero() {
        // CRC-8 has the property CRC(m || CRC(m)) == 0 with no xorout.
        let msg = b"\xff\xf8\x69\x02\x00\x00\x9a\x00";
        let c = crc8_atm(msg);
        let mut with_crc = msg.to_vec();
        with_crc.push(c);
        assert_eq!(crc8_atm(&with_crc), 0);
    }
}
