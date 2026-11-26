//! Ogg page writer.
//!
//! Ported from the page-flush path of FFmpeg's `libavformat/oggenc.c` and the
//! page layout in the Ogg container specification.
//!
//! An Ogg page is:
//!
//! ```text
//! "OggS" (4) | version (1) | flags (1) | granule position (8, LE)
//! serial (4, LE) | sequence (4, LE) | CRC (4) | segments (1) |
//! lacing values (nsegs) | body
//! ```
//!
//! The CRC field is stored big-endian by this implementation, matching the
//! demuxer in this crate and `crate::crc::page_crc` (which returns the
//! byte-swapped `AV_CRC_32_IEEE` value FFmpeg uses). The four on-disk bytes
//! are also exactly the little-endian encoding of the standard Ogg CRC-32,
//! so the pages are interoperable with other Ogg implementations.

use wedeo_core::error::{Error, Result};
use wedeo_format::io::BufferedIo;

use crate::crc::page_crc;
use crate::mapping::OGG_NOGRANULE_VALUE;

pub(crate) const PAGE_FLAG_CONT: u8 = 0x01;
pub(crate) const PAGE_FLAG_BOS: u8 = 0x02;
pub(crate) const PAGE_FLAG_EOS: u8 = 0x04;

/// Maximum lacing values (and therefore segments) in one page.
pub(crate) const MAX_SEGMENTS: usize = 255;
/// Maximum page size, including the 27-byte header and the segment table.
pub(crate) const MAX_PAGE_SIZE: usize = 27 + MAX_SEGMENTS + MAX_SEGMENTS * 255;

/// Split a packet length into Ogg lacing values.
///
/// A packet whose length is an exact multiple of 255 ends with a zero lacing
/// value; without it the final 255-byte chunk would look like a continuation
/// to a demuxer. See `ogg_stream_packetin()` in `libogg/src/stream.c`.
pub(crate) fn lacing_values(packet_len: usize) -> Vec<u8> {
    let mut values = Vec::with_capacity(packet_len / 255 + 1);
    let mut remaining = packet_len;
    while remaining >= 255 {
        values.push(255);
        remaining -= 255;
    }
    values.push(remaining as u8);
    values
}

/// Incremental builder for one logical Ogg bitstream.
pub(crate) struct PageBuilder {
    serial: u32,
    sequence: u32,
    /// The next flushed page is the beginning of stream.
    first_page: bool,
    /// The next flushed page starts in the middle of a packet.
    continued: bool,
    segments: Vec<u8>,
    body: Vec<u8>,
    /// Granule position of the last packet that ended on the current page.
    /// `None` when no packet ends on this page (the page then carries -1).
    last_granule: Option<u64>,
}

impl PageBuilder {
    pub(crate) fn new(serial: u32) -> Self {
        Self {
            serial,
            sequence: 0,
            first_page: true,
            continued: false,
            segments: Vec::with_capacity(MAX_SEGMENTS),
            body: Vec::with_capacity(MAX_SEGMENTS * 255),
            last_granule: None,
        }
    }

    /// Whether the page is full. Packets must be split before adding more.
    pub(crate) fn is_full(&self) -> bool {
        self.segments.len() >= MAX_SEGMENTS
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Current page body size in bytes (FFmpeg's `page.size`).
    pub(crate) fn body_len(&self) -> usize {
        self.body.len()
    }

    /// Flag the next page as starting with a continuation of the current
    /// packet. Called after flushing a page in the middle of a packet.
    pub(crate) fn set_continued(&mut self, continued: bool) {
        self.continued = continued;
    }

    /// Record the granule position of the last completed packet on this page.
    pub(crate) fn set_granule(&mut self, granule: u64) {
        self.last_granule = Some(granule);
    }

    /// Append one piece of a packet to the current page.
    ///
    /// `lacing` holds the segment sizes and `body` the matching bytes. Pass
    /// `granule` only when the piece completes a packet; the muxer does this
    /// through [`PageBuilder::set_granule`] instead.
    pub(crate) fn add(&mut self, lacing: &[u8], body: &[u8]) -> Result<()> {
        if lacing.is_empty() || lacing.len() + self.segments.len() > MAX_SEGMENTS {
            return Err(Error::InvalidArgument);
        }
        let expected: usize = lacing.iter().map(|&v| usize::from(v)).sum();
        if expected != body.len() {
            return Err(Error::InvalidArgument);
        }
        self.segments.extend_from_slice(lacing);
        self.body.extend_from_slice(body);
        Ok(())
    }

    /// Write the current page and reset the builder for the next one.
    pub(crate) fn flush(&mut self, io: &mut BufferedIo, eos: bool) -> Result<()> {
        let mut flags = 0u8;
        if self.first_page {
            flags |= PAGE_FLAG_BOS;
        }
        if self.continued {
            flags |= PAGE_FLAG_CONT;
        }
        if eos {
            flags |= PAGE_FLAG_EOS;
        }
        let granule = self.last_granule.unwrap_or(OGG_NOGRANULE_VALUE);
        let nsegs = self.segments.len();
        if nsegs > MAX_SEGMENTS {
            return Err(Error::Bug);
        }
        debug_assert!(27 + nsegs + self.body.len() <= MAX_PAGE_SIZE);

        let mut page = Vec::with_capacity(27 + nsegs + self.body.len());
        page.extend_from_slice(b"OggS");
        page.push(0); // stream version
        page.push(flags);
        page.extend_from_slice(&granule.to_le_bytes());
        page.extend_from_slice(&self.serial.to_le_bytes());
        page.extend_from_slice(&self.sequence.to_le_bytes());
        page.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
        page.push(nsegs as u8);
        page.extend_from_slice(&self.segments);
        page.extend_from_slice(&self.body);

        let crc = page_crc(&page);
        page[22..26].copy_from_slice(&crc.to_be_bytes());
        io.write_bytes(&page)?;

        self.sequence = self.sequence.wrapping_add(1);
        self.first_page = false;
        self.continued = false;
        self.segments.clear();
        self.body.clear();
        self.last_granule = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;
    use std::sync::{Arc, Mutex};

    use wedeo_core::error::Result;
    use wedeo_format::io::{BufferedIo, IoContext};

    use super::*;

    struct MemoryIo(Arc<Mutex<Vec<u8>>>);

    impl IoContext for MemoryIo {
        fn read(&mut self, _buf: &mut [u8]) -> Result<usize> {
            Ok(0)
        }
        fn write(&mut self, buf: &[u8]) -> Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn seek(&mut self, _pos: SeekFrom) -> Result<u64> {
            Ok(0)
        }
        fn tell(&mut self) -> Result<u64> {
            Ok(0)
        }
        fn size(&mut self) -> Result<u64> {
            Ok(self.0.lock().unwrap().len() as u64)
        }
        fn is_seekable(&self) -> bool {
            false
        }
    }

    fn flush_page(serial: u32, granule: Option<u64>, eos: bool, body: &[u8]) -> Vec<u8> {
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut io = BufferedIo::new(Box::new(MemoryIo(Arc::clone(&shared))));
        let mut builder = PageBuilder::new(serial);
        let lacing = lacing_values(body.len());
        builder.add(&lacing, body).unwrap();
        if let Some(granule) = granule {
            builder.set_granule(granule);
        }
        builder.flush(&mut io, eos).unwrap();
        io.flush().unwrap();
        shared.lock().unwrap().clone()
    }

    #[test]
    fn page_header_layout_and_crc() {
        let body = b"\x01vorbis header";
        let page = flush_page(0x1234_5678, Some(0), false, body);
        assert_eq!(&page[..4], b"OggS");
        assert_eq!(page[4], 0);
        assert_eq!(page[5], PAGE_FLAG_BOS);
        assert_eq!(u64::from_le_bytes(page[6..14].try_into().unwrap()), 0);
        assert_eq!(
            u32::from_le_bytes(page[14..18].try_into().unwrap()),
            0x1234_5678
        );
        assert_eq!(u32::from_le_bytes(page[18..22].try_into().unwrap()), 0);
        assert_eq!(page[26], 1);
        assert_eq!(page[27], body.len() as u8);
        assert_eq!(&page[28..], body);
        assert_eq!(page.len(), 27 + 1 + body.len());

        let stored = u32::from_be_bytes(page[22..26].try_into().unwrap());
        let mut zeroed = page.clone();
        zeroed[22..26].fill(0);
        assert_eq!(page_crc(&zeroed), stored);
    }

    #[test]
    fn page_flags_and_no_granule() {
        let page = flush_page(7, None, true, b"x");
        assert_eq!(page[5], PAGE_FLAG_BOS | PAGE_FLAG_EOS);
        assert_eq!(
            u64::from_le_bytes(page[6..14].try_into().unwrap()),
            OGG_NOGRANULE_VALUE
        );
    }

    #[test]
    fn continuation_flag_is_set_on_next_page() {
        let shared = Arc::new(Mutex::new(Vec::new()));
        let mut io = BufferedIo::new(Box::new(MemoryIo(Arc::clone(&shared))));
        let mut builder = PageBuilder::new(1);
        builder.add(&[3], b"abc").unwrap();
        builder.flush(&mut io, false).unwrap();
        builder.set_continued(true);
        builder.add(&[3], b"def").unwrap();
        builder.set_granule(9);
        builder.flush(&mut io, true).unwrap();
        io.flush().unwrap();
        let bytes = shared.lock().unwrap().clone();

        let first_len = 27 + 1 + 3;
        assert_eq!(bytes[5], PAGE_FLAG_BOS);
        assert_eq!(&bytes[28..31], b"abc");
        assert_eq!(bytes[first_len + 5], PAGE_FLAG_CONT | PAGE_FLAG_EOS);
        assert_eq!(bytes[first_len + 26], 1);
        assert_eq!(&bytes[first_len + 28..], b"def");
        assert_eq!(
            u64::from_le_bytes(bytes[first_len + 6..first_len + 14].try_into().unwrap()),
            9
        );
    }

    #[test]
    fn lacing_ends_multiples_of_255_with_zero() {
        assert_eq!(lacing_values(0), vec![0]);
        assert_eq!(lacing_values(1), vec![1]);
        assert_eq!(lacing_values(254), vec![254]);
        assert_eq!(lacing_values(255), vec![255, 0]);
        assert_eq!(lacing_values(256), vec![255, 1]);
        assert_eq!(lacing_values(510), vec![255, 255, 0]);
        assert_eq!(lacing_values(511), vec![255, 255, 1]);
    }

    #[test]
    fn add_validates_lacing_and_capacity() {
        let mut builder = PageBuilder::new(0);
        assert!(builder.add(&[2], b"x").is_err());
        assert!(builder.add(&[], b"").is_err());
        let full = vec![255u8; MAX_SEGMENTS];
        let body = vec![0u8; MAX_SEGMENTS * 255];
        builder.add(&full, &body).unwrap();
        assert!(builder.is_full());
        assert!(builder.add(&[1], b"x").is_err());
        assert_eq!(27 + MAX_SEGMENTS + MAX_SEGMENTS * 255, MAX_PAGE_SIZE);
    }
}
