//! Unit tests for the Ogg muxer: page layout, CRC, granule positions and
//! round trips through the crate's demuxer.

use std::io::SeekFrom;
use std::sync::{Arc, Mutex};

use wedeo_codec::decoder::CodecParameters;
use wedeo_core::channel_layout::ChannelLayout;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;
use wedeo_core::rational::Rational;
use wedeo_format::demuxer::Stream;
use wedeo_format::io::{BufferedIo, IoContext};
use wedeo_format::muxer::{Muxer, OutputFormatFlags};
use wedeo_format::registry::{
    DemuxerFactory, MuxerFactory, find_muxer_by_name, guess_muxer_by_extension, muxers,
};

use crate::crc::page_crc;
use crate::demuxer::OggDemuxerFactory;
use crate::muxer::{OggMuxer, OggMuxerFactory, opus_headers, vorbis_headers};
use crate::page::{PAGE_FLAG_BOS, PAGE_FLAG_CONT, PAGE_FLAG_EOS};
use crate::vorbis_parser::VorbisParser;

struct MemoryIo {
    shared: Arc<Mutex<Vec<u8>>>,
    pos: usize,
}

impl MemoryIo {
    fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
        let shared = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                shared: Arc::clone(&shared),
                pos: 0,
            },
            shared,
        )
    }

    fn from_bytes(data: Vec<u8>) -> Self {
        Self {
            shared: Arc::new(Mutex::new(data)),
            pos: 0,
        }
    }
}

impl IoContext for MemoryIo {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let shared = self.shared.lock().unwrap();
        let remaining = shared.len().saturating_sub(self.pos);
        let n = buf.len().min(remaining);
        buf[..n].copy_from_slice(&shared[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let mut shared = self.shared.lock().unwrap();
        let end = self.pos + buf.len();
        if end > shared.len() {
            shared.resize(end, 0);
        }
        shared[self.pos..end].copy_from_slice(buf);
        self.pos = end;
        Ok(buf.len())
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        let shared = self.shared.lock().unwrap();
        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(off) => shared.len() as i64 + off,
            SeekFrom::Current(off) => self.pos as i64 + off,
        };
        self.pos = new_pos.max(0) as usize;
        Ok(self.pos as u64)
    }

    fn tell(&mut self) -> Result<u64> {
        Ok(self.pos as u64)
    }

    fn size(&mut self) -> Result<u64> {
        Ok(self.shared.lock().unwrap().len() as u64)
    }

    fn is_seekable(&self) -> bool {
        true
    }
}

// --- fixtures -------------------------------------------------------------

fn vorbis_id_header() -> Vec<u8> {
    let mut p = vec![1u8];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&0u32.to_le_bytes()); // version
    p.push(2); // channels
    p.extend_from_slice(&44_100u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // max bitrate
    p.extend_from_slice(&128_000u32.to_le_bytes()); // nominal bitrate
    p.extend_from_slice(&0u32.to_le_bytes()); // min bitrate
    p.push(0xb8); // block sizes: 256 and 2048
    p.push(1); // framing flag
    assert_eq!(p.len(), 30);
    p
}

fn vorbis_comment_header() -> Vec<u8> {
    let mut p = vec![3u8];
    p.extend_from_slice(b"vorbis");
    let vendor = b"wedeo";
    p.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    p.extend_from_slice(vendor);
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

/// Minimal setup header that initializes `VorbisParser` with one short block
/// mode. The parser scans the packet backwards for the framing bit and one
/// mode entry; see `parse_setup_header()` in `vorbis_parser.rs`.
fn vorbis_setup_header() -> Vec<u8> {
    let mut p = vec![5u8];
    p.extend_from_slice(b"vorbis");
    p.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0x80]);
    p
}

fn xiph_headers(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![2u8];
    for p in packets.iter().take(2) {
        let mut len = p.len();
        while len >= 255 {
            out.push(255);
            len -= 255;
        }
        out.push(len as u8);
    }
    for p in packets {
        out.extend_from_slice(p);
    }
    out
}

fn vorbis_extradata() -> Vec<u8> {
    xiph_headers(&[
        vorbis_id_header(),
        vorbis_comment_header(),
        vorbis_setup_header(),
    ])
}

fn opus_head() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"OpusHead");
    p.push(1); // version
    p.push(2); // channels
    p.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    p.extend_from_slice(&48_000u32.to_le_bytes());
    p.extend_from_slice(&0i16.to_le_bytes()); // gain
    p.push(0); // mapping family
    p
}

fn opus_tags() -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"OpusTags");
    let vendor = b"wedeo";
    p.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    p.extend_from_slice(vendor);
    p.extend_from_slice(&0u32.to_le_bytes());
    p
}

fn stream(codec: CodecId, extradata: Vec<u8>, sample_rate: u32) -> Stream {
    let mut params = CodecParameters::new(codec, MediaType::Audio);
    params.extradata = extradata;
    params.sample_rate = sample_rate;
    params.channel_layout = ChannelLayout::unspec(2);
    let mut stream = Stream::new(0, params);
    stream.time_base = Rational::new(1, sample_rate.max(1) as i32);
    stream
}

fn write_ogg(
    codec: CodecId,
    extradata: Vec<u8>,
    sample_rate: u32,
    packets: &[Vec<u8>],
    serial: u32,
) -> Vec<u8> {
    let (io, shared) = MemoryIo::new();
    let mut io = BufferedIo::new(Box::new(io));
    let mut muxer = OggMuxer::new();
    muxer.set_option("serial", &serial.to_string()).unwrap();
    muxer
        .write_header(&mut io, &[stream(codec, extradata, sample_rate)])
        .unwrap();
    for (i, data) in packets.iter().enumerate() {
        let mut packet = Packet::from_slice(data);
        packet.pts = i as i64 * 1024;
        packet.duration = 1024;
        muxer.write_packet(&mut io, &packet).unwrap();
    }
    muxer.write_trailer(&mut io).unwrap();
    io.flush().unwrap();
    shared.lock().unwrap().clone()
}

#[derive(Debug)]
struct ParsedPage {
    flags: u8,
    granule: u64,
    serial: u32,
    sequence: u32,
    segments: Vec<u8>,
    body: Vec<u8>,
}

fn parse_pages(data: &[u8]) -> Vec<ParsedPage> {
    let mut pages = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        assert_eq!(&data[at..at + 4], b"OggS", "capture pattern at {at}");
        assert_eq!(data[at + 4], 0, "version at {at}");
        let flags = data[at + 5];
        let granule = u64::from_le_bytes(data[at + 6..at + 14].try_into().unwrap());
        let serial = u32::from_le_bytes(data[at + 14..at + 18].try_into().unwrap());
        let sequence = u32::from_le_bytes(data[at + 18..at + 22].try_into().unwrap());
        let crc = u32::from_be_bytes(data[at + 22..at + 26].try_into().unwrap());
        let nsegs = data[at + 26] as usize;
        let segments = data[at + 27..at + 27 + nsegs].to_vec();
        let body_len: usize = segments.iter().map(|&v| usize::from(v)).sum();
        let body = data[at + 27 + nsegs..at + 27 + nsegs + body_len].to_vec();

        let mut raw = data[at..at + 27 + nsegs + body_len].to_vec();
        raw[22..26].fill(0);
        assert_eq!(page_crc(&raw), crc, "CRC mismatch on page {sequence}");

        pages.push(ParsedPage {
            flags,
            granule,
            serial,
            sequence,
            segments,
            body,
        });
        at += 27 + nsegs + body_len;
    }
    pages
}

fn packets_in_page(page: &ParsedPage) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut current = Vec::new();
    let mut at = 0usize;
    for &seg in &page.segments {
        let n = usize::from(seg);
        current.extend_from_slice(&page.body[at..at + n]);
        at += n;
        if seg < 255 {
            packets.push(std::mem::take(&mut current));
        }
    }
    packets
}

fn demux_stream(data: Vec<u8>) -> (wedeo_format::demuxer::DemuxerHeader, Vec<Packet>) {
    let mut io = BufferedIo::new(Box::new(MemoryIo::from_bytes(data)));
    let mut demuxer = OggDemuxerFactory.create().unwrap();
    let header = demuxer.read_header(&mut io).unwrap();
    let mut packets = Vec::new();
    while let Ok(packet) = demuxer.read_packet(&mut io) {
        packets.push(packet);
    }
    (header, packets)
}

// --- tests ----------------------------------------------------------------

#[test]
fn vorbis_header_pages_layout_crc_and_granule() {
    let data = vec![b'B', b'B', b'B']; // even first byte: audio packet, not a header
    let bytes = write_ogg(
        CodecId::Vorbis,
        vorbis_extradata(),
        44_100,
        std::slice::from_ref(&data),
        0x1234_5678,
    );
    let pages = parse_pages(&bytes);
    assert_eq!(pages.len(), 3);

    assert_eq!(pages[0].flags, PAGE_FLAG_BOS);
    assert_eq!(pages[0].granule, 0);
    assert_eq!(pages[0].serial, 0x1234_5678);
    assert_eq!(pages[0].sequence, 0);
    assert_eq!(packets_in_page(&pages[0]), vec![vorbis_id_header()]);

    assert_eq!(pages[1].flags, 0);
    assert_eq!(pages[1].granule, 0);
    assert_eq!(pages[1].sequence, 1);
    assert_eq!(
        packets_in_page(&pages[1]),
        vec![vorbis_comment_header(), vorbis_setup_header()]
    );

    assert_eq!(pages[2].flags, PAGE_FLAG_EOS);
    assert_eq!(pages[2].sequence, 2);
    // FFmpeg granule = pts + duration (stream tb is 1/sample_rate here).
    assert_eq!(pages[2].granule, 1024);
    assert_eq!(packets_in_page(&pages[2]), vec![data]);
}

#[test]
fn vorbis_granule_accumulates_across_packets() {
    let packets = vec![vec![0u8], vec![2u8], vec![4u8]]; // even first bytes: audio packets
    let bytes = write_ogg(CodecId::Vorbis, vorbis_extradata(), 44_100, &packets, 99);
    let pages = parse_pages(&bytes);
    assert_eq!(pages.len(), 3);
    let last = pages.last().unwrap();
    assert_eq!(last.flags, PAGE_FLAG_EOS);
    // granule = pts + duration of the last packet (2*1024 + 1024).
    assert_eq!(last.granule, 3 * 1024);
    assert_eq!(packets_in_page(last), packets);
}

#[test]
fn opus_headers_and_granule() {
    let mut extradata = opus_head();
    extradata.extend_from_slice(&opus_tags());
    let packet = vec![0x78, 0x00, 0x00]; // 960-sample Opus frame
    let bytes = write_ogg(
        CodecId::Opus,
        extradata,
        48_000,
        std::slice::from_ref(&packet),
        7,
    );
    let pages = parse_pages(&bytes);
    assert_eq!(pages.len(), 3);
    assert_eq!(pages[0].flags, PAGE_FLAG_BOS);
    assert_eq!(pages[0].granule, 0);
    assert_eq!(packets_in_page(&pages[0]), vec![opus_head()]);
    assert_eq!(pages[1].flags, 0);
    assert_eq!(packets_in_page(&pages[1]), vec![opus_tags()]);
    assert_eq!(pages[2].flags, PAGE_FLAG_EOS);
    // granule = pts + duration + OpusHead pre-skip (1024 + 312).
    assert_eq!(pages[2].granule, 1024 + 312);
    assert_eq!(packets_in_page(&pages[2]), vec![packet]);
}

#[test]
fn opus_headers_are_synthesized_without_extradata() {
    let headers = opus_headers(&[], 44_100, 1).unwrap();
    assert_eq!(headers.len(), 2);
    assert_eq!(&headers[0][..8], b"OpusHead");
    assert_eq!(headers[0][9], 1);
    assert_eq!(
        u32::from_le_bytes(headers[0][12..16].try_into().unwrap()),
        44_100
    );
    assert_eq!(&headers[1][..8], b"OpusTags");
    assert!(vorbis_headers(&[]).is_err());
}

#[test]
fn vorbis_extradata_with_be_lengths_is_accepted() {
    let (id, comment, setup) = (
        vorbis_id_header(),
        vorbis_comment_header(),
        vorbis_setup_header(),
    );
    let mut extradata = Vec::new();
    for p in [&id, &comment, &setup] {
        extradata.extend_from_slice(&(p.len() as u16).to_be_bytes());
        extradata.extend_from_slice(p);
    }
    assert_eq!(
        vorbis_headers(&extradata).unwrap(),
        vec![id, comment, setup]
    );
}

#[test]
fn valid_synthetic_setup_header_initializes_the_vorbis_parser() {
    let parser = VorbisParser::new(&vorbis_extradata()).unwrap();
    let mut parser = parser;
    assert_eq!(parser.frame(&[0u8]).unwrap(), 128);
}

#[test]
fn large_packet_is_split_with_continuation_flag() {
    let packet = vec![0x5au8; 70_000];
    let bytes = write_ogg(
        CodecId::Vorbis,
        vorbis_extradata(),
        44_100,
        std::slice::from_ref(&packet),
        42,
    );
    let pages = parse_pages(&bytes);
    assert_eq!(pages.len(), 4);
    assert_eq!(pages[0].flags, PAGE_FLAG_BOS);
    assert_eq!(pages[1].flags, 0);

    assert_eq!(pages[2].flags, 0, "page ending mid-packet has no CONT flag");
    assert_eq!(pages[2].segments.len(), 255);
    assert_eq!(pages[2].body.len(), 255 * 255);
    assert_eq!(pages[2].granule, u64::MAX, "no packet ends on this page");

    assert_eq!(pages[3].flags, PAGE_FLAG_CONT | PAGE_FLAG_EOS);
    assert_eq!(pages[3].granule, 1024);
    let assembled: Vec<u8> = pages[2]
        .body
        .iter()
        .chain(pages[3].body.iter())
        .copied()
        .collect();
    assert_eq!(assembled, packet);
}

#[test]
fn vorbis_round_trip_through_demuxer() {
    let payloads = vec![vec![0u8], vec![2u8], vec![4u8]];
    let bytes = write_ogg(
        CodecId::Vorbis,
        vorbis_extradata(),
        44_100,
        &payloads,
        0xdead_beef,
    );
    let (header, packets) = demux_stream(bytes);
    assert_eq!(header.streams.len(), 1);
    assert_eq!(header.streams[0].codec_params.codec_id, CodecId::Vorbis);
    assert_eq!(header.streams[0].codec_params.sample_rate, 44_100);
    assert_eq!(
        packets
            .iter()
            .map(|p| p.data.data().to_vec())
            .collect::<Vec<_>>(),
        payloads
    );
    assert!(packets.iter().all(|p| p.duration == 128));
}

#[test]
fn opus_round_trip_through_demuxer() {
    let payloads = vec![vec![0x78u8; 8], vec![0x78u8; 8]];
    let bytes = write_ogg(CodecId::Opus, opus_head(), 48_000, &payloads, 11);
    let (header, packets) = demux_stream(bytes);
    assert_eq!(header.streams.len(), 1);
    assert_eq!(header.streams[0].codec_params.codec_id, CodecId::Opus);
    assert_eq!(header.streams[0].codec_params.sample_rate, 48_000);
    assert_eq!(
        packets
            .iter()
            .map(|p| p.data.data().to_vec())
            .collect::<Vec<_>>(),
        payloads
    );
    assert!(packets.iter().all(|p| p.duration == 960));
}

#[test]
fn rejects_bad_streams_and_bad_extradata() {
    let (io, _) = MemoryIo::new();
    let mut io = BufferedIo::new(Box::new(io));
    let mut muxer = OggMuxer::new();
    assert_eq!(
        muxer.write_header(&mut io, &[]).err(),
        Some(Error::StreamNotFound)
    );
    assert_eq!(
        muxer
            .write_header(&mut io, &[stream(CodecId::Vorbis, Vec::new(), 44_100)])
            .err(),
        Some(Error::InvalidData)
    );
    assert_eq!(
        muxer
            .write_header(&mut io, &[stream(CodecId::H264, Vec::new(), 44_100)])
            .err(),
        Some(Error::InvalidArgument)
    );
    let packet = Packet::from_slice(b"x");
    assert_eq!(
        muxer.write_packet(&mut io, &packet).err(),
        Some(Error::InvalidArgument)
    );
}

#[test]
fn factory_descriptor_and_registration() {
    let factory = OggMuxerFactory;
    let desc = factory.descriptor();
    assert_eq!(desc.name, "ogg");
    assert_eq!(desc.long_name, "Ogg");
    assert_eq!(desc.extensions, "ogg");
    assert_eq!(desc.mime_types, "application/ogg");
    assert!(desc.flags.contains(OutputFormatFlags::GLOBALHEADER));
    assert_eq!(desc.audio_codec, CodecId::Vorbis);
    assert_eq!(desc.video_codec, CodecId::Theora);
    assert!(factory.create().is_ok());

    assert!(find_muxer_by_name("ogg").is_some());
    assert_eq!(
        guess_muxer_by_extension("ogg").map(|f| f.descriptor().name),
        Some("ogg")
    );
    assert!(muxers().any(|f| f.descriptor().name == "ogg"));
}
