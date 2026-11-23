//! Short-term reference picture set parsing (`ff_hevc_decode_short_term_rps`
//! in FFmpeg `ps.c`).

use wedeo_core::{Error, Result};

use crate::MAX_REFS;
use crate::bitreader::BitReader;

/// FFmpeg's `ShortTermRPS` (the fields relevant to parsing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortTermRps {
    pub num_negative_pics: u32,
    pub num_delta_pocs: u32,
    /// Bitmask of used pictures (`used_by_curr_pic_s0_flag` / `s1`).
    pub used: u32,
    /// Sorted delta POC values.
    pub delta_poc: [i32; 32],
    /// Set when this RPS is predicted from an earlier SPS RPS.
    pub rps_predict: bool,
    /// `delta_idx_minus1 + 1` (slice header prediction only).
    pub delta_idx: u32,
    pub delta_rps_sign: bool,
    pub abs_delta_rps: u32,
    /// Per-reference `use_delta_flag` from the prediction loop.
    pub use_delta: bool,
    /// Number of delta POCs in the referenced RPS (prediction only).
    pub rps_idx_num_delta_pocs: u32,
}

/// Decode a short-term RPS.
///
/// `sps_st_rps` is the SPS list of RPS; `is_slice_header` selects the slice
/// header syntax where an extra `delta_idx_minus1` is read. When called for an
/// entry of the SPS list itself, `sps_index` is the index of that entry; pass
/// `None` in the slice-header case. This mirrors FFmpeg's check
/// `rps != sps->st_rps && sps->nb_st_rps`.
pub fn parse_short_term_rps(
    br: &mut BitReader<'_>,
    sps_st_rps: &[ShortTermRps],
    sps_index: Option<usize>,
    is_slice_header: bool,
) -> Result<ShortTermRps> {
    let mut rps = ShortTermRps::default();

    // FFmpeg's condition is `rps != sps->st_rps && sps->nb_st_rps`: the flag
    // is absent only for the first SPS RPS (sps_index == 0). Slice-header RPS
    // and later SPS entries both read it.
    if sps_index != Some(0) && !sps_st_rps.is_empty() {
        rps.rps_predict = br.read_flag()?;
    }

    if rps.rps_predict {
        let rps_ridx: &ShortTermRps;
        if is_slice_header {
            rps.delta_idx = br.read_ue()? + 1;
            if rps.delta_idx as usize > sps_st_rps.len() {
                return Err(Error::InvalidData);
            }
            let idx = sps_st_rps.len() - rps.delta_idx as usize;
            rps_ridx = &sps_st_rps[idx];
            rps.rps_idx_num_delta_pocs = rps_ridx.num_delta_pocs;
        } else {
            let idx = sps_index.ok_or(Error::InvalidData)?;
            if idx == 0 || idx > sps_st_rps.len() {
                return Err(Error::InvalidData);
            }
            rps_ridx = &sps_st_rps[idx - 1];
        }

        rps.delta_rps_sign = br.read_flag()?;
        rps.abs_delta_rps = br.read_ue()? + 1;
        if rps.abs_delta_rps > 32768 {
            return Err(Error::InvalidData);
        }
        let delta_rps = if rps.delta_rps_sign {
            -(rps.abs_delta_rps as i32)
        } else {
            rps.abs_delta_rps as i32
        };

        let mut used = [false; 32];
        let mut k = 0usize;
        let mut k0 = 0usize;
        for i in 0..=rps_ridx.num_delta_pocs as usize {
            if k >= used.len() {
                return Err(Error::InvalidData);
            }
            used[k] = br.read_flag()?;
            rps.use_delta = false;
            if !used[k] {
                rps.use_delta = br.read_flag()?;
            }
            if used[k] || rps.use_delta {
                let delta_poc = if i < rps_ridx.num_delta_pocs as usize {
                    delta_rps.wrapping_add(rps_ridx.delta_poc[i])
                } else {
                    delta_rps
                };
                rps.delta_poc[k] = delta_poc;
                if delta_poc < 0 {
                    k0 += 1;
                }
                k += 1;
            }
        }
        if k >= used.len() {
            return Err(Error::InvalidData);
        }
        rps.num_delta_pocs = k as u32;
        rps.num_negative_pics = k0 as u32;

        // Sort in increasing order (smallest first), carrying `used`.
        for i in 1..rps.num_delta_pocs as usize {
            let delta_poc = rps.delta_poc[i];
            let u = used[i];
            let mut k = i as isize - 1;
            while k >= 0 {
                let tmp = rps.delta_poc[k as usize];
                if delta_poc < tmp {
                    rps.delta_poc[k as usize + 1] = tmp;
                    used[k as usize + 1] = used[k as usize];
                    rps.delta_poc[k as usize] = delta_poc;
                    used[k as usize] = u;
                    k -= 1;
                } else {
                    break;
                }
            }
        }
        // Flip the negative values to largest first.
        if (rps.num_negative_pics >> 1) != 0 {
            let mut hi = rps.num_negative_pics as usize - 1;
            for i in 0..(rps.num_negative_pics >> 1) as usize {
                let delta_poc = rps.delta_poc[i];
                let u = used[i];
                rps.delta_poc[i] = rps.delta_poc[hi];
                used[i] = used[hi];
                rps.delta_poc[hi] = delta_poc;
                used[hi] = u;
                hi -= 1;
            }
        }
        for i in 0..used.len() {
            rps.used |= u32::from(used[i]) << i;
        }
    } else {
        rps.num_negative_pics = br.read_ue()?;
        let nb_positive_pics = br.read_ue()?;
        if rps.num_negative_pics as usize >= MAX_REFS || nb_positive_pics as usize >= MAX_REFS {
            return Err(Error::InvalidData);
        }
        rps.num_delta_pocs = rps.num_negative_pics + nb_positive_pics;
        if rps.num_delta_pocs != 0 {
            let mut prev = 0i32;
            for i in 0..rps.num_negative_pics as usize {
                let delta_poc = br.read_ue()? + 1;
                if delta_poc == 0 || delta_poc > 32768 {
                    return Err(Error::InvalidData);
                }
                prev -= delta_poc as i32;
                rps.delta_poc[i] = prev;
                if br.read_flag()? {
                    rps.used |= 1 << i;
                }
            }
            prev = 0;
            for i in 0..nb_positive_pics as usize {
                let delta_poc = br.read_ue()? + 1;
                if delta_poc == 0 || delta_poc > 32768 {
                    return Err(Error::InvalidData);
                }
                prev += delta_poc as i32;
                rps.delta_poc[rps.num_negative_pics as usize + i] = prev;
                if br.read_flag()? {
                    rps.used |= 1 << (rps.num_negative_pics as usize + i);
                }
            }
        }
    }

    Ok(rps)
}
