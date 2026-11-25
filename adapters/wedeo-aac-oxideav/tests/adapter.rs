//! End-to-end tests for the pure-Rust AAC decoder/encoder adapter.

use wedeo_codec::decoder::CodecParameters as DecoderParams;
use wedeo_codec::encoder::{CodecFlags, EncoderBuilder};
use wedeo_codec::registry::{find_decoder_by_name, find_encoder};
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;

// Keep the adapter's `inventory` registrations linked into the test binary.
use wedeo_aac_oxideav as _;

fn make_frame(n: u16) -> Frame {
    const SAMPLES: usize = 1024;
    let mut data = Vec::with_capacity(SAMPLES * 2 * 2);
    for i in 0..SAMPLES {
        let l = ((i as i16).wrapping_mul(31)).wrapping_add(n as i16);
        let r = ((i as i16).wrapping_mul(-17)).wrapping_sub(n as i16);
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

fn builder(global_header: bool) -> EncoderBuilder {
    let mut b = EncoderBuilder::new(CodecId::Aac, MediaType::Audio);
    b.sample_rate = 48_000;
    b.channel_layout = wedeo_core::channel_layout::ChannelLayout::stereo();
    b.sample_format = SampleFormat::S16;
    b.time_base = Rational::new(1, 48_000);
    if global_header {
        b.flags |= CodecFlags::GLOBAL_HEADER;
    }
    b
}

fn encode(b: EncoderBuilder) -> (Vec<wedeo_core::packet::Packet>, Vec<u8>) {
    let mut enc = b.open().expect("AAC encoder opens");
    let extradata = enc.extradata().expect("AAC encoder emits ASC");
    for n in 0..3u16 {
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
    (packets, extradata)
}

#[test]
fn registered_for_aac() {
    let enc = find_encoder(CodecId::Aac).expect("AAC encoder not registered");
    assert_eq!(enc.descriptor().name, "aac_oxideav");
    let dec = find_decoder_by_name("aac_oxideav").expect("AAC decoder not registered");
    assert_eq!(dec.descriptor().codec_id, CodecId::Aac);
}

#[test]
fn asc_extradata_is_valid() {
    let (_, asc) = encode(builder(false));
    assert!(asc.len() >= 2, "ASC too short: {}", asc.len());
    // audioObjectType = 2 (AAC LC), 48 kHz index = 3, stereo config = 2.
    assert_eq!(asc[0] >> 3, 2, "AAC LC object type");
    assert_eq!((asc[0] & 7) << 1 | (asc[1] >> 7), 3, "48 kHz index");
}

#[test]
fn encodes_adts_packets_without_global_header() {
    let (packets, _) = encode(builder(false));
    assert!(!packets.is_empty(), "encoder produced no packets");
    for pkt in &packets {
        let d = pkt.data.data();
        assert!(d.len() > 7, "ADTS packet too short");
        assert_eq!(d[0], 0xff, "ADTS syncword");
        assert_eq!(d[1] & 0xf0, 0xf0, "ADTS syncword");
    }
}

#[test]
fn global_header_strips_adts_and_round_trips() {
    let (packets, asc) = encode(builder(true));
    assert!(!packets.is_empty());
    for pkt in &packets {
        let d = pkt.data.data();
        assert!(d.len() >= 2, "raw_data_block too short");
        assert_ne!(d[0], 0xff, "ADTS header must be stripped for MP4");
    }

    let mut params = DecoderParams::new(CodecId::Aac, MediaType::Audio);
    params.sample_rate = 48_000;
    params.channel_layout = wedeo_core::channel_layout::ChannelLayout::stereo();
    params.extradata = asc;
    let factory = find_decoder_by_name("aac_oxideav").unwrap();
    let mut dec = factory.create(params).expect("AAC decoder opens");
    for pkt in packets {
        // The oxideav decoder expects ADTS; re-wrap the raw block for the
        // round-trip check (MP4 demuxing adds the ADTS header back).
        let payload = pkt.data.data();
        let len = 7 + payload.len();
        assert!(len < 8192, "test packet too large for ADTS");
        let mut data = vec![
            0xff,
            0xf1,
            0x4c, // profile LC (1), 48 kHz index 3, stereo (2)
            ((len >> 11) & 0x03) as u8,
            ((len >> 3) & 0xff) as u8,
            (((len & 7) << 5) | 0x1f) as u8,
            0xfc,
        ];
        data.extend_from_slice(payload);
        let mut p = pkt;
        p.data = Buffer::from_slice(&data);
        dec.send_packet(Some(&p)).expect("send_packet");
    }
    dec.send_packet(None).expect("drain decoder");
    let mut samples = 0;
    loop {
        match dec.receive_frame() {
            Ok(f) => samples += f.audio().map(|a| a.nb_samples).unwrap_or(0),
            Err(Error::Eof) | Err(Error::Again) => break,
            Err(e) => panic!("receive_frame: {e:?}"),
        }
    }
    assert!(samples > 0, "decoded no samples from encoded packets");
}
