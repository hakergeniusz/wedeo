//! MPEG-TS constants, copied verbatim from FFmpeg's `libavformat/mpegts.h`
//! and `libavformat/mpegts.c`.
//!
//! The full FFmpeg constant tables are kept verbatim (rather than pruned to
//! the handful this demuxer currently maps) so ports of additional stream
//! types can reference the same names.
#![allow(dead_code)]

/// `TS_FEC_PACKET_SIZE` in `mpegts.h`.
pub const TS_FEC_PACKET_SIZE: usize = 204;
/// `TS_DVHS_PACKET_SIZE` in `mpegts.h`.
pub const TS_DVHS_PACKET_SIZE: usize = 192;
/// `TS_PACKET_SIZE` in `mpegts.h`.
pub const TS_PACKET_SIZE: usize = 188;
/// `TS_MAX_PACKET_SIZE` in `mpegts.h`.
pub const TS_MAX_PACKET_SIZE: usize = 204;
/// `NB_PID_MAX` in `mpegts.h`.
pub const NB_PID_MAX: usize = 8192;
/// `MAX_SECTION_SIZE` in `mpegts.h`.
pub const MAX_SECTION_SIZE: usize = 4096;
/// `SYNC_BYTE` in `mpegts.h`.
pub const SYNC_BYTE: u8 = 0x47;
/// `STUFFING_BYTE` in `mpegts.h`.
pub const STUFFING_BYTE: u8 = 0xFF;
/// `SYSTEM_CLOCK_FREQUENCY_DIVISOR` in `mpegts.h`: convert 27 MHz to 90 kHz.
pub const SYSTEM_CLOCK_FREQUENCY_DIVISOR: i64 = 300;

/// `PAT_PID` in `mpegts.h` (Program Association Table).
pub const PAT_PID: u16 = 0x0000;
/// `SDT_PID` in `mpegts.h` (Service Description Table).
pub const SDT_PID: u16 = 0x0011;
/// `NULL_PID` in `mpegts.h` (null packet used for fixed bandwidth padding).
pub const NULL_PID: u16 = 0x1FFF;

/// `PAT_TID` in `mpegts.h`: Program Association section.
pub const PAT_TID: u8 = 0x00;
/// `PMT_TID` in `mpegts.h`: Program Map section.
pub const PMT_TID: u8 = 0x02;

/// `MAX_RESYNC_SIZE`: maximum size in which we look for synchronization if
/// synchronization is lost (FFmpeg `mpegts.c`).
pub const MAX_RESYNC_SIZE: usize = 65536;

/// `PES_START_SIZE`: enough for the 6-byte MPEG-1 PES prefix.
pub const PES_START_SIZE: usize = 6;
/// `PES_HEADER_SIZE`: enough for the 9-byte MPEG-2 PES header.
pub const PES_HEADER_SIZE: usize = 9;
/// `MAX_PES_HEADER_SIZE` in `mpegts.c` (9 + 255).
pub const MAX_PES_HEADER_SIZE: usize = 9 + 255;

/// ISO/IEC 13818-1 Table 2-34 stream types (FFmpeg `mpegts.h`).
pub mod stream_type {
    pub const VIDEO_MPEG1: u8 = 0x01;
    pub const VIDEO_MPEG2: u8 = 0x02;
    pub const AUDIO_MPEG1: u8 = 0x03;
    pub const AUDIO_MPEG2: u8 = 0x04;
    pub const PRIVATE_SECTION: u8 = 0x05;
    pub const PRIVATE_DATA: u8 = 0x06;
    pub const AUDIO_AAC: u8 = 0x0f;
    pub const AUDIO_AAC_LATM: u8 = 0x11;
    pub const VIDEO_MPEG4: u8 = 0x10;
    pub const ISO_IEC_14496_PES: u8 = 0x12;
    pub const ISO_IEC_14496_SECTION: u8 = 0x13;
    pub const METADATA: u8 = 0x15;
    pub const VIDEO_H264: u8 = 0x1b;
    pub const AUDIO_MPEG4: u8 = 0x1c;
    pub const VIDEO_MVC: u8 = 0x20;
    pub const VIDEO_HEVC: u8 = 0x24;
    pub const VIDEO_JPEGXS: u8 = 0x32;
    pub const VIDEO_VVC: u8 = 0x33;
    pub const VIDEO_CAVS: u8 = 0x42;
    pub const VIDEO_AVS2: u8 = 0xd2;
    pub const VIDEO_AVS3: u8 = 0xd4;
    pub const VIDEO_VC1: u8 = 0xea;
    pub const VIDEO_DIRAC: u8 = 0xd1;
    /// ATSC A/52 (`MISC_types` in `mpegts.c`).
    pub const ATSC_AUDIO_AC3: u8 = 0x81;
    pub const ATSC_AUDIO_EAC3: u8 = 0x87;
    /// HLS sample-encryption types (`HLS_SAMPLE_ENC_types` in `mpegts.c`).
    pub const HLS_SE_VIDEO_H264: u8 = 0xdb;
    pub const HLS_SE_AUDIO_AAC: u8 = 0xcf;
    pub const HLS_SE_AUDIO_AC3: u8 = 0xc1;
    pub const HLS_SE_AUDIO_EAC3: u8 = 0xc2;
}

/// ISO/IEC 13818-1 Table 2-22 stream ids (FFmpeg `mpegts.h`).
pub mod stream_id {
    pub const PROGRAM_STREAM_MAP: u8 = 0xbc;
    pub const PRIVATE_STREAM_1: u8 = 0xbd;
    pub const PADDING_STREAM: u8 = 0xbe;
    pub const PRIVATE_STREAM_2: u8 = 0xbf;
    pub const AUDIO_STREAM_0: u8 = 0xc0;
    pub const VIDEO_STREAM_0: u8 = 0xe0;
    pub const ECM_STREAM: u8 = 0xf0;
    pub const EMM_STREAM: u8 = 0xf1;
    pub const DSMCC_STREAM: u8 = 0xf2;
    pub const TYPE_E_STREAM: u8 = 0xf8;
    pub const METADATA_STREAM: u8 = 0xfc;
    pub const EXTENDED_STREAM_ID: u8 = 0xfd;
    pub const PROGRAM_STREAM_DIRECTORY: u8 = 0xff;
}

/// Descriptor tags from FFmpeg `mpegts.h`.
pub mod descriptor {
    pub const REGISTRATION: u8 = 0x05;
    pub const ISO_639_LANGUAGE: u8 = 0x0a;
    pub const AC3: u8 = 0x6a;
    pub const ENHANCED_AC3: u8 = 0x7a;
    pub const DTS: u8 = 0x7b;
}
