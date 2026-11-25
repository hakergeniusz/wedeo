//! End-to-end tests for the pure-Rust MPEG-4 Part 2 adapter.

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
use wedeo_mpeg4_oxideav as _;

fn make_frame(w: usize, h: usize, n: u8) -> Frame {
    let mut frame = Frame::new_video(w as u32, h as u32, PixelFormat::Yuv420p);
    frame.pts = n as i64;
    let plane = |pw: usize, ph: usize, base: u8| FramePlane {
        buffer: Buffer::from_slice(
            &(0..pw * ph)
                .map(|i| (i as u8).wrapping_mul(7).wrapping_add(base))
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
fn registered_for_mpeg4() {
    assert_eq!(
        find_encoder(CodecId::Mpeg4).unwrap().descriptor().name,
        "mpeg4_oxideav"
    );
    assert_eq!(
        find_decoder(CodecId::Mpeg4).unwrap().descriptor().name,
        "mpeg4_oxideav"
    );
}

#[test]
fn encodes_config_headers_and_decodes() {
    let mut b = EncoderBuilder::new(CodecId::Mpeg4, MediaType::Video);
    b.width = 64;
    b.height = 48;
    b.pixel_format = PixelFormat::Yuv420p;
    b.time_base = Rational::new(1, 25);
    let mut enc = find_encoder(CodecId::Mpeg4)
        .unwrap()
        .create(b)
        .expect("encoder opens");
    let extradata = enc.extradata().expect("MPEG-4 config headers");
    assert!(extradata.len() > 4, "no VOS/VO/VOL headers");
    for n in 0..3u8 {
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
    assert_eq!(packets.len(), 3);

    let mut params = DecoderParams::new(CodecId::Mpeg4, MediaType::Video);
    params.width = 64;
    params.height = 48;
    params.pixel_format = PixelFormat::Yuv420p;
    params.extradata = extradata;
    let mut dec = find_decoder(CodecId::Mpeg4)
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
    assert_eq!(frames, 3, "every frame decodes back");
}
