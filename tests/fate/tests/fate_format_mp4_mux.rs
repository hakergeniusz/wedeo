//! Conformance tests for `wedeo-format-mp4`'s MP4/MOV muxer.
//!
//! Covers muxer registration, FFmpeg-compatible box structure (ftyp/moov/mdat,
//! mvhd/trak/tkhd/mdia/mdhd/hdlr/minf/stbl, stsd/stts/stsc/stsz/stco, ctts,
//! stss, avcC/hvcC/esds), B-frame composition offsets, faststart relocation,
//! HEVC and AAC tracks, and a cross-decode round trip through system ffmpeg.

// Link the crate's inventory registrations into this test binary.
use wedeo_format_mp4 as _;

use wedeo_codec::decoder::CodecParameters;
use wedeo_core::codec_id::CodecId;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketFlags};
use wedeo_core::rational::Rational;
use wedeo_format::context::{InputContext, OutputContext};
use wedeo_format::demuxer::{Demuxer, Stream};
use wedeo_format::io::{BufferedIo, FileIo};
use wedeo_format::muxer::Muxer;
use wedeo_format::registry::{find_muxer_by_name, guess_muxer_by_extension, muxers};
use wedeo_format_mp4::muxer::{Mp4Muxer, Mp4MuxerOptions};

fn tmp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// Minimal avcC extradata (version 1, Baseline profile, level 30).
fn avcc() -> Vec<u8> {
    vec![
        0x01, 0x42, 0xC0, 0x1E, 0xFF, 0xE1, 0x00, 0x04, 0x67, 0x42, 0xC0, 0x1E, 0x01, 0x00, 0x02,
        0x68, 0xCE,
    ]
}

/// Minimal hvcC extradata (>= 23 bytes, configurationVersion = 1).
fn hvcc() -> Vec<u8> {
    vec![
        0x01, 0x01, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0xf0, 0x00,
        0xfc, 0xfd, 0xf8, 0xf8, 0x00, 0x00, 0x00, 0x01, 0x00,
    ]
}

/// Exact `ftyp` box emitted by `ffmpeg -bitexact -f mp4` with an H.264 track:
/// `isom` major/minor 0x200, compatible brands `isom iso2 avc1 mp41`.
const FFMPEG_BITEXACT_FTYP_VIDEO: &[u8] =
    b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00isomiso2avc1mp41";

fn video_stream(codec: CodecId, extradata: &[u8]) -> Stream {
    let mut params = CodecParameters::new(codec, MediaType::Video);
    params.width = 320;
    params.height = 240;
    params.time_base = Rational::new(1, 25);
    params.extradata = extradata.to_vec();
    Stream::new(0, params)
}

fn audio_stream() -> Stream {
    let mut params = CodecParameters::new(CodecId::Aac, MediaType::Audio);
    params.sample_rate = 44100;
    params.channel_layout.nb_channels = 2;
    params.bits_per_coded_sample = 16;
    params.extradata = vec![0x12, 0x10];
    Stream::new(0, params)
}

/// Parse MP4 box headers from `data` into (fourcc, start, size) triples.
fn parse_boxes(data: &[u8]) -> Vec<([u8; 4], usize, usize)> {
    let mut boxes = Vec::new();
    let mut off = 0usize;
    while off + 8 <= data.len() {
        let size32 = u32::from_be_bytes(data[off..off + 4].try_into().unwrap());
        let ty: [u8; 4] = data[off + 4..off + 8].try_into().unwrap();
        let size = if size32 == 1 {
            if off + 16 > data.len() {
                break;
            }
            u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap()) as usize
        } else {
            size32 as usize
        };
        if size < 8 || off + size > data.len() {
            break;
        }
        boxes.push((ty, off, size));
        off += size;
    }
    boxes
}

fn has_box(data: &[u8], name: &[u8; 4]) -> bool {
    data.windows(4).any(|w| w == name)
}

#[test]
fn muxer_is_registered() {
    let factory = find_muxer_by_name("mp4").expect("mp4 muxer not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "mp4");
    assert_eq!(desc.extensions, "mp4,m4a,m4v");
    assert_eq!(desc.video_codec, CodecId::H264);
    assert_eq!(desc.audio_codec, CodecId::Aac);
    assert!(factory.create().is_ok());
    assert!(muxers().any(|f| f.descriptor().name == "mp4"));
    assert_eq!(
        guess_muxer_by_extension("mp4").map(|f| f.descriptor().name),
        Some("mp4")
    );
    assert_eq!(
        guess_muxer_by_extension("m4a").map(|f| f.descriptor().name),
        Some("mp4")
    );
}

#[test]
fn ffmpeg_compatible_box_structure() {
    let path = tmp_path("wedeo_fate_mp4_mux_structure.mp4");
    let mut out =
        OutputContext::create(&path, "mp4", &[video_stream(CodecId::H264, &avcc())]).unwrap();
    for i in 0..3 {
        let mut pkt = Packet::from_slice(&vec![0x11u8; 100 + i * 20]);
        pkt.stream_index = 0;
        pkt.pts = i as i64;
        pkt.dts = i as i64;
        pkt.duration = 1;
        if i == 0 {
            pkt.flags = PacketFlags::KEY;
        }
        out.write_packet(&pkt).unwrap();
    }
    out.finish().unwrap();

    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();

    let top = parse_boxes(&data);
    let types: Vec<[u8; 4]> = top.iter().map(|(t, _, _)| *t).collect();
    assert_eq!(types, vec![*b"ftyp", *b"mdat", *b"moov"]);

    // ftyp major brand (isom for video, M4A for audio-only).
    let (_, ftyp_start, _) = top[0];
    assert_eq!(&data[ftyp_start + 8..ftyp_start + 12], b"isom"); // major brand
    assert_eq!(&data[ftyp_start + 16..ftyp_start + 20], b"isom"); // compatible brand
    assert_eq!(&data[ftyp_start + 20..ftyp_start + 24], b"iso2");
    assert_eq!(&data[ftyp_start + 24..ftyp_start + 28], b"avc1");

    // Every mandatory box from the FFmpeg movenc layout is present.
    for name in [
        b"mvhd", b"trak", b"tkhd", b"mdia", b"mdhd", b"hdlr", b"minf", b"vmhd", b"dinf", b"dref",
        b"stbl", b"stsd", b"avc1", b"avcC", b"stts", b"stsc", b"stsz", b"stco", b"stss",
    ] {
        assert!(
            has_box(&data, name),
            "missing box {}",
            String::from_utf8_lossy(name)
        );
    }
}

#[test]
fn ftyp_matches_ffmpeg_bitexact() {
    // `ffmpeg -bitexact -f mp4` with H.264 produces this 32-byte ftyp box.
    let path = tmp_path("wedeo_fate_mp4_mux_ftyp.mp4");
    let mut out = OutputContext::create(&path, "mp4", &[video_stream(CodecId::H264, &avcc())])
        .expect("create output");
    let mut pkt = Packet::from_slice(&[0u8; 16]);
    pkt.stream_index = 0;
    pkt.pts = 0;
    pkt.dts = 0;
    pkt.duration = 1;
    pkt.flags = PacketFlags::KEY;
    out.write_packet(&pkt).unwrap();
    out.finish().unwrap();
    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();
    assert_eq!(
        &data[..FFMPEG_BITEXACT_FTYP_VIDEO.len()],
        FFMPEG_BITEXACT_FTYP_VIDEO,
        "ftyp must be byte-identical to ffmpeg -bitexact output"
    );
}

#[test]
fn h264_roundtrip_and_b_frame_ctts() {
    let path = tmp_path("wedeo_fate_mp4_mux_roundtrip.mp4");
    let mut out =
        OutputContext::create(&path, "mp4", &[video_stream(CodecId::H264, &avcc())]).unwrap();
    // IPBBP: pts/dts differ, so ctts is required. First/last are keyframes.
    let pts_dts = [(0i64, 0i64), (2, 1), (3, 2), (1, 3), (4, 4)];
    let payloads: Vec<Vec<u8>> = (0..5).map(|i| vec![0x5a + i as u8; 60 + i * 7]).collect();
    for (i, (pts, dts)) in pts_dts.iter().enumerate() {
        let mut pkt = Packet::from_slice(&payloads[i]);
        pkt.stream_index = 0;
        pkt.pts = *pts;
        pkt.dts = *dts;
        pkt.duration = 1;
        if i == 0 || i == 4 {
            pkt.flags = PacketFlags::KEY;
        }
        out.write_packet(&pkt).unwrap();
    }
    out.finish().unwrap();

    let data = std::fs::read(&path).unwrap();
    assert!(has_box(&data, b"ctts"), "B-frames must produce a ctts box");
    assert!(
        has_box(&data, b"stss"),
        "keyframes must produce an stss box"
    );

    // Demux with wedeo's own MP4 demuxer.
    let mut demuxer = wedeo_format_mp4::demuxer::Mp4Demuxer::new();
    let io = FileIo::open(&path).unwrap();
    let mut io = BufferedIo::new(Box::new(io));
    let header = demuxer.read_header(&mut io).unwrap();
    assert_eq!(header.streams.len(), 1);
    assert_eq!(header.streams[0].codec_params.codec_id, CodecId::H264);
    assert_eq!(header.streams[0].codec_params.width, 320);
    assert_eq!(header.streams[0].codec_params.height, 240);

    let mut got = Vec::new();
    let mut payload_idx = 0;
    while let Ok(pkt) = demuxer.read_packet(&mut io) {
        assert_eq!(pkt.data.data(), &payloads[payload_idx]);
        got.push((pkt.pts, pkt.dts));
        payload_idx += 1;
    }
    assert_eq!(payload_idx, 5);
    assert_eq!(got, pts_dts.to_vec());
    std::fs::remove_file(&path).ok();
}

#[test]
fn audio_only_uses_m4a_brand_and_esds() {
    let path = tmp_path("wedeo_fate_mp4_mux_audio.m4a");
    let stream = audio_stream();
    let mut out = OutputContext::create(&path, "mp4", &[stream]).unwrap();
    for i in 0..2 {
        let mut pkt = Packet::from_slice(&[0u8; 128]);
        pkt.stream_index = 0;
        pkt.pts = i as i64 * 1024;
        pkt.dts = pkt.pts;
        pkt.duration = 1024;
        pkt.flags = PacketFlags::KEY;
        out.write_packet(&pkt).unwrap();
    }
    out.finish().unwrap();

    let data = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).ok();
    let top = parse_boxes(&data);
    let (_, ftyp_start, _) = top[0];
    assert_eq!(&data[ftyp_start + 8..ftyp_start + 12], b"M4A ");
    for name in [b"mp4a", b"esds", b"smhd", b"stts", b"stsc", b"stco"] {
        assert!(
            has_box(&data, name),
            "missing box {}",
            String::from_utf8_lossy(name)
        );
    }
}

#[test]
fn hevc_hvc1_sample_entry() {
    let path = tmp_path("wedeo_fate_mp4_mux_hevc.mp4");
    let mut out =
        OutputContext::create(&path, "mp4", &[video_stream(CodecId::Hevc, &hvcc())]).unwrap();
    let mut pkt = Packet::from_slice(&[0u8; 200]);
    pkt.stream_index = 0;
    pkt.pts = 0;
    pkt.dts = 0;
    pkt.duration = 1;
    pkt.flags = PacketFlags::KEY;
    out.write_packet(&pkt).unwrap();
    out.finish().unwrap();

    let data = std::fs::read(&path).unwrap();
    assert!(has_box(&data, b"hvc1"), "missing hvc1 sample entry");
    assert!(has_box(&data, b"hvcC"), "missing hvcC configuration box");

    let mut demuxer = wedeo_format_mp4::demuxer::Mp4Demuxer::new();
    let io = FileIo::open(&path).unwrap();
    let mut io = BufferedIo::new(Box::new(io));
    let header = demuxer.read_header(&mut io).unwrap();
    assert_eq!(header.streams.len(), 1);
    assert_eq!(header.streams[0].codec_params.codec_id, CodecId::Hevc);
    std::fs::remove_file(&path).ok();
}

#[test]
fn faststart_places_moov_before_mdat() {
    let path = tmp_path("wedeo_fate_mp4_mux_faststart.mp4");
    let stream = video_stream(CodecId::H264, &avcc());
    let mut muxer = Mp4Muxer::with_options(Mp4MuxerOptions { faststart: true });
    let io = FileIo::create(&path).unwrap();
    let mut io = BufferedIo::new(Box::new(io));
    muxer.write_header(&mut io, &[stream]).unwrap();
    let expected: Vec<Vec<u8>> = (0..5)
        .map(|i| vec![0x33u8 + i as u8; 80 + i * 13])
        .collect();
    for (i, payload) in expected.iter().enumerate() {
        let mut pkt = Packet::from_slice(payload);
        pkt.stream_index = 0;
        pkt.pts = i as i64;
        pkt.dts = i as i64;
        pkt.duration = 1;
        if i == 0 {
            pkt.flags = PacketFlags::KEY;
        }
        muxer.write_packet(&mut io, &pkt).unwrap();
    }
    muxer.write_trailer(&mut io).unwrap();
    drop(io);

    let data = std::fs::read(&path).unwrap();
    let top = parse_boxes(&data);
    let types: Vec<[u8; 4]> = top.iter().map(|(t, _, _)| *t).collect();
    assert_eq!(types, vec![*b"ftyp", *b"moov", *b"mdat"]);
    // Verify the relocated mdat payload is byte-for-byte intact.
    let (_, mdat_start, mdat_size) = top[2];
    let payload = &data[mdat_start + 16..mdat_start + mdat_size];
    let mut off = 0;
    for p in &expected {
        assert_eq!(&payload[off..off + p.len()], p.as_slice());
        off += p.len();
    }
    assert_eq!(off, payload.len());

    // A demuxer must handle moov-before-mdat just as well.
    let mut demuxer = wedeo_format_mp4::demuxer::Mp4Demuxer::new();
    let io = FileIo::open(&path).unwrap();
    let mut io = BufferedIo::new(Box::new(io));
    demuxer.read_header(&mut io).unwrap();
    let mut count = 0;
    while demuxer.read_packet(&mut io).is_ok() {
        count += 1;
    }
    assert_eq!(count, 5);
    std::fs::remove_file(&path).ok();
}

/// Generate an H.264 MP4 with ffmpeg, copy its packets through wedeo's demuxer
/// and muxer, then verify system ffmpeg can decode the re-muxed file with the
/// same frame count. Skipped when ffmpeg is unavailable.
#[test]
fn cross_decode_ffmpeg_h264() {
    let Some(ffmpeg) = find_program("ffmpeg") else {
        eprintln!("ffmpeg not found; skipping cross_decode_ffmpeg_h264");
        return;
    };

    let ref_path = tmp_path("wedeo_fate_mp4_mux_ffmpeg_ref.mp4");
    let out_path = tmp_path("wedeo_fate_mp4_mux_ffmpeg_remux.mp4");

    let generate = std::process::Command::new(&ffmpeg)
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10",
            "-frames:v",
            "10",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-bitexact",
            "-x264-params",
            "keyint=5:min-keyint=5:scenecut=0",
            "-f",
            "mp4",
            &ref_path,
        ])
        .status()
        .expect("run ffmpeg");
    assert!(
        generate.success(),
        "ffmpeg failed to generate reference H.264 MP4"
    );

    // Demux the reference with wedeo to obtain real avcC + length-prefixed NALs.
    let mut input = InputContext::open(&ref_path).expect("open reference with wedeo");
    let streams = input.streams.clone();
    assert!(!streams.is_empty());
    let mut out = OutputContext::create(&out_path, "mp4", &streams).expect("create remux output");
    let mut count = 0usize;
    while let Ok(mut pkt) = input.read_packet() {
        pkt.stream_index = 0;
        out.write_packet(&pkt).expect("write remux packet");
        count += 1;
    }
    out.finish().unwrap();

    assert!(count >= 10, "expected at least 10 packets, got {count}");

    // Decode both files with ffmpeg and compare decoded frame counts.
    let decoded = |path: &str| -> usize {
        let output = std::process::Command::new(&ffmpeg)
            .args([
                "-v", "error", "-i", path, "-map", "0:v:0", "-f", "null", "-",
            ])
            .output()
            .expect("run ffmpeg decode");
        assert!(
            output.status.success(),
            "ffmpeg decode failed for {path}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // ffmpeg prints the frame count on stderr at info level; re-run with
        // -v info and parse the final `frame=` line for a robust count.
        let info = std::process::Command::new(&ffmpeg)
            .args(["-v", "info", "-i", path, "-map", "0:v:0", "-f", "null", "-"])
            .output()
            .expect("run ffmpeg decode info");
        let stderr = String::from_utf8_lossy(&info.stderr);
        stderr
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                let rest = line.strip_prefix("frame=")?;
                let n = rest.split_whitespace().next()?;
                n.parse::<usize>().ok()
            })
            .next_back()
            .unwrap_or(0)
    };

    let ref_frames = decoded(&ref_path);
    let out_frames = decoded(&out_path);
    assert!(
        ref_frames >= 10,
        "reference decode produced {ref_frames} frames"
    );
    assert_eq!(
        out_frames, ref_frames,
        "remuxed file decoded to a different frame count"
    );

    std::fs::remove_file(&ref_path).ok();
    std::fs::remove_file(&out_path).ok();
}

fn find_program(name: &str) -> Option<String> {
    let output = std::process::Command::new("sh")
        .args(["-c", &format!("command -v {name}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}
