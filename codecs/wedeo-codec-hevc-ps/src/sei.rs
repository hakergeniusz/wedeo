//! Minimal SEI message framing (`decode_nal_sei_message` in FFmpeg
//! `libavcodec/hevc/sei.c`).
//!
//! Parameter sets and slice headers are the focus of this crate; SEI payloads
//! are exposed as raw bytes with a typed payload id so later HEVC work units
//! can decode the ones they need (picture timing, recovery point, hashes).

use wedeo_core::{Error, Result};

use crate::nal::ebsp_to_rbsp;

/// SEI payload types used by HEVC (`SEI_TYPE_*` in `libavcodec/sei.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SeiPayloadType {
    BufferingPeriod = 0,
    PicTiming = 1,
    PanScanRect = 2,
    Fill = 3,
    UserDataRegisteredItu = 4,
    UserDataUnregistered = 5,
    RecoveryPoint = 6,
    DecodingUnitsInfo = 7,
    ScalableNesting = 8,
    ActiveParameterSets = 9,
    DecodedPictureHash = 132,
    /// A payload type not explicitly named here.
    Other(u32),
}

impl SeiPayloadType {
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => Self::BufferingPeriod,
            1 => Self::PicTiming,
            2 => Self::PanScanRect,
            3 => Self::Fill,
            4 => Self::UserDataRegisteredItu,
            5 => Self::UserDataUnregistered,
            6 => Self::RecoveryPoint,
            7 => Self::DecodingUnitsInfo,
            8 => Self::ScalableNesting,
            9 => Self::ActiveParameterSets,
            132 => Self::DecodedPictureHash,
            other => Self::Other(other),
        }
    }

    pub fn code(self) -> u32 {
        match self {
            Self::BufferingPeriod => 0,
            Self::PicTiming => 1,
            Self::PanScanRect => 2,
            Self::Fill => 3,
            Self::UserDataRegisteredItu => 4,
            Self::UserDataUnregistered => 5,
            Self::RecoveryPoint => 6,
            Self::DecodingUnitsInfo => 7,
            Self::ScalableNesting => 8,
            Self::ActiveParameterSets => 9,
            Self::DecodedPictureHash => 132,
            Self::Other(c) => c,
        }
    }
}

/// A single SEI message: payload type, size and raw payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeiMessage {
    pub payload_type: SeiPayloadType,
    pub payload_size: usize,
    pub payload: Vec<u8>,
}

/// Parse the SEI messages in an SEI NAL payload (EBSP).
///
/// The `0xFF` continuation-byte encoding for `payloadType` and `payloadSize`
/// follows `decode_nal_sei_message()` in FFmpeg.
pub fn parse_sei_message(ebsp: &[u8]) -> Result<Vec<SeiMessage>> {
    let rbsp = ebsp_to_rbsp(ebsp);
    let mut pos = 0usize;
    let mut messages = Vec::new();
    // The SEI RBSP ends with rbsp_trailing_bits (a byte containing the stop
    // bit, typically 0x80). Stop when fewer than two bytes remain.
    while pos + 2 <= rbsp.len() {
        let mut payload_type = 0u32;
        let mut byte = 0xffu8;
        while byte == 0xff {
            if pos >= rbsp.len() {
                return Err(Error::InvalidData);
            }
            byte = rbsp[pos];
            pos += 1;
            payload_type = payload_type
                .checked_add(byte as u32)
                .ok_or(Error::InvalidData)?;
        }
        let mut payload_size = 0usize;
        byte = 0xff;
        while byte == 0xff {
            if pos >= rbsp.len() {
                return Err(Error::InvalidData);
            }
            byte = rbsp[pos];
            pos += 1;
            payload_size = payload_size
                .checked_add(byte as usize)
                .ok_or(Error::InvalidData)?;
        }
        if pos + payload_size > rbsp.len() {
            return Err(Error::InvalidData);
        }
        messages.push(SeiMessage {
            payload_type: SeiPayloadType::from_code(payload_type),
            payload_size,
            payload: rbsp[pos..pos + payload_size].to_vec(),
        });
        pos += payload_size;
    }
    Ok(messages)
}

#[cfg(test)]
mod sei_tests {
    use super::*;

    #[test]
    fn parses_two_messages() {
        // msg1: type=6, size=2, payload=[0xAA,0xBB]; msg2: type=1, size=1, [0x80]; trailing 0x80
        let ebsp = [6, 2, 0xAA, 0xBB, 1, 1, 0x80, 0x80];
        let msgs = parse_sei_message(&ebsp).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].payload_type, SeiPayloadType::RecoveryPoint);
        assert_eq!(msgs[0].payload, vec![0xAA, 0xBB]);
        assert_eq!(msgs[1].payload_type, SeiPayloadType::PicTiming);
        assert_eq!(msgs[1].payload, vec![0x80]);
    }

    #[test]
    fn parses_extended_type() {
        // type = 255 + 1 = 256, size = 1
        let ebsp = [0xFF, 1, 1, 0x42, 0x80];
        let msgs = parse_sei_message(&ebsp).unwrap();
        assert_eq!(msgs[0].payload_type, SeiPayloadType::Other(256));
    }
}
