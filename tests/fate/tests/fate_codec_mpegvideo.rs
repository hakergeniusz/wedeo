//! Conformance / API smoke tests for `wedeo-codec-mpegvideo`.
//!
//! This crate is a reusable building-block library (no decoder registration),
//! so there is no framecrc vector to compare here. These tests instead verify
//! the public API from a separate crate, exactly as `wedeo-codec-mpeg12` will
//! consume it.

use wedeo_codec_mpegvideo::tables::{
    MPEG1_DEFAULT_INTRA_MATRIX, MPEG12_RL_NB_ELEMS, ZIGZAG_DIRECT,
};

#[test]
fn public_tables_are_consistent() {
    assert_eq!(MPEG1_DEFAULT_INTRA_MATRIX[0], 8);
    assert_eq!(MPEG12_RL_NB_ELEMS, 111);
    let mut seen = [false; 64];
    for &idx in &ZIGZAG_DIRECT {
        assert!(!seen[idx as usize]);
        seen[idx as usize] = true;
    }
}

#[test]
fn public_api_builds_context_and_decodes_vlc() {
    use wedeo_codec::bitstream::{BitRead, BitReadBE};
    use wedeo_codec_mpegvideo::picture::MpegEncContext;
    use wedeo_codec_mpegvideo::scan::init_scantable_identity;
    use wedeo_codec_mpegvideo::vlc::{dc_luma_vlc, get_xbits};

    let ctx = MpegEncContext::new(352, 288);
    assert_eq!(ctx.mb_width, 22);
    assert_eq!(ctx.b8_stride, 45);

    let st = init_scantable_identity(&ZIGZAG_DIRECT);
    assert_eq!(st.permutated, ZIGZAG_DIRECT);

    let luma = dc_luma_vlc();
    // Code 100 (size 0) followed by padding.
    let data = [0x80u8, 0, 0, 0, 0, 0, 0, 0];
    let mut br = BitReadBE::new(&data);
    assert_eq!(luma.decode(&mut br).unwrap(), 0);
    assert_eq!(get_xbits(&mut br, 2).unwrap(), -3);
}
