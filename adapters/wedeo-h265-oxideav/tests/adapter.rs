//! End-to-end tests for the pure-Rust HEVC decoder/encoder adapter.

use wedeo_codec::decoder::{CodecParameters as DecoderParams, DecoderBuilder};
use wedeo_codec::encoder::{CodecFlags, EncoderBuilder};
use wedeo_codec::registry::{find_decoder, find_encoder};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::PacketFlags;
use wedeo_core::pixel_format::PixelFormat;
use wedeo_core::rational::Rational;

// Keep the adapter's `inventory` registrations linked into the test binary.
use wedeo_h265_oxideav as _;

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
            FramePlane {
                buffer: Buffer::from_slice(&y),
                offset: 0,
                linesize: w,
            },
            FramePlane {
                buffer: Buffer::from_slice(&u),
                offset: 0,
                linesize: cw,
            },
            FramePlane {
                buffer: Buffer::from_slice(&v),
                offset: 0,
                linesize: cw,
            },
        ];
    }
    frame
}

fn builder(global_header: bool) -> EncoderBuilder {
    let mut b = EncoderBuilder::new(CodecId::Hevc, MediaType::Video);
    b.width = 64;
    b.height = 48;
    b.pixel_format = PixelFormat::Yuv420p;
    b.time_base = Rational::new(1, 30);
    if global_header {
        b.flags |= CodecFlags::GLOBAL_HEADER;
    }
    b
}

fn encode(b: EncoderBuilder) -> (Vec<wedeo_core::packet::Packet>, Option<Vec<u8>>) {
    let mut enc = b.open().expect("HEVC encoder opens");
    let extradata = enc.extradata();
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
    (packets, extradata)
}

#[test]
fn registered_for_hevc() {
    let enc = find_encoder(CodecId::Hevc).expect("HEVC encoder not registered");
    assert_eq!(enc.descriptor().name, "hevc_oxideav");
    let dec = find_decoder(CodecId::Hevc).expect("HEVC decoder not registered");
    assert_eq!(dec.descriptor().name, "hevc_oxideav");
}

#[test]
fn encodes_annexb_without_global_header() {
    let (packets, extradata) = encode(builder(false));
    assert!(extradata.is_none(), "no hvcC without GLOBAL_HEADER");
    assert_eq!(packets.len(), 3);
    assert!(
        packets[0].flags.contains(PacketFlags::KEY),
        "first packet is an IDR"
    );
    for pkt in &packets {
        let d = pkt.data.data();
        assert!(
            d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1]),
            "expected an Annex B start code"
        );
    }
}

#[test]
fn global_header_builds_hvcc_and_length_prefixed_packets() {
    let (packets, extradata) = encode(builder(true));
    let hvcc = extradata.expect("hvcC with GLOBAL_HEADER");
    assert_eq!(hvcc[0], 1, "configurationVersion");
    assert!(hvcc.len() > 23 + 3 * 5, "hvcC carries parameter sets");
    assert_eq!(hvcc[21] & 3, 3, "lengthSizeMinusOne = 3");
    assert_eq!(hvcc[22], 3, "VPS, SPS and PPS arrays");

    let mut offset = 23;
    for expected_type in [32u8, 33, 34] {
        assert_eq!(hvcc[offset] & 0x3f, expected_type, "array type");
        let count = u16::from_be_bytes([hvcc[offset + 1], hvcc[offset + 2]]);
        assert_eq!(count, 1);
        let len = u16::from_be_bytes([hvcc[offset + 3], hvcc[offset + 4]]) as usize;
        assert!(len > 2, "parameter set NAL too short");
        offset += 5 + len;
    }
    assert_eq!(offset, hvcc.len(), "hvcC array sizes add up");

    for (i, pkt) in packets.iter().enumerate() {
        assert_eq!(pkt.pts, i as i64);
        let d = pkt.data.data();
        let mut pos = 0;
        while pos < d.len() {
            assert!(pos + 4 <= d.len(), "truncated NAL length prefix");
            let len = u32::from_be_bytes([d[pos], d[pos + 1], d[pos + 2], d[pos + 3]]) as usize;
            assert!(
                len > 0 && pos + 4 + len <= d.len(),
                "invalid NAL length {len}"
            );
            pos += 4 + len;
        }
        assert_eq!(pos, d.len(), "NAL lengths cover the whole packet");
    }
}

#[test]
fn round_trip_decode_of_length_prefixed_packets() {
    let (packets, extradata) = encode(builder(true));
    let mut params = DecoderParams::new(CodecId::Hevc, MediaType::Video);
    params.extradata = extradata.expect("hvcC");
    params.width = 64;
    params.height = 48;
    params.pixel_format = PixelFormat::Yuv420p;
    let mut dec = DecoderBuilder::new(params)
        .open()
        .expect("HEVC decoder opens");
    for pkt in packets {
        dec.send_packet(Some(&pkt)).expect("send_packet");
    }
    dec.send_packet(None).expect("drain decoder");
    let mut frames = 0;
    loop {
        match dec.receive_frame() {
            Ok(_) => frames += 1,
            Err(Error::Eof) | Err(Error::Again) => break,
            Err(e) => panic!("receive_frame: {e:?}"),
        }
    }
    assert_eq!(frames, 3, "all encoded frames decode back");
}
