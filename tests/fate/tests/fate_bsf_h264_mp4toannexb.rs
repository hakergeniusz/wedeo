//! FATE registration and conformance tests for `wedeo-bsf-h264-mp4toannexb`.

// Ensure the inventory registration is linked into this test binary.
use wedeo_bsf_h264_mp4toannexb as _;

use wedeo_codec::bitstream_filter::{bitstream_filters, find_bitstream_filter};
use wedeo_core::codec_id::CodecId;
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
