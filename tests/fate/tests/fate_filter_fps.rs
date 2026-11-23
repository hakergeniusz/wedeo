//! Conformance and registration tests for `wedeo-filter-fps`.
//!
//! The expected frame counts and timestamps below were produced with the
//! system FFmpeg (n9.1) using `-vf fps=N -f framecrc` on synthetic inputs:
//!
//! - VFR: frames at 0s, 1s and 3s (tb 1/10), `fps=5` -> 20 frames, pts 0..19
//! - Duplication: 1 fps input, 3 frames (0s, 1s, 2s), `fps=10` -> 30 frames,
//!   pts 0..29
//! - Dropping: 10 fps input, 10 frames (0s..0.9s), `fps=5` -> 5 frames,
//!   pts 0..4

use wedeo_core::frame::Frame;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_filter_fps::{FilterFlags, FilterTrait, FpsFilter, find_filter_by_name, is_registered};

fn video_frame(pts: i64, tb: Rational, duration: i64) -> Frame {
    let mut f = Frame::new_video(16, 16, PixelFormat::Yuv420p);
    f.pts = pts;
    f.time_base = tb;
    f.duration = duration;
    f
}

fn output_pts(in_tb: Rational, rate: Rational, frames: &[(i64, i64)]) -> Vec<i64> {
    let mut filter = FpsFilter::new(rate).unwrap();
    let mut out = Vec::new();
    for &(pts, duration) in frames {
        out.extend(
            filter
                .filter_frame(video_frame(pts, in_tb, duration))
                .unwrap(),
        );
    }
    out.extend(filter.finish());
    out.into_iter().map(|f| f.pts).collect()
}

#[test]
fn fps_filter_is_registered() {
    assert!(is_registered());
    let factory = find_filter_by_name("fps").expect("fps factory must be registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "fps");
    assert_eq!(desc.description, "Force constant framerate.");
    assert!(desc.flags.contains(FilterFlags::METADATA_ONLY));

    let filter = factory.create().unwrap();
    assert_eq!(filter.descriptor().name, "fps");
    assert_eq!(
        FpsFilter::with_default_rate().unwrap().target_rate(),
        Rational::new(25, 1)
    );
}

#[test]
fn fps_converts_vfr_to_cfr() {
    // FFmpeg: frames at 0s, 1s, 3s in tb 1/10 with fps=5 -> 20 frames 0..19.
    let pts = output_pts(
        Rational::new(1, 10),
        Rational::new(5, 1),
        &[(0, 10), (10, 10), (30, 10)],
    );
    assert_eq!(pts, (0..20).collect::<Vec<_>>());
}

#[test]
fn fps_duplicates_and_drops_frames() {
    // Duplication: 1 fps -> 10 fps yields 30 frames.
    let pts = output_pts(
        Rational::new(1, 1),
        Rational::new(10, 1),
        &[(0, 1), (1, 1), (2, 1)],
    );
    assert_eq!(pts, (0..30).collect::<Vec<_>>());

    // Dropping: 10 fps -> 5 fps yields 5 frames at 0..4.
    let frames: Vec<(i64, i64)> = (0..10).map(|p| (p, 1)).collect();
    let pts = output_pts(Rational::new(1, 10), Rational::new(5, 1), &frames);
    assert_eq!(pts, vec![0, 1, 2, 3, 4]);
}

#[test]
fn fps_output_format_and_metadata() {
    let mut filter = FpsFilter::with_fps(25, 1).unwrap();
    let mut a = video_frame(0, Rational::new(1, 25), 1);
    a.metadata.set("title", "fps-test");
    let mut b = video_frame(2, Rational::new(1, 25), 1);
    b.metadata.set("title", "fps-test");

    let mut out = filter.filter_frame(a).unwrap();
    out.extend(filter.filter_frame(b).unwrap());
    out.extend(filter.finish());

    assert!(!out.is_empty());
    for frame in &out {
        assert_eq!(frame.time_base, Rational::new(1, 25));
        assert_eq!(frame.duration, 1);
        assert_eq!(frame.metadata.get("title"), Some("fps-test"));
    }
}
