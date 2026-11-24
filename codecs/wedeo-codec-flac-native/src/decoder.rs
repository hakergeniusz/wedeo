//! FLAC frame decoder, ported from FFmpeg `libavcodec/flacdec.c`,
//! `flacdsp.c`, `flacdsp_template.c`, `flac.c` and `flacdata.c`.
//!
//! This is a native, pure-Rust implementation: no FFI, no bindgen.  All
//! arithmetic uses wrapping operations where the C original relies on
//! unsigned/signed overflow, so the decoded samples are bit-identical to
//! FFmpeg's.

use std::collections::VecDeque;

use tracing::{debug, warn};
use wedeo_codec::decoder::{CodecParameters, Decoder, DecoderDescriptor};
use wedeo_codec::descriptor::{CodecCapabilities, CodecDescriptor, CodecProperties};
use wedeo_codec::registry::DecoderFactory;
use wedeo_core::buffer::Buffer;
use wedeo_core::codec_id::CodecId;
use wedeo_core::error::{Error, Result};
use wedeo_core::frame::{Frame, FrameData, FramePlane};
use wedeo_core::media_type::MediaType;
use wedeo_core::packet::{Packet, PacketSideDataType};
use wedeo_core::sample_format::SampleFormat;

use crate::bitstream::BitReader;
use crate::crc::crc8_atm;
use crate::data::{BLOCKSIZE_TABLE, SAMPLE_RATE_TABLE, SAMPLE_SIZE_TABLE, flac_channel_layout};
use crate::streaminfo::{StreamInfo, parse_extradata, parse_raw_header};

/// FLAC_MAX_CHANNELS from flac.h.
const FLAC_MAX_CHANNELS: u32 = 8;
/// FLAC_MIN_FRAME_SIZE from flac.h.
const FLAC_MIN_FRAME_SIZE: usize = 10;

/// Channel decorrelation modes (flac.h).
const FLAC_CHMODE_INDEPENDENT: u32 = 0;
const FLAC_CHMODE_LEFT_SIDE: u32 = 1;
const FLAC_CHMODE_RIGHT_SIDE: u32 = 2;
const FLAC_CHMODE_MID_SIDE: u32 = 3;

/// `FLACFrameInfo` from flac.h.
#[derive(Clone, Debug)]
struct FrameInfo {
    channels: u32,
    ch_mode: u32,
    bps: u32,
    blocksize: usize,
    samplerate: u32,
    #[allow(dead_code)] // parsed for parity with FFmpeg, unused by the decoder
    is_var_size: bool,
    #[allow(dead_code)] // frame/sample number does not affect decoded samples
    frame_or_sample_num: i64,
}

/// FFmpeg `av_log2`: floor(log2(v)), 0 for v == 0.
fn av_log2(v: u32) -> u32 {
    if v == 0 { 0 } else { 31 - v.leading_zeros() }
}

/// UTF-8 reading equivalent to FFmpeg's `GET_UTF8` macro (used for the frame
/// or sample number in the frame header).  Returns -1 on invalid input.
fn get_utf8(gb: &mut BitReader<'_>) -> i64 {
    let mut val = gb.get_bits(8);
    let mut top = (val & 128) >> 1;
    if (val & 0xc0) == 0x80 || val >= 0xFE {
        return -1;
    }
    while val & top != 0 {
        let tmp = gb.get_bits(8).wrapping_sub(128);
        if tmp >> 6 != 0 {
            return -1;
        }
        val = (val << 6).wrapping_add(tmp);
        top = top.wrapping_shl(5);
    }
    (val & (top.wrapping_shl(1).wrapping_sub(1))) as i64
}

/// `ff_flac_decode_frame_header` from flac.c.  The input `gb` is positioned at
/// the start of the FLAC frame; `data` is the whole packet (needed by the
/// CRC-8 check).
fn decode_frame_header(gb: &mut BitReader<'_>, data: &[u8]) -> Result<FrameInfo> {
    // frame sync code
    if gb.get_bits(15) & 0x7FFF != 0x7FFC {
        return Err(Error::InvalidData);
    }

    // variable block size stream code
    let is_var_size = gb.get_bits1() != 0;

    // block size and sample rate codes
    let bs_code = gb.get_bits(4);
    let sr_code = gb.get_bits(4);

    // channels and decorrelation
    let ch_mode_raw = gb.get_bits(4);
    let (channels, ch_mode) = if ch_mode_raw < FLAC_MAX_CHANNELS {
        (ch_mode_raw + 1, FLAC_CHMODE_INDEPENDENT)
    } else if ch_mode_raw < FLAC_MAX_CHANNELS + FLAC_CHMODE_MID_SIDE {
        (2, ch_mode_raw - (FLAC_MAX_CHANNELS - 1))
    } else {
        return Err(Error::InvalidData);
    };

    // bits per sample
    let bps_code = gb.get_bits(3);
    if bps_code == 3 {
        return Err(Error::InvalidData);
    }
    let bps = SAMPLE_SIZE_TABLE[bps_code as usize];

    // reserved bit
    if gb.get_bits1() != 0 {
        return Err(Error::InvalidData);
    }

    // sample or frame count
    let frame_or_sample_num = get_utf8(gb);
    if frame_or_sample_num < 0 {
        return Err(Error::InvalidData);
    }

    // blocksize
    let blocksize = if bs_code == 0 {
        return Err(Error::InvalidData);
    } else if bs_code == 6 {
        gb.get_bits(8) as usize + 1
    } else if bs_code == 7 {
        gb.get_bits(16) as usize + 1
    } else {
        BLOCKSIZE_TABLE[bs_code as usize] as usize
    };

    // sample rate
    let samplerate = if sr_code < 12 {
        SAMPLE_RATE_TABLE[sr_code as usize]
    } else if sr_code == 12 {
        gb.get_bits(8) * 1000
    } else if sr_code == 13 {
        gb.get_bits(16)
    } else if sr_code == 14 {
        gb.get_bits(16) * 10
    } else {
        return Err(Error::InvalidData);
    };

    // header CRC-8 check. FFmpeg computes the CRC over all header bytes,
    // including the stored CRC byte: CRC(m || CRC(m)) == 0.
    gb.skip_bits(8);
    let crc_bytes = gb.byte_position();
    if crc_bytes > data.len() || crc8_atm(&data[..crc_bytes]) != 0 {
        debug!("flac: header crc mismatch");
        return Err(Error::InvalidData);
    }

    Ok(FrameInfo {
        channels,
        ch_mode,
        bps,
        blocksize,
        samplerate,
        is_var_size,
        frame_or_sample_num,
    })
}

/// `get_sr_golomb_flac` from golomb.h.  Returns `i32::MIN` on error, which
/// `decode_residuals` treats as an invalid residual.
fn get_sr_golomb_flac(gb: &mut BitReader<'_>, k: u32, limit: i32, esc_len: u32) -> i32 {
    let mut i: usize = 0;
    // The cached FFmpeg path checks `get_bits_left(gb) > 0` before each bit.
    while i < limit as usize && gb.get_bits_left() > 0 && gb.get_bits1() == 0 {
        i += 1;
    }
    let limit_minus_1 = (limit as usize).wrapping_sub(1);
    let v = if i < limit_minus_1 {
        (i as u32).wrapping_shl(k).wrapping_add(gb.get_bits(k))
    } else if esc_len != 0 && i == limit_minus_1 {
        gb.get_bits(esc_len).wrapping_add(1)
    } else {
        return i32::MIN;
    };
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// `decode_residuals` from flacdec.c.  Rice-coded residuals are written into
/// `decoded` starting at index `pred_order`.
fn decode_residuals(
    gb: &mut BitReader<'_>,
    decoded: &mut [i32],
    pred_order: usize,
    blocksize: usize,
) -> Result<()> {
    let method_type = gb.get_bits(2);
    let rice_order = gb.get_bits(4);

    let samples = blocksize >> rice_order;
    let rice_bits = 4 + method_type;
    let rice_esc = (1u32 << rice_bits) - 1;

    if method_type > 1 {
        warn!("illegal residual coding method {method_type}");
        return Err(Error::InvalidData);
    }
    if (samples << rice_order) != blocksize {
        warn!("invalid rice order: {rice_order} blocksize {blocksize}");
        return Err(Error::InvalidData);
    }
    if pred_order > samples {
        warn!("invalid predictor order: {pred_order} > {samples}");
        return Err(Error::InvalidData);
    }

    let mut i = pred_order;
    for partition in 0..(1usize << rice_order) {
        // FFmpeg advances `decoded` continuously across partitions: partition
        // `p` writes at `decoded[p * samples + i]`. The first partition starts
        // at `pred_order`, every later one at zero.
        let output_base = partition * samples;
        let tmp = gb.get_bits(rice_bits);
        if tmp == rice_esc {
            let tmp_bits = gb.get_bits(5);
            while i < samples {
                decoded[output_base + i] = gb.get_sbits_long(tmp_bits);
                i += 1;
            }
        } else {
            let real_limit = if tmp > 1 {
                (i32::MAX >> (tmp - 1)) + 2
            } else {
                i32::MAX
            };
            while i < samples {
                let v = get_sr_golomb_flac(gb, tmp, real_limit, 1);
                if v == i32::MIN {
                    warn!("invalid residual");
                    return Err(Error::InvalidData);
                }
                decoded[output_base + i] = v;
                i += 1;
            }
        }
        i = 0;
    }

    Ok(())
}

/// `decode_subframe_fixed` from flacdec.c.  C's 32-bit and "wide" variants
/// produce identical low 32 bits; wrapping `u32` arithmetic mirrors both.
fn decode_subframe_fixed(
    gb: &mut BitReader<'_>,
    decoded: &mut [i32],
    pred_order: usize,
    bps: u32,
    blocksize: usize,
) -> Result<()> {
    // warm up samples
    for sample in decoded.iter_mut().take(pred_order) {
        *sample = gb.get_sbits_long(bps);
    }

    decode_residuals(gb, decoded, pred_order, blocksize)?;

    let mut a = 0u32;
    let mut b = 0u32;
    let mut c = 0u32;
    let mut d = 0u32;
    if pred_order > 0 {
        a = decoded[pred_order - 1] as u32;
    }
    if pred_order > 1 {
        b = a.wrapping_sub(decoded[pred_order - 2] as u32);
    }
    if pred_order > 2 {
        c = b
            .wrapping_sub(decoded[pred_order - 2] as u32)
            .wrapping_add(decoded[pred_order - 3] as u32);
    }
    if pred_order > 3 {
        d = c
            .wrapping_sub(decoded[pred_order - 2] as u32)
            .wrapping_add(2u32.wrapping_mul(decoded[pred_order - 3] as u32))
            .wrapping_sub(decoded[pred_order - 4] as u32);
    }

    match pred_order {
        0 => {}
        1 => {
            for i in pred_order..blocksize {
                a = a.wrapping_add(decoded[i] as u32);
                decoded[i] = a as i32;
            }
        }
        2 => {
            for i in pred_order..blocksize {
                b = b.wrapping_add(decoded[i] as u32);
                a = a.wrapping_add(b);
                decoded[i] = a as i32;
            }
        }
        3 => {
            for i in pred_order..blocksize {
                c = c.wrapping_add(decoded[i] as u32);
                b = b.wrapping_add(c);
                a = a.wrapping_add(b);
                decoded[i] = a as i32;
            }
        }
        4 => {
            for i in pred_order..blocksize {
                d = d.wrapping_add(decoded[i] as u32);
                c = c.wrapping_add(d);
                b = b.wrapping_add(c);
                a = a.wrapping_add(b);
                decoded[i] = a as i32;
            }
        }
        _ => {
            warn!("illegal pred order {pred_order}");
            return Err(Error::InvalidData);
        }
    }

    Ok(())
}

/// `flac_lpc_16_c` from flacdsp.c.
fn lpc16(decoded: &mut [i32], coeffs: &[i32; 32], pred_order: usize, qlevel: u32, len: usize) {
    let mut i = pred_order;
    // Sample index of the current prediction window (`decoded` pointer in C).
    let mut base = 0usize;
    while i < len.saturating_sub(1) {
        let mut c = coeffs[0] as u32;
        let mut d = decoded[base] as u32;
        // FFmpeg declares s0/s1 as `int` (not SUINT): the right shift below
        // must be arithmetic, even though the products are unsigned.
        let mut s0 = 0i32;
        let mut s1 = 0i32;
        for j in 1..pred_order {
            s0 = s0.wrapping_add(c.wrapping_mul(d) as i32);
            d = decoded[base + j] as u32;
            s1 = s1.wrapping_add(c.wrapping_mul(d) as i32);
            c = coeffs[j] as u32;
        }
        s0 = s0.wrapping_add(c.wrapping_mul(d) as i32);
        d = (decoded[base + pred_order] as u32).wrapping_add((s0 >> qlevel) as u32);
        decoded[base + pred_order] = d as i32;
        s1 = s1.wrapping_add(c.wrapping_mul(d) as i32);
        decoded[base + pred_order + 1] =
            (decoded[base + pred_order + 1] as u32).wrapping_add((s1 >> qlevel) as u32) as i32;

        i += 2;
        base += 2;
    }
    if i < len {
        let mut sum = 0i32;
        for j in 0..pred_order {
            sum =
                sum.wrapping_add((coeffs[j] as u32).wrapping_mul(decoded[base + j] as u32) as i32);
        }
        decoded[base + pred_order] = decoded[base + pred_order].wrapping_add(sum >> qlevel);
    }
}

/// `flac_lpc_32_c` from flacdsp.c.
fn lpc32(decoded: &mut [i32], coeffs: &[i32; 32], pred_order: usize, qlevel: u32, len: usize) {
    for i in pred_order..len {
        let mut sum = 0i64;
        for j in 0..pred_order {
            sum += coeffs[j] as i64 * decoded[i - pred_order + j] as i64;
        }
        decoded[i] = decoded[i].wrapping_add((sum >> qlevel) as i32);
    }
}

/// `lpc_analyze_remodulate` from flacdec.c.  Workaround for old buggy lavc
/// streams where the decoder must re-integrate an LPC region.
fn lpc_analyze_remodulate(
    decoded: &mut [i32],
    coeffs: &[i32; 32],
    pred_order: usize,
    qlevel: i32,
    len: usize,
    bps: u32,
) {
    let ebps = 1i32 << (bps - 1);
    let mut sigma = 0u32;
    for i in pred_order..len {
        sigma |= decoded[i].wrapping_add(ebps) as u32;
    }
    if sigma < (2i64 * ebps as i64) as u32 {
        return;
    }

    for i in (pred_order..len).rev() {
        let mut p = 0i64;
        for j in 0..pred_order {
            p += coeffs[j] as i64 * decoded[i - pred_order + j] as i64;
        }
        decoded[i] = decoded[i].wrapping_sub((p >> qlevel) as i32);
    }
    for i in pred_order..len {
        // `decoded` advances by one each iteration in C, so at sample `i` the
        // prediction window starts at `i - pred_order`.
        let base = i - pred_order;
        let mut p = 0i32;
        for j in 0..pred_order {
            p = p.wrapping_add((coeffs[j] as u32).wrapping_mul(decoded[base + j] as u32) as i32);
        }
        decoded[i] = decoded[i].wrapping_add(p >> qlevel);
    }
}

/// `decode_subframe_lpc` from flacdec.c.
fn decode_subframe_lpc(
    gb: &mut BitReader<'_>,
    decoded: &mut [i32],
    pred_order: usize,
    bps: u32,
    blocksize: usize,
    buggy_lpc: bool,
    stream_bps: u32,
) -> Result<()> {
    // warm up samples
    for sample in decoded.iter_mut().take(pred_order) {
        *sample = gb.get_sbits_long(bps);
    }

    let coeff_prec = gb.get_bits(4) + 1;
    if coeff_prec == 16 {
        warn!("invalid coeff precision");
        return Err(Error::InvalidData);
    }
    let qlevel = gb.get_sbits(5);
    if qlevel < 0 {
        warn!("qlevel {qlevel} not supported, maybe buggy stream");
        return Err(Error::InvalidData);
    }

    let mut coeffs = [0i32; 32];
    for i in 0..pred_order {
        coeffs[pred_order - i - 1] = gb.get_sbits(coeff_prec);
    }

    decode_residuals(gb, decoded, pred_order, blocksize)?;

    if (buggy_lpc && stream_bps <= 16)
        || (!buggy_lpc && bps <= 16 && bps + coeff_prec + av_log2(pred_order as u32) <= 32)
    {
        lpc16(decoded, &coeffs, pred_order, qlevel as u32, blocksize);
    } else {
        lpc32(decoded, &coeffs, pred_order, qlevel as u32, blocksize);
        if stream_bps <= 16 {
            lpc_analyze_remodulate(decoded, &coeffs, pred_order, qlevel, blocksize, bps);
        }
    }

    Ok(())
}

/// Fixed prediction for 33-bit subframes (`DECODER_SUBFRAME_FIXED_WIDE` with
/// the i64 sample array from flacdec.c).
fn decode_subframe_fixed_33(
    gb: &mut BitReader<'_>,
    decoded33: &mut [i64],
    residual: &mut [i32],
    pred_order: usize,
    blocksize: usize,
) -> Result<()> {
    for sample in decoded33.iter_mut().take(pred_order) {
        *sample = gb.get_sbits64(33);
    }
    decode_residuals(gb, residual, pred_order, blocksize)?;

    for i in pred_order..blocksize {
        let r = residual[i] as i64;
        let value = match pred_order {
            0 => r,
            1 => r.wrapping_add(decoded33[i - 1]),
            2 => r
                .wrapping_add(2i64.wrapping_mul(decoded33[i - 1]))
                .wrapping_sub(decoded33[i - 2]),
            3 => r
                .wrapping_add(3i64.wrapping_mul(decoded33[i - 1]))
                .wrapping_sub(3i64.wrapping_mul(decoded33[i - 2]))
                .wrapping_add(decoded33[i - 3]),
            4 => r
                .wrapping_add(4i64.wrapping_mul(decoded33[i - 1]))
                .wrapping_sub(6i64.wrapping_mul(decoded33[i - 2]))
                .wrapping_add(4i64.wrapping_mul(decoded33[i - 3]))
                .wrapping_sub(decoded33[i - 4]),
            _ => {
                warn!("illegal pred order {pred_order}");
                return Err(Error::InvalidData);
            }
        };
        decoded33[i] = value;
    }
    Ok(())
}

/// `flac_lpc_33_c` from flacdsp.c.
fn lpc33(
    decoded: &mut [i64],
    residual: &[i32],
    coeffs: &[i32; 32],
    pred_order: usize,
    qlevel: u32,
    len: usize,
) {
    for i in pred_order..len {
        let mut sum = 0i64;
        for j in 0..pred_order {
            sum = sum.wrapping_add(
                (coeffs[j] as i64).wrapping_mul(decoded[i - pred_order + j] as u64 as i64),
            );
        }
        decoded[i] = (residual[i] as u64 as i64).wrapping_add(sum >> qlevel);
    }
}

/// `decode_subframe_lpc_33bps` from flacdec.c.
fn decode_subframe_lpc_33(
    gb: &mut BitReader<'_>,
    decoded33: &mut [i64],
    residual: &mut [i32],
    pred_order: usize,
    blocksize: usize,
) -> Result<()> {
    for sample in decoded33.iter_mut().take(pred_order) {
        *sample = gb.get_sbits64(33);
    }

    let coeff_prec = gb.get_bits(4) + 1;
    if coeff_prec == 16 {
        warn!("invalid coeff precision");
        return Err(Error::InvalidData);
    }
    let qlevel = gb.get_sbits(5);
    if qlevel < 0 {
        warn!("qlevel {qlevel} not supported, maybe buggy stream");
        return Err(Error::InvalidData);
    }

    let mut coeffs = [0i32; 32];
    for i in 0..pred_order {
        coeffs[pred_order - i - 1] = gb.get_sbits(coeff_prec);
    }

    decode_residuals(gb, residual, pred_order, blocksize)?;
    lpc33(
        decoded33,
        residual,
        &coeffs,
        pred_order,
        qlevel as u32,
        blocksize,
    );
    Ok(())
}

/// `decode_subframe` from flacdec.c.
#[allow(clippy::too_many_arguments)] // mirrors the FFmpeg function signature
fn decode_subframe(
    gb: &mut BitReader<'_>,
    decoded: &mut [i32],
    decoded33: &mut [i64],
    channel: u32,
    blocksize: usize,
    stream_bps: u32,
    ch_mode: u32,
    buggy_lpc: bool,
) -> Result<()> {
    let mut bps = stream_bps;
    if channel == 0 {
        if ch_mode == FLAC_CHMODE_RIGHT_SIDE {
            bps += 1;
        }
    } else if ch_mode == FLAC_CHMODE_LEFT_SIDE || ch_mode == FLAC_CHMODE_MID_SIDE {
        bps += 1;
    }

    if gb.get_bits1() != 0 {
        warn!("invalid subframe padding");
        return Err(Error::InvalidData);
    }
    let subframe_type = gb.get_bits(6);

    let mut wasted = 0u32;
    if gb.get_bits1() != 0 {
        let left = gb.get_bits_left();
        if left <= 0
            || (left < bps as i32 && gb.show_bits_long(left as u32) == 0)
            || gb.show_bits_long(bps - 1) == 0
        {
            warn!("Invalid number of wasted bits > available bits ({bps}) - left={left}");
            return Err(Error::InvalidData);
        }
        wasted = 1 + gb.get_unary(1, left as usize) as u32;
        bps -= wasted;
    }

    if bps < 33 {
        match subframe_type {
            0 => {
                let tmp = gb.get_sbits_long(bps);
                decoded[..blocksize].fill(tmp);
            }
            1 => {
                for sample in decoded.iter_mut().take(blocksize) {
                    *sample = gb.get_sbits_long(bps);
                }
            }
            8..=12 => {
                let order = (subframe_type - 8) as usize;
                if bps as usize + order <= 32 {
                    decode_subframe_fixed(gb, decoded, order, bps, blocksize)?;
                } else {
                    // FFmpeg's "wide" variant has equivalent low 32 bits.
                    decode_subframe_fixed(gb, decoded, order, bps, blocksize)?;
                }
            }
            32..=63 => {
                let order = (subframe_type - 32 + 1) as usize;
                decode_subframe_lpc(gb, decoded, order, bps, blocksize, buggy_lpc, stream_bps)?;
            }
            _ => {
                warn!("invalid coding type");
                return Err(Error::InvalidData);
            }
        }
        if wasted != 0 && wasted < 32 {
            // flac_wasted_32_c
            for sample in decoded.iter_mut().take(blocksize) {
                *sample = (*sample as u32).wrapping_shl(wasted) as i32;
            }
        }
    } else {
        // bps == 33: 32-bit samples with stereo decorrelation.
        match subframe_type {
            0 => {
                let tmp = gb.get_sbits64(33);
                decoded33[..blocksize].fill(tmp);
            }
            1 => {
                for sample in decoded33.iter_mut().take(blocksize) {
                    *sample = gb.get_sbits64(33);
                }
            }
            8..=12 => {
                let order = (subframe_type - 8) as usize;
                decode_subframe_fixed_33(gb, decoded33, decoded, order, blocksize)?;
            }
            32..=63 => {
                let order = (subframe_type - 32 + 1) as usize;
                decode_subframe_lpc_33(gb, decoded33, decoded, order, blocksize)?;
            }
            _ => {
                warn!("invalid coding type");
                return Err(Error::InvalidData);
            }
        }
        if wasted != 0 && wasted + bps == 33 {
            // flac_wasted_33_c
            for i in 0..blocksize {
                decoded33[i] = (decoded[i] as u32 as u64).wrapping_shl(wasted) as i64;
            }
        }
    }

    Ok(())
}

/// `decorrelate_33bps` from flacdec.c.
fn decorrelate_33bps(ch_mode: u32, decoded: &mut [Vec<i32>], decoded33: &[i64], len: usize) {
    match ch_mode {
        FLAC_CHMODE_LEFT_SIDE => {
            for i in 0..len {
                decoded[1][i] = decoded[0][i].wrapping_sub(decoded33[i] as i32);
            }
        }
        FLAC_CHMODE_RIGHT_SIDE => {
            for i in 0..len {
                decoded[0][i] = decoded[1][i].wrapping_add(decoded33[i] as i32);
            }
        }
        FLAC_CHMODE_MID_SIDE => {
            for i in 0..len {
                let mut a = decoded[0][i] as u32 as u64;
                let b = decoded33[i];
                a = a.wrapping_sub((b >> 1) as u64);
                decoded[0][i] = a.wrapping_add(b as u64) as i32;
                decoded[1][i] = a as i32;
            }
        }
        _ => {}
    }
}

/// Pack the decorrelated i32 samples into a packed (interleaved) audio plane,
/// matching FFmpeg's `flac_decorrelate_*_c` functions from flacdsp_template.c.
fn write_output(
    decoded: &[Vec<i32>],
    channels: u32,
    blocksize: usize,
    shift: u32,
    format: SampleFormat,
    ch_mode: u32,
) -> Vec<u8> {
    let bytes_per_sample = if format == SampleFormat::S16 { 2 } else { 4 };
    let mut out = vec![0u8; blocksize * channels as usize * bytes_per_sample];
    for i in 0..blocksize {
        for ch in 0..channels as usize {
            let value = match ch_mode {
                FLAC_CHMODE_LEFT_SIDE => {
                    let a = decoded[0][i] as u32;
                    let b = decoded[1][i] as u32;
                    match ch {
                        0 => a,
                        _ => a.wrapping_sub(b),
                    }
                }
                FLAC_CHMODE_RIGHT_SIDE => {
                    let a = decoded[0][i] as u32;
                    let b = decoded[1][i] as u32;
                    match ch {
                        0 => a.wrapping_add(b),
                        _ => b,
                    }
                }
                FLAC_CHMODE_MID_SIDE => {
                    let a = decoded[0][i] as u32;
                    let b = decoded[1][i];
                    let a = a.wrapping_sub((b >> 1) as u32);
                    match ch {
                        0 => a.wrapping_add(b as u32),
                        _ => a,
                    }
                }
                _ => decoded[ch][i] as u32,
            };
            let value = value.wrapping_shl(shift);
            let offset = (i * channels as usize + ch) * bytes_per_sample;
            if bytes_per_sample == 2 {
                out[offset..offset + 2].copy_from_slice(&(value as i16).to_ne_bytes());
            } else {
                out[offset..offset + 4].copy_from_slice(&(value as i32).to_ne_bytes());
            }
        }
    }
    out
}

/// Native FLAC decoder.
pub struct FlacDecoder {
    codec_descriptor: CodecDescriptor,
    stream_info: StreamInfo,
    got_streaminfo: bool,
    output_queue: VecDeque<Frame>,
    draining: bool,
    buggy_lpc: bool,
    output_format: SampleFormat,
    sample_shift: u32,
    decoded: Vec<Vec<i32>>,
    decoded33: Vec<i64>,
    channel_layout: wedeo_core::channel_layout::ChannelLayout,
    time_base: wedeo_core::rational::Rational,
}

impl FlacDecoder {
    /// Create a decoder from stream parameters.
    pub fn new(params: CodecParameters) -> Result<Self> {
        let mut decoder = Self {
            codec_descriptor: CodecDescriptor {
                id: CodecId::Flac,
                media_type: MediaType::Audio,
                name: "flac",
                long_name: "FLAC (Free Lossless Audio Codec)",
                properties: CodecProperties::LOSSLESS,
                profiles: &[],
            },
            stream_info: StreamInfo::default(),
            got_streaminfo: false,
            output_queue: VecDeque::new(),
            draining: false,
            buggy_lpc: params.options.get_bool("use_buggy_lpc").unwrap_or(false),
            output_format: SampleFormat::S16,
            sample_shift: 0,
            decoded: Vec::new(),
            decoded33: Vec::new(),
            channel_layout: params.channel_layout.clone(),
            time_base: params.time_base,
        };

        if !params.extradata.is_empty() {
            if let Ok(si) = parse_extradata(&params.extradata) {
                decoder.apply_streaminfo(si);
            } else {
                warn!("flac: invalid extradata, will rely on frame headers");
            }
        }

        Ok(decoder)
    }

    /// `setup_streaminfo` + `flac_set_bps` from flacdec.c.
    fn apply_streaminfo(&mut self, stream_info: StreamInfo) {
        self.stream_info = stream_info;
        self.got_streaminfo = true;
        self.flac_set_bps();
        self.channel_layout = flac_channel_layout(self.stream_info.channels);
    }

    /// Select the packed output format and sample shift
    /// (`flac_set_bps` in flacdec.c). FFmpeg with the default
    /// `request_sample_fmt == NONE` uses S16 for <= 16 bps and S32 above.
    fn flac_set_bps(&mut self) {
        let bps = self.stream_info.bps;
        if bps > 16 {
            self.output_format = SampleFormat::S32;
            self.sample_shift = 32u32.saturating_sub(bps);
        } else {
            self.output_format = SampleFormat::S16;
            self.sample_shift = 16u32.saturating_sub(bps);
        }
    }

    /// Apply NEW_EXTRADATA side data (FLAC streams whose STREAMINFO changes
    /// mid-stream), matching flac_decode_frame in flacdec.c.
    fn apply_new_extradata(&mut self, data: &[u8]) -> Result<()> {
        let streaminfo = if data.len() >= 8 + 34 && &data[..4] == b"fLaC" {
            &data[8..]
        } else if data.len() == 34 {
            data
        } else {
            return Err(Error::InvalidData);
        };
        let si = if streaminfo.len() >= 8 + 34 && &streaminfo[..4] == b"fLaC" {
            parse_raw_header(streaminfo)?.0
        } else {
            StreamInfo::parse(streaminfo)?
        };
        self.apply_streaminfo(si);
        Ok(())
    }

    /// Decode one FLAC frame packet into an output frame.
    fn decode_frame(&mut self, data: &[u8], pts: i64) -> Result<()> {
        let mut gb = BitReader::new(data);
        let mut fi = decode_frame_header(&mut gb, data)?;

        if self.stream_info.channels != 0
            && fi.channels != self.stream_info.channels
            && self.got_streaminfo
        {
            self.stream_info.channels = fi.channels;
        }
        self.stream_info.channels = fi.channels;
        self.channel_layout = flac_channel_layout(fi.channels);
        let ch_mode = fi.ch_mode;

        if self.stream_info.bps == 0 && fi.bps == 0 {
            warn!("bps not found in STREAMINFO or frame header");
            return Err(Error::InvalidData);
        }
        if fi.bps == 0 {
            fi.bps = self.stream_info.bps;
        } else if self.stream_info.bps != 0 && fi.bps != self.stream_info.bps {
            warn!("switching bps mid-stream is not supported");
            return Err(Error::InvalidData);
        }
        if self.stream_info.bps == 0 {
            self.stream_info.bps = fi.bps;
            self.flac_set_bps();
        }

        if self.stream_info.max_blocksize == 0 {
            self.stream_info.max_blocksize = u16::MAX;
        }
        if fi.blocksize > self.stream_info.max_blocksize as usize {
            warn!(
                "blocksize {} > {}",
                fi.blocksize, self.stream_info.max_blocksize
            );
            return Err(Error::InvalidData);
        }

        if self.stream_info.samplerate == 0 && fi.samplerate == 0 {
            warn!("sample rate not found in STREAMINFO or frame header");
            return Err(Error::InvalidData);
        }
        if fi.samplerate == 0 {
            fi.samplerate = self.stream_info.samplerate;
        }
        self.stream_info.samplerate = fi.samplerate;

        let blocksize = fi.blocksize;
        let channels = fi.channels as usize;
        let bps = self.stream_info.bps;

        // Allocate/reuse the per-channel sample buffers (FFmpeg allocate_buffers).
        if self.decoded.len() < channels {
            self.decoded.resize_with(channels, Vec::new);
        }
        let needed = blocksize.max(self.decoded33.len());
        for channel in &mut self.decoded {
            channel.resize(needed, 0);
        }
        if self.decoded33.len() < blocksize {
            self.decoded33.resize(blocksize, 0);
        }

        let buggy_lpc = self.buggy_lpc;
        for channel in 0..channels as u32 {
            // Each subframe must operate on its own channel buffer.
            let (_, after) = self.decoded.split_at_mut(channel as usize);
            let ch_buf = &mut after[0];
            let rest33 = &mut self.decoded33;
            decode_subframe(
                &mut gb, ch_buf, rest33, channel, blocksize, bps, ch_mode, buggy_lpc,
            )?;
        }

        gb.align_to_byte();
        // frame footer: 16-bit data CRC. FFmpeg only verifies it when
        // AV_EF_CRCCHECK is enabled, which is not the default.
        gb.skip_bits(16);

        // 33 bps path: decorrelate the wide samples first, then use the plain
        // independent output stage (flac_decode_frame in flacdec.c).
        let output_mode = if bps == 32 && ch_mode > 0 {
            decorrelate_33bps(ch_mode, &mut self.decoded, &self.decoded33, blocksize);
            FLAC_CHMODE_INDEPENDENT
        } else {
            ch_mode
        };

        let pcm = write_output(
            &self.decoded[..channels],
            fi.channels,
            blocksize,
            self.sample_shift,
            self.output_format,
            output_mode,
        );

        let mut frame = Frame::new_audio(
            blocksize as u32,
            self.output_format,
            self.stream_info.samplerate,
            self.channel_layout.clone(),
        );
        frame.pts = pts;
        frame.best_effort_timestamp = pts;
        frame.duration = blocksize as i64;
        frame.time_base = self.time_base;
        if let FrameData::Audio(ref mut audio) = frame.data {
            audio.planes = vec![FramePlane {
                buffer: Buffer::from_slice(&pcm),
                offset: 0,
                linesize: pcm.len(),
            }];
        }
        self.output_queue.push_back(frame);
        Ok(())
    }
}

impl Decoder for FlacDecoder {
    fn send_packet(&mut self, packet: Option<&Packet>) -> Result<()> {
        let packet = match packet {
            None => {
                self.draining = true;
                return Ok(());
            }
            Some(pkt) => pkt,
        };

        // AVC_PKT_DATA_NEW_EXTRADATA
        if let Some(side_data) = packet
            .side_data
            .iter()
            .find(|sd| sd.data_type == PacketSideDataType::NewExtradata)
        {
            self.apply_new_extradata(&side_data.data)?;
        }

        let data = packet.data.data();
        if data.is_empty() {
            return Ok(());
        }

        // "for now, the raw FLAC header is allowed to be passed to the decoder
        // as frame data instead of extradata." (flacdec.c)
        if data.len() > 5 && &data[..5] == b"\x7fFLAC" {
            debug!("skipping flac header packet 1");
            return Ok(());
        }
        if data[0] & 0x7F == 4 {
            debug!("skipping vorbis comment");
            return Ok(());
        }

        /* check that there is at least the smallest decodable amount of data.
        this amount corresponds to the smallest valid FLAC frame possible.
        FF F8 69 02 00 00 9A 00 00 34 */
        if data.len() < FLAC_MIN_FRAME_SIZE {
            return Ok(());
        }

        /* check for inline header */
        if &data[..4] == b"fLaC" {
            if !self.got_streaminfo {
                let (si, _size) = parse_raw_header(data)?;
                self.apply_streaminfo(si);
            }
            return Ok(());
        }

        self.decode_frame(data, packet.pts)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.output_queue.pop_front() {
            return Ok(frame);
        }
        if self.draining {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.output_queue.clear();
        self.draining = false;
    }

    fn descriptor(&self) -> &CodecDescriptor {
        &self.codec_descriptor
    }
}

/// Decoder factory (priority 100: native implementation beats the symphonia
/// FLAC adapter at priority 50).
struct FlacNativeDecoderFactory;

impl DecoderFactory for FlacNativeDecoderFactory {
    fn descriptor(&self) -> &DecoderDescriptor {
        static DESC: DecoderDescriptor = DecoderDescriptor {
            codec_id: CodecId::Flac,
            name: "flac",
            long_name: "FLAC (Free Lossless Audio Codec)",
            media_type: MediaType::Audio,
            capabilities: CodecCapabilities::DR1,
            properties: CodecProperties::LOSSLESS,
            priority: 100,
        };
        &DESC
    }

    fn create(&self, params: CodecParameters) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(FlacDecoder::new(params)?))
    }
}

inventory::submit!(&FlacNativeDecoderFactory as &dyn DecoderFactory);

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny MSB-first bit writer used to build hand-crafted FLAC bitstreams.
    struct BitWriter {
        bits: Vec<u8>,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bits: Vec::new() }
        }

        fn put(&mut self, value: u64, n: usize) {
            for k in (0..n).rev() {
                self.bits.push(((value >> k) & 1) as u8);
            }
        }

        /// Signed value to FLAC Rice/Golomb code as decoded by
        /// `get_sr_golomb_flac`: u = 2*v for v >= 0, u = -2*v-1 otherwise.
        fn rice(&mut self, k: u32, value: i32) {
            let unsigned = if value >= 0 {
                (value as u64) << 1
            } else {
                ((-(value as i64) - 1) as u64) << 1 | 1
            };
            let quotient = (unsigned >> k) as usize;
            for _ in 0..quotient {
                self.put(0, 1);
            }
            self.put(1, 1);
            self.put(unsigned & ((1u64 << k) - 1), k as usize);
        }

        fn into_bytes(mut self) -> Vec<u8> {
            while !self.bits.len().is_multiple_of(8) {
                self.bits.push(0);
            }
            let mut bytes = vec![0u8; self.bits.len() / 8];
            for (i, bit) in self.bits.iter().enumerate() {
                bytes[i / 8] |= bit << (7 - (i % 8));
            }
            bytes
        }
    }

    /// Build a minimal FLAC frame with a CONSTANT subframe (mono, 44100 Hz,
    /// 16 bps, blocksize 256). The header CRC byte is at index 5.
    fn constant_frame(sample: i16, blocksize_code: u8) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(0x7FFC, 15); // sync
        w.put(0, 1); // variable blocksize
        w.put(blocksize_code as u64, 4);
        w.put(9, 4); // 44100 Hz
        w.put(0, 4); // 1 channel, independent
        w.put(4, 3); // 16 bps
        w.put(0, 1); // reserved
        w.put(0, 8); // UTF-8 frame number
        w.put(0, 8); // CRC-8 placeholder
        // Subframe header: padding=0, type=CONSTANT(0), wasted flag=0.
        w.put(0, 1);
        w.put(0, 6);
        w.put(0, 1);
        w.put(sample as u16 as u64, 16);
        w.put(0, 16); // data CRC
        let mut bytes = w.into_bytes();
        // CRC(m || CRC(m)) == 0, with m the first five header bytes.
        let crc = crc8_atm(&bytes[..5]);
        bytes[5] = crc;
        bytes
    }

    #[test]
    fn frame_constant_subframe() {
        let data = constant_frame(-1234, 8); // bs_code 8 -> 256 samples
        let mut gb = BitReader::new(&data);
        let fi = decode_frame_header(&mut gb, &data).unwrap();
        assert_eq!(fi.channels, 1);
        assert_eq!(fi.bps, 16);
        assert_eq!(fi.blocksize, 256);
        assert_eq!(fi.samplerate, 44100);

        let mut decoded = vec![0i32; 256];
        let mut decoded33 = vec![0i64; 256];
        decode_subframe(
            &mut gb,
            &mut decoded,
            &mut decoded33,
            0,
            256,
            16,
            FLAC_CHMODE_INDEPENDENT,
            false,
        )
        .unwrap();
        assert!(decoded.iter().all(|&s| s == -1234));
    }

    #[test]
    fn constant_frame_crc_must_match() {
        let mut data = constant_frame(42, 8);
        let mut gb = BitReader::new(&data);
        assert!(decode_frame_header(&mut gb, &data).is_ok());
        data[2] ^= 0xFF; // corrupt the header before the CRC byte
        let mut gb = BitReader::new(&data);
        assert!(decode_frame_header(&mut gb, &data).is_err());
    }

    #[test]
    fn decoder_emits_frame_from_constructed_packet() {
        let params = CodecParameters::new(CodecId::Flac, MediaType::Audio);
        let mut decoder = FlacDecoder::new(params).unwrap();
        let mut packet = Packet::from_slice(&constant_frame(-1234, 8));
        packet.pts = 123;
        decoder.send_packet(Some(&packet)).unwrap();
        let frame = decoder.receive_frame().unwrap();
        let audio = frame.audio().unwrap();
        assert_eq!(audio.nb_samples, 256);
        assert_eq!(audio.format, SampleFormat::S16);
        assert_eq!(audio.sample_rate, 44100);
        assert_eq!(frame.pts, 123);
        let pcm = audio.planes[0].buffer.data();
        assert_eq!(pcm.len(), 256 * 2);
        for chunk in pcm.chunks_exact(2) {
            assert_eq!(i16::from_ne_bytes([chunk[0], chunk[1]]), -1234);
        }
        assert_eq!(decoder.receive_frame().unwrap_err(), Error::Again);
        decoder.send_packet(None).unwrap();
        assert_eq!(decoder.receive_frame().unwrap_err(), Error::Eof);
    }

    #[test]
    fn utf8_frame_number() {
        let data = [0x00, 0x80, 0x00];
        let mut gb = BitReader::new(&data);
        assert_eq!(get_utf8(&mut gb), 0);
        // 0x80 as a leading byte is invalid (continuation byte).
        let data = [0x80, 0x00];
        let mut gb = BitReader::new(&data);
        assert_eq!(get_utf8(&mut gb), -1);
    }

    #[test]
    fn fixed_order_2_reconstruction() {
        // Two warm-up samples of 8 bits, then six Rice residuals of zero
        // (blocksize 8). With order 2 the prediction is
        // x[n] = 2*x[n-1] - x[n-2] + residual[n].
        let mut w = BitWriter::new();
        w.put(10, 8);
        w.put(12, 8);
        w.put(0, 2); // method type 0
        w.put(0, 4); // rice order 0 -> one partition
        w.put(4, 4); // rice parameter k = 4
        for _ in 0..6 {
            w.rice(4, 0);
        }
        let bytes = w.into_bytes();
        let mut gb = BitReader::new(&bytes);
        let mut decoded = vec![0i32; 8];
        decode_subframe_fixed(&mut gb, &mut decoded, 2, 8, 8).unwrap();
        assert_eq!(&decoded[..2], &[10, 12]);
        assert_eq!(&decoded[2..], &[14, 16, 18, 20, 22, 24]);
    }

    #[test]
    fn lpc_subframe_reconstruction() {
        // Order 2, bps 8, coefficient precision 3, qlevel 3.
        // Coefficients are written in reverse: coeffs[1] = -2, coeffs[0] = 1.
        let mut w = BitWriter::new();
        w.put(10, 8);
        w.put(12, 8);
        w.put(2, 4); // coeff_prec = 3
        w.put(3, 5); // qlevel = 3
        w.put(0b110, 3); // coeffs[1] = -2
        w.put(0b001, 3); // coeffs[0] = 1
        w.put(0, 2); // method type 0
        w.put(0, 4); // rice order 0
        w.put(3, 4); // rice parameter k = 3
        let residuals = [0i32, 1, 0, -1, 2, 0];
        for &r in &residuals {
            w.rice(3, r);
        }
        let bytes = w.into_bytes();
        let mut gb = BitReader::new(&bytes);
        let mut decoded = vec![0i32; 8];
        decode_subframe_lpc(&mut gb, &mut decoded, 2, 8, 8, false, 8).unwrap();

        let mut expected = vec![10i32, 12];
        for i in 2..8 {
            let mut sum = expected[i - 2] + -2i32 * expected[i - 1];
            sum >>= 3;
            expected.push(residuals[i - 2].wrapping_add(sum));
        }
        assert_eq!(decoded, expected);
    }

    #[test]
    fn golomb_rice_signed_values() {
        let mut w = BitWriter::new();
        for &v in &[0i32, 8, -12, 1, -1, 1000] {
            w.rice(4, v);
        }
        let bytes = w.into_bytes();
        let mut gb = BitReader::new(&bytes);
        for &v in &[0i32, 8, -12, 1, -1, 1000] {
            assert_eq!(get_sr_golomb_flac(&mut gb, 4, i32::MAX, 1), v);
        }
    }

    #[test]
    fn decorrelation_modes() {
        let decoded = vec![vec![10, 20], vec![3, 4]];
        let out = write_output(&decoded, 2, 2, 0, SampleFormat::S16, FLAC_CHMODE_LEFT_SIDE);
        let samples: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_ne_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(samples, vec![10, 7, 20, 16]);

        let out = write_output(&decoded, 2, 2, 0, SampleFormat::S16, FLAC_CHMODE_RIGHT_SIDE);
        let samples: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_ne_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(samples, vec![13, 3, 24, 4]);

        let out = write_output(&decoded, 2, 2, 0, SampleFormat::S16, FLAC_CHMODE_MID_SIDE);
        // mid = 10, side = 3: a = 10 - (3>>1) = 9,
        // left = a + side = 12, right = a = 9.
        let samples: Vec<i16> = out
            .chunks_exact(2)
            .map(|b| i16::from_ne_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(samples, vec![12, 9, 22, 18]);
    }

    #[test]
    fn mid_side_33bps_constant_subframes() {
        // Channel 0 (the mid channel) has bps=32; channel 1 (the side channel)
        // gets bps=33 because of mid-side decorrelation, exercising the
        // 33-bit sample path in flacdec.c.
        let mut w = BitWriter::new();
        w.put(0x7FFC, 15); // sync
        w.put(0, 1);
        w.put(8, 4); // blocksize 256
        w.put(9, 4); // 44100 Hz
        w.put(10, 4); // 2 channels, mid-side
        w.put(7, 3); // 32 bps
        w.put(0, 1); // reserved
        w.put(0, 8); // frame number
        w.put(0, 8); // header CRC placeholder

        // Channel 0: CONSTANT, 32-bit sample = 1000 (mid)
        w.put(0, 1);
        w.put(0, 6);
        w.put(0, 1);
        w.put(1000i32 as u32 as u64, 32);
        // Channel 1: CONSTANT, 33-bit sample = 200 (side)
        w.put(0, 1);
        w.put(0, 6);
        w.put(0, 1);
        w.put(200u64 & ((1u64 << 33) - 1), 33);
        w.put(0, 16); // data CRC

        let mut bytes = w.into_bytes();
        let crc = crc8_atm(&bytes[..5]);
        bytes[5] = crc;

        let params = CodecParameters::new(CodecId::Flac, MediaType::Audio);
        let mut decoder = FlacDecoder::new(params).unwrap();
        let mut packet = Packet::from_slice(&bytes);
        packet.pts = 0;
        decoder.send_packet(Some(&packet)).unwrap();
        let frame = decoder.receive_frame().unwrap();
        let audio = frame.audio().unwrap();
        assert_eq!(audio.format, SampleFormat::S32);
        assert_eq!(audio.nb_samples, 256);
        let pcm = audio.planes[0].buffer.data();
        let left = i32::from_ne_bytes([pcm[0], pcm[1], pcm[2], pcm[3]]);
        let right = i32::from_ne_bytes([pcm[4], pcm[5], pcm[6], pcm[7]]);
        // a = mid - (side >> 1) = 1000 - 100 = 900
        // left = a + side = 1100, right = a = 900
        assert_eq!(left, 1100);
        assert_eq!(right, 900);
    }

    #[test]
    fn wasted_bits_subframe() {
        // CONSTANT subframe with 3 wasted bits: a 13-bit value is stored and
        // shifted left by 3 to reconstruct the 16-bit sample.
        let mut w = BitWriter::new();
        w.put(0, 1); // no padding
        w.put(0, 6); // CONSTANT
        w.put(1, 1); // wasted-bits flag present
        w.put(0, 1);
        w.put(0, 1);
        w.put(1, 1); // unary(1): two zeros then stop -> wasted = 3
        w.put(5, 13); // stored sample
        let bytes = w.into_bytes();
        let mut gb = BitReader::new(&bytes);
        let mut decoded = vec![0i32; 4];
        let mut decoded33 = vec![0i64; 4];
        decode_subframe(
            &mut gb,
            &mut decoded,
            &mut decoded33,
            0,
            4,
            16,
            FLAC_CHMODE_INDEPENDENT,
            false,
        )
        .unwrap();
        assert_eq!(decoded, vec![40; 4]);
    }

    #[test]
    fn rice_escape_partition() {
        // method type 0 (rice_bits = 4), one partition, escaped parameter 15,
        // followed by a 5-bit literal width and two signed literals.
        let mut w = BitWriter::new();
        w.put(0, 2);
        w.put(0, 4);
        w.put(15, 4); // escape parameter
        w.put(8, 5); // 8-bit literals
        w.put(-5i32 as u8 as u64, 8);
        w.put(7, 8);
        let bytes = w.into_bytes();
        let mut gb = BitReader::new(&bytes);
        let mut decoded = vec![0i32; 2];
        decode_residuals(&mut gb, &mut decoded, 0, 2).unwrap();
        assert_eq!(decoded, vec![-5, 7]);
    }
}
