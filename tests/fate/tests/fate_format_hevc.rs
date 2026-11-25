//! Conformance tests for `wedeo-format-hevc`.
//!
//! Covers registration/probe at priority 100, lossless raw Annex B demuxing,
//! bitexact packet CRC/size parity against FFmpeg's `-c copy` framecrc for
//! FATE HEVC vectors, hvcC extradata generation, and a smoke test that
//! `wedeo-framecrc` can now open raw `.hevc` directly. Samples are read-only
//! from `$FATE_SUITE`; tests skip cleanly when the suite or `ffmpeg` is
//! unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;

// Link the demuxer registration into this test binary.
use wedeo_format_hevc as _;

use wedeo_codec_hevc_ps::{NalUnitType, annex_b_split, parse_nal_header};
use wedeo_core::codec_id::CodecId;
use wedeo_format::context::InputContext;
use wedeo_format::demuxer::{PROBE_SCORE_MAX, ProbeData};
use wedeo_format::registry::{find_demuxer_by_name, probe};
use wedeo_format_hevc::annexb_to_hvcc;

fn fate_path(rel: &str) -> Option<PathBuf> {
    let suite = std::env::var("FATE_SUITE").ok()?;
    let path = Path::new(&suite).join(rel);
    path.exists().then_some(path)
}

fn framecrc_bin() -> String {
    env!("CARGO_BIN_EXE_wedeo-framecrc").to_string()
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

/// Parse the per-packet `(size, crc)` pairs from an FFmpeg framecrc output.
fn parse_packet_crcs(output: &str) -> Vec<(usize, u32)> {
    output
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 6 {
                return None;
            }
            let size = fields[4].parse::<usize>().ok()?;
            let crc = u32::from_str_radix(fields[5].strip_prefix("0x")?, 16).ok()?;
            Some((size, crc))
        })
        .collect()
}

/// Demux all packets through wedeo and return their `(size, crc)` pairs.
fn wedeo_packet_crcs(path: &Path) -> Vec<(usize, u32)> {
    let mut ctx = match InputContext::open(path.to_str().unwrap()) {
        Ok(ctx) => ctx,
        Err(e) => panic!("open {}: {e}", path.display()),
    };
    let mut out = Vec::new();
    while let Ok(pkt) = ctx.read_packet() {
        let data = pkt.data.data();
        out.push((data.len(), framecrc_adler32(data)));
    }
    out
}

/// FFmpeg `-c copy` framecrc: parser output packets in Annex B form.
fn ffmpeg_packet_crcs(path: &Path) -> Option<Vec<(usize, u32)>> {
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-bitexact",
            "-i",
            path.to_str()?,
            "-c",
            "copy",
            "-f",
            "framecrc",
            "-",
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_packet_crcs(&String::from_utf8_lossy(&output.stdout)))
}

#[test]
fn hevc_demuxer_is_registered_at_priority_100() {
    let factory = find_demuxer_by_name("hevc").expect("hevc demuxer must be registered");
    assert_eq!(factory.descriptor().name, "hevc");
    assert_eq!(factory.descriptor().priority, 100);
    assert!(factory.descriptor().extensions.contains("hevc"));
}

#[test]
fn probe_detects_hevc_annexb() {
    // VPS (nal_unit_type 32) right after an Annex B start code.
    let data = [0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c];
    let probe_data = ProbeData {
        filename: "anonymous.bin",
        buf: &data,
    };
    assert_eq!(
        probe(&probe_data).map(|f| f.descriptor().name),
        Some("hevc")
    );

    // H.264 SPS (nal_unit_type 7) must not get a definitive HEVC score.
    let h264 = [0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00];
    let probe_data = ProbeData {
        filename: "anonymous.bin",
        buf: &h264,
    };
    let factory = find_demuxer_by_name("hevc").unwrap();
    assert!(factory.probe(&probe_data) < PROBE_SCORE_MAX);
}

#[test]
fn hvcc_is_generated_from_fate_parameter_sets() {
    let Some(path) = fate_path("hevc/ipcm_A_NEC_2.bit") else {
        return;
    };
    let Ok(data) = std::fs::read(&path) else {
        return;
    };
    let ps_nals: Vec<&[u8]> = annex_b_split(&data)
        .into_iter()
        .filter(|n| {
            parse_nal_header(n).is_ok_and(|(h, _)| {
                matches!(
                    h.nal_unit_type,
                    NalUnitType::Vps | NalUnitType::Sps | NalUnitType::Pps
                )
            })
        })
        .collect();
    let hvcc = annexb_to_hvcc(&ps_nals).expect("hvcC generation from Annex B PS");
    assert_eq!(hvcc[0], 1); // configurationVersion
    assert_eq!(hvcc[21] & 0x03, 3); // lengthSizeMinusOne = 4-byte lengths
    assert!(hvcc[22] >= 3); // VPS, SPS and PPS arrays
}

/// Compare demuxed packet bytes bitexactly against FFmpeg's parser. This is
/// the demuxer's own conformance surface; decoder pixel parity is a separate
/// work unit and is intentionally not asserted here.
#[test]
fn packet_crcs_match_ffmpeg_copy_for_fate_vectors() {
    let vectors = [
        "hevc/cbf_cr_cb_TUDepth_4_circle.h265",
        "hevc/flush-reuse.hevc",
        "hevc/pir.hevc",
        "hevc/tile_row_order_multislice.hevc",
        "hevc-conformance/AMP_A_Samsung_4.bit",
        "hevc-conformance/ipcm_A_NEC_2.bit",
    ];
    let mut compared = 0usize;
    let mut packets = 0usize;
    for rel in vectors {
        let Some(path) = fate_path(rel) else {
            continue;
        };
        let Some(reference) = ffmpeg_packet_crcs(&path) else {
            continue;
        };
        let actual = wedeo_packet_crcs(&path);
        assert_eq!(
            actual.len(),
            reference.len(),
            "packet count for {rel}: wedeo {} vs ffmpeg {}",
            actual.len(),
            reference.len()
        );
        for (i, (a, f)) in actual.iter().zip(reference.iter()).enumerate() {
            assert_eq!(a.0, f.0, "packet {i} size for {rel}");
            assert_eq!(a.1, f.1, "packet {i} CRC for {rel}");
        }
        packets += actual.len();
        compared += 1;
    }
    wedeo_fate::skip_if_no_samples!(compared, "HEVC packet CRC");
    eprintln!("PACKET_CONFORMANCE compared={compared} packets={packets}");
}

#[test]
fn all_fate_hevc_packet_crcs_match_ffmpeg() {
    let Ok(suite) = std::env::var("FATE_SUITE") else {
        return;
    };
    let mut total = 0usize;
    let mut matched = 0usize;
    let mut mismatched: Vec<String> = Vec::new();
    let mut packets = 0usize;
    for dir in ["hevc", "hevc-conformance"] {
        let Ok(entries) = std::fs::read_dir(Path::new(&suite).join(dir)) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                matches!(ext, "hevc" | "h265" | "bit" | "265")
                    && std::fs::metadata(p).is_ok_and(|m| m.len() < 3_000_000)
            })
            .collect();
        paths.sort();
        for path in paths {
            let Some(reference) = ffmpeg_packet_crcs(&path) else {
                continue;
            };
            let actual = std::panic::catch_unwind(|| wedeo_packet_crcs(&path));
            total += 1;
            match actual {
                Ok(a) if a == reference => {
                    matched += 1;
                    packets += a.len();
                }
                Ok(a) => mismatched.push(format!(
                    "{}: wedeo {} pkts vs ffmpeg {} pkts",
                    path.display(),
                    a.len(),
                    reference.len()
                )),
                Err(_) => mismatched.push(format!("{}: PANIC", path.display())),
            }
        }
    }
    eprintln!("PACKET_SURVEY total={total} matched={matched} packets={packets}");
    wedeo_fate::skip_if_no_samples!(total, "HEVC packet survey");
    assert!(
        mismatched.is_empty(),
        "{} of {total} HEVC vectors mismatched: {}",
        mismatched.len(),
        mismatched[..mismatched.len().min(10)].join("; ")
    );
}

/// `wedeo-framecrc` used to fail to open raw `.hevc`; it must now select the
/// `hevc` demuxer and emit valid framecrc headers plus decoded frames.
#[test]
fn framecrc_opens_raw_hevc_and_emits_frames() {
    let Some(path) = fate_path("hevc/cbf_cr_cb_TUDepth_4_circle.h265") else {
        return;
    };
    let output = Command::new(framecrc_bin()).arg(&path).output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "framecrc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("#tb 0: 1/25"), "headers: {stdout}");
    assert!(stdout.contains("#dimensions 0: 64x64"), "headers: {stdout}");
    let frames = parse_packet_crcs(&stdout);
    assert!(!frames.is_empty(), "no frames in framecrc output: {stdout}");
    assert_eq!(frames[0].0, 12288, "frame size: {stdout}");
}

/// Demuxing must preserve every byte of the elementary stream.
#[test]
fn demux_is_lossless() {
    let Some(path) = fate_path("hevc/flush-reuse.hevc") else {
        return;
    };
    let original = std::fs::read(&path).unwrap();
    let mut ctx = InputContext::open(path.to_str().unwrap()).unwrap();
    assert_eq!(ctx.streams[0].codec_params.codec_id, CodecId::Hevc);
    let mut reassembled = Vec::new();
    let mut count = 0;
    while let Ok(pkt) = ctx.read_packet() {
        assert_eq!(pkt.pts, count);
        assert_eq!(pkt.dts, count);
        reassembled.extend_from_slice(pkt.data.data());
        count += 1;
    }
    assert!(count > 0);
    assert_eq!(reassembled, original);
}
