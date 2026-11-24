//! Conformance / registration tests for the `vflip` video filter.
//!
//! FFmpeg reference: `libavfilter/vf_vflip.c`. The filter reverses the order
//! of the rows in every video plane (with chroma planes using the format's
//! vertical subsampling). The expected byte sequences below are derived
//! directly from that behaviour and are therefore bit-exact.

use wedeo_core::buffer::Buffer;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_filter_vflip::registry::find_filter_by_name;
use wedeo_filter_vflip::{FilterFlags, FilterFormat, FilterPadDirection, FormatsConfig};

fn make_frame(width: u32, height: u32, format: PixelFormat, planes: &[(&[u8], usize)]) -> Frame {
    let mut frame = Frame::new_video(width, height, format);
    if let Some(video) = frame.video_mut() {
        video.planes = planes
            .iter()
            .map(|(data, linesize)| FramePlane {
                buffer: Buffer::from_slice(data),
                offset: 0,
                linesize: *linesize,
            })
            .collect();
    }
    frame
}

fn run_vflip(frame: Frame) -> Frame {
    let factory =
        find_filter_by_name("vflip").expect("vflip filter must be registered via inventory");
    let mut filter = factory.create().expect("failed to create vflip filter");
    filter.activate().expect("activate failed");
    let mut out = filter
        .filter_frame(frame)
        .expect("vflip filter_frame failed");
    assert_eq!(out.len(), 1, "vflip must pass through exactly one frame");
    out.remove(0)
}

fn plane_bytes(frame: &Frame, index: usize) -> &[u8] {
    frame.video().unwrap().planes[index].buffer.data()
}

#[test]
fn test_vflip_registered() {
    let factory = find_filter_by_name("vflip").expect("vflip filter not registered");
    let desc = factory.descriptor();
    assert_eq!(desc.name, "vflip");
    assert_eq!(desc.description, "Flip the input video vertically.");
    assert_eq!(desc.inputs.len(), 1);
    assert_eq!(desc.inputs[0].name, "default");
    assert_eq!(desc.inputs[0].media_type, MediaType::Video);
    assert_eq!(desc.inputs[0].direction, FilterPadDirection::Input);
    assert_eq!(desc.outputs.len(), 1);
    assert_eq!(desc.outputs[0].direction, FilterPadDirection::Output);
    assert!(desc.flags.contains(FilterFlags::SUPPORT_TIMELINE_GENERIC));
}

/// Bitexact check: a YUV420P frame with a clearly identifiable value in each
/// row of every plane must come out with all three planes' rows reversed.
#[test]
fn test_vflip_yuv420p_bitexact() {
    let y: Vec<u8> = (1..=16).collect(); // 4 rows of 4
    let u: Vec<u8> = (101..=104).collect(); // 2 rows of 2
    let v: Vec<u8> = (201..=204).collect(); // 2 rows of 2
    let frame = make_frame(4, 4, PixelFormat::Yuv420p, &[(&y, 4), (&u, 2), (&v, 2)]);

    let out = run_vflip(frame);

    assert_eq!(
        plane_bytes(&out, 0),
        &[13, 14, 15, 16, 9, 10, 11, 12, 5, 6, 7, 8, 1, 2, 3, 4]
    );
    assert_eq!(plane_bytes(&out, 1), &[103, 104, 101, 102]);
    assert_eq!(plane_bytes(&out, 2), &[203, 204, 201, 202]);
}

/// Packed RGB24: flip whole linesize-sized rows, not individual pixels.
#[test]
fn test_vflip_rgb24_bitexact() {
    let data: Vec<u8> = (0..12).collect(); // 2 rows of 6 bytes (2 RGB pixels per row)
    let frame = make_frame(2, 2, PixelFormat::Rgb24, &[(&data, 6)]);

    let out = run_vflip(frame);

    assert_eq!(
        plane_bytes(&out, 0),
        &[6, 7, 8, 9, 10, 11, 0, 1, 2, 3, 4, 5]
    );
}

/// The plane stride (which may include padding) must be respected.
#[test]
fn test_vflip_stride_padding_bitexact() {
    let data: Vec<u8> = vec![
        1, 2, 0xff, 0xff, // row 0 (2 active bytes, 2 padding)
        3, 4, 0xff, 0xff, // row 1
        5, 6, 0xff, 0xff, // row 2
    ];
    let frame = make_frame(2, 3, PixelFormat::Gray8, &[(&data, 4)]);

    let out = run_vflip(frame);

    assert_eq!(
        plane_bytes(&out, 0),
        &[5, 6, 0xff, 0xff, 3, 4, 0xff, 0xff, 1, 2, 0xff, 0xff]
    );
}

/// Metadata, timestamps and side data must survive the filter untouched.
#[test]
fn test_vflip_preserves_frame_properties() {
    let data: Vec<u8> = (1..=8).collect();
    let mut frame = make_frame(2, 4, PixelFormat::Gray8, &[(&data, 2)]);
    frame.pts = 1234;
    frame.duration = 10;
    frame.metadata.set("title", "vflip test");

    let out = run_vflip(frame);

    assert_eq!(out.pts, 1234);
    assert_eq!(out.duration, 10);
    assert_eq!(out.metadata.get("title"), Some("vflip test"));
    assert_eq!(plane_bytes(&out, 0), &[7, 8, 5, 6, 3, 4, 1, 2]);
}

/// Applying the filter twice must restore the original image.
#[test]
fn test_vflip_double_flip_is_identity() {
    let y: Vec<u8> = (0..16).collect();
    let original = y.clone();
    let frame = make_frame(4, 4, PixelFormat::Gray8, &[(&y, 4)]);

    let out = run_vflip(run_vflip(frame));

    assert_eq!(plane_bytes(&out, 0), original.as_slice());
}

#[test]
fn test_vflip_rejects_audio_frames() {
    use wedeo_core::channel_layout::ChannelLayout;
    use wedeo_core::sample_format::SampleFormat;

    let frame = Frame::new_audio(128, SampleFormat::S16, 48000, ChannelLayout::stereo());
    let factory = find_filter_by_name("vflip").unwrap();
    let mut filter = factory.create().unwrap();
    assert_eq!(filter.filter_frame(frame).err(), Some(Error::InvalidData));
}

#[test]
fn test_vflip_query_formats_covers_all_real_pixel_formats() {
    let factory = find_filter_by_name("vflip").unwrap();
    let filter = factory.create().unwrap();
    let mut cfg = FormatsConfig::from_descriptor(filter.descriptor());
    filter.query_formats(&mut cfg).unwrap();

    // FFmpeg's vflip has no query_formats() callback, so all real formats are
    // accepted on both pads.
    for accepted in [
        PixelFormat::Yuv420p,
        PixelFormat::Yuyv422,
        PixelFormat::MonoWhite,
        PixelFormat::Pal8,
        PixelFormat::Nv12,
        PixelFormat::Yuv420p10le,
    ] {
        assert!(cfg.inputs[0].accepts(FilterFormat::Pixel(accepted)));
        assert!(cfg.outputs[0].accepts(FilterFormat::Pixel(accepted)));
    }
    assert!(!cfg.outputs[0].accepts(FilterFormat::Pixel(PixelFormat::None)));
    assert_eq!(cfg.inputs[0].formats, cfg.outputs[0].formats);
}

#[test]
fn test_vflip_factory_rejects_options_as_ffmpeg_does() {
    let factory = find_filter_by_name("vflip").unwrap();
    assert!(factory.create_with_options(&[]).is_ok());
    assert!(factory.create_with_options(&[("dir", "1")]).is_err());
}
