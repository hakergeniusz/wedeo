//! Vorbis audio parser - determines packet durations.
//!
//! Ported from FFmpeg's `libavcodec/vorbis_parser.c`.  The parser needs only
//! the identification header's block sizes and the setup header's mode table;
//! it does not decode audio.

use wedeo_core::error::{Error, Result};

/// Flags reported for special Vorbis packets (`vorbis_parser_internal.h`).
pub const VORBIS_FLAG_HEADER: u32 = 0x01;
pub const VORBIS_FLAG_COMMENT: u32 = 0x02;
pub const VORBIS_FLAG_SETUP: u32 = 0x04;

/// MSB-first bit reader, equivalent to FFmpeg's `GetBitContext`.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pub(crate) bitpos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, bitpos: 0 }
    }

    pub(crate) fn bits(&mut self, n: usize) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = self.bitpos / 8;
            let bit = (self.bitpos % 8) as u32;
            let b = if byte < self.data.len() {
                (self.data[byte] >> (7 - bit)) & 1
            } else {
                0
            };
            v = (v << 1) | b as u32;
            self.bitpos += 1;
        }
        v
    }

    pub(crate) fn bit(&mut self) -> u32 {
        self.bits(1)
    }

    pub(crate) fn skip(&mut self, n: usize) {
        self.bitpos += n;
    }

    pub(crate) fn left(&self) -> isize {
        self.data.len() as isize * 8 - self.bitpos as isize
    }

    pub(crate) fn count(&self) -> usize {
        self.bitpos
    }
}

/// `av_log2(v)`: floor(log2(v)), with av_log2(0) == 0 in FFmpeg.
fn av_log2(mut v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let mut n = 0;
    while v > 1 {
        v >>= 1;
        n += 1;
    }
    n
}

/// Split a Xiph-laced extradata buffer into its three headers.
///
/// Equivalent to `avpriv_split_xiph_headers(extradata, size, 30, ...)`.
/// Also used by the Ogg muxer to recover the header packets from extradata.
pub(crate) fn split_xiph_headers(
    extradata: &[u8],
    first_header_size: usize,
) -> Result<[(&[u8], usize); 3]> {
    if extradata.len() >= 6
        && u16::from_be_bytes([extradata[0], extradata[1]]) as usize == first_header_size
    {
        // Two-byte big-endian lengths.
        let mut out = [(&[][..], 0usize); 3];
        let mut pos = 0usize;
        let mut overall_len = 6usize;
        for i in 0..3 {
            if pos + 2 > extradata.len() {
                return Err(Error::InvalidData);
            }
            let len = u16::from_be_bytes([extradata[pos], extradata[pos + 1]]) as usize;
            pos += 2;
            if pos + len > extradata.len() {
                return Err(Error::InvalidData);
            }
            out[i] = (&extradata[pos..pos + len], len);
            pos += len;
            overall_len += len;
            if overall_len > extradata.len() {
                return Err(Error::InvalidData);
            }
        }
        return Ok(out);
    }

    if extradata.len() >= 3 && extradata[0] == 2 {
        // Xiph lacing.
        let mut pos = 1usize;
        let mut overall_len = 3usize;
        let mut lens = [0usize; 3];
        for i in 0..2 {
            lens[i] = 0;
            while overall_len < extradata.len() && pos < extradata.len() && extradata[pos] == 0xff {
                lens[i] += 0xff;
                overall_len += 0xff + 1;
                pos += 1;
            }
            if pos >= extradata.len() {
                return Err(Error::InvalidData);
            }
            lens[i] += extradata[pos] as usize;
            overall_len += extradata[pos] as usize;
            pos += 1;
            if overall_len > extradata.len() {
                return Err(Error::InvalidData);
            }
        }
        if overall_len > extradata.len() {
            return Err(Error::InvalidData);
        }
        lens[2] = extradata.len() - overall_len;
        let s0 = pos;
        let s1 = s0 + lens[0];
        let s2 = s1 + lens[1];
        if s2 + lens[2] > extradata.len() {
            return Err(Error::InvalidData);
        }
        return Ok([
            (&extradata[s0..s1], lens[0]),
            (&extradata[s1..s2], lens[1]),
            (&extradata[s2..s2 + lens[2]], lens[2]),
        ]);
    }

    Err(Error::InvalidData)
}

/// State of the Vorbis packet parser.
#[derive(Clone, Debug)]
pub struct VorbisParser {
    valid: bool,
    blocksize: [u32; 2],
    previous_blocksize: u32,
    mode_blocksize: [u8; 64],
    mode_count: usize,
    mode_mask: u8,
    prev_mask: u8,
}

impl VorbisParser {
    /// Initialize from Vorbis extradata (the packed identification, comment
    /// and setup headers).
    pub fn new(extradata: &[u8]) -> Result<Self> {
        let headers = split_xiph_headers(extradata, 30)?;

        // parse_id_header
        let id = headers[0].0;
        if id.len() < 30 || id[0] != 1 || &id[1..7] != b"vorbis" || id[29] & 1 == 0 {
            return Err(Error::InvalidData);
        }
        let blocksize = [1u32 << (id[28] & 0x0f), 1u32 << (id[28] >> 4)];

        // parse_setup_header
        let setup = headers[2].0;
        if setup.len() < 7 || setup[0] != 5 || &setup[1..7] != b"vorbis" {
            return Err(Error::InvalidData);
        }
        let mut rev = setup.to_vec();
        rev.reverse();

        let mut gb = BitReader::new(&rev);
        let mut got_framing_bit = 0usize;
        while gb.left() > 97 {
            if gb.bit() == 1 {
                got_framing_bit = gb.count();
                break;
            }
        }
        if got_framing_bit == 0 {
            return Err(Error::InvalidData);
        }

        let mut mode_count = 0usize;
        let mut got_mode_header = false;
        let mut last_mode_count = 0usize;
        while gb.left() >= 97 {
            let a = gb.bits(8);
            if a > 63 {
                break;
            }
            if gb.bits(16) != 0 {
                break;
            }
            if gb.bits(16) != 0 {
                break;
            }
            gb.skip(1);
            mode_count += 1;
            if mode_count > 64 {
                break;
            }
            let save = gb.count();
            if gb.bits(6) as usize + 1 == mode_count {
                got_mode_header = true;
                last_mode_count = mode_count;
            }
            // Restore the bit position: FFmpeg copies the GetBitContext `gb0`
            // and reads from the copy.
            gb.bitpos = save;
        }
        if !got_mode_header || last_mode_count > 63 || last_mode_count == 0 {
            return Err(Error::InvalidData);
        }
        mode_count = last_mode_count;

        let mode_mask = (((1u32 << (av_log2((mode_count - 1) as u32) + 1)) - 1) << 1) as u8;
        let prev_mask = (mode_mask | 0x01) + 1;

        let mut mode_blocksize = [0u8; 64];
        let mut gb = BitReader::new(&rev);
        gb.skip(got_framing_bit);
        for i in (0..mode_count).rev() {
            gb.skip(40);
            mode_blocksize[i] = gb.bit() as u8;
        }

        Ok(Self {
            valid: true,
            blocksize,
            previous_blocksize: blocksize[mode_blocksize[0] as usize],
            mode_blocksize,
            mode_count,
            mode_mask,
            prev_mask,
        })
    }

    /// Reset the parser's window-state at the start of a new page/stream.
    pub fn reset(&mut self) {
        if self.valid {
            self.previous_blocksize = self.blocksize[0];
        }
    }

    /// Return the duration in samples of `packet`, plus any special-packet
    /// flags.  Returns `Error::InvalidData` for a malformed packet.
    pub fn frame_flags(&mut self, packet: &[u8], flags: &mut u32) -> Result<i64> {
        if !self.valid || packet.is_empty() {
            return Ok(0);
        }

        if packet[0] & 1 != 0 {
            match packet[0] {
                1 => *flags |= VORBIS_FLAG_HEADER,
                3 => *flags |= VORBIS_FLAG_COMMENT,
                5 => *flags |= VORBIS_FLAG_SETUP,
                _ => {}
            }
            // Special packets have no duration.
            return Ok(0);
        }

        let mode = if self.mode_count == 1 {
            0usize
        } else {
            ((packet[0] & self.mode_mask) >> 1) as usize
        };
        if mode >= self.mode_count {
            return Err(Error::InvalidData);
        }

        let mut previous_blocksize = self.previous_blocksize;
        if self.mode_blocksize[mode] != 0 {
            let flag = usize::from(packet[0] & self.prev_mask != 0);
            previous_blocksize = self.blocksize[flag];
        }
        let current_blocksize = self.blocksize[self.mode_blocksize[mode] as usize];
        let duration = ((previous_blocksize + current_blocksize) >> 2) as i64;
        self.previous_blocksize = current_blocksize;
        Ok(duration)
    }

    /// A variant that reports no flags (equivalent to `av_vorbis_parse_frame`).
    pub fn frame(&mut self, packet: &[u8]) -> Result<i64> {
        let mut flags = 0;
        self.frame_flags(packet, &mut flags)
    }
}
