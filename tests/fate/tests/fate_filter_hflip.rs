//! Conformance tests for `wedeo-filter-hflip`.
//!
//! Two layers of verification:
//! 1. The filter is discoverable through the `FilterFactory` registry and its
//!    descriptor matches FFmpeg's `ff_vf_hflip`.
//! 2. Synthetic frames are flipped and the resulting plane bytes are compared
//!    bit-exactly against system FFmpeg's `-vf hflip` on equivalent raw input.
//!    (When FFmpeg is unavailable, hardcoded golden bytes are still checked.)

use std::path::PathBuf;
use std::process::Command;

use wedeo_core::buffer::Buffer;
use wedeo_core::frame::{Frame, FramePlane};
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter_hflip::{FilterFormat, FormatsConfig, RegisteredFilter as Filter, registry};

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("wedeo-fate-hflip");
    std::fs::create_dir_all(&dir).expect("create tmp dir");
    dir
}

/// Build a video frame from tightly packed plane data. Each tuple is
/// `(plane bytes, linesize)`.
fn frame_from_raw(
    format: PixelFormat,
    width: u32,
    height: u32,
    planes: &[(&[u8], usize)],
) -> Frame {
    let mut frame = Frame::new_video(width, height, format);
    let video = frame.video_mut().unwrap();
    for &(data, linesize) in planes {
        video.planes.push(FramePlane {
            buffer: Buffer::from_slice(data),
            offset: 0,
            linesize,
        });
    }
    frame
}

/// Concatenate the plane data of a frame in plane order (tightly packed in
/// these tests).
fn raw_from_frame(frame: &Frame) -> Vec<u8> {
    let mut out = Vec::new();
    for plane in &frame.video().unwrap().planes {
        out.extend_from_slice(plane.buffer.data());
    }
    out
}

/// Run FFmpeg's rawvideo hflip on `raw` and return the tight plane bytes.
/// Returns `None` when FFmpeg is not installed or rejects the request.
fn ffmpeg_hflip(pix_fmt: &str, width: u32, height: u32, raw: &[u8], tag: &str) -> Option<Vec<u8>> {
    let dir = tmp_dir();
    let input = dir.join(format!("{tag}_in.raw"));
    let output = dir.join(format!("{tag}_out.raw"));
    std::fs::write(&input, raw).ok()?;

    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pix_fmt,
            "-s",
            &format!("{width}x{height}"),
            "-i",
            input.to_str().unwrap(),
            "-vf",
            "hflip",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pix_fmt,
            output.to_str().unwrap(),
        ])
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    std::fs::read(&output).ok()
}

fn registered_filter() -> Box<dyn Filter> {
    let factory = registry::find_filter_by_name("hflip").expect("hflip filter is not registered");
    let mut filter = factory.create().expect("failed to create hflip filter");
    filter.activate().expect("activate failed");
    filter
}

/// Flip one frame through the registered filter and return the single output.
fn flip(frame: Frame) -> Frame {
    let mut filter = registered_filter();
    let mut out = filter
        .filter_frame(frame)
        .expect("hflip filter_frame failed");
    assert_eq!(out.len(), 1, "hflip must emit exactly one frame");
    out.remove(0)
}

#[test]
fn hflip_registered_descriptor_matches_ffmpeg() {
    let factory = registry::find_filter_by_name("hflip").expect("hflip filter is not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "hflip");
    assert_eq!(desc.description, "Horizontally flip the input video.");
    assert_eq!(desc.inputs.len(), 1);
    assert_eq!(desc.inputs[0].name, "default");
    assert_eq!(desc.outputs.len(), 1);
    assert_eq!(desc.outputs[0].name, "default");
}

#[test]
fn hflip_preserves_frame_properties() {
    let mut frame = frame_from_raw(PixelFormat::Gray8, 4, 1, &[(&[1, 2, 3, 4], 4)]);
    frame.pts = 42;
    frame.duration = 7;
    frame.metadata.set("title", "flip me");

    let out = flip(frame);
    assert_eq!(out.pts, 42);
    assert_eq!(out.duration, 7);
    assert_eq!(out.metadata.get("title"), Some("flip me"));
    assert_eq!(raw_from_frame(&out), vec![4, 3, 2, 1]);
}

#[test]
fn hflip_yuv420p_bitexact_vs_ffmpeg() {
    let input = [1u8, 2, 3, 4, 5, 6, 7, 8, 10, 11, 20, 21];
    let frame = frame_from_raw(
        PixelFormat::Yuv420p,
        4,
        2,
        &[(&input[0..8], 4), (&input[8..10], 2), (&input[10..12], 2)],
    );
    let got = raw_from_frame(&flip(frame));
    let golden = [4u8, 3, 2, 1, 8, 7, 6, 5, 11, 10, 21, 20];
    assert_eq!(got, golden);
    if let Some(reference) = ffmpeg_hflip("yuv420p", 4, 2, &input, "yuv420p") {
        assert_eq!(got, reference, "hflip yuv420p differs from FFmpeg");
    }
}

#[test]
fn hflip_gray16le_bitexact_vs_ffmpeg() {
    let input = [1u8, 0, 2, 0, 3, 0];
    let frame = frame_from_raw(PixelFormat::Gray16le, 3, 1, &[(&input, 6)]);
    let got = raw_from_frame(&flip(frame));
    let golden = [3u8, 0, 2, 0, 1, 0];
    assert_eq!(got, golden);
    if let Some(reference) = ffmpeg_hflip("gray16le", 3, 1, &input, "gray16le") {
        assert_eq!(got, reference, "hflip gray16le differs from FFmpeg");
    }
}

#[test]
fn hflip_yuv420p16le_bitexact_vs_ffmpeg() {
    let input = [
        1u8, 0, 2, 0, 3, 0, 4, 0, // Y row 0
        5, 0, 6, 0, 7, 0, 8, 0, // Y row 1
        10, 0, 11, 0, // U
        20, 0, 21, 0, // V
    ];
    let frame = frame_from_raw(
        PixelFormat::Yuv420p16le,
        4,
        2,
        &[(&input[0..16], 8), (&input[16..20], 4), (&input[20..24], 4)],
    );
    let got = raw_from_frame(&flip(frame));
    let golden = [
        4u8, 0, 3, 0, 2, 0, 1, 0, 8, 0, 7, 0, 6, 0, 5, 0, 11, 0, 10, 0, 21, 0, 20, 0,
    ];
    assert_eq!(got, golden);
    if let Some(reference) = ffmpeg_hflip("yuv420p16le", 4, 2, &input, "yuv420p16le") {
        assert_eq!(got, reference, "hflip yuv420p16le differs from FFmpeg");
    }
}

#[test]
fn hflip_rgb24_bitexact_vs_ffmpeg() {
    let input = [1u8, 2, 3, 4, 5, 6];
    let frame = frame_from_raw(PixelFormat::Rgb24, 2, 1, &[(&input, 6)]);
    let got = raw_from_frame(&flip(frame));
    let golden = [4u8, 5, 6, 1, 2, 3];
    assert_eq!(got, golden);
    if let Some(reference) = ffmpeg_hflip("rgb24", 2, 1, &input, "rgb24") {
        assert_eq!(got, reference, "hflip rgb24 differs from FFmpeg");
    }
}

#[test]
fn hflip_nv12_bitexact_vs_ffmpeg() {
    let input = [1u8, 2, 3, 4, 10, 11, 12, 13];
    let frame = frame_from_raw(
        PixelFormat::Nv12,
        4,
        1,
        &[(&input[0..4], 4), (&input[4..8], 4)],
    );
    let got = raw_from_frame(&flip(frame));
    let golden = [4u8, 3, 2, 1, 12, 13, 10, 11];
    assert_eq!(got, golden);
    if let Some(reference) = ffmpeg_hflip("nv12", 4, 1, &input, "nv12") {
        assert_eq!(got, reference, "hflip nv12 differs from FFmpeg");
    }
}

#[test]
fn hflip_query_formats_matches_ffmpeg_filter() {
    let filter = registered_filter();
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();

    // Planar 4:2:0 and 4:2:2 (with chroma on separate planes) are accepted.
    for accepted in [
        PixelFormat::Yuv420p,
        PixelFormat::Yuv422p,
        PixelFormat::Yuv422p10le,
        PixelFormat::Nv12,
    ] {
        assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(accepted)));
        assert!(cfg.outputs[0].accepts(FilterFormat::Pixel(accepted)));
    }
    // AV_PIX_FMT_FLAG_BITSTREAM and packed 4:2:2 (both chroma components on
    // plane 0) are rejected by vf_hflip.c's query_formats().
    for rejected in [
        PixelFormat::None,
        PixelFormat::MonoWhite,
        PixelFormat::MonoBlack,
        PixelFormat::Yuyv422,
        PixelFormat::Uyvy422,
    ] {
        assert!(!cfg.outputs[0].accepts(FilterFormat::Pixel(rejected)));
    }
}

#[test]
fn hflip_factory_rejects_options_as_ffmpeg_does() {
    let factory = registry::find_filter_by_name("hflip").unwrap();
    assert!(factory.create_with_options(&[]).is_ok());
    assert!(factory.create_with_options(&[("dir", "1")]).is_err());
}
