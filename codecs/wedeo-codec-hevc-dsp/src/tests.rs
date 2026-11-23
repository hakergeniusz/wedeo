//! Unit tests against vectors generated from FFmpeg's C source.
//!
//! `reference_vectors.rs` is produced by `tools/gen_vectors.py`, which extracts
//! the C functions from `~/Projects/FFmpeg/libavcodec` verbatim, compiles them
//! into a harness and records their outputs for deterministic inputs.

use super::*;
use crate::deblock::*;
use crate::inter::*;
use crate::intra::*;
use crate::sao::*;
use crate::transform::*;

fn refv(name: &str) -> &'static [i32] {
    reference_vectors::get(name)
}

fn pixels(name: &str) -> Vec<u16> {
    refv(name).iter().map(|&v| v as u16).collect()
}

fn i16s(name: &str) -> Vec<i16> {
    refv(name).iter().map(|&v| v as i16).collect()
}

fn assert_pixels(name: &str, got: &[u16]) {
    let expected = pixels(name);
    assert_eq!(got.len(), expected.len(), "length mismatch for {name}");
    for (i, (a, b)) in got.iter().zip(expected.iter()).enumerate() {
        assert_eq!(a, b, "{name}[{i}]");
    }
}

fn assert_i16(name: &str, got: &[i16]) {
    let expected = i16s(name);
    assert_eq!(got.len(), expected.len(), "length mismatch for {name}");
    for (i, (a, b)) in got.iter().zip(expected.iter()).enumerate() {
        assert_eq!(a, b, "{name}[{i}]");
    }
}

fn intra_refs(size: usize) -> IntraRefSamples {
    IntraRefSamples {
        corner: pixels(&format!("INTRA_CORNER_{size}"))[0],
        top: pixels(&format!("INTRA_REF_TOP_{size}")),
        left: pixels(&format!("INTRA_REF_LEFT_{size}")),
    }
}

#[test]
fn intra_planar_dc_and_angular_match_ffmpeg() {
    for log2 in 2..=5 {
        let size = 1usize << log2;
        let mut dst = vec![0u16; size * size];
        let refs = intra_refs(size);

        intra_predict(&mut dst, size, log2, IntraPredMode::Planar, 0, &refs, 8).unwrap();
        assert_pixels(&format!("INTRA_PLANAR_{size}"), &dst);

        intra_predict(&mut dst, size, log2, IntraPredMode::DC, 0, &refs, 8).unwrap();
        assert_pixels(&format!("INTRA_DC_LUMA_{size}"), &dst);
        intra_predict(&mut dst, size, log2, IntraPredMode::DC, 1, &refs, 8).unwrap();
        assert_pixels(&format!("INTRA_DC_CHROMA_{size}"), &dst);

        let modes: &[u8] = match size {
            4 => &(2..=34).collect::<Vec<_>>(),
            8 | 16 => &[2, 3, 9, 10, 11, 17, 18, 19, 25, 26, 27, 33, 34],
            _ => &[2, 10, 18, 26, 34],
        };
        for &mode in modes {
            let parsed = IntraPredMode::try_from(mode).unwrap();
            intra_predict(&mut dst, size, log2, parsed, 0, &refs, 8).unwrap();
            assert_pixels(&format!("INTRA_ANG_{size}_{mode}_L"), &dst);
            intra_predict(&mut dst, size, log2, parsed, 1, &refs, 8).unwrap();
            assert_pixels(&format!("INTRA_ANG_{size}_{mode}_C"), &dst);
        }
    }
}

#[test]
fn intra_reference_filters_match_ffmpeg() {
    for log2 in 3..=5 {
        let size = 1usize << log2;
        let mut left = pixels(&format!("REF_FILT3_INPUT_LEFT_{size}"));
        let mut top = pixels(&format!("REF_FILT3_INPUT_TOP_{size}"));
        // The C filter harness uses corner = 100 for the 3-tap cases.
        let expected_corner = (u32::from(left[0]) + 200 + u32::from(top[0]) + 2) >> 2;
        let corner = ref_filter_3tap(100, &mut left, &mut top).unwrap();
        assert_eq!(corner, expected_corner as u16);
        assert_pixels(&format!("REF_FILT3_LEFT_{size}"), &left);
        assert_pixels(&format!("REF_FILT3_TOP_{size}"), &top);
    }

    let mut left = pixels("REF_STRONG_INPUT_LEFT");
    let mut top = pixels("REF_STRONG_INPUT_TOP");
    ref_filter_strong(80, &mut left, &mut top);
    assert_pixels("REF_STRONG_LEFT", &left);
    assert_pixels("REF_STRONG_TOP", &top);
}

#[test]
fn intra_reference_inference_covers_all_availability_cases() {
    let bit_depth = 8;
    let top = [10u16, 20, 30, 40];
    let left = [50u16, 60, 70, 80];

    let refs = build_reference_samples(2, IntraNeighbors::default(), bit_depth).unwrap();
    assert_eq!(refs.corner, 128);
    assert!(refs.top.iter().all(|&v| v == 128));
    assert!(refs.left.iter().all(|&v| v == 128));

    let refs = build_reference_samples(
        2,
        IntraNeighbors {
            top: Some(&top),
            ..Default::default()
        },
        bit_depth,
    )
    .unwrap();
    assert_eq!(refs.corner, 10);
    assert_eq!(&refs.top[..4], &top);
    assert!(refs.left.iter().all(|&v| v == 10));

    let refs = build_reference_samples(
        2,
        IntraNeighbors {
            left: Some(&left),
            bottom_left: Some(&[90, 100]),
            ..Default::default()
        },
        bit_depth,
    )
    .unwrap();
    assert_eq!(&refs.left[..4], &left);
    assert_eq!(&refs.left[4..6], &[90, 100]);
    assert_eq!(refs.left[6], 100);
    assert!(refs.top.iter().all(|&v| v == 50));
}

#[test]
fn inter_luma_and_chroma_interpolation_match_ffmpeg() {
    let src = pixels("INTER_SRC");
    let src_stride = 16;
    let x = 4;
    let y = 4;
    let w = 8;
    let h = 8;
    let src2 = i16s("INTER_SRC2");

    let luma_cases: &[(usize, usize)] = &[(0, 0), (1, 0), (0, 2), (3, 3), (2, 1)];
    for &(mx, my) in luma_cases {
        let params = McParams::new(8, mx, my).unwrap();
        let mut tmp = vec![0i16; h * MAX_PB_SIZE];
        put_luma(&mut tmp, MAX_PB_SIZE, &src, src_stride, w, h, x, y, params).unwrap();
        assert_i16(&format!("INTER_PUT_LUMA_{mx}_{my}"), &tmp);

        let mut dst = vec![0u16; h * w];
        uni_luma(&mut dst, w, &src, src_stride, w, h, x, y, params).unwrap();
        assert_pixels(&format!("INTER_UNI_LUMA_{mx}_{my}"), &dst);

        let mut dst = vec![0u16; h * w];
        bi_luma(
            &mut dst,
            w,
            &src,
            src_stride,
            &src2,
            MAX_PB_SIZE,
            w,
            h,
            x,
            y,
            params,
        )
        .unwrap();
        assert_pixels(&format!("INTER_BI_LUMA_{mx}_{my}"), &dst);
    }

    let chroma_cases: &[(usize, usize)] = &[(0, 0), (1, 0), (0, 3), (5, 7)];
    for &(mx, my) in chroma_cases {
        let params = McParams::new(8, mx, my).unwrap();
        let mut tmp = vec![0i16; h * MAX_PB_SIZE];
        put_chroma(&mut tmp, MAX_PB_SIZE, &src, src_stride, w, h, x, y, params).unwrap();
        assert_i16(&format!("INTER_PUT_CHROMA_{mx}_{my}"), &tmp);

        let mut dst = vec![0u16; h * w];
        uni_chroma(&mut dst, w, &src, src_stride, w, h, x, y, params).unwrap();
        assert_pixels(&format!("INTER_UNI_CHROMA_{mx}_{my}"), &dst);

        let mut dst = vec![0u16; h * w];
        bi_chroma(
            &mut dst,
            w,
            &src,
            src_stride,
            &src2,
            MAX_PB_SIZE,
            w,
            h,
            x,
            y,
            params,
        )
        .unwrap();
        assert_pixels(&format!("INTER_BI_CHROMA_{mx}_{my}"), &dst);
    }
}

#[test]
fn inter_weighted_prediction_matches_ffmpeg() {
    let src = pixels("INTER_SRC");
    let src_stride = 16;
    let (x, y, w, h) = (4usize, 4usize, 8usize, 8usize);
    let src2 = i16s("INTER_SRC2");

    let mut dst = vec![0u16; h * w];
    uni_w_luma(
        &mut dst,
        w,
        &src,
        src_stride,
        w,
        h,
        x,
        y,
        McParams::new(8, 0, 0).unwrap(),
        UniWeight {
            denom: 5,
            wx: 26,
            ox: 3,
        },
    )
    .unwrap();
    assert_pixels("INTER_UNI_W_PEL", &dst);

    let mut dst = vec![0u16; h * w];
    bi_w_luma(
        &mut dst,
        w,
        &src,
        src_stride,
        &src2,
        MAX_PB_SIZE,
        w,
        h,
        x,
        y,
        McParams::new(8, 0, 0).unwrap(),
        BiWeight {
            denom: 5,
            wx0: 13,
            wx1: 19,
            ox: 4,
        },
    )
    .unwrap();
    assert_pixels("INTER_BI_W_PEL", &dst);

    let mut dst = vec![0u16; h * w];
    uni_w_luma(
        &mut dst,
        w,
        &src,
        src_stride,
        w,
        h,
        x,
        y,
        McParams::new(8, 1, 2).unwrap(),
        UniWeight {
            denom: 5,
            wx: 26,
            ox: -2,
        },
    )
    .unwrap();
    assert_pixels("INTER_UNI_W_LUMA", &dst);

    let mut dst = vec![0u16; h * w];
    bi_w_luma(
        &mut dst,
        w,
        &src,
        src_stride,
        &src2,
        MAX_PB_SIZE,
        w,
        h,
        x,
        y,
        McParams::new(8, 2, 1).unwrap(),
        BiWeight {
            denom: 5,
            wx0: 13,
            wx1: 19,
            ox: 4,
        },
    )
    .unwrap();
    assert_pixels("INTER_BI_W_LUMA", &dst);

    let mut dst = vec![0u16; h * w];
    uni_w_chroma(
        &mut dst,
        w,
        &src,
        src_stride,
        w,
        h,
        x,
        y,
        McParams::new(8, 4, 3).unwrap(),
        UniWeight {
            denom: 4,
            wx: 12,
            ox: 1,
        },
    )
    .unwrap();
    assert_pixels("INTER_UNI_W_CHROMA", &dst);

    let mut dst = vec![0u16; h * w];
    bi_w_chroma(
        &mut dst,
        w,
        &src,
        src_stride,
        &src2,
        MAX_PB_SIZE,
        w,
        h,
        x,
        y,
        McParams::new(8, 4, 5).unwrap(),
        BiWeight {
            denom: 4,
            wx0: 9,
            wx1: 7,
            ox: 2,
        },
    )
    .unwrap();
    assert_pixels("INTER_BI_W_CHROMA", &dst);
}

#[test]
fn integer_mc_matches_ffmpeg() {
    let src = pixels("INTER_SRC");
    let mut tmp = vec![0i16; 8 * MAX_PB_SIZE];
    put_luma(
        &mut tmp,
        MAX_PB_SIZE,
        &src,
        16,
        8,
        8,
        4,
        4,
        McParams::new(8, 0, 0).unwrap(),
    )
    .unwrap();
    assert_i16("INTER_PUT_PEL", &tmp);
}

#[test]
fn transforms_match_ffmpeg() {
    let offsets: &[(&str, i32)] = &[
        ("DEQUANT_8_4", 8),
        ("DEQUANT_8_8", 8),
        ("DEQUANT_8_16", 8),
        ("DEQUANT_8_32", 8),
        ("DEQUANT_8_4_10", 10),
        ("DEQUANT_8_8_10", 10),
        ("DEQUANT_8_16_10", 10),
        ("DEQUANT_8_32_10", 10),
    ];
    for log2 in 2..=5 {
        let size = 1usize << log2;
        let input = i16s(&format!("COEFF_INPUT_{size}"));
        for &(name, depth) in offsets {
            if !name.ends_with(&format!("_{size}")) && !name.ends_with(&format!("_{size}_10")) {
                continue;
            }
            let mut coeffs = input.clone();
            dequant(&mut coeffs, log2, depth as u8).unwrap();
            assert_i16(name, &coeffs);
        }
    }

    for log2 in 2..=5 {
        let size = 1usize << log2;
        let input = i16s(&format!("COEFF_INPUT_{size}"));
        let mut coeffs = input.clone();
        if size == 4 {
            transform_4x4_luma(&mut coeffs, 8).unwrap();
        } else {
            idct(&mut coeffs, log2, 0, 8).unwrap();
        }
        assert_i16(&format!("IDCT_{size}"), &coeffs);

        if size > 4 {
            let mut coeffs = input.clone();
            let limit = match size {
                8 => 4,
                16 => 8,
                _ => 12,
            };
            idct(&mut coeffs, log2, limit, 8).unwrap();
            assert_i16(&format!("IDCT_LIMIT_{size}"), &coeffs);

            let mut coeffs = input.clone();
            idct_dc(&mut coeffs, log2, 8).unwrap();
            assert_i16(&format!("IDCT_DC_{size}"), &coeffs);
        }

        let mut coeffs = input.clone();
        transform_rdpcm(&mut coeffs, log2, 0).unwrap();
        assert_i16(&format!("RDPCM_H_{size}"), &coeffs);
        let mut coeffs = input;
        transform_rdpcm(&mut coeffs, log2, 1).unwrap();
        assert_i16(&format!("RDPCM_V_{size}"), &coeffs);
    }
}

#[test]
fn deblocking_kernels_match_ffmpeg() {
    let input: Vec<u16> = pixels("DEBLOCK_INPUT").into_iter().collect();
    let stride = 16isize;
    let base = 6 * 16 + 6;
    let tc = [10i32, 12i32];
    let no_p = [false, false];
    let no_q = [false, true];

    let mut block = input.clone();
    loop_filter_luma(&mut block, base, 1, stride, 26, &tc, &no_p, &no_q, 8).unwrap();
    assert_pixels("DEBLOCK_LUMA", &block);

    let mut block = input.clone();
    loop_filter_luma_strong(&mut block, base, 1, stride, 12, 24, 24, false, false, 8).unwrap();
    assert_pixels("DEBLOCK_LUMA_STRONG", &block);

    let mut block = input.clone();
    loop_filter_luma_weak(&mut block, base, 1, stride, 10, false, false, 2, 2, 8).unwrap();
    assert_pixels("DEBLOCK_LUMA_WEAK", &block);

    let mut block = input;
    loop_filter_chroma(&mut block, base, stride, 1, &tc, &no_p, &no_q, 8).unwrap();
    assert_pixels("DEBLOCK_CHROMA", &block);
}

#[test]
fn sao_band_and_edge_match_ffmpeg() {
    let src: Vec<u16> = pixels("SAO_SRC").into_iter().collect();
    let stride = 2 * MAX_PB_SIZE + AV_INPUT_BUFFER_PADDING_SIZE;
    let offsets = [0i16, -3, 5, -7, 9];
    let mut dst = vec![0u16; 64];

    sao_band_filter(
        &mut dst,
        8,
        &src[stride + 1..],
        stride,
        &offsets,
        7,
        8,
        8,
        8,
    )
    .unwrap();
    assert_pixels("SAO_BAND", &dst);

    for eo in 0..=3 {
        let mut dst = vec![0u16; 64];
        sao_edge_filter(&mut dst, 8, &src, stride, &offsets, eo, 8, 8, 8).unwrap();
        assert_pixels(&format!("SAO_EDGE_{eo}"), &dst);
    }
}

#[test]
fn sao_offset_values_and_edge_restore_match_ffmpeg_semantics() {
    let mut sao = SaoParams::default();
    sao.type_idx[0] = SaoType::Band;
    sao.offset_abs[0] = [1, 2, 3, 4];
    sao.offset_sign[0] = [1, 0, 1, 0];
    sao.compute_offset_vals(0, 2).unwrap();
    assert_eq!(sao.offset_val[0], [0, -4, 8, -12, 16]);

    sao.type_idx[1] = SaoType::Edge;
    sao.offset_abs[1] = [1, 2, 3, 4];
    sao.compute_offset_vals(1, 0).unwrap();
    assert_eq!(sao.offset_val[1], [0, 1, 2, -3, -4]);

    let src = vec![100u16; 64];
    let mut dst = src.clone();
    sao.offset_val[0][0] = 5;
    sao.eo_class[0] = eo_class::DIAG_135;
    sao_edge_restore(
        &mut dst,
        8,
        &src,
        8,
        &sao,
        SaoBorders([false; 4]),
        8,
        8,
        0,
        SaoEdgeFlags {
            vert: [true, false],
            horiz: [false, false],
            diag: [false; 4],
        },
        8,
    )
    .unwrap();
    assert!(dst.iter().all(|&v| v == 100));
}

#[test]
fn tables_and_error_paths() {
    assert_eq!(TC_TABLE.len(), 54);
    assert_eq!(BETA_TABLE.len(), 52);
    assert_eq!(tc_from_qp(26, 2, 0), 2);
    assert_eq!(beta_from_qp(26, 0), 16);
    assert_eq!(chroma_tc(26, 1, 0, 0, 0, 1), 2);
    assert!(matches!(
        IntraPredMode::try_from(35),
        Err(DspError::InvalidIntraMode(35))
    ));
    assert!(matches!(
        McParams::new(8, 8, 0),
        Err(DspError::InvalidFraction(8))
    ));
    assert!(matches!(
        put_luma(
            &mut [0i16; 64],
            8,
            &[0u16; 256],
            16,
            8,
            8,
            4,
            4,
            McParams::new(8, 4, 0).unwrap()
        ),
        Err(DspError::InvalidFraction(4))
    ));
    assert!(matches!(
        build_reference_samples(1, IntraNeighbors::default(), 8),
        Err(DspError::InvalidLog2Size(1))
    ));
}
