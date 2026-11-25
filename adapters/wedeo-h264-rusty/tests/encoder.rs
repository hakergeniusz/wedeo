//! End-to-end tests for the pure-Rust H.264 encoder adapter.

use wedeo_codec::encoder::EncoderBuilder;
use wedeo_codec::registry::find_encoder;
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FrameData, VideoFrameData};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::PacketFlags;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;

use wedeo_h264_rusty::create_h264_encoder;

fn make_frame(w: usize, h: usize, n: u8) -> Frame {
    let cw = w / 2;
    let ch = h / 2;
    let y: Vec<u8> = (0..w * h)
        .map(|i| ((i % w) as u8).wrapping_add(n.wrapping_mul(7)))
        .collect();
    let u: Vec<u8> = vec![128 + n % 16; cw * ch];
    let v: Vec<u8> = vec![128 - n % 16; cw * ch];
    let mut frame = Frame::new_video(w as u32, h as u32, PixelFormat::Yuv420p);
    frame.pts = n as i64;
    if let FrameData::Video(video) = &mut frame.data {
        video.planes = vec![
            wedeo_core::frame::FramePlane {
                buffer: Buffer::from_slice(&y),
                offset: 0,
                linesize: w,
            },
            wedeo_core::frame::FramePlane {
                buffer: Buffer::from_slice(&u),
                offset: 0,
                linesize: cw,
            },
            wedeo_core::frame::FramePlane {
                buffer: Buffer::from_slice(&v),
                offset: 0,
                linesize: cw,
            },
        ];
    }
    frame
}

#[test]
fn avcc_extradata_is_valid() {
    let mut builder = EncoderBuilder::new(CodecId::H264, MediaType::Video);
    builder.width = 64;
    builder.height = 48;
    builder.pixel_format = PixelFormat::Yuv420p;
    builder.time_base = Rational::new(1, 30);

    let (_, extradata) = create_h264_encoder(&builder).expect("encoder");
    assert!(extradata.len() >= 11, "avcC too short: {}", extradata.len());
    assert_eq!(extradata[0], 1, "configurationVersion");
    assert_eq!(extradata[4], 0xff, "lengthSizeMinusOne = 3");
    assert_eq!(extradata[5], 0xe1, "one SPS");
    // SPS NAL follows its 2-byte big-endian length at offsets 6..8.
    assert_eq!(extradata[8] & 0x1f, 7, "SPS NAL type");
}

#[test]
fn encodes_yuv420p_frames_to_avcc_packets() {
    let mut builder = EncoderBuilder::new(CodecId::H264, MediaType::Video);
    builder.width = 64;
    builder.height = 48;
    builder.pixel_format = PixelFormat::Yuv420p;
    builder.time_base = Rational::new(1, 30);
    builder = builder.option("preset", "fast");

    let (mut encoder, _) = create_h264_encoder(&builder).expect("encoder");
    for n in 0..3u8 {
        let frame = make_frame(64, 48, n);
        encoder.send_frame(Some(&frame)).expect("send_frame");
    }
    encoder.send_frame(None).expect("flush");

    let mut packets = Vec::new();
    loop {
        match encoder.receive_packet() {
            Ok(pkt) => packets.push(pkt),
            Err(Error::Eof) => break,
            Err(Error::Again) => panic!("packets still buffered after flush"),
            Err(e) => panic!("receive_packet: {e:?}"),
        }
    }
    assert_eq!(packets.len(), 3);
    assert!(
        packets[0].flags.contains(PacketFlags::KEY),
        "first packet is IDR"
    );
    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(pkt.pts, i as i64);
        assert_eq!(pkt.dts, pkt.pts);
        assert!(pkt.data.size() > 4, "AVCC packet has NAL length prefixes");
    }
}

#[test]
fn registered_for_h264() {
    let factory = find_encoder(CodecId::H264).expect("H.264 encoder not registered");
    assert_eq!(factory.descriptor().name, "h264_rusty");
}
