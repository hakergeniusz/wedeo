//! End-to-end tests for the pure-Rust AC-3 / E-AC-3 adapter.

use wedeo_codec::decoder::CodecParameters as DecoderParams;
use wedeo_codec::encoder::EncoderBuilder;
use wedeo_codec::registry::{find_decoder_by_name, find_encoder_by_name};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;

// Keep the adapter's `inventory` registrations linked into the test binary.
use wedeo_ac3_oxideav as _;

fn make_frame(n: u16) -> Frame {
    const SAMPLES: usize = 1536;
    let mut data = Vec::with_capacity(SAMPLES * 2 * 2);
    for i in 0..SAMPLES {
        let l = ((i as i16).wrapping_div(8)).wrapping_add(n as i16);
        let r = ((i as i16).wrapping_div(11)).wrapping_sub(n as i16);
        data.extend_from_slice(&l.to_ne_bytes());
        data.extend_from_slice(&r.to_ne_bytes());
    }
    let mut frame = Frame::new_audio(
        SAMPLES as u32,
        SampleFormat::S16,
        48_000,
        wedeo_core::channel_layout::ChannelLayout::stereo(),
    );
    frame.pts = n as i64;
    if let FrameData::Audio(audio) = &mut frame.data {
        audio.planes = vec![FramePlane {
            buffer: Buffer::from_slice(&data),
            offset: 0,
            linesize: data.len(),
        }];
    }
    frame
}

fn builder() -> EncoderBuilder {
    let mut b = EncoderBuilder::new(CodecId::Ac3, MediaType::Audio);
    b.sample_rate = 48_000;
    b.channel_layout = wedeo_core::channel_layout::ChannelLayout::stereo();
    b.sample_format = SampleFormat::S16;
    b.bit_rate = 192_000;
    b.time_base = Rational::new(1, 48_000);
    b
}

fn encode(id: CodecId, name: &str) -> Vec<wedeo_core::packet::Packet> {
    let mut b = builder();
    b.codec_id = id;
    let mut enc = find_encoder_by_name(name)
        .unwrap()
        .create(b)
        .expect("encoder opens");
    for n in 0..2u16 {
        let frame = make_frame(n);
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
    packets
}

fn round_trip(id: CodecId, name: &str, packets: Vec<wedeo_core::packet::Packet>) {
    let mut params = DecoderParams::new(id, MediaType::Audio);
    params.sample_rate = 48_000;
    params.channel_layout = wedeo_core::channel_layout::ChannelLayout::stereo();
    params.sample_format = SampleFormat::S16;
    let mut dec = find_decoder_by_name(name)
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
                assert!(f.audio().unwrap().nb_samples > 0);
                frames += 1;
            }
            Err(Error::Eof) | Err(Error::Again) => break,
            Err(e) => panic!("receive_frame: {e:?}"),
        }
    }
    assert!(frames > 0, "decoded no frames");
}

#[test]
fn registered_for_both_ids() {
    for (id, name) in [
        (CodecId::Ac3, "ac3_oxideav"),
        (CodecId::Eac3, "eac3_oxideav"),
    ] {
        assert_eq!(
            find_encoder_by_name(name).unwrap().descriptor().codec_id,
            id
        );
        assert_eq!(
            find_decoder_by_name(name).unwrap().descriptor().codec_id,
            id
        );
    }
}

#[test]
fn ac3_encodes_syncframes_and_decodes() {
    let packets = encode(CodecId::Ac3, "ac3_oxideav");
    assert!(!packets.is_empty(), "encoder produced no packets");
    for pkt in &packets {
        let d = pkt.data.data();
        assert!(d.len() >= 7, "AC-3 syncframe too short");
        assert_eq!(d[0], 0x0b, "AC-3 syncword");
        assert_eq!(d[1], 0x77, "AC-3 syncword");
    }
    round_trip(CodecId::Ac3, "ac3_oxideav", packets);
}

#[test]
fn eac3_encodes_syncframes_and_decodes() {
    let packets = encode(CodecId::Eac3, "eac3_oxideav");
    assert!(!packets.is_empty(), "E-AC-3 encoder produced no packets");
    for pkt in &packets {
        let d = pkt.data.data();
        assert!(d.len() >= 7, "E-AC-3 syncframe too short");
        assert_eq!(d[0], 0x0b, "E-AC-3 syncword");
        assert_eq!(d[1], 0x77, "E-AC-3 syncword");
    }
    round_trip(CodecId::Eac3, "eac3_oxideav", packets);
}
