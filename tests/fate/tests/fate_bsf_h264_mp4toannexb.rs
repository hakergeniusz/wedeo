//! FATE registration and conformance tests for `wedeo-bsf-h264-mp4toannexb`.

// Ensure the inventory registration is linked into this test binary.
use wedeo_bsf_h264_mp4toannexb as _;

use wedeo_codec::bitstream_filter::{
    bitstream_filters, create_initialized_bitstream_filter, find_bitstream_filter,
};
use wedeo_codec::decoder::CodecParameters;
use wedeo_core::codec_id::CodecId;
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::Packet;

#[test]
fn registered_by_name() {
    let factory = find_bitstream_filter("h264_mp4toannexb")
        .expect("h264_mp4toannexb must be registered in the bitstream filter registry");
    assert_eq!(factory.descriptor().name, "h264_mp4toannexb");
    assert_eq!(
        factory.descriptor().long_name,
        "H.264 MP4 to Annex B byte stream format filter"
    );
    assert_eq!(factory.descriptor().codec_id, CodecId::H264);
    assert_eq!(factory.descriptor().priority, 100);
}

#[test]
fn present_in_registry_iteration() {
    assert!(bitstream_filters().any(|f| f.descriptor().name == "h264_mp4toannexb"));
}

#[test]
fn factory_creates_working_filter() {
    let factory = find_bitstream_filter("h264_mp4toannexb").unwrap();
    let mut filter = factory.create().unwrap();

    // No extradata is supplied through the factory interface, so an AVCC
    // packet with NEW_EXTRADATA side data is the way to enable conversion.
    let sps: &[u8] = &[0x67, 0x42, 0x00, 0x1e];
    let pps: &[u8] = &[0x68, 0xce, 0x3c, 0x80];
    let idr: &[u8] = &[0x65, 0x00, 0xaa];

    let mut extradata = vec![1, 0x42, 0x00, 0x1e, 0xff, 0xe1];
    extradata.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    extradata.extend_from_slice(sps);
    extradata.push(1);
    extradata.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    extradata.extend_from_slice(pps);

    let mut avcc = Vec::new();
    for nal in [sps, pps, idr] {
        avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        avcc.extend_from_slice(nal);
    }

    let mut packet = Packet::from_slice(&avcc);
    packet.side_data.push(wedeo_core::packet::PacketSideData {
        data_type: wedeo_core::packet::PacketSideDataType::NewExtradata,
        data: extradata,
    });

    let out = filter.filter(&packet).unwrap();
    assert_eq!(out.len(), 1);

    let expected: Vec<u8> = [
        &[0, 0, 0, 1][..],
        sps,
        &[0, 0, 0, 1][..],
        pps,
        &[0, 0, 1][..], // 3-byte start code: output is non-empty after PS injection
        idr,
    ]
    .concat();
    assert_eq!(out[0].data.data(), expected.as_slice());
    assert!(
        out[0]
            .side_data
            .iter()
            .all(|s| s.data_type != wedeo_core::packet::PacketSideDataType::NewExtradata)
    );

    assert!(filter.flush().unwrap().is_empty());
}

/// Real FFmpeg fixture (libx264 AVCC packets + the byte stream produced by
/// `ffmpeg -bsf:v h264_mp4toannexb`), driven entirely through the generic
/// registry path. This proves `CodecParameters.extradata` is consumed by the
/// registered filter without needing `with_extradata()`.
const FIXTURE_EXTRADATA: &[u8] =
    include_bytes!("../../../codecs/wedeo-bsf-h264-mp4toannexb/tests/data/extradata.bin");
const FIXTURE_PACKETS: &[u8] =
    include_bytes!("../../../codecs/wedeo-bsf-h264-mp4toannexb/tests/data/packets.bin");
const FIXTURE_EXPECTED: &[u8] =
    include_bytes!("../../../codecs/wedeo-bsf-h264-mp4toannexb/tests/data/expected_annexb.bin");

fn be32(bytes: &[u8]) -> usize {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize
}

#[test]
fn initialized_filter_matches_ffmpeg_fixture_byte_for_byte() {
    let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = FIXTURE_EXTRADATA.to_vec();
    let mut bsf = create_initialized_bitstream_filter("h264_mp4toannexb", &params).unwrap();

    let count = be32(&FIXTURE_PACKETS[..4]);
    let mut pos = 4;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = be32(&FIXTURE_PACKETS[pos..pos + 4]);
        pos += 4;
        let packet = Packet::from_slice(&FIXTURE_PACKETS[pos..pos + len]);
        pos += len;
        let filtered = bsf.filter(&packet).unwrap();
        assert_eq!(filtered.len(), 1);
        out.extend_from_slice(filtered[0].data.data());
    }
    assert_eq!(pos, FIXTURE_PACKETS.len());

    assert_eq!(out.len(), FIXTURE_EXPECTED.len());
    assert_eq!(out, FIXTURE_EXPECTED);
}
