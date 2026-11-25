//! Tests for the pure-Rust Vorbis encoder adapter.

use wedeo_codec::encoder::EncoderBuilder;
use wedeo_codec::registry::find_encoder;
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::Error;
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::rational::Rational;
use wedeo_core::sample_format::SampleFormat;

// Keep the adapter's `inventory` registration linked into the test binary.
use wedeo_vorbis_enc as _;

#[test]
fn registered_for_vorbis() {
    let factory = find_encoder(CodecId::Vorbis).expect("Vorbis encoder registered");
    assert_eq!(factory.descriptor().name, "vorbis_oxideav");
}

#[test]
fn produces_xiph_headers_and_audio_packets() {
    const SAMPLES: usize = 1024;
    let mut b = EncoderBuilder::new(CodecId::Vorbis, MediaType::Audio);
    b.sample_rate = 44_100;
    b.channel_layout = wedeo_core::channel_layout::ChannelLayout::stereo();
    b.sample_format = SampleFormat::Flt;
    b.time_base = Rational::new(1, 44_100);
    let mut enc = find_encoder(CodecId::Vorbis)
        .unwrap()
        .create(b)
        .expect("encoder opens");
    let extradata = enc.extradata().expect("Xiph-laced headers");
    assert_eq!(extradata[0], 2, "three Vorbis headers");
    assert!(extradata.len() > 4);

    for n in 0..4u16 {
        let mut data = Vec::with_capacity(SAMPLES * 2 * 4);
        for i in 0..SAMPLES {
            let v = ((i as f32 * 0.03) + n as f32).sin() * 0.4;
            data.extend_from_slice(&v.to_le_bytes());
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut frame = Frame::new_audio(
            SAMPLES as u32,
            SampleFormat::Flt,
            44_100,
            wedeo_core::channel_layout::ChannelLayout::stereo(),
        );
        frame.pts = (n as i64) * SAMPLES as i64;
        if let FrameData::Audio(audio) = &mut frame.data {
            audio.planes = vec![FramePlane {
                buffer: Buffer::from_slice(&data),
                offset: 0,
                linesize: data.len(),
            }];
        }
        enc.send_frame(Some(&frame)).expect("send_frame");
    }
    enc.send_frame(None).expect("flush");
    let mut packets = 0;
    loop {
        match enc.receive_packet() {
            Ok(_) => packets += 1,
            Err(Error::Eof) => break,
            Err(e) => panic!("receive_packet: {e:?}"),
        }
    }
    assert!(packets > 0, "no Vorbis audio packets were emitted");
}
