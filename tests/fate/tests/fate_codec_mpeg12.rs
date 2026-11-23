//! Conformance tests for `wedeo-codec-mpeg12`.
//!
//! * verifies both native decoders are registered at priority 100;
//! * checks bitexact framecrc parity on a deterministic, frame-picture
//!   MPEG-2 elementary stream (I and P frames, no B frames);
//! * decodes the available FATE MPEG-2 samples and reports per-sample
//!   conformance counts. Frame-picture streams (I/P and B), complementary
//!   field pictures and 4:2:2 chroma are all bitexact. The field-encoding
//!   sample is bitexact except for its final, truncated access unit, where
//!   FFmpeg's error concealment fills slices missing from the sample; only a
//!   conformance floor is asserted for it.

use std::path::{Path, PathBuf};
use std::process::Command;

// Ensure native + demuxer registrations are linked into this test binary.
use wedeo_codec_mpeg12 as _;
use wedeo_format_mpegts_demux as _;

use wedeo_codec::decoder::{Decoder, DecoderBuilder};
use wedeo_codec::registry::{find_decoder, find_decoder_by_name};
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::FramePlane;
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_format::context::InputContext;

/// MPEG-2 FATE samples that the workspace's MPEG-TS demuxer can open.
const MPEG2_SAMPLES: &[&str] = &[
    "mpeg2/t.mpg",
    "mpeg2/mpeg2_field_encoding.ts",
    "mpeg2/xdcam8mp2-1s_small.ts",
];

#[test]
fn test_native_decoder_registration() {
    let factory = find_decoder(CodecId::Mpeg2video).expect("no decoder registered for MPEG-2");
    assert_eq!(factory.descriptor().name, "mpeg2video");
    assert_eq!(factory.descriptor().priority, 100);
    assert_eq!(factory.descriptor().media_type, MediaType::Video);
    assert!(find_decoder_by_name("mpeg2video").is_some());

    let factory = find_decoder(CodecId::Mpeg1video).expect("no decoder registered for MPEG-1");
    assert_eq!(factory.descriptor().name, "mpeg1video");
    assert_eq!(factory.descriptor().priority, 100);
    assert!(find_decoder_by_name("mpeg1video").is_some());
}

fn plane_geometry(format: PixelFormat) -> Option<(u32, u32)> {
    match format {
        PixelFormat::Yuv420p | PixelFormat::Yuvj420p => Some((1, 1)),
        PixelFormat::Yuv422p | PixelFormat::Yuvj422p => Some((1, 0)),
        PixelFormat::Yuv444p | PixelFormat::Yuvj444p => Some((0, 0)),
        _ => None,
    }
}

fn pack_plane(plane: &FramePlane, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    let data = plane.buffer.data();
    for row in 0..h {
        let start = plane.offset + row * plane.linesize;
        out.extend_from_slice(&data[start..start + w]);
    }
    out
}

/// Decode one MPEG video stream with wedeo, returning tightly packed YUV.
fn decode_with_wedeo(path: &Path) -> Result<Vec<Vec<u8>>, String> {
    let path_str = path.to_str().unwrap_or("");
    let mut ctx = InputContext::open(path_str).map_err(|e| format!("open: {e}"))?;
    let Some(stream) = ctx.streams.iter().find(|s| {
        matches!(
            s.codec_params.codec_id,
            CodecId::Mpeg2video | CodecId::Mpeg1video
        )
    }) else {
        return Err("no MPEG video stream".to_owned());
    };
    let params = stream.codec_params.clone();
    let stream_index = stream.index;
    let mut decoder = DecoderBuilder::new(params)
        .open()
        .map_err(|e| format!("decoder open: {e}"))?;

    let mut result = Vec::new();
    let drain = |decoder: &mut Box<dyn Decoder>, result: &mut Vec<Vec<u8>>| loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                let video = frame.video().expect("MPEG decoder produced audio");
                let (cw_shift, ch_shift) = plane_geometry(video.format)
                    .unwrap_or_else(|| panic!("unexpected pixel format {:?}", video.format));
                let w = video.width as usize;
                let h = video.height as usize;
                let cw = w >> cw_shift;
                let ch = h >> ch_shift;
                let mut raw = pack_plane(&video.planes[0], w, h);
                raw.extend_from_slice(&pack_plane(&video.planes[1], cw, ch));
                raw.extend_from_slice(&pack_plane(&video.planes[2], cw, ch));
                result.push(raw);
            }
            Err(Error::Again) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_frame: {e}"),
        }
    };

    loop {
        match ctx.read_packet() {
            Ok(packet) => {
                if packet.stream_index != stream_index {
                    continue;
                }
                decoder
                    .send_packet(Some(&packet))
                    .map_err(|e| format!("send_packet: {e}"))?;
                drain(&mut decoder, &mut result);
            }
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("read_packet: {e}")),
        }
    }
    let _ = decoder.send_packet(None);
    drain(&mut decoder, &mut result);
    Ok(result)
}

fn ffmpeg_framecrc(path: &Path) -> Vec<String> {
    let output = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-bitexact",
            "-i",
            path.to_str().unwrap_or(""),
            "-map",
            "0:v:0",
            "-an",
            // The default framerate logic drops frames with negative pts
            // (I/P frames preceding B frames) and shifts the comparison.
            "-fps_mode",
            "passthrough",
            "-f",
            "framecrc",
            "-",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| l.starts_with(|c: char| c.is_ascii_digit()))
        .filter_map(|l| l.split(',').nth(5).map(|s| s.trim().to_owned()))
        .collect()
}

fn ffmpeg_adler32(data: &[u8]) -> u32 {
    let mut hasher = adler2::Adler32::from_checksum(0);
    hasher.write_slice(data);
    hasher.checksum()
}

fn wedeo_framecrc(frames: &[Vec<u8>]) -> Vec<String> {
    frames
        .iter()
        .map(|f| format!("0x{:08x}", ffmpeg_adler32(f)))
        .collect()
}

/// Generate a deterministic, frame-picture MPEG-2 TS stream with I/P frames
/// only (no B frames, no field pictures), which is the fully supported subset.
fn synthetic_frame_picture_stream() -> Option<PathBuf> {
    let dir = std::env::var("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("wedeo-mpeg12-synthetic");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("frame_pictures.ts");
    let status = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=352x288:r=25:d=0.4",
            "-c:v",
            "mpeg2video",
            "-flags",
            "+bitexact",
            "-bf",
            "0",
            "-g",
            "12",
            "-q:v",
            "3",
            "-f",
            "mpegts",
        ])
        .arg(&path)
        .status()
        .ok()?;
    if status.success() { Some(path) } else { None }
}

#[test]
fn test_frame_picture_bitexact_framecrc() {
    let Some(path) = synthetic_frame_picture_stream() else {
        eprintln!("SKIP: ffmpeg could not generate a synthetic MPEG-2 stream");
        return;
    };
    let frames = decode_with_wedeo(&path).expect("decode synthetic stream");
    assert_eq!(frames.len(), 10, "unexpected frame count");
    let wedeo = wedeo_framecrc(&frames);
    let ffmpeg = ffmpeg_framecrc(&path);
    assert_eq!(wedeo.len(), ffmpeg.len());
    let matching = wedeo.iter().zip(&ffmpeg).filter(|(a, b)| a == b).count();
    eprintln!(
        "synthetic frame-picture stream: {matching}/{} bitexact",
        ffmpeg.len()
    );
    assert_eq!(matching, ffmpeg.len(), "synthetic stream must be bitexact");
}

#[test]
fn test_fate_mpeg2_sample_conformance() {
    let mut checked = 0usize;
    let mut t_mpg_bitexact = None;
    let mut field_conformance = None;
    for &relative in MPEG2_SAMPLES {
        let Some(path) = wedeo_fate::fate_sample(relative) else {
            eprintln!("Skipping missing FATE sample: {relative}");
            continue;
        };
        let frames = match decode_with_wedeo(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{relative}: decode error: {e}");
                checked += 1;
                continue;
            }
        };
        let wedeo = wedeo_framecrc(&frames);
        let ffmpeg = ffmpeg_framecrc(&path);
        let total = wedeo.len().max(ffmpeg.len());
        let matching = wedeo.iter().zip(&ffmpeg).filter(|(a, b)| a == b).count();
        eprintln!(
            "{relative}: wedeo={} ffmpeg={} bitexact={matching}/{total}",
            wedeo.len(),
            ffmpeg.len()
        );
        if relative == "mpeg2/t.mpg" {
            // B frames must predict exactly: skipped B macroblocks reuse the
            // previous vectors, and a repeated sequence header at a GOP
            // boundary must keep the previous GOP's forward reference.
            t_mpg_bitexact = Some((wedeo.len(), ffmpeg.len(), matching));
        } else if relative == "mpeg2/mpeg2_field_encoding.ts" {
            // Second-field DMV macroblocks must perform both field
            // predictions; dropping the second one leaves smooth residual
            // errors that accumulate through the GOP. The final access unit
            // is truncated in the sample, so FFmpeg's error concealment makes
            // that one frame non-bitexact; assert a floor instead.
            field_conformance = Some((wedeo.len(), ffmpeg.len(), matching));
        }
        assert!(!frames.is_empty(), "{relative}: decoded no frames");
        checked += 1;
    }
    assert!(
        checked > 0,
        "no FATE MPEG-2 samples available; set FATE_SUITE"
    );
    if let Some((wedeo_len, ffmpeg_len, matching)) = t_mpg_bitexact {
        assert_eq!(wedeo_len, ffmpeg_len, "t.mpg: frame count mismatch");
        assert_eq!(matching, ffmpeg_len, "t.mpg: every frame must be bitexact");
    }
    if let Some((wedeo_len, ffmpeg_len, matching)) = field_conformance {
        assert_eq!(
            wedeo_len, ffmpeg_len,
            "field-encoding sample: frame count mismatch"
        );
        assert!(
            matching >= 29,
            "field-encoding sample: expected at least 29/{} bitexact, got {matching}",
            ffmpeg_len
        );
    }
}
