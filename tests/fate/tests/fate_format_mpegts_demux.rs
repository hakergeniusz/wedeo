//! Conformance tests for `wedeo-format-mpegts-demux`.
//!
//! Covers registration/probe, low-level demuxing, bitexact framecrc comparison
//! against FFmpeg for H264 FATE MPEG-TS samples, and bitexact MD5 comparison
//! of the demuxed elementary-stream payloads for audio samples. Samples are
//! read-only from `$FATE_SUITE`; tests skip cleanly when the suite or
//! `ffmpeg` is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;

// Link the demuxer registration into this test binary.
use wedeo_format_mpegts_demux as _;

use md5::{Digest, Md5};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;
use wedeo_format::context::InputContext;
use wedeo_format::demuxer::ProbeData;
use wedeo_format::registry::{find_demuxer_by_name, probe};

fn fate_path(rel: &str) -> Option<PathBuf> {
    let suite = std::env::var("FATE_SUITE").ok()?;
    let path = Path::new(&suite).join(rel);
    path.exists().then_some(path)
}

fn framecrc_bin() -> String {
    env!("CARGO_BIN_EXE_wedeo-framecrc").to_string()
}

/// Run `wedeo-framecrc` and return `(stdout, stderr)`.
fn wedeo_framecrc(path: &Path) -> Option<(String, String)> {
    let output = Command::new(framecrc_bin()).arg(path).output().ok()?;
    Some((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// Run FFmpeg's framecrc, optionally in stream-copy mode.
fn ffmpeg_framecrc(path: &Path, copy: bool) -> Option<String> {
    let mut args: Vec<&str> = vec!["-hide_banner", "-v", "error", "-bitexact", "-i"];
    args.push(path.to_str()?);
    if copy {
        args.extend_from_slice(&["-c", "copy"]);
    }
    args.extend_from_slice(&["-f", "framecrc", "-"]);
    let output = Command::new("ffmpeg").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// FFmpeg's `-f md5` muxer hashes the concatenation of all packet payloads.
fn ffmpeg_payload_md5(path: &Path, stream: Option<usize>) -> Option<String> {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-bitexact".into(),
        "-i".into(),
        path.to_str()?.to_string(),
    ];
    if let Some(index) = stream {
        args.push("-map".into());
        args.push(format!("0:{index}"));
    }
    args.extend(
        ["-c", "copy", "-f", "md5", "-"]
            .iter()
            .map(|s| (*s).to_string()),
    );
    let output = Command::new("ffmpeg").args(args).output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return None;
    }
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("MD5=").map(str::to_string))
}

/// Demux all packets of a stream and MD5 their concatenated payloads.
fn wedeo_payload_md5(path: &Path, stream_index: usize) -> (String, usize, u64) {
    let mut ctx = InputContext::open(path.to_str().unwrap()).unwrap();
    let mut hasher = Md5::new();
    let mut packets = 0usize;
    let mut bytes = 0u64;
    while let Ok(pkt) = ctx.read_packet() {
        if pkt.stream_index != stream_index {
            continue;
        }
        hasher.update(pkt.data.data());
        packets += 1;
        bytes += pkt.data.data().len() as u64;
    }
    (format!("{:x}", hasher.finalize()), packets, bytes)
}

/// FFmpeg's non-standard Adler-32 init (`s1 = s2 = 0`), as used by framecrc.
fn framecrc_adler32(data: &[u8]) -> u32 {
    let mut s1 = 0u32;
    let mut s2 = 0u32;
    for &b in data {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    (s2 << 16) | s1
}

/// Parse FFmpeg framecrc lines into `(stream, size, checksum)` tuples.
fn parse_framecrc(output: &str) -> Vec<(String, String, String)> {
    output
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 6 {
                return None;
            }
            Some((
                fields[0].to_string(),
                fields[4].to_string(),
                fields[5].to_string(),
            ))
        })
        .collect()
}

/// Compare the payload size and checksum of every decoded frame. Timestamps
/// are rescaled differently by FFmpeg's parsers (e.g. H264 90k -> 1/15), so
/// the comparison focuses on the actual frame bytes, which is what bitexact
/// demuxing + decoding must reproduce.
fn compare_framecrc(wedeo: &str, ffmpeg: &str, label: &str) {
    let w = parse_framecrc(wedeo);
    let f = parse_framecrc(ffmpeg);
    assert!(!w.is_empty(), "{label}: wedeo produced no framecrc lines");
    assert!(!f.is_empty(), "{label}: ffmpeg produced no framecrc lines");
    assert_eq!(
        w.len(),
        f.len(),
        "{label}: frame count differs (wedeo={}, ffmpeg={})",
        w.len(),
        f.len(),
    );
    for (i, (wline, fline)) in w.iter().zip(f.iter()).enumerate() {
        assert_eq!(
            wline, fline,
            "{label}: frame {i} data mismatch (wedeo={wline:?}, ffmpeg={fline:?})"
        );
    }
}

#[test]
fn demuxer_is_registered() {
    let factory = find_demuxer_by_name("mpegts").expect("mpegts demuxer not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "mpegts");
    assert_eq!(desc.long_name, "MPEG-TS (MPEG-2 Transport Stream)");
    assert_eq!(desc.extensions, "ts,m2ts,mts");
    assert_eq!(desc.mime_types, "video/mp2t");
    assert!(factory.create().is_ok());
}

#[test]
fn probe_prefers_mpegts() {
    // Twelve aligned 188-byte packets with sync bytes and non-zero
    // adaptation-field control are a definite MPEG-TS signature.
    let mut data = vec![0u8; 188 * 12];
    for i in 0..12 {
        let base = i * 188;
        data[base] = 0x47;
        data[base + 1] = 0x40; // PUSI, PID 0
        data[base + 3] = 0x10; // payload only
    }
    let probe_data = ProbeData {
        filename: "synthetic.ts",
        buf: &data,
    };
    let factory = probe(&probe_data).expect("mpegts probe must match");
    assert_eq!(factory.descriptor().name, "mpegts");
    assert_eq!(factory.probe(&probe_data), 100);
}

#[test]
fn fate_h264_demux_and_decoded_framecrc() {
    let Some(path) = fate_path("mpegts/h264small.ts") else {
        return;
    };
    let Some((wedeo, stderr)) = wedeo_framecrc(&path) else {
        panic!("failed to run wedeo-framecrc");
    };
    let Some(ffmpeg) = ffmpeg_framecrc(&path, false) else {
        return; // ffmpeg not installed
    };
    assert!(
        !stderr.contains("Error opening"),
        "demuxer failed to open sample: {stderr}"
    );
    compare_framecrc(&wedeo, &ffmpeg, "mpegts/h264small.ts");
}

#[test]
fn fate_h264_stream_metadata() {
    let Some(path) = fate_path("mpegts/h264small.ts") else {
        return;
    };
    let ctx = InputContext::open(path.to_str().unwrap()).unwrap();
    assert_eq!(ctx.nb_streams(), 1);
    let stream = ctx.stream(0).unwrap();
    assert_eq!(stream.codec_params.codec_id, CodecId::H264);
    assert_eq!(stream.codec_params.media_type, MediaType::Video);
    assert_eq!(stream.time_base, Rational::new(1, 90000));
}

#[test]
fn fate_payload_md5_matches_ffmpeg() {
    // Compare the *exact* elementary-stream bytes emitted by the demuxer:
    // FFmpeg's `-f md5` muxer hashes the concatenation of all packet
    // payloads, which is independent of how a parser later splits them.
    let candidates: &[(&str, usize)] =
        &[("mpegts/h264small.ts", 0), ("aac/latm_stereo_to_51.ts", 0)];
    let mut tested = 0;
    for &(rel, stream) in candidates {
        let Some(path) = fate_path(rel) else {
            continue;
        };
        let Some(ffmpeg_md5) = ffmpeg_payload_md5(&path, Some(stream)) else {
            continue;
        };
        let (wedeo_md5, packets, bytes) = wedeo_payload_md5(&path, stream);
        assert!(
            packets > 0,
            "{rel}: demuxer produced no packets for stream {stream}"
        );
        assert_eq!(
            wedeo_md5, ffmpeg_md5,
            "{rel} stream {stream}: payload MD5 mismatch ({packets} packets, {bytes} bytes)"
        );
        tested += 1;
    }
    assert!(
        tested > 0,
        "no FATE MPEG-TS payload MD5 references available"
    );
}

#[test]
fn fate_packet_boundaries_match_ffmpeg() {
    // At the demuxer level (no parser), every PES packet's size and Adler-32
    // must match FFmpeg's `-c copy -f framecrc` output exactly.
    let Some(path) = fate_path("mpegts/h264small.ts") else {
        return;
    };
    let Some(ffmpeg) = ffmpeg_framecrc(&path, true) else {
        return;
    };
    let expected = parse_framecrc(&ffmpeg);
    let mut ctx = InputContext::open(path.to_str().unwrap()).unwrap();
    let mut produced = Vec::new();
    while let Ok(pkt) = ctx.read_packet() {
        produced.push((
            pkt.stream_index.to_string(),
            pkt.data.data().len().to_string(),
            format!("0x{:08x}", framecrc_adler32(pkt.data.data())),
        ));
    }
    assert_eq!(
        produced.len(),
        expected.len(),
        "PES packet count differs (wedeo={}, ffmpeg={})",
        produced.len(),
        expected.len()
    );
    for (i, (w, f)) in produced.iter().zip(expected.iter()).enumerate() {
        assert_eq!(w, f, "PES packet {i} differs (wedeo={w:?}, ffmpeg={f:?})");
    }
}

#[test]
fn eof_is_reported_after_last_packet() {
    let Some(path) = fate_path("mpegts/h264small.ts") else {
        return;
    };
    let mut ctx = InputContext::open(path.to_str().unwrap()).unwrap();
    let mut packets = 0;
    while let Ok(pkt) = ctx.read_packet() {
        assert_eq!(pkt.stream_index, 0);
        packets += 1;
        assert!(packets < 100_000, "runaway demuxer");
    }
    assert!(packets > 10, "expected several demuxed PES packets");
    assert_eq!(ctx.read_packet().err(), Some(Error::Eof));
}
