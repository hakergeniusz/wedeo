//! Unit tests using synthetic Ogg pages.

use std::io::SeekFrom;

use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_format::demuxer::{PROBE_SCORE_MAX, ProbeData};
use wedeo_format::io::{BufferedIo, IoContext};
use wedeo_format::registry::DemuxerFactory;

use crate::crc::page_crc;
use crate::demuxer::OggDemuxerFactory;

/// In-memory I/O context for synthetic streams.
struct MemIo {
    data: Vec<u8>,
    pos: usize,
}

impl IoContext for MemIo {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = (self.data.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(p) => p as usize,
            SeekFrom::Current(p) => (self.pos as i64 + p).max(0) as usize,
            SeekFrom::End(p) => (self.data.len() as i64 + p).max(0) as usize,
        };
        Ok(self.pos as u64)
    }
    fn tell(&mut self) -> Result<u64> {
        Ok(self.pos as u64)
    }
    fn size(&mut self) -> Result<u64> {
        Ok(self.data.len() as u64)
    }
    fn is_seekable(&self) -> bool {
        true
    }
}

/// Build a complete Ogg page with a valid CRC.
fn make_page(
    version: u8,
    flags: u8,
    granule: u64,
    serial: u32,
    seq: u32,
    payload: &[u8],
) -> Vec<u8> {
    // Split the payload into 255-byte segments terminated by a short segment.
    let mut segments = Vec::new();
    let mut remaining = payload.len();
    let mut off = 0;
    while remaining > 0 {
        let n = remaining.min(255);
        segments.push(n as u8);
        off += n;
        remaining -= n;
    }
    if segments.is_empty() {
        segments.push(0);
    }

    let mut page = Vec::new();
    page.extend_from_slice(b"OggS");
    page.push(version);
    page.push(flags);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
    page.push(segments.len() as u8);
    page.extend_from_slice(&segments);
    page.extend_from_slice(&payload[..off]);

    let crc = page_crc(&page);
    page[22..26].copy_from_slice(&crc.to_be_bytes());
    page
}

/// A synthetic FLAC STREAMINFO header packet (51 bytes).
fn flac_streaminfo() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"\x7fFLAC");
    p.push(1); // major version
    p.push(0); // minor version
    p.extend_from_slice(&1u16.to_be_bytes()); // header count
    p.extend_from_slice(b"fLaC");
    p.extend_from_slice(&34u32.to_be_bytes()); // METADATA_BLOCK_HEADER length
    let mut si = vec![0u8; 34];
    // STREAMINFO: min/max blocksize, min/max framesize, then 20 bits of
    // sample rate (44.1 kHz), channels, bps and total samples.
    si[10] = 0x0a;
    si[11] = 0xc4;
    si[12] = 0x40; // 44100 << 4
    p.extend_from_slice(&si);
    p
}

fn buffered_io(data: Vec<u8>) -> BufferedIo {
    BufferedIo::new(Box::new(MemIo { data, pos: 0 }))
}

fn vorbis_id_header() -> Vec<u8> {
    let mut p = vec![1u8];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(2); // channels
    p.extend_from_slice(&44100u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // max bitrate
    p.extend_from_slice(&128000u32.to_le_bytes()); // nominal bitrate
    p.extend_from_slice(&0u32.to_le_bytes()); // min bitrate
    p.push(0xb8); // bs0=8, bs1=11
    p.push(1); // framing flag
    p
}

#[test]
fn crc_and_page_layout() {
    let payload = flac_streaminfo();
    let mut page = make_page(0, 2, 0, 0x1234_5678, 0, &payload);
    assert_eq!(&page[0..4], b"OggS");
    assert_eq!(page[4], 0); // version
    assert_eq!(page[5], 2); // BOS flag
    assert_eq!(u64::from_le_bytes(page[6..14].try_into().unwrap()), 0);
    assert_eq!(
        u32::from_le_bytes(page[14..18].try_into().unwrap()),
        0x1234_5678
    );
    assert_eq!(u32::from_le_bytes(page[18..22].try_into().unwrap()), 0);
    let stored = u32::from_be_bytes(page[22..26].try_into().unwrap());
    page[22..26].copy_from_slice(&[0, 0, 0, 0]);
    assert_eq!(page_crc(&page), stored);
}

#[test]
fn probe_detects_ogg() {
    let factory = OggDemuxerFactory;
    let good = ProbeData {
        filename: "x.ogg",
        buf: b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00",
    };
    assert_eq!(factory.probe(&good), PROBE_SCORE_MAX);
    let bad_version = ProbeData {
        filename: "x.ogg",
        buf: b"OggS\x09\x02\x00\x00\x00\x00\x00\x00\x00\x00",
    };
    assert_eq!(factory.probe(&bad_version), 0);
    assert!(factory.create().is_ok());
}

#[test]
fn vorbis_identification_header_parsing() {
    let id = vorbis_id_header();
    assert_eq!(id.len(), 30);
    assert_eq!(&id[1..7], b"vorbis");
    // blocksize byte and framing flag live at bytes 28 and 29.
    assert_eq!(id[28], 0xb8);
    assert_eq!(id[29], 1);
}

#[test]
fn synthetic_flac_stream_demux() {
    let serial = 0xabcd_1234;
    let mut data = Vec::new();
    data.extend(make_page(0, 2, 0, serial, 0, &flac_streaminfo()));
    data.extend(make_page(0, 0, 0, serial, 1, b"\x04comment"));

    // One data page with a synthetic FLAC frame (sync 0xfff8, blocksize 256).
    let frame = [0xff, 0xf8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    data.extend(make_page(0, 4, 256, serial, 2, &frame));

    let mut io = buffered_io(data);
    let mut demuxer = OggDemuxerFactory.create().unwrap();
    let header = demuxer.read_header(&mut io).unwrap();
    assert_eq!(header.streams.len(), 1);
    assert_eq!(header.streams[0].codec_params.codec_id, CodecId::Flac);
    assert_eq!(header.streams[0].codec_params.sample_rate, 44_100);
    assert_eq!(header.streams[0].codec_params.extradata.len(), 34);

    let pkt = demuxer.read_packet(&mut io).unwrap();
    assert_eq!(pkt.stream_index, 0);
    assert_eq!(pkt.data.data(), &frame);
    assert!(matches!(demuxer.read_packet(&mut io), Err(Error::Eof)));
}

#[test]
fn packet_assembly_across_pages() {
    // A 300-byte packet split into a 255-byte segment on the first page and a
    // 45-byte continuation on the second (flagged CONT).
    let mut first = vec![0u8; 255];
    // 0xff makes the FLAC mapping treat this as a data packet.
    first[0] = 0xff;
    let second = vec![0x42u8; 45];
    let serial = 7;
    let mut data = Vec::new();
    data.extend(make_page(0, 2, 0, serial, 0, &flac_streaminfo()));
    data.extend(make_page(0, 0, 0, serial, 1, b"\x04comment"));
    // Explicitly build the two continuation pages (255 lacing value then 45).
    data.extend(make_continuation_page(serial, 2, 0, &[255], &first));
    data.extend(make_continuation_page(serial, 3, 0, &[45], &second));

    let mut io = buffered_io(data);
    let mut demuxer = OggDemuxerFactory.create().unwrap();
    demuxer.read_header(&mut io).unwrap();
    let pkt = demuxer.read_packet(&mut io).unwrap();
    let expected: Vec<u8> = first.iter().chain(second.iter()).copied().collect();
    assert_eq!(pkt.data.data(), expected.as_slice());
}

/// Build a page with explicit lacing values (for continuation tests).
fn make_continuation_page(
    serial: u32,
    seq: u32,
    flags: u8,
    lacing: &[u8],
    payload: &[u8],
) -> Vec<u8> {
    let mut page = Vec::new();
    page.extend_from_slice(b"OggS");
    page.push(0); // version
    page.push(flags);
    page.extend_from_slice(&0u64.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0, 0, 0, 0]);
    page.push(lacing.len() as u8);
    page.extend_from_slice(lacing);
    page.extend_from_slice(payload);
    let crc = page_crc(&page);
    page[22..26].copy_from_slice(&crc.to_be_bytes());
    page
}

#[test]
fn invalid_crc_page_is_rejected() {
    let mut page = make_page(0, 2, 0, 1, 0, &flac_streaminfo());
    page[22] ^= 0xff; // corrupt the stored CRC
    let mut io = buffered_io(page);
    let mut demuxer = OggDemuxerFactory.create().unwrap();
    // No valid page can be assembled, so header parsing must fail.
    assert!(demuxer.read_header(&mut io).is_err());
}
