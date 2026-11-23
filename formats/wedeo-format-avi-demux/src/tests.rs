//! Unit tests using synthetic RIFF/AVI chunks.

use wedeo_core::error::Error;
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;
use wedeo_format::context::InputContext;
use wedeo_format::demuxer::{PROBE_SCORE_MAX, ProbeData};
use wedeo_format::registry::{demuxers, find_demuxer_by_name};

fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(tag);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() & 1 != 0 {
        out.push(0);
    }
    out
}

fn list(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(kind);
    body.extend_from_slice(payload);
    chunk(b"LIST", &body)
}

fn avih(width: u32, height: u32, frames: u32) -> Vec<u8> {
    let fields = [
        40_000u32, // dwMicroSecPerFrame
        0,         // dwMaxBytesPerSec
        0,         // dwPaddingGranularity
        0,         // dwFlags
        0,         // reserved[0]
        0,         // reserved[1]
        frames,    // dwTotalFrames
        0,         // dwInitialFrames
        width,     // dwWidth
        height,    // dwHeight
    ];
    let mut payload = Vec::new();
    for f in fields {
        payload.extend_from_slice(&f.to_le_bytes());
    }
    payload.resize(56, 0);
    chunk(b"avih", &payload)
}

fn strh_video(scale: u32, rate: u32, frames: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"vids"); // fccType
    p.extend_from_slice(b"H264"); // fccHandler
    p.extend_from_slice(&0u32.to_le_bytes()); // dwFlags
    p.extend_from_slice(&0u16.to_le_bytes()); // wPriority
    p.extend_from_slice(&0u16.to_le_bytes()); // wLanguage
    p.extend_from_slice(&0u32.to_le_bytes()); // dwInitialFrames
    p.extend_from_slice(&scale.to_le_bytes());
    p.extend_from_slice(&rate.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes()); // dwStart
    p.extend_from_slice(&frames.to_le_bytes()); // dwLength
    p.extend_from_slice(&0u32.to_le_bytes()); // dwSuggestedBufferSize
    p.extend_from_slice(&0u32.to_le_bytes()); // dwQuality
    p.extend_from_slice(&0u32.to_le_bytes()); // dwSampleSize
    p.resize(56, 0);
    chunk(b"strh", &p)
}

fn strh_audio(sample_size: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"auds");
    p.extend_from_slice(b"\x01\x00\x00\x00"); // WAVE_FORMAT_PCM handler
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&0u16.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&1u32.to_le_bytes()); // scale
    p.extend_from_slice(&8000u32.to_le_bytes()); // rate
    p.extend_from_slice(&0u32.to_le_bytes()); // start
    p.extend_from_slice(&0u32.to_le_bytes()); // length
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&0u32.to_le_bytes());
    p.extend_from_slice(&sample_size.to_le_bytes());
    p.resize(56, 0);
    chunk(b"strh", &p)
}

fn strf_video(width: u32, height: u32, fourcc: &[u8; 4]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&40u32.to_le_bytes()); // biSize
    p.extend_from_slice(&(width as i32).to_le_bytes());
    p.extend_from_slice(&(height as i32).to_le_bytes());
    p.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    p.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    p.extend_from_slice(fourcc);
    p.resize(40, 0);
    chunk(b"strf", &p)
}

fn strf_audio(channels: u16, sample_rate: u32, bits: u16, block_align: u16) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&1u16.to_le_bytes()); // WAVE_FORMAT_PCM
    p.extend_from_slice(&channels.to_le_bytes());
    p.extend_from_slice(&sample_rate.to_le_bytes());
    p.extend_from_slice(&(sample_rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
    p.extend_from_slice(&block_align.to_le_bytes());
    p.extend_from_slice(&bits.to_le_bytes());
    p.resize(16, 0);
    chunk(b"strf", &p)
}

/// Build an AVI with one video stream and optional PCM audio stream.
fn build_avi(frames: &[&[u8]], audio: Option<&[u8]>, info_tags: &[([u8; 4], &[u8])]) -> Vec<u8> {
    let mut hdrl = avih(160, 120, frames.len() as u32);
    let mut strl = strh_video(1, 25, frames.len() as u32);
    strl.extend_from_slice(&strf_video(160, 120, b"H264"));
    hdrl.extend_from_slice(&list(b"strl", &strl));

    let mut idx_entries = Vec::new();
    let mut movi = Vec::new();
    let mut video_pos = 0u32;
    for (i, frame) in frames.iter().enumerate() {
        let tag = if i == 0 { b"00dc" } else { b"00db" };
        let c = chunk(tag, frame);
        idx_entries.extend_from_slice(b"00dc");
        let key_flag: u32 = if i == 0 { 0x10 } else { 0 };
        idx_entries.extend_from_slice(&key_flag.to_le_bytes());
        idx_entries.extend_from_slice(&video_pos.to_le_bytes());
        idx_entries.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        video_pos += c.len() as u32;
        movi.extend_from_slice(&c);
    }
    if let Some(samples) = audio {
        let mut strl = strh_audio(2);
        strl.extend_from_slice(&strf_audio(1, 8000, 8, 2));
        hdrl.extend_from_slice(&list(b"strl", &strl));
        let c = chunk(b"01wb", samples);
        idx_entries.extend_from_slice(b"01wb");
        idx_entries.extend_from_slice(&0x10u32.to_le_bytes());
        idx_entries.extend_from_slice(&video_pos.to_le_bytes());
        idx_entries.extend_from_slice(&(samples.len() as u32).to_le_bytes());
        movi.extend_from_slice(&c);
    }

    let mut body = Vec::new();
    if !info_tags.is_empty() {
        let mut info = Vec::new();
        for (tag, value) in info_tags {
            info.extend_from_slice(&chunk(tag, value));
        }
        body.extend_from_slice(&list(b"INFO", &info));
    }
    body.extend_from_slice(&list(b"hdrl", &hdrl));
    body.extend_from_slice(&list(b"movi", &movi));
    body.extend_from_slice(&chunk(b"idx1", &idx_entries));

    let mut avi = Vec::new();
    avi.extend_from_slice(b"RIFF");
    avi.extend_from_slice(&(body.len() as u32).to_le_bytes());
    avi.extend_from_slice(b"AVI ");
    avi.extend_from_slice(&body);
    avi
}

fn tmp_path(name: &str) -> String {
    std::env::temp_dir()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

#[test]
fn demuxer_is_registered() {
    let factory = find_demuxer_by_name("avi").expect("avi demuxer not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "avi");
    assert_eq!(desc.long_name, "AVI (Audio Video Interleaved)");
    assert_eq!(desc.extensions, "avi");
    assert_eq!(desc.mime_types, "video/x-msvideo");
    assert!(factory.create().is_ok());
    assert!(demuxers().any(|f| f.descriptor().name == "avi"));
}

#[test]
fn probe_detects_avi_headers() {
    let factory = find_demuxer_by_name("avi").unwrap();
    for form in [b"AVI ".as_slice(), b"AVIX", b"AVI\x19"] {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(form);
        let data = ProbeData {
            filename: "x.avi",
            buf: &buf,
        };
        assert_eq!(factory.probe(&data), PROBE_SCORE_MAX);
    }
    let bad = b"RIFF\x64\x00\x00\x00WAVEfmt ";
    let data = ProbeData {
        filename: "x.wav",
        buf: bad,
    };
    assert_eq!(factory.probe(&data), 0);
    let short = ProbeData {
        filename: "x.avi",
        buf: b"RIFF",
    };
    assert_eq!(factory.probe(&short), 0);
}

#[test]
fn parses_synthetic_video_avi() {
    let data = build_avi(&[b"frame-one", b"two", b"three!"], None, &[]);
    let path = tmp_path("wedeo_avi_unit_video.avi");
    std::fs::write(&path, &data).unwrap();

    let mut input = InputContext::open(&path).unwrap();
    assert_eq!(input.nb_streams(), 1);
    let st = input.stream(0).unwrap();
    assert_eq!(
        st.codec_params.codec_id,
        wedeo_core::codec_id::CodecId::H264
    );
    assert_eq!(st.codec_params.media_type, MediaType::Video);
    assert_eq!(st.codec_params.width, 160);
    assert_eq!(st.codec_params.height, 120);
    assert_eq!(st.time_base, Rational::new(1, 25));
    assert_eq!(st.nb_frames, 3);
    assert_eq!(st.duration, 3);

    let mut frames = Vec::new();
    let mut dts = Vec::new();
    loop {
        match input.read_packet() {
            Ok(p) => {
                frames.push(p.data.data().to_vec());
                dts.push(p.dts);
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(
        frames,
        vec![b"frame-one".to_vec(), b"two".to_vec(), b"three!".to_vec()]
    );
    assert_eq!(dts, vec![0, 1, 2]);
}

#[test]
fn parses_synthetic_audio_video_avi() {
    let samples = [0u8, 1, 2, 3, 4, 5, 6, 7];
    let data = build_avi(&[b"v1", b"v2"], Some(&samples), &[]);
    let path = tmp_path("wedeo_avi_unit_av.avi");
    std::fs::write(&path, &data).unwrap();

    let mut input = InputContext::open(&path).unwrap();
    assert_eq!(input.nb_streams(), 2);
    assert_eq!(
        input.stream(0).unwrap().codec_params.media_type,
        MediaType::Video
    );
    let audio = input.stream(1).unwrap();
    assert_eq!(audio.codec_params.media_type, MediaType::Audio);
    assert_eq!(
        audio.codec_params.codec_id,
        wedeo_core::codec_id::CodecId::PcmU8
    );
    assert_eq!(audio.codec_params.sample_rate, 8000);
    assert_eq!(audio.codec_params.channel_layout.nb_channels, 1);
    assert_eq!(audio.time_base, Rational::new(1, 8000));

    // First the video frames, then the audio chunk (all packets are emitted
    // in file order).
    let mut packets = Vec::new();
    loop {
        match input.read_packet() {
            Ok(p) => packets.push((p.stream_index, p.dts, p.data.data().to_vec())),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].1, 0);
    assert_eq!(packets[1].1, 1);
    assert_eq!(packets[2].0, 1);
    assert_eq!(packets[2].2, samples.to_vec());
}

#[test]
fn parses_info_metadata() {
    let info: [([u8; 4], &[u8]); 2] = [
        (*b"INAM", b"Wedeo Test Clip\0"),
        (*b"ISFT", b"wedeo-unit-test\0"),
    ];
    let data = build_avi(&[b"frame"], None, &info);
    let path = tmp_path("wedeo_avi_unit_info.avi");
    std::fs::write(&path, &data).unwrap();

    let input = InputContext::open(&path).unwrap();
    assert_eq!(input.metadata.get("INAM"), Some("Wedeo Test Clip"));
    assert_eq!(input.metadata.get("ISFT"), Some("wedeo-unit-test"));
}

#[test]
fn rejects_invalid_riff_header() {
    let data = b"RIFF\x00\x00\x00\x00WAVEfmt ";
    let path = tmp_path("wedeo_avi_unit_invalid.avi");
    std::fs::write(&path, data).unwrap();
    assert!(InputContext::open(&path).is_err());
}
