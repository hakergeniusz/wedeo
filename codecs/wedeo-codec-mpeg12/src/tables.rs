//! MPEG-1/2 constant tables, copied from FFmpeg's `mpeg12data.c`,
//! `mpeg12data.h`, `mpeg12.c`, `mpegvideodata.c` and `mathtables.c`.

use std::sync::OnceLock;

use crate::bitreader::Vlc;

pub const ZIGZAG_DIRECT: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

pub const ALTERNATE_VERTICAL_SCAN: [u8; 64] = [
    0, 8, 16, 24, 1, 9, 2, 10, 17, 25, 32, 40, 48, 56, 57, 49, 41, 33, 26, 18, 3, 11, 4, 12, 19,
    27, 34, 42, 50, 58, 35, 43, 51, 59, 20, 28, 5, 13, 6, 14, 21, 29, 36, 44, 52, 60, 37, 45, 53,
    61, 22, 30, 7, 15, 23, 31, 38, 46, 54, 62, 39, 47, 55, 63,
];

pub const MPEG1_DEFAULT_INTRA_MATRIX: [u16; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38,
    22, 22, 26, 27, 29, 34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58,
    26, 27, 29, 34, 38, 46, 56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
];

pub const MPEG1_DEFAULT_NON_INTRA_MATRIX: [u16; 64] = [16; 64];

pub const NON_LINEAR_QSCALE: [u8; 32] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 18, 20, 22, 24, 28, 32, 36, 40, 44, 48, 52, 56, 64,
    72, 80, 88, 96, 104, 112,
];

pub const MPEG2_FRAME_RATE: [(u32, u32); 62] = [
    (1, 1),
    (2, 1),
    (3, 1),
    (4, 1),
    (5, 1),
    (6, 1),
    (8, 1),
    (9, 1),
    (10, 1),
    (12, 1),
    (15, 1),
    (16, 1),
    (18, 1),
    (20, 1),
    (24, 1),
    (25, 1),
    (30, 1),
    (32, 1),
    (36, 1),
    (40, 1),
    (45, 1),
    (48, 1),
    (50, 1),
    (60, 1),
    (72, 1),
    (75, 1),
    (80, 1),
    (90, 1),
    (96, 1),
    (100, 1),
    (120, 1),
    (150, 1),
    (180, 1),
    (200, 1),
    (240, 1),
    (750, 1001),
    (800, 1001),
    (960, 1001),
    (1000, 1001),
    (1200, 1001),
    (1250, 1001),
    (1500, 1001),
    (1600, 1001),
    (1875, 1001),
    (2000, 1001),
    (2400, 1001),
    (2500, 1001),
    (3000, 1001),
    (3750, 1001),
    (4000, 1001),
    (4800, 1001),
    (5000, 1001),
    (6000, 1001),
    (7500, 1001),
    (8000, 1001),
    (10000, 1001),
    (12000, 1001),
    (15000, 1001),
    (20000, 1001),
    (24000, 1001),
    (30000, 1001),
    (60000, 1001),
];

// ---- DC coefficient VLCs ----

const DC_LUM_CODE: [u16; 12] = [
    0x4, 0x0, 0x1, 0x5, 0x6, 0xe, 0x1e, 0x3e, 0x7e, 0xfe, 0x1fe, 0x1ff,
];
const DC_LUM_BITS: [u8; 12] = [3, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 9];
const DC_CHROMA_CODE: [u16; 12] = [
    0x0, 0x1, 0x2, 0x6, 0xe, 0x1e, 0x3e, 0x7e, 0xfe, 0x1fe, 0x3fe, 0x3ff,
];
const DC_CHROMA_BITS: [u8; 12] = [2, 2, 2, 3, 4, 5, 6, 7, 8, 9, 10, 10];

// ---- Motion vector VLC ----

pub const MV_TABLE: [(u16, u8); 17] = [
    (0x1, 1),
    (0x1, 2),
    (0x1, 3),
    (0x1, 4),
    (0x3, 6),
    (0x5, 7),
    (0x4, 7),
    (0x3, 7),
    (0xb, 9),
    (0xa, 9),
    (0x9, 9),
    (0x11, 10),
    (0x10, 10),
    (0xf, 10),
    (0xe, 10),
    (0xd, 10),
    (0xc, 10),
];

// ---- Macroblock address increment VLC ----

const MB_ADDR_INCR: [(u16, u8); 36] = [
    (0x1, 1),
    (0x3, 3),
    (0x2, 3),
    (0x3, 4),
    (0x2, 4),
    (0x3, 5),
    (0x2, 5),
    (0x7, 7),
    (0x6, 7),
    (0xb, 8),
    (0xa, 8),
    (0x9, 8),
    (0x8, 8),
    (0x7, 8),
    (0x6, 8),
    (0x17, 10),
    (0x16, 10),
    (0x15, 10),
    (0x14, 10),
    (0x13, 10),
    (0x12, 10),
    (0x23, 11),
    (0x22, 11),
    (0x21, 11),
    (0x20, 11),
    (0x1f, 11),
    (0x1e, 11),
    (0x1d, 11),
    (0x1c, 11),
    (0x1b, 11),
    (0x1a, 11),
    (0x19, 11),
    (0x18, 11),
    (0x8, 11),
    (0xf, 11),
    (0x0, 8),
];

// ---- Coded block pattern VLC: symbol == CBP value ----

const MB_PAT: [(u16, u8); 64] = [
    (0x1, 9),
    (0xb, 5),
    (0x9, 5),
    (0xd, 6),
    (0xd, 4),
    (0x17, 7),
    (0x13, 7),
    (0x1f, 8),
    (0xc, 4),
    (0x16, 7),
    (0x12, 7),
    (0x1e, 8),
    (0x13, 5),
    (0x1b, 8),
    (0x17, 8),
    (0x13, 8),
    (0xb, 4),
    (0x15, 7),
    (0x11, 7),
    (0x1d, 8),
    (0x11, 5),
    (0x19, 8),
    (0x15, 8),
    (0x11, 8),
    (0xf, 6),
    (0xf, 8),
    (0xd, 8),
    (0x3, 9),
    (0xf, 5),
    (0xb, 8),
    (0x7, 8),
    (0x7, 9),
    (0xa, 4),
    (0x14, 7),
    (0x10, 7),
    (0x1c, 8),
    (0xe, 6),
    (0xe, 8),
    (0xc, 8),
    (0x2, 9),
    (0x10, 5),
    (0x18, 8),
    (0x14, 8),
    (0x10, 8),
    (0xe, 5),
    (0xa, 8),
    (0x6, 8),
    (0x6, 9),
    (0x12, 5),
    (0x1a, 8),
    (0x16, 8),
    (0x12, 8),
    (0xd, 5),
    (0x9, 8),
    (0x5, 8),
    (0x5, 9),
    (0xc, 5),
    (0x8, 8),
    (0x4, 8),
    (0x4, 9),
    (0x7, 3),
    (0xa, 5),
    (0x8, 5),
    (0xc, 6),
];

// P/B macroblock type tables (code, bits) and symbol values from mpeg12.c
const MB_PTYPE: [(u16, u8, i32); 7] = [
    (3, 5, 1 << 0),
    (1, 2, (1 << 12) | (1 << 10) | (1 << 9) | (1 << 3)),
    (1, 3, 1 << 12),
    (1, 1, (1 << 12) | (1 << 10)),
    (1, 6, (1 << 11) | (1 << 0)),
    (
        1,
        5,
        (1 << 11) | (1 << 12) | (1 << 10) | (1 << 9) | (1 << 3),
    ),
    (2, 5, (1 << 11) | (1 << 12) | (1 << 10)),
];

const MB_BTYPE: [(u16, u8, i32); 11] = [
    (3, 5, 1 << 0),
    (2, 3, 1 << 13),
    (3, 3, (1 << 13) | (1 << 10)),
    (2, 4, 1 << 12),
    (3, 4, (1 << 12) | (1 << 10)),
    (2, 2, (1 << 12) | (1 << 13)),
    (3, 2, (1 << 12) | (1 << 13) | (1 << 10)),
    (1, 6, (1 << 11) | (1 << 0)),
    (2, 6, (1 << 11) | (1 << 13) | (1 << 10)),
    (3, 6, (1 << 11) | (1 << 12) | (1 << 10)),
    (2, 5, (1 << 11) | (1 << 12) | (1 << 13) | (1 << 10)),
];

// ---- Run/level tables for coefficient decoding ----

pub const RL_LEVEL: [i8; 111] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    13, 14, 15, 16, 17, 18, 1, 2, 3, 4, 5, 1, 2, 3, 4, 1, 2, 3, 1, 2, 3, 1, 2, 3, 1, 2, 1, 2, 1, 2,
    1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];

pub const RL_RUN: [i8; 111] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 5, 6, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14,
    15, 15, 16, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
];

const MPEG1_VLC_TABLE: [(u16, u8); 113] = [
    (0x3, 2),
    (0x4, 4),
    (0x5, 5),
    (0x6, 7),
    (0x26, 8),
    (0x21, 8),
    (0xa, 10),
    (0x1d, 12),
    (0x18, 12),
    (0x13, 12),
    (0x10, 12),
    (0x1a, 13),
    (0x19, 13),
    (0x18, 13),
    (0x17, 13),
    (0x1f, 14),
    (0x1e, 14),
    (0x1d, 14),
    (0x1c, 14),
    (0x1b, 14),
    (0x1a, 14),
    (0x19, 14),
    (0x18, 14),
    (0x17, 14),
    (0x16, 14),
    (0x15, 14),
    (0x14, 14),
    (0x13, 14),
    (0x12, 14),
    (0x11, 14),
    (0x10, 14),
    (0x18, 15),
    (0x17, 15),
    (0x16, 15),
    (0x15, 15),
    (0x14, 15),
    (0x13, 15),
    (0x12, 15),
    (0x11, 15),
    (0x10, 15),
    (0x3, 3),
    (0x6, 6),
    (0x25, 8),
    (0xc, 10),
    (0x1b, 12),
    (0x16, 13),
    (0x15, 13),
    (0x1f, 15),
    (0x1e, 15),
    (0x1d, 15),
    (0x1c, 15),
    (0x1b, 15),
    (0x1a, 15),
    (0x19, 15),
    (0x13, 16),
    (0x12, 16),
    (0x11, 16),
    (0x10, 16),
    (0x5, 4),
    (0x4, 7),
    (0xb, 10),
    (0x14, 12),
    (0x14, 13),
    (0x7, 5),
    (0x24, 8),
    (0x1c, 12),
    (0x13, 13),
    (0x6, 5),
    (0xf, 10),
    (0x12, 12),
    (0x7, 6),
    (0x9, 10),
    (0x12, 13),
    (0x5, 6),
    (0x1e, 12),
    (0x14, 16),
    (0x4, 6),
    (0x15, 12),
    (0x7, 7),
    (0x11, 12),
    (0x5, 7),
    (0x11, 13),
    (0x27, 8),
    (0x10, 13),
    (0x23, 8),
    (0x1a, 16),
    (0x22, 8),
    (0x19, 16),
    (0x20, 8),
    (0x18, 16),
    (0xe, 10),
    (0x17, 16),
    (0xd, 10),
    (0x16, 16),
    (0x8, 10),
    (0x15, 16),
    (0x1f, 12),
    (0x1a, 12),
    (0x19, 12),
    (0x17, 12),
    (0x16, 12),
    (0x1f, 13),
    (0x1e, 13),
    (0x1d, 13),
    (0x1c, 13),
    (0x1b, 13),
    (0x1f, 16),
    (0x1e, 16),
    (0x1d, 16),
    (0x1c, 16),
    (0x1b, 16),
    (0x1, 6),
    (0x2, 2),
];

const MPEG2_VLC_TABLE: [(u16, u8); 113] = [
    (0x02, 2),
    (0x06, 3),
    (0x07, 4),
    (0x1c, 5),
    (0x1d, 5),
    (0x05, 6),
    (0x04, 6),
    (0x7b, 7),
    (0x7c, 7),
    (0x23, 8),
    (0x22, 8),
    (0xfa, 8),
    (0xfb, 8),
    (0xfe, 8),
    (0xff, 8),
    (0x1f, 14),
    (0x1e, 14),
    (0x1d, 14),
    (0x1c, 14),
    (0x1b, 14),
    (0x1a, 14),
    (0x19, 14),
    (0x18, 14),
    (0x17, 14),
    (0x16, 14),
    (0x15, 14),
    (0x14, 14),
    (0x13, 14),
    (0x12, 14),
    (0x11, 14),
    (0x10, 14),
    (0x18, 15),
    (0x17, 15),
    (0x16, 15),
    (0x15, 15),
    (0x14, 15),
    (0x13, 15),
    (0x12, 15),
    (0x11, 15),
    (0x10, 15),
    (0x02, 3),
    (0x06, 5),
    (0x79, 7),
    (0x27, 8),
    (0x20, 8),
    (0x16, 13),
    (0x15, 13),
    (0x1f, 15),
    (0x1e, 15),
    (0x1d, 15),
    (0x1c, 15),
    (0x1b, 15),
    (0x1a, 15),
    (0x19, 15),
    (0x13, 16),
    (0x12, 16),
    (0x11, 16),
    (0x10, 16),
    (0x05, 5),
    (0x07, 7),
    (0xfc, 8),
    (0x0c, 10),
    (0x14, 13),
    (0x07, 5),
    (0x26, 8),
    (0x1c, 12),
    (0x13, 13),
    (0x06, 6),
    (0xfd, 8),
    (0x12, 12),
    (0x07, 6),
    (0x04, 9),
    (0x12, 13),
    (0x06, 7),
    (0x1e, 12),
    (0x14, 16),
    (0x04, 7),
    (0x15, 12),
    (0x05, 7),
    (0x11, 12),
    (0x78, 7),
    (0x11, 13),
    (0x7a, 7),
    (0x10, 13),
    (0x21, 8),
    (0x1a, 16),
    (0x25, 8),
    (0x19, 16),
    (0x24, 8),
    (0x18, 16),
    (0x05, 9),
    (0x17, 16),
    (0x07, 9),
    (0x16, 16),
    (0x0d, 10),
    (0x15, 16),
    (0x1f, 12),
    (0x1a, 12),
    (0x19, 12),
    (0x17, 12),
    (0x16, 12),
    (0x1f, 13),
    (0x1e, 13),
    (0x1d, 13),
    (0x1c, 13),
    (0x1b, 13),
    (0x1f, 16),
    (0x1e, 16),
    (0x1d, 16),
    (0x1c, 16),
    (0x1b, 16),
    (0x01, 6),
    (0x06, 4),
];

fn build_vlc(entries: &[(u16, u8, i32)]) -> Vlc {
    let triples: Vec<(u32, u8, i32)> = entries
        .iter()
        .map(|&(code, bits, sym)| (code as u32, bits, sym))
        .collect();
    Vlc::new(&triples)
}

fn build_index_vlc(table: &[(u16, u8)]) -> Vlc {
    let triples: Vec<(u32, u8, i32)> = table
        .iter()
        .enumerate()
        .map(|(i, &(code, bits))| (code as u32, bits, i as i32))
        .collect();
    Vlc::new(&triples)
}

static DC_LUM_VLC: OnceLock<Vlc> = OnceLock::new();
static DC_CHROMA_VLC: OnceLock<Vlc> = OnceLock::new();
static MV_VLC: OnceLock<Vlc> = OnceLock::new();
static MB_INCR_VLC: OnceLock<Vlc> = OnceLock::new();
static MB_PAT_VLC: OnceLock<Vlc> = OnceLock::new();
static MB_PTYPE_VLC: OnceLock<Vlc> = OnceLock::new();
static MB_BTYPE_VLC: OnceLock<Vlc> = OnceLock::new();
static RL_MPEG1_VLC: OnceLock<Vlc> = OnceLock::new();
static RL_MPEG2_VLC: OnceLock<Vlc> = OnceLock::new();

pub fn dc_lum_vlc() -> &'static Vlc {
    DC_LUM_VLC.get_or_init(|| {
        let entries: Vec<(u16, u8, i32)> = DC_LUM_CODE
            .iter()
            .zip(DC_LUM_BITS.iter())
            .enumerate()
            .map(|(i, (&c, &b))| (c, b, i as i32))
            .collect();
        build_vlc(&entries)
    })
}

pub fn dc_chroma_vlc() -> &'static Vlc {
    DC_CHROMA_VLC.get_or_init(|| {
        let entries: Vec<(u16, u8, i32)> = DC_CHROMA_CODE
            .iter()
            .zip(DC_CHROMA_BITS.iter())
            .enumerate()
            .map(|(i, (&c, &b))| (c, b, i as i32))
            .collect();
        build_vlc(&entries)
    })
}

pub fn mv_vlc() -> &'static Vlc {
    MV_VLC.get_or_init(|| {
        let entries: Vec<(u16, u8, i32)> = MV_TABLE
            .iter()
            .enumerate()
            .map(|(i, &(c, b))| (c, b, i as i32))
            .collect();
        build_vlc(&entries)
    })
}

pub fn mb_incr_vlc() -> &'static Vlc {
    MB_INCR_VLC.get_or_init(|| build_index_vlc(&MB_ADDR_INCR))
}

pub fn mb_pat_vlc() -> &'static Vlc {
    MB_PAT_VLC.get_or_init(|| build_index_vlc(&MB_PAT))
}

pub fn mb_ptype_vlc() -> &'static Vlc {
    MB_PTYPE_VLC.get_or_init(|| build_vlc(&MB_PTYPE))
}

pub fn mb_btype_vlc() -> &'static Vlc {
    MB_BTYPE_VLC.get_or_init(|| build_vlc(&MB_BTYPE))
}

pub fn rl_mpeg1_vlc() -> &'static Vlc {
    RL_MPEG1_VLC.get_or_init(|| build_index_vlc(&MPEG1_VLC_TABLE))
}

pub fn rl_mpeg2_vlc() -> &'static Vlc {
    RL_MPEG2_VLC.get_or_init(|| build_index_vlc(&MPEG2_VLC_TABLE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;

    #[test]
    fn zigzag_starts_dc_then_ac() {
        assert_eq!(ZIGZAG_DIRECT[0], 0);
        assert_eq!(ZIGZAG_DIRECT[1], 1);
        assert_eq!(ZIGZAG_DIRECT[2], 8);
        assert_eq!(ZIGZAG_DIRECT.len(), 64);
    }

    #[test]
    fn dc_lum_vlc_decodes_sizes() {
        let mut br = BitReader::new(&[0x60]); // 0110 0000 -> "01" is size 2
        assert_eq!(br.decode_vlc(dc_lum_vlc()).unwrap(), 2);
    }

    #[test]
    fn mb_incr_one_is_single_one_bit() {
        let mut br = BitReader::new(&[0x80]);
        assert_eq!(br.decode_vlc(mb_incr_vlc()).unwrap(), 0); // increment 1
    }

    #[test]
    fn run_level_tables_match_ffmpeg_length() {
        assert_eq!(RL_LEVEL.len(), 111);
        assert_eq!(RL_RUN.len(), 111);
        assert_eq!(MPEG1_DEFAULT_INTRA_MATRIX[0], 8);
    }
}
