//! Conformance tests for `wedeo-format-ivf-mux`.
//!
//! Covers registration, the FFmpeg-compatible IVF byte layout (bitexact
//! comparison against an `ffmpeg -f ivf` reference), fourcc mapping, and a
//! mux/demux round-trip with the existing IVF demuxer.

// Ensure the muxer and demuxer registrations are linked into the test binary.
use wedeo_format_ivf as _;
use wedeo_format_ivf_mux as _;

use wedeo_codec::decoder::CodecParameters;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;
use wedeo_core::rational::Rational;
use wedeo_format::context::{InputContext, OutputContext};
use wedeo_format::demuxer::Stream;
use wedeo_format::registry::{find_muxer_by_name, guess_muxer_by_extension, muxers};

/// A 16x16 VP8 keyframe muxed by ffmpeg n9.1.
///
/// Generated with:
///   ffmpeg -f lavfi -i color=c=blue:s=16x16:r=30 -frames:v 1 \
///       -c:v libvpx -bitexact -b:v 10k -deadline realtime -cpu-used 8 \
///       -an -f ivf vp8_blue.ivf
///
/// `ffmpeg -i vp8_blue.ivf -c copy -bitexact -f ivf` and a second independent
/// encode with the same command both reproduce these 101 bytes exactly.
const FFMPEG_VP8_IVF: &[u8] = &[
    0x44, 0x4b, 0x49, 0x46, 0x00, 0x00, 0x20, 0x00, 0x56, 0x50, 0x38, 0x30, 0x10, 0x00, 0x10, 0x00,
    0x1e, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x39, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xb0, 0x02, 0x00, 0x9d,
    0x01, 0x2a, 0x10, 0x00, 0x10, 0x00, 0x00, 0x47, 0x08, 0x85, 0x85, 0x88, 0x99, 0x84, 0x88, 0x02,
    0x02, 0x02, 0x75, 0xaa, 0x03, 0xf8, 0x02, 0x0c, 0xfd, 0xfb, 0x00, 0xfe, 0xff, 0x4d, 0x12, 0xff,
    0xfc, 0x4a, 0xa7, 0xf1, 0x2a, 0x9f, 0xc4, 0xaa, 0x7f, 0xf1, 0x2a, 0x9f, 0xfd, 0x18, 0x7f, 0x0c,
    0x3f, 0x86, 0x1f, 0xde, 0x10,
];

fn tmp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn video_stream(codec_id: CodecId, width: u32, height: u32, time_base: Rational) -> Stream {
    let mut params = CodecParameters::new(codec_id, MediaType::Video);
    params.width = width;
    params.height = height;
    let mut stream = Stream::new(0, params);
    stream.time_base = time_base;
    stream
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}

#[test]
fn muxer_is_registered() {
    let factory = find_muxer_by_name("ivf").expect("ivf muxer not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "ivf");
    assert_eq!(desc.long_name, "On2 IVF");
    assert_eq!(desc.extensions, "ivf");
    assert_eq!(desc.mime_types, "video/x-ivf");
    assert_eq!(desc.video_codec, CodecId::Vp8);
    assert_eq!(desc.audio_codec, CodecId::None);
    assert!(factory.create().is_ok());

    assert_eq!(
        guess_muxer_by_extension("ivf").map(|f| f.descriptor().name),
        Some("ivf")
    );
    assert!(muxers().any(|f| f.descriptor().name == "ivf"));
}

#[test]
fn bitexact_against_ffmpeg_ivf() {
    assert_eq!(&FFMPEG_VP8_IVF[0..4], b"DKIF");
    assert_eq!(&FFMPEG_VP8_IVF[8..12], b"VP80");

    let width = u16::from_le_bytes([FFMPEG_VP8_IVF[12], FFMPEG_VP8_IVF[13]]) as u32;
    let height = u16::from_le_bytes([FFMPEG_VP8_IVF[14], FFMPEG_VP8_IVF[15]]) as u32;
    let fps_num = le_u32(&FFMPEG_VP8_IVF[16..20]);
    let fps_den = le_u32(&FFMPEG_VP8_IVF[20..24]);
    let stream = video_stream(
        CodecId::Vp8,
        width,
        height,
        Rational::new(fps_den as i32, fps_num as i32),
    );

    let path = tmp_path("wedeo_fate_ivf_mux_bitexact.ivf");
    let mut out = OutputContext::create(&path, "ivf", &[stream]).unwrap();

    // Re-mux the exact packet bytes carried by the ffmpeg-generated file.
    let mut offset = 32;
    while offset < FFMPEG_VP8_IVF.len() {
        let size = le_u32(&FFMPEG_VP8_IVF[offset..offset + 4]) as usize;
        let pts = i64::from_le_bytes(FFMPEG_VP8_IVF[offset + 4..offset + 12].try_into().unwrap());
        let payload = &FFMPEG_VP8_IVF[offset + 12..offset + 12 + size];
        let mut packet = Packet::from_slice(payload);
        packet.pts = pts;
        out.write_packet(&packet).unwrap();
        offset += 12 + size;
    }
    out.finish().unwrap();

    let produced = std::fs::read(&path).unwrap();
    assert_eq!(
        produced, FFMPEG_VP8_IVF,
        "IVF output must be byte-for-byte identical to `ffmpeg -f ivf`"
    );
}

#[test]
fn roundtrip_with_ivf_demuxer() {
    let path = tmp_path("wedeo_fate_ivf_mux_roundtrip.ivf");
    let stream = video_stream(CodecId::Vp9, 128, 96, Rational::new(1, 30));
    let mut out = OutputContext::create(&path, "ivf", &[stream]).unwrap();
    let frames: [(i64, &[u8]); 3] = [(0, b"first frame"), (15, b"second"), (30, b"third!")];
    for (pts, payload) in frames {
        let mut packet = Packet::from_slice(payload);
        packet.pts = pts;
        out.write_packet(&packet).unwrap();
    }
    out.finish().unwrap();

    let raw = std::fs::read(&path).unwrap();
    assert_eq!(&raw[0..4], b"DKIF");
    assert_eq!(&raw[8..12], b"VP90");
    assert_eq!(le_u32(&raw[24..]), 3, "frame count patched in trailer");

    let mut input = InputContext::open(&path).unwrap();
    assert_eq!(input.streams.len(), 1);
    assert_eq!(input.streams[0].codec_params.codec_id, CodecId::Vp9);
    assert_eq!(input.streams[0].codec_params.width, 128);
    assert_eq!(input.streams[0].codec_params.height, 96);
    assert_eq!(input.streams[0].time_base, Rational::new(1, 30));

    for (pts, payload) in frames {
        let packet = input.read_packet().unwrap();
        assert_eq!(packet.pts, pts);
        assert_eq!(packet.data.data(), payload);
    }
    assert_eq!(input.read_packet().err(), Some(Error::Eof));
}

#[test]
fn fourcc_for_all_supported_codecs() {
    let cases = [
        (CodecId::Vp8, *b"VP80"),
        (CodecId::Vp9, *b"VP90"),
        (CodecId::Av1, *b"AV01"),
    ];
    for (codec_id, fourcc) in cases {
        let path = tmp_path(&format!("wedeo_fate_ivf_mux_{}.ivf", codec_id.name()));
        let stream = video_stream(codec_id, 320, 240, Rational::new(1, 24));
        OutputContext::create(&path, "ivf", &[stream])
            .unwrap()
            .finish()
            .unwrap();
        let header = std::fs::read(&path).unwrap();
        assert_eq!(header.len(), 32);
        assert_eq!(&header[8..12], &fourcc, "fourcc for {codec_id:?}");
        assert_eq!(le_u32(&header[16..]), 24);
        assert_eq!(le_u32(&header[20..]), 1);
    }
}
