//! Conformance tests for `wedeo-format-flv-demux`.
//!
//! Covers registration/probing, FLV header + tag parsing, AVC/AAC extradata
//! extraction, and bitexact comparison against FFmpeg on FATE FLV samples.
//!
//! Conformance check: packet-level CRC parity against
//! `ffmpeg -c copy -f framecrc` (tests the demuxer in isolation, independent of
//! decoder status).
//!
//! NOTE: decoded-video framecrc parity is not asserted here.  The wedeo H.264
//! decoder in this snapshot fails on FLV's avcC extradata path (the same NALs
//! decode bit-exact when remuxed to Annex B), so an end-to-end assertion would
//! measure the decoder rather than this demuxer.  This is tracked as a
//! cross-crate divergence in the final report.
//!
//! Samples come from $FATE_SUITE (read-only).  Tests skip when the suite or
//! ffmpeg is unavailable.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Per-packet tuple collected for bitexact comparison.
type PacketInfo = (i64, i64, u32, u32, bool);

// Ensure the demuxer registration is linked into the test binary.
use wedeo_format_flv_demux as _;

use wedeo_core::codec_id::CodecId;
use wedeo_core::media_type::MediaType;
use wedeo_format::context::InputContext;
use wedeo_format::demuxer::{PROBE_SCORE_MAX, ProbeData};
use wedeo_format::registry::{demuxers, find_demuxer_by_name};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fate_suite_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("FATE_SUITE") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .parent()?
        .join("fate-suite");
    p.is_dir().then_some(p)
}

fn sample(rel: &str) -> Option<PathBuf> {
    let path = fate_suite_dir()?.join(rel);
    path.exists().then_some(path)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Return the 6th comma-separated field (the CRC) of each data line.
fn crc_field(line: &str) -> Option<String> {
    if line.starts_with('#') || line.trim().is_empty() {
        return None;
    }
    line.split(',').nth(5).map(|f| f.trim().to_string())
}

/// Frame CRC values for the single video stream of `ffmpeg -map 0:v:0`.
fn ffmpeg_video_crcs(path: &Path, copy: bool) -> Vec<String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-bitexact", "-i"]);
    cmd.arg(path);
    cmd.args(["-map", "0:v:0"]);
    if copy {
        cmd.args(["-c", "copy"]);
    }
    cmd.args(["-f", "framecrc", "-"]);

    let output = cmd.output().expect("failed to run ffmpeg");
    assert!(
        output.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().filter_map(crc_field).collect()
}

fn adler32(data: &[u8]) -> u32 {
    let mut h = adler2::Adler32::from_checksum(0);
    h.write_slice(data);
    h.checksum()
}

/// Demux `path`, returning the index of the first video stream and the CRC of
/// each packet on that stream (AVCC-format bytes, matching `-c copy`).
fn wedeo_packet_crcs(path: &Path) -> (usize, Vec<u32>) {
    let mut ctx = InputContext::open(path.to_str().unwrap())
        .unwrap_or_else(|e| panic!("open {}: {e:?}", path.display()));
    let video_index = ctx
        .streams
        .iter()
        .find(|s| s.codec_params.media_type == MediaType::Video)
        .map(|s| s.index)
        .unwrap_or_else(|| panic!("no video stream in {}", path.display()));

    let mut crcs = Vec::new();
    loop {
        match ctx.read_packet() {
            Ok(pkt) if pkt.stream_index == video_index => {
                crcs.push(adler32(pkt.data.data()));
            }
            Ok(_) => {}
            Err(wedeo_core::error::Error::Eof) => break,
            Err(e) => panic!("read_packet on {}: {e:?}", path.display()),
        }
    }
    (video_index, crcs)
}

fn ffmpeg_packet_crcs(path: &Path) -> Vec<u32> {
    ffmpeg_video_crcs(path, true)
        .into_iter()
        .map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap())
        .collect()
}

fn assert_crc_sequences(name: &str, wedeo: &[u32], ffmpeg: &[u32]) {
    let total = wedeo.len().max(ffmpeg.len());
    let matching = wedeo
        .iter()
        .zip(ffmpeg.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let first_diff = (0..total.min(wedeo.len()).min(ffmpeg.len())).find(|&i| wedeo[i] != ffmpeg[i]);
    assert_eq!(
        wedeo.len(),
        ffmpeg.len(),
        "{name}: packet count mismatch (wedeo={}, ffmpeg={})",
        wedeo.len(),
        ffmpeg.len()
    );
    assert!(
        matching == total,
        "{name}: CRC mismatch at {:?} ({matching}/{total} bitexact)",
        first_diff
    );
}

// ---------------------------------------------------------------------------
// Registration / probing
// ---------------------------------------------------------------------------

#[test]
fn demuxer_is_registered() {
    let factory = find_demuxer_by_name("flv").expect("flv demuxer not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "flv");
    assert_eq!(desc.long_name, "FLV (Flash Video)");
    assert_eq!(desc.extensions, "flv");
    assert_eq!(desc.mime_types, "video/x-flv");
    assert!(factory.create().is_ok());
    assert!(demuxers().any(|f| f.descriptor().name == "flv"));
}

#[test]
fn probe_rejects_non_flv_and_live_flv() {
    let factory = find_demuxer_by_name("flv").unwrap();
    let mut buf = vec![0u8; 256];
    buf[0..3].copy_from_slice(b"FLV");
    buf[3] = 1;
    buf[5..9].copy_from_slice(&9u32.to_be_bytes());
    let data = ProbeData {
        filename: "test.flv",
        buf: &buf,
    };
    assert_eq!(factory.probe(&data), PROBE_SCORE_MAX);

    // Live FLV is handled by the separate `live_flv` demuxer.
    buf[49..59].copy_from_slice(b"NGINX RTMP");
    let data = ProbeData {
        filename: "test.flv",
        buf: &buf,
    };
    assert_eq!(factory.probe(&data), 0);

    let data = ProbeData {
        filename: "test.bin",
        buf: b"RIFF----WAVE",
    };
    assert_eq!(factory.probe(&data), 0);
}

#[test]
fn probe_fate_flv_samples() {
    let Some(dir) = fate_suite_dir() else { return };
    let factory = find_demuxer_by_name("flv").unwrap();
    let mut checked = 0;
    for entry in std::fs::read_dir(dir.join("flv")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("flv") {
            continue;
        }
        let buf = std::fs::read(&path).unwrap();
        let head = &buf[..buf.len().min(4096)];
        let data = ProbeData {
            filename: path.to_str().unwrap(),
            buf: head,
        };
        assert_eq!(
            factory.probe(&data),
            PROBE_SCORE_MAX,
            "probe failed for {}",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no FATE FLV samples probed");
}

// ---------------------------------------------------------------------------
// Demux structure checks
// ---------------------------------------------------------------------------

#[test]
fn demux_streamloop_header_and_packets() {
    let Some(path) = sample("flv/streamloop.flv") else {
        return;
    };
    let mut ctx = InputContext::open(path.to_str().unwrap()).unwrap();
    assert_eq!(ctx.nb_streams(), 1);
    let video = &ctx.streams[0];
    assert_eq!(video.codec_params.codec_id, CodecId::H264);
    assert_eq!(video.codec_params.media_type, MediaType::Video);
    assert_eq!(video.time_base.num, 1);
    assert_eq!(video.time_base.den, 1000);
    assert!(
        !video.codec_params.extradata.is_empty(),
        "AVC sequence header extradata missing"
    );
    assert_eq!(video.codec_params.extradata[0], 1); // configurationVersion

    let mut packets = 0;
    let mut keyframes = 0;
    loop {
        match ctx.read_packet() {
            Ok(pkt) => {
                assert_eq!(pkt.stream_index, 0);
                assert!(pkt.data.size() > 0);
                if pkt.flags.contains(wedeo_core::packet::PacketFlags::KEY) {
                    keyframes += 1;
                }
                packets += 1;
            }
            Err(wedeo_core::error::Error::Eof) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(packets, 25, "streamloop.flv frame count");
    assert!(keyframes > 0);
}

#[test]
fn demux_avc_aac_extradata_from_fate() {
    for rel in [
        "h264/test-4867.flv",
        "flv/Enigma_Principles_of_Lust-part.flv",
    ] {
        let Some(path) = sample(rel) else { continue };
        let mut ctx = InputContext::open(path.to_str().unwrap()).unwrap();
        assert_eq!(ctx.nb_streams(), 2, "{rel}: expected 2 streams");
        let video = ctx
            .streams
            .iter()
            .find(|s| s.codec_params.media_type == MediaType::Video)
            .expect("no video stream");
        let audio = ctx
            .streams
            .iter()
            .find(|s| s.codec_params.media_type == MediaType::Audio)
            .expect("no audio stream");
        assert_eq!(video.codec_params.codec_id, CodecId::H264, "{rel}");
        assert_eq!(audio.codec_params.codec_id, CodecId::Aac, "{rel}");
        assert!(!video.codec_params.extradata.is_empty(), "{rel}: no avcC");
        assert!(!audio.codec_params.extradata.is_empty(), "{rel}: no ASC");
        let _ = ctx.read_packet().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Bitexact conformance against FFmpeg
// ---------------------------------------------------------------------------

#[test]
fn bitexact_packet_crc_vs_ffmpeg_copy() {
    if !ffmpeg_available() {
        return;
    }
    for rel in [
        "flv/streamloop.flv",
        "h264/test-4867.flv",
        "h264/brokensps.flv",
        "flv/Enigma_Principles_of_Lust-part.flv",
    ] {
        let Some(path) = sample(rel) else { continue };
        let (_, wedeo) = wedeo_packet_crcs(&path);
        let ffmpeg = ffmpeg_packet_crcs(&path);
        assert_crc_sequences(rel, &wedeo, &ffmpeg);
    }
}

#[test]
fn diagnostic_all_flv_samples() {
    if !ffmpeg_available() {
        return;
    }
    let mut paths = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("flv") {
                out.push(p);
            }
        }
    }
    walk(&fate_suite_dir().unwrap(), &mut paths);
    paths.sort();
    for path in paths {
        let rel = path
            .strip_prefix(fate_suite_dir().unwrap())
            .unwrap()
            .display()
            .to_string();
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-bitexact", "-i"])
            .arg(&path)
            .args(["-map", "0", "-c", "copy", "-f", "framecrc", "-"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let mut ff: Vec<Vec<Vec<String>>> = vec![Vec::new(); 16];
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let f: Vec<String> = line.split(',').map(|x| x.trim().to_string()).collect();
            let si: usize = f[0].parse().unwrap();
            ff[si].push(f);
        }
        println!("=== {rel}: ffmpeg streams:");
        for line in text.lines().filter(|l| {
            l.starts_with("#media_type")
                || l.starts_with("#codec_id")
                || l.starts_with("#extradata")
        }) {
            println!("  {line}");
        }
        let mut ctx = match InputContext::open(path.to_str().unwrap()) {
            Ok(c) => c,
            Err(e) => {
                println!("  OPEN ERROR: {e:?}");
                continue;
            }
        };
        println!("  wedeo streams: {}", ctx.nb_streams());
        for st in &ctx.streams {
            println!(
                "   stream {}: {:?} {:?} tb={}/{} extradata={} dur={}",
                st.index,
                st.codec_params.media_type,
                st.codec_params.codec_id,
                st.time_base.num,
                st.time_base.den,
                st.codec_params.extradata.len(),
                st.duration
            );
        }
        let mut w: Vec<Vec<PacketInfo>> = vec![Vec::new(); ctx.streams.len()];
        loop {
            match ctx.read_packet() {
                Ok(p) => w[p.stream_index].push((
                    p.dts,
                    p.pts,
                    p.data.size() as u32,
                    adler32(p.data.data()),
                    p.flags.contains(wedeo_core::packet::PacketFlags::KEY),
                )),
                Err(wedeo_core::error::Error::Eof) => break,
                Err(e) => {
                    println!("  READ ERROR: {e:?}");
                    break;
                }
            }
        }
        for i in 0..ctx.streams.len() {
            let ffcrc: Vec<u32> = ff[i]
                .iter()
                .map(|f| u32::from_str_radix(f[5].trim_start_matches("0x"), 16).unwrap())
                .collect();
            let wcrc: Vec<u32> = w[i].iter().map(|x| x.3).collect();
            let eq = ffcrc == wcrc;
            let firstdiff = (0..ffcrc.len().min(wcrc.len())).find(|&j| ffcrc[j] != wcrc[j]);
            println!(
                "  stream {i}: ff={} wedeo={} crc_equal={eq} firstdiff={firstdiff:?}",
                ffcrc.len(),
                wcrc.len()
            );
            if !eq {
                for j in 0..ffcrc.len().min(wcrc.len()).min(3) {
                    println!(
                        "    [{j}] ff={:?} w={:?}",
                        (
                            ff[i][j][1].clone(),
                            ff[i][j][2].clone(),
                            ff[i][j][3].clone(),
                            ff[i][j][4].clone(),
                            ff[i][j][5].clone(),
                            ff[i][j].get(6).cloned()
                        ),
                        w[i][j]
                    );
                }
            }
            let mut tsdiff = 0;
            let mut flagdiff = 0;
            for j in 0..ffcrc.len().min(wcrc.len()) {
                if ffcrc[j] != wcrc[j] {
                    continue;
                }
                let fdts: i64 = ff[i][j][1].parse().unwrap();
                let fpts: i64 = ff[i][j][2].parse().unwrap();
                if fdts != w[i][j].0 || fpts != w[i][j].1 {
                    if tsdiff < 2 {
                        println!(
                            "    ts diff [{j}] ff dts/pts={fdts}/{fpts} w={:?}/{:?} dur ff={}",
                            w[i][j].0, w[i][j].1, ff[i][j][3]
                        );
                    }
                    tsdiff += 1;
                }
                let fkey = ff[i][j].len() == 6 || !ff[i][j][6].starts_with("F=");
                if fkey != w[i][j].4 {
                    if flagdiff < 2 {
                        println!("    flag diff [{j}] ff key={fkey} w={}", w[i][j].4);
                    }
                    flagdiff += 1;
                }
            }
            if tsdiff > 0 || flagdiff > 0 {
                println!("    tsdiff={tsdiff} flagdiff={flagdiff}");
            }
        }
    }
}
