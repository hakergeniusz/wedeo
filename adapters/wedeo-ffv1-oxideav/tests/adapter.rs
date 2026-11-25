//! End-to-end tests for the pure-Rust FFV1 adapter.

use wedeo_codec::decoder::CodecParameters as DecoderParams;
use wedeo_codec::encoder::EncoderBuilder;
use wedeo_codec::registry::{find_decoder, find_encoder};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;

// Keep the adapter's `inventory` registrations linked into the test binary.
use wedeo_ffv1_oxideav as _;

fn make_frame(w: usize, h: usize, n: u8) -> Frame {
    let mut frame = Frame::new_video(w as u32, h as u32, PixelFormat::Yuv420p);
    frame.pts = n as i64;
    let plane = |pw: usize, ph: usize, base: u8| FramePlane {
        buffer: Buffer::from_slice(
            &(0..pw * ph)
                .map(|i| (i as u8).wrapping_mul(13).wrapping_add(base))
                .collect::<Vec<_>>(),
        ),
        offset: 0,
        linesize: pw,
    };
    if let FrameData::Video(video) = &mut frame.data {
        video.planes = vec![
            plane(w, h, n),
            plane(w / 2, h / 2, 64 + n),
            plane(w / 2, h / 2, 128 + n),
        ];
    }
    frame
}

#[test]
fn registered_for_ffv1() {
    let enc = find_encoder(CodecId::Ffv1).expect("FFV1 encoder registered");
    let dec = find_decoder(CodecId::Ffv1).expect("FFV1 decoder registered");
    assert_eq!(enc.descriptor().name, "ffv1_oxideav");
    assert_eq!(dec.descriptor().name, "ffv1_oxideav");
}

#[test]
fn lossless_round_trip() {
    let mut b = EncoderBuilder::new(CodecId::Ffv1, MediaType::Video);
    b.width = 64;
    b.height = 48;
    b.pixel_format = PixelFormat::Yuv420p;
    b.time_base = Rational::new(1, 25);
    let mut enc = find_encoder(CodecId::Ffv1)
        .unwrap()
        .create(b)
        .expect("encoder opens");
    for n in 0..2u8 {
        let frame = make_frame(64, 48, n);
        enc.send_frame(Some(&frame)).expect("send_frame");
    }
    enc.send_frame(None).expect("flush");
    let mut packets = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(pkt) => packets.push(pkt),
            Err(Error::Eof) => break,
            Err(e) => panic!("receive_packet: {e:?}"),
        }
    }
    assert_eq!(packets.len(), 2);

    let mut params = DecoderParams::new(CodecId::Ffv1, MediaType::Video);
    params.width = 64;
    params.height = 48;
    params.pixel_format = PixelFormat::Yuv420p;
    let mut dec = find_decoder(CodecId::Ffv1)
        .unwrap()
        .create(params)
        .expect("decoder opens");
    for pkt in packets {
        dec.send_packet(Some(&pkt)).expect("send_packet");
    }
    dec.send_packet(None).expect("drain decoder");
    let mut frames = 0;
    loop {
        match dec.receive_frame() {
            Ok(f) => {
                let v = f.video().unwrap();
                assert_eq!(v.width, 64);
                assert_eq!(v.height, 48);
                frames += 1;
            }
            Err(Error::Eof) | Err(Error::Again) => break,
            Err(e) => panic!("receive_frame: {e:?}"),
        }
    }
    assert_eq!(frames, 2, "every frame decodes back");
}
