//! FLAC STREAMINFO / metadata parsing, ported from FFmpeg `libavcodec/flac.c`
//! and the header handling in `flacdec.c`.

use wedeo_core::error::{Error, Result};

use crate::bitstream::BitReader;

/// FLAC stream information (34-byte STREAMINFO block).
#[derive(Clone, Debug, Default)]
pub struct StreamInfo {
    pub min_blocksize: u16,
    pub max_blocksize: u16,
    pub min_framesize: u32,
    pub max_framesize: u32,
    pub samplerate: u32,
    pub channels: u32,
    pub bps: u32,
    pub samples: u64,
    pub md5: [u8; 16],
}

impl StreamInfo {
    /// Parse a 34-byte STREAMINFO block (`ff_flac_parse_streaminfo`).
    pub fn parse(buffer: &[u8]) -> Result<Self> {
        if buffer.len() < 34 {
            return Err(Error::InvalidData);
        }
        let mut gb = BitReader::new(&buffer[..34]);
        let min_blocksize = gb.get_bits(16) as u16;
        let max_blocksize = gb.get_bits(16) as u16;
        if max_blocksize < 16 {
            // FFmpeg warns about an invalid max blocksize, clamps it to 16 and
            // reports invalid data (so the streaminfo is not applied).
            let _ = min_blocksize;
            return Err(Error::InvalidData);
        }
        let min_framesize = gb.get_bits(24);
        let max_framesize = gb.get_bits(24);
        let samplerate = gb.get_bits(20);
        let channels = gb.get_bits(3) + 1;
        let bps = gb.get_bits(5) + 1;
        if bps < 4 {
            // FFmpeg warns, clamps to 16 and reports invalid data.
            let _ = (min_framesize, max_framesize, samplerate, channels);
            return Err(Error::InvalidData);
        }
        let samples = gb.get_bits64(36);
        let mut md5 = [0u8; 16];
        for byte in &mut md5 {
            *byte = gb.get_bits(8) as u8;
        }
        Ok(Self {
            min_blocksize,
            max_blocksize,
            min_framesize,
            max_framesize,
            samplerate,
            channels,
            bps,
            samples,
            md5,
        })
    }
}

/// Parse the metadata-block header (`flac_parse_block_header`).
fn parse_block_header(buf: &[u8]) -> Option<(bool, u32, usize)> {
    if buf.len() < 4 {
        return None;
    }
    let last = buf[0] & 0x80 != 0;
    let kind = (buf[0] & 0x7F) as u32;
    let size = ((buf[1] as usize) << 16) | ((buf[2] as usize) << 8) | buf[3] as usize;
    Some((last, kind, size))
}

/// Find the STREAMINFO block in raw FLAC metadata (starting with "fLaC").
/// Returns the parsed streaminfo and the total size of the metadata header,
/// mirroring `parse_streaminfo` + `get_metadata_size` in flacdec.c.
pub fn parse_raw_header(buf: &[u8]) -> Result<(StreamInfo, usize)> {
    if buf.len() < 8 || &buf[..4] != b"fLaC" {
        return Err(Error::InvalidData);
    }
    let mut pos = 4usize;
    let mut streaminfo = None;
    loop {
        let (last, kind, size) = parse_block_header(buf.get(pos..).ok_or(Error::InvalidData)?)
            .ok_or(Error::InvalidData)?;
        pos += 4;
        if buf.len() < pos.saturating_add(size) {
            return Err(Error::InvalidData);
        }
        if kind == 0 {
            if size != 34 {
                return Err(Error::InvalidData);
            }
            streaminfo = Some(StreamInfo::parse(&buf[pos..pos + 34])?);
        }
        pos += size;
        if last {
            break;
        }
    }
    match streaminfo {
        Some(si) => Ok((si, pos)),
        None => Err(Error::InvalidData),
    }
}

/// Extract STREAMINFO from demuxer extradata. FFmpeg's
/// `ff_flac_is_extradata_valid`: either 34 bytes of raw STREAMINFO, or an
/// "fLaC" header followed by metadata blocks.
pub fn parse_extradata(extradata: &[u8]) -> Result<StreamInfo> {
    if extradata.len() < 34 {
        return Err(Error::InvalidData);
    }
    if &extradata[..4] == b"fLaC" {
        Ok(parse_raw_header(extradata)?.0)
    } else {
        StreamInfo::parse(&extradata[..34])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid STREAMINFO: 4096-sample blocks, 44100 Hz, stereo, 16 bps.
    fn streaminfo_bytes() -> Vec<u8> {
        let mut v = vec![0u8; 34];
        v[2..4].copy_from_slice(&4096u16.to_be_bytes()); // max blocksize
        // offset 10..14: sample rate (20 bits) + channels (3) + bps (5)
        let bits: u64 = (44100u64 << 8) | (((2 - 1) as u64) << 5) | ((16 - 1) as u64);
        v[10] = (bits >> 20) as u8;
        v[11] = (bits >> 12) as u8;
        v[12] = (bits >> 4) as u8;
        v[13] = (bits << 4) as u8;
        v
    }

    #[test]
    fn parse_streaminfo_fields() {
        let si = StreamInfo::parse(&streaminfo_bytes()).unwrap();
        assert_eq!(si.max_blocksize, 4096);
        assert_eq!(si.samplerate, 44100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.bps, 16);
    }

    #[test]
    fn parse_raw_header_with_padding() {
        let mut data = b"fLaC".to_vec();
        // Last metadata block, type 0 (STREAMINFO), size 34.
        data.push(0x80);
        data.extend_from_slice(&34u32.to_be_bytes()[1..]);
        data.extend_from_slice(&streaminfo_bytes());
        let (si, size) = parse_raw_header(&data).unwrap();
        assert_eq!(si.samplerate, 44100);
        assert_eq!(size, data.len());
    }
}
