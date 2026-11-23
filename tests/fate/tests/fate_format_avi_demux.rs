//! Conformance tests for `wedeo-format-avi-demux`.
//!
//! Registration, probe, and bitexact packet-CRC comparison against
//! `ffmpeg -bitexact -i <FATE AVI sample> -c copy -f framecrc -`.
//! Comparing in stream-copy mode isolates the demuxer from decoder behaviour:
//! every CRC and size must match FFmpeg's packet extraction exactly.

use wedeo_format_avi_demux as _;

use std::path::{Path, PathBuf};
use std::process::Command;

use adler2::Adler32;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;
use wedeo_format::context::InputContext;
use wedeo_format::demuxer::{PROBE_SCORE_MAX, ProbeData};
use wedeo_format::registry::{demuxers, find_demuxer_by_name};

fn fate_sample(relative: &str) -> Option<PathBuf> {
    let dir = std::env::var("FATE_SUITE").unwrap_or_else(|_| "fate-suite".to_string());
    let path = PathBuf::from(dir).join(relative);
    path.exists().then_some(path)
}

/// FFmpeg's `av_adler32_update(0, ...)`: s1 = s2 = 0, not RFC 1950's s1 = 1.
fn adler32(data: &[u8]) -> u32 {
    let mut hasher = Adler32::from_checksum(0);
    hasher.write_slice(data);
    hasher.checksum()
}

struct Pkt {
    stream: usize,
    size: usize,
    crc: u32,
}

fn wedeo_packets(path: &Path) -> Vec<Pkt> {
    let mut input = InputContext::open(path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
    let mut packets = Vec::new();
    loop {
        match input.read_packet() {
            Ok(p) => packets.push(Pkt {
                stream: p.stream_index,
                size: p.data.size(),
                crc: adler32(p.data.data()),
            }),
            Err(Error::Eof) => break,
            Err(e) => panic!("reading {}: {e}", path.display()),
        }
    }
    packets
}

fn ffmpeg_packets(path: &Path) -> Vec<Pkt> {
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-bitexact",
            "-i",
            path.to_str().unwrap(),
            "-c",
            "copy",
            "-f",
            "framecrc",
            "-",
        ])
        .output()
        .expect("failed to run ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            assert!(fields.len() >= 6, "malformed framecrc line: {line}");
            let stream: usize = fields[0].parse().unwrap();
            let size: usize = fields[4].parse().unwrap();
            let crc = u32::from_str_radix(fields[5].trim_start_matches("0x"), 16).unwrap();
            Pkt { stream, size, crc }
        })
        .collect()
}

fn compare_packets(relative: &str, stream_filter: Option<usize>) {
    let Some(path) = fate_sample(relative) else {
        eprintln!("SKIP: FATE sample {relative} not found");
        return;
    };
    let wedeo: Vec<Pkt> = wedeo_packets(&path)
        .into_iter()
        .filter(|p| stream_filter.is_none_or(|s| p.stream == s))
        .collect();
    let ffmpeg: Vec<Pkt> = ffmpeg_packets(&path)
        .into_iter()
        .filter(|p| stream_filter.is_none_or(|s| p.stream == s))
        .collect();
    assert!(
        !wedeo.is_empty(),
        "wedeo produced no packets for {}",
        path.display()
    );
    assert_eq!(
        wedeo.len(),
        ffmpeg.len(),
        "packet count mismatch for {}",
        path.display()
    );
    for (i, (w, f)) in wedeo.iter().zip(ffmpeg.iter()).enumerate() {
        assert_eq!(
            (w.stream, w.size, w.crc),
            (f.stream, f.size, f.crc),
            "packet {i} mismatch for {}\n  wedeo:  stream={} size={} crc=0x{:08x}\n  ffmpeg: stream={} size={} crc=0x{:08x}",
            path.display(),
            w.stream,
            w.size,
            w.crc,
            f.stream,
            f.size,
            f.crc
        );
    }
}

#[test]
fn registration_and_probe() {
    let factory = find_demuxer_by_name("avi").expect("avi demuxer not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "avi");
    assert_eq!(desc.long_name, "AVI (Audio Video Interleaved)");
    assert_eq!(desc.extensions, "avi");
    assert_eq!(desc.mime_types, "video/x-msvideo");
    assert!(factory.create().is_ok());
    assert!(demuxers().any(|f| f.descriptor().name == "avi"));

    let mut buf = Vec::from(b"RIFF\x00\x00\x00\x00AVI ");
    let data = ProbeData {
        filename: "clip.avi",
        buf: &buf,
    };
    assert_eq!(factory.probe(&data), PROBE_SCORE_MAX);
    buf[8..12].copy_from_slice(b"WAVE");
    let data = ProbeData {
        filename: "clip.avi",
        buf: &buf,
    };
    assert_eq!(factory.probe(&data), 0);
}

#[test]
fn bitexact_fate_avi_h264_mp3() {
    // FFmpeg's MP3 parser splits some audio chunks into several packets in
    // `-c copy` mode; the AVI chunks themselves are compared video-only.
    compare_packets("h264/ref_10.avi", Some(0));
}

#[test]
fn bitexact_fate_avi_mpeg4() {
    compare_packets("mpeg4/packed_bframes.avi", None);
}

#[test]
fn bitexact_fate_avi_vcr2_pcm() {
    compare_packets("vcr2/VCR2test.avi", Some(0));
}

#[test]
fn bitexact_fate_avi_sp5x_pcm() {
    compare_packets("sp5x/sp5x_problem.avi", Some(0));
}

#[test]
fn fate_avi_headers_are_parsed() {
    let Some(path) = fate_sample("h264/ref_10.avi") else {
        eprintln!("SKIP: FATE sample not found");
        return;
    };
    let input = InputContext::open(path.to_str().unwrap()).unwrap();
    assert_eq!(input.nb_streams(), 2);

    let video = input.stream(0).unwrap();
    assert_eq!(video.codec_params.codec_id, CodecId::H264);
    assert_eq!(video.codec_params.codec_tag, u32::from_le_bytes(*b"SMV2"));
    assert_eq!(video.codec_params.media_type, MediaType::Video);
    assert_eq!(video.codec_params.width, 320);
    assert_eq!(video.codec_params.height, 240);
    assert_eq!(video.time_base, Rational::new(100, 2397));
    assert_eq!(video.nb_frames, 30);

    let audio = input.stream(1).unwrap();
    assert_eq!(audio.codec_params.media_type, MediaType::Audio);
    assert_eq!(audio.codec_params.codec_id, CodecId::Mp3);
    assert_eq!(audio.codec_params.sample_rate, 44100);
}
