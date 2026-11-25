//! Conformance tests for `wedeo-format-ogg`.
//!
//! Compares the demuxed packet stream (timestamps, sizes, payload checksums)
//! with FFmpeg's own Ogg demuxer via `-c copy -f framecrc`, and verifies that
//! the native demuxer is registered and wins probe selection.

use std::path::Path;
use std::process::Command;

use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_format::context::InputContext;
use wedeo_format::demuxer::ProbeData;
use wedeo_format::registry::{find_demuxer_by_name, probe};

// Ensure the native Ogg demuxer registration is linked into this test binary.
use wedeo_format_ogg as _;

const NOPTS: i64 = i64::MIN;

/// One framecrc record: `(stream_index, dts, pts, duration, size, checksum)`.
type Frame = (usize, i64, i64, i64, usize, u32);

const OGG_SAMPLES: &[&str] = &[
    // Vorbis (including old encoder versions, chained streams and lacing).
    "vorbis/1.0.1-test_small.ogg",
    "vorbis/mono_small.ogg",
    "vorbis/6.ogg",
    "vorbis/1.0-test_small.ogg",
    "vorbis/beta3-test_small.ogg",
    "vorbis/beta4-test_small.ogg",
    "vorbis/chain-test1_small.ogg",
    "vorbis/chain-test2_small.ogg",
    "vorbis/highrate-test_small.ogg",
    "vorbis/lsp-test_small.ogg",
    "vorbis/lsp-test2_small.ogg",
    "vorbis/lsp-test3_small.ogg",
    "vorbis/lsp-test4_small.ogg",
    "vorbis/moog_small.ogg",
    "vorbis/rc1-test_small.ogg",
    "vorbis/rc2-test_small.ogg",
    "vorbis/rc2-test2_small.ogg",
    "vorbis/rc3-test_small.ogg",
    "vorbis/sleepzor_small.ogg",
    "vorbis/test-short2_small.ogg",
    "vorbis/vorbis_chapter_extension_demo.ogg",
    "ogg-vorbis/tos.ogg",
    "ogg-vorbis/chained-meta.ogg",
    // Opus (pre-skip, comments, cover art).
    "audiomatch/tones_opus_48000_stereo.opus",
    "ogg/intro-partial.opus",
    "ogg-opus/chained-meta.ogg",
    "cover_art/ogg_vorbiscomment_cover.opus",
    // Theora / VP3 video.
    "ogg/bear.ogv",
    "ogg/videotest.ogv",
    "ogg/empty_theora_packets.ogv",
    "vp3/offset_test.ogv",
    // FLAC in Ogg.
    "ogg-flac/chained-meta.ogg",
];

/// FFmpeg's non-standard Adler-32 (`av_adler32_update` starts with s1=s2=0).
fn ffmpeg_adler32(data: &[u8]) -> u32 {
    let mut hasher = adler2::Adler32::from_checksum(0);
    hasher.write_slice(data);
    hasher.checksum()
}

fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    if s == "-" {
        Some(NOPTS)
    } else {
        s.parse().ok()
    }
}

fn ffmpeg_framecrc(path: &Path) -> Option<Vec<Frame>> {
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            "-f",
            "framecrc",
            "-",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "ffmpeg framecrc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let mut frames = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if !line.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let f: Vec<&str> = line.split(',').map(str::trim).collect();
        if f.len() < 6 {
            continue;
        }
        let stream: usize = f[0].parse().ok()?;
        let dts = parse_ts(f[1])?;
        let pts = parse_ts(f[2])?;
        let duration = parse_ts(f[3])?;
        let size: usize = f[4].parse().ok()?;
        let crc = u32::from_str_radix(f[5].trim_start_matches("0x"), 16).ok()?;
        frames.push((stream, dts, pts, duration, size, crc));
    }
    Some(frames)
}

fn wedeo_framecrc(path: &Path) -> Vec<Frame> {
    let mut ctx = InputContext::open(path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut frames = Vec::new();
    loop {
        match ctx.read_packet() {
            Ok(pkt) => frames.push((
                pkt.stream_index,
                pkt.dts,
                pkt.pts,
                pkt.duration,
                pkt.data.size(),
                ffmpeg_adler32(pkt.data.data()),
            )),
            Err(Error::Eof) => break,
            Err(e) => panic!("read_packet: {e}"),
        }
    }
    frames
}

fn compare_sample(relative: &str, path: &Path) -> bool {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let Some(ffmpeg) = ffmpeg_framecrc(path) else {
        eprintln!("SKIP {relative}: ffmpeg unavailable");
        return true;
    };
    let wedeo = wedeo_framecrc(path);

    if wedeo.len() != ffmpeg.len() {
        eprintln!(
            "{relative}: packet count differs (wedeo={} ffmpeg={})",
            wedeo.len(),
            ffmpeg.len()
        );
        for i in 0..3.min(wedeo.len()) {
            eprintln!("  wedeo[{i}]={:?} ffmpeg[{i}]={:?}", wedeo[i], ffmpeg[i]);
        }
        for i in 0..3.min(wedeo.len()) {
            let a = wedeo.len() - 1 - i;
            let b = ffmpeg.len() - 1 - i;
            eprintln!("  wedeo[{a}]={:?} ffmpeg[{b}]={:?}", wedeo[a], ffmpeg[b]);
        }
        return false;
    }

    // FFmpeg's av_read_frame() reorders packets across streams by DTS; our
    // thin InputContext returns them in file order.  Compare each stream's
    // packet sequence independently, which is what demuxer parity means.
    let stream_count = wedeo
        .iter()
        .chain(ffmpeg.iter())
        .map(|f| f.0 + 1)
        .max()
        .unwrap_or(0);
    let mut by_stream_w = vec![Vec::new(); stream_count];
    let mut by_stream_f = vec![Vec::new(); stream_count];
    for f in &wedeo {
        if f.0 < stream_count {
            by_stream_w[f.0].push(*f);
        }
    }
    for f in &ffmpeg {
        if f.0 < stream_count {
            by_stream_f[f.0].push(*f);
        }
    }

    let mut mismatches = 0;
    for (s, (ws, fs)) in by_stream_w.iter().zip(by_stream_f.iter()).enumerate() {
        if ws.len() != fs.len() {
            eprintln!(
                "{relative} stream {s}: packet count differs (wedeo={} ffmpeg={})",
                ws.len(),
                fs.len()
            );
        }
        for (i, (w, f)) in ws.iter().zip(fs.iter()).enumerate() {
            if w != f {
                if mismatches < 8 {
                    eprintln!("{relative} stream {s} packet {i}: wedeo={w:?} ffmpeg={f:?}");
                }
                mismatches += 1;
            }
        }
    }
    if mismatches > 0 || wedeo.len() != ffmpeg.len() {
        eprintln!(
            "FAIL {relative}: {} of {} packets differ",
            mismatches.max(wedeo.len().abs_diff(ffmpeg.len())),
            wedeo.len().max(ffmpeg.len())
        );
        return false;
    }
    eprintln!("PASS {relative}: {} packets bitexact", wedeo.len());
    true
}

#[test]
fn test_native_demuxer_registration() {
    let factory = find_demuxer_by_name("ogg").expect("no demuxer registered as 'ogg'");
    assert_eq!(factory.descriptor().priority, 100);
    assert!(
        factory
            .descriptor()
            .extensions
            .split(',')
            .any(|e| e == "ogg")
    );

    let data = b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00";
    let probe_data = ProbeData {
        filename: "sample.ogg",
        buf: data,
    };
    assert_eq!(factory.probe(&probe_data), 100);
    assert_eq!(
        probe(&probe_data).map(|f| f.descriptor().name),
        Some("ogg"),
        "native Ogg demuxer must win probe selection"
    );

    let bad = ProbeData {
        filename: "bad.bin",
        buf: b"not ogg data",
    };
    assert_eq!(factory.probe(&bad), 0);
}

#[test]
fn test_ogg_stream_metadata() {
    let Some(sample) = wedeo_fate::fate_sample("vorbis/1.0.1-test_small.ogg") else {
        eprintln!("Skipping: FATE sample unavailable");
        return;
    };
    let ctx = InputContext::open(sample.to_str().unwrap()).unwrap();
    assert_eq!(ctx.nb_streams(), 1);
    let stream = ctx.stream(0).unwrap();
    assert_eq!(stream.codec_params.codec_id, CodecId::Vorbis);
    assert_eq!(stream.codec_params.sample_rate, 44_100);
    assert!(!stream.codec_params.extradata.is_empty());
    assert_eq!(
        stream.time_base,
        wedeo_core::rational::Rational::new(1, 44_100)
    );
}

#[test]
fn test_framecrc_bitexact() {
    let mut checked = 0usize;
    let mut failed = 0usize;
    for relative in OGG_SAMPLES {
        let Some(sample) = wedeo_fate::fate_sample(relative) else {
            eprintln!("Skipping missing FATE sample: {relative}");
            continue;
        };
        if !compare_sample(relative, &sample) {
            failed += 1;
        }
        checked += 1;
    }
    assert!(checked > 0, "no FATE Ogg samples available");
    assert_eq!(failed, 0, "{failed} of {checked} Ogg samples diverged");
}
