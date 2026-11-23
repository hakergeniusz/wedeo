//! Conformance/registration tests for `wedeo-filter-pad`.
//!
//! Owned by the subagent implementing this work unit. Verifies that the `pad`
//! filter is registered with the framework and that its output pixels and
//! metadata match FFmpeg's `vf_pad` behavior on synthetic frames.

// Ensure the registration is linked into the test binary.
use wedeo_filter_pad as _;

use wedeo_core::buffer::Buffer;
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::metadata::Metadata;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;
use wedeo_filter_pad::{Filter, PadColor, PadFilter, find_filter_by_name};

/// Build a YUV420p frame with a distinct constant value on each plane.
fn make_frame(w: u32, h: u32) -> Frame {
    let cw = (w / 2) as usize;
    let ch = (h / 2) as usize;
    let mut frame = Frame::new_video(w, h, PixelFormat::Yuv420p);
    frame.pts = 7;
    frame.duration = 2;
    frame.time_base = Rational::new(1, 25);
    frame.metadata = Metadata::new();
    frame.metadata.set("source", "synthetic");
    if let FrameData::Video(ref mut video) = frame.data {
        video.planes = vec![
            FramePlane {
                buffer: Buffer::from_slice(&vec![100u8; (w * h) as usize]),
                offset: 0,
                linesize: w as usize,
            },
            FramePlane {
                buffer: Buffer::from_slice(&vec![90u8; cw * ch]),
                offset: 0,
                linesize: cw,
            },
            FramePlane {
                buffer: Buffer::from_slice(&vec![160u8; cw * ch]),
                offset: 0,
                linesize: cw,
            },
        ];
    }
    frame
}

fn pixel(frame: &Frame, plane: usize, x: usize, y: usize) -> u8 {
    let video = frame.video().unwrap();
    let p = &video.planes[plane];
    let (hsub, vsub) = if plane == 0 { (0, 0) } else { (1, 1) };
    p.buffer.data()[p.offset + (y >> vsub) * p.linesize + (x >> hsub)]
}

#[test]
fn pad_filter_is_registered() {
    let factory = find_filter_by_name("pad").expect("pad filter not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "pad");
    assert_eq!(desc.description, "Pad the input video.");
    assert_eq!(desc.inputs.len(), 1);
    assert_eq!(desc.outputs.len(), 1);
    let mut filter = factory.create().expect("failed to create pad filter");
    filter.activate().unwrap();
    assert_eq!(filter.descriptor().name, "pad");
}

#[test]
fn pad_black_border_and_preserves_metadata() {
    let input = make_frame(4, 4);
    let mut filter = PadFilter::with_color(8, 8, 2, 2, PadColor::BLACK);
    filter.activate().unwrap();
    let out = filter.filter_frame(input).unwrap();
    assert_eq!(out.len(), 1);
    let out = &out[0];
    let video = out.video().unwrap();
    assert_eq!((video.width, video.height), (8, 8));
    assert_eq!(video.planes.len(), 3);
    // FFmpeg's default black for limited-range YUV420p: Y=16, U=V=128.
    assert_eq!(pixel(out, 0, 0, 0), 16);
    assert_eq!(pixel(out, 1, 0, 0), 128);
    assert_eq!(pixel(out, 2, 0, 0), 128);
    assert_eq!(pixel(out, 0, 7, 7), 16);
    // Original pixels are copied at the requested offset.
    assert_eq!(pixel(out, 0, 2, 2), 100);
    assert_eq!(pixel(out, 1, 2, 2), 90);
    assert_eq!(pixel(out, 2, 2, 2), 160);
    assert_eq!(pixel(out, 0, 5, 5), 100);
    // Metadata survives the transform.
    assert_eq!(out.pts, 7);
    assert_eq!(out.duration, 2);
    assert_eq!(out.metadata.get("source"), Some("synthetic"));
}

#[test]
fn pad_expression_options() {
    let input = make_frame(4, 4);
    let mut filter = PadFilter::from_options([
        ("w", "iw+2"),
        ("h", "ih+2"),
        ("x", "1"),
        ("y", "1"),
        ("color", "white"),
    ])
    .unwrap();
    let out = filter.filter_frame(input).unwrap().remove(0);
    let video = out.video().unwrap();
    // x/y=1 round down to 0 for 4:2:0; dimensions stay even.
    assert_eq!((video.width, video.height), (6, 6));
    assert_eq!(pixel(&out, 0, 0, 0), 100);
}
