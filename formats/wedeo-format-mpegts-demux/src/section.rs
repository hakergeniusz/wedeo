//! MPEG-TS PSI section reassembly, ported from FFmpeg's `write_section_data()`,
//! `skip_identical()` and `parse_section_header()` in `libavformat/mpegts.c`.

use crate::constants::{MAX_SECTION_SIZE, STUFFING_BYTE};

/// The kind of PSI section carried by a PID, used to pick the callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Pat,
    Pmt,
}

/// `MpegTSSectionFilter` in FFmpeg `mpegts.c` (the fields used here).
#[derive(Debug, Clone)]
pub struct SectionFilter {
    pub kind: SectionKind,
    buf: Vec<u8>,
    section_h_size: Option<usize>,
    end_of_section_reached: bool,
    last_ver: Option<u8>,
    last_crc: u32,
    check_crc: bool,
}

impl SectionFilter {
    pub fn new(kind: SectionKind, check_crc: bool) -> Self {
        Self {
            kind,
            buf: Vec::with_capacity(MAX_SECTION_SIZE),
            section_h_size: None,
            end_of_section_reached: false,
            last_ver: None,
            last_crc: 0,
            check_crc,
        }
    }

    /// `skip_identical()` in FFmpeg `mpegts.c`.
    pub fn skip_identical(&mut self, version: u8, crc: u32) -> bool {
        if self.last_ver == Some(version) && self.last_crc == crc {
            return true;
        }
        self.last_ver = Some(version);
        self.last_crc = crc;
        false
    }

    /// `write_section_data()` in FFmpeg `mpegts.c`. Returns every complete
    /// section found in the reassembled buffer.
    pub fn write_data(&mut self, data: &[u8], is_start: bool) -> Vec<Vec<u8>> {
        if is_start {
            self.buf.clear();
            self.buf.extend_from_slice(data);
            self.section_h_size = None;
            self.end_of_section_reached = false;
        } else {
            if self.end_of_section_reached {
                return Vec::new();
            }
            let len = (MAX_SECTION_SIZE - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..len]);
        }

        let mut sections = Vec::new();
        let mut offset = 0usize;
        while self.buf.len() - offset > 0 && self.buf[offset] != STUFFING_BYTE {
            /* compute section length if possible */
            if self.section_h_size.is_none() && self.buf.len() - offset >= 3 {
                let len = ((self.buf[offset + 1] as usize & 0x0f) << 8
                    | self.buf[offset + 2] as usize)
                    + 3;
                if len > MAX_SECTION_SIZE {
                    return sections;
                }
                self.section_h_size = Some(len);
            }

            if let Some(len) = self.section_h_size {
                if self.buf.len() - offset >= len {
                    self.end_of_section_reached = true;
                    let section = self.buf[offset..offset + len].to_vec();
                    if !self.check_crc || crc_valid(&section) {
                        sections.push(section);
                    }
                    offset += len;
                    self.section_h_size = None;
                } else {
                    self.section_h_size = None;
                    self.end_of_section_reached = false;
                    break;
                }
            } else {
                break;
            }
        }
        sections
    }
}

/// The section header parsed by `parse_section_header()` in `mpegts.c`.
#[derive(Debug, Clone)]
pub struct SectionHeader {
    pub tid: u8,
    pub version: u8,
    pub current_next: u8,
}

/// `parse_section_header()` in FFmpeg `mpegts.c`. `section` includes the
/// trailing CRC, which is excluded from the parse range (`p_end = end - 4`).
pub fn parse_section_header(section: &[u8]) -> Option<SectionHeader> {
    if section.len() < 12 {
        return None;
    }
    let end = section.len() - 4;
    let tid = *section.first()?;
    let val = *section.get(5)?;
    let version = (val >> 1) & 0x1f;
    let current_next = val & 0x01;
    if 8 > end {
        return None;
    }
    Some(SectionHeader {
        tid,
        version,
        current_next,
    })
}

/// CRC-32/MPEG-2 (`AV_CRC_32_IEEE` in FFmpeg): polynomial 0x04C11DB7, initial
/// value 0xFFFFFFFF, no reflection, no final XOR. A section carrying its CRC
/// in the last four bytes hashes to zero.
pub fn crc_valid(section: &[u8]) -> bool {
    if section.len() < 4 {
        return false;
    }
    let mut crc = 0xffff_ffffu32;
    for &byte in section {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append a CRC-32/MPEG-2 to a section body so `crc_valid()` succeeds.
    fn append_crc(body: &[u8]) -> Vec<u8> {
        let mut crc = 0xffff_ffffu32;
        for &byte in body {
            crc ^= u32::from(byte) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04c1_1db7
                } else {
                    crc << 1
                };
            }
        }
        let mut out = body.to_vec();
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    #[test]
    fn validates_section_crc() {
        let body = [
            0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xe0, 0x10,
        ];
        let section = append_crc(&body);
        assert!(crc_valid(&section));
        let mut bad = section.clone();
        bad[3] ^= 0xff;
        assert!(!crc_valid(&bad));
    }

    #[test]
    fn parses_section_header() {
        let body = [
            0x02, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xe0, 0x10,
        ];
        let section = append_crc(&body);
        let h = parse_section_header(&section).unwrap();
        assert_eq!(h.tid, 0x02);
        assert_eq!(h.version, 0);
        assert_eq!(h.current_next, 1);
    }

    #[test]
    fn reassembles_section_across_payloads() {
        let body = [
            0x00, 0xb0, 0x0d, 0x00, 0x01, 0xc1, 0x00, 0x00, 0x00, 0x01, 0xe0, 0x10,
        ];
        let section = append_crc(&body);
        let mut filter = SectionFilter::new(SectionKind::Pat, true);
        assert!(filter.write_data(&section[..8], true).is_empty());
        let out = filter.write_data(&section[8..], false);
        assert_eq!(out, vec![section]);
    }
}
