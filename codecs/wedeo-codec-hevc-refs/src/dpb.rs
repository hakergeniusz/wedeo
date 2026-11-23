//! Decoded picture buffer with short-term/long-term RPS management.
//!
//! FFmpeg reference: `libavcodec/hevc/refs.c`
//! (`ff_hevc_set_new_ref`, `ff_hevc_frame_rps`, `ff_hevc_clear_refs`,
//! `ff_hevc_flush_dpb`, `find_ref_idx`, `generate_missing_ref`,
//! `add_candidate_ref`, `mark_ref`, `ff_hevc_output_frames`) and
//! `libavutil/refstruct.c` (reference-counted attachments).

use wedeo_codec_hevc_ps::{MAX_DPB_SIZE, NalUnitType, SliceHeader};
use wedeo_core::{Error, Result};

use crate::frame::{FrameFlags, FrameRef, HevcFrame, MvFieldTable};
use crate::rpl::{NB_RPS_TYPE, RefPicList, RpsType};

/// Parameters for [`Dpb::set_new_ref`].
pub struct NewRefParams {
    pub poc: i32,
    pub pic_output_flag: bool,
    pub is_irap: bool,
    pub is_recovering: bool,
    pub recovery_poc: i32,
    pub output_corrupt: bool,
    pub show_all: bool,
    /// Initial per-PU motion-vector table (normally sized by the caller).
    pub tab_mvf: MvFieldTable,
}

/// Parameters for [`Dpb::frame_rps`].
pub struct FrameRpsParams<'a> {
    pub sh: &'a SliceHeader,
    pub nal_unit_type: NalUnitType,
    /// POC of the current picture.
    pub poc: i32,
    pub log2_max_poc_lsb: u32,
    pub recovery_poc: i32,
    pub output_corrupt: bool,
    pub show_all: bool,
    /// Inter-layer reference from layer 0, if present (`l0->cur_frame->poc`).
    pub inter_layer_poc: Option<i32>,
}

/// The decoded picture buffer.
///
/// Frames live in a fixed array of `MAX_DPB_SIZE` slots, mirroring FFmpeg's
/// `HEVCLayerContext.DPB`. `Option::None` denotes a free slot (FFmpeg's
/// `frame->f == NULL`). A monotonically increasing id makes [`FrameRef`]
/// handles reject stale references after a slot is recycled.
#[derive(Debug, Clone)]
pub struct Dpb {
    pub frames: Vec<Option<HevcFrame>>,
    pub rps: [RefPicList; NB_RPS_TYPE],
    pub cur_frame: Option<usize>,
    pub collocated_ref: Option<FrameRef>,
    next_id: u64,
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new(MAX_DPB_SIZE)
    }
}

impl Dpb {
    pub fn new(size: usize) -> Self {
        Self {
            frames: vec![None; size],
            rps: std::array::from_fn(|_| RefPicList::default()),
            cur_frame: None,
            collocated_ref: None,
            next_id: 1,
        }
    }

    /// FFmpeg's `alloc_frame()`: find a free DPB slot and allocate a frame.
    pub fn alloc_frame(&mut self) -> Result<usize> {
        let slot = self
            .frames
            .iter()
            .position(Option::is_none)
            .ok_or(Error::OutOfMemory)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.frames[slot] = Some(HevcFrame {
            id,
            ..Default::default()
        });
        Ok(slot)
    }

    pub fn frame(&self, slot: usize) -> Option<&HevcFrame> {
        self.frames.get(slot).and_then(Option::as_ref)
    }

    pub fn frame_mut(&mut self, slot: usize) -> Option<&mut HevcFrame> {
        self.frames.get_mut(slot).and_then(Option::as_mut)
    }

    /// Resolve a [`FrameRef`], rejecting stale handles after slot reuse.
    pub fn resolve(&self, handle: FrameRef) -> Option<&HevcFrame> {
        let frame = self.frame(handle.slot)?;
        (frame.id == handle.id).then_some(frame)
    }

    /// FFmpeg's `ff_hevc_set_new_ref()`.
    pub fn set_new_ref(&mut self, params: NewRefParams) -> Result<usize> {
        // check that this POC doesn't already exist
        if self.frames.iter().flatten().any(|f| f.poc == params.poc) {
            return Err(Error::InvalidData);
        }

        let slot = self.alloc_frame()?;
        {
            let frame = &mut self.frames[slot].as_mut().ok_or(Error::Bug)?;
            frame.tab_mvf = params.tab_mvf;
            // no_output = !IS_IRAP && (poc < recovery_poc) && HEVC_IS_RECOVERING
            //            && !(OUTPUT_CORRUPT) && !(SHOW_ALL)
            let no_output = !params.is_irap
                && params.poc < params.recovery_poc
                && params.is_recovering
                && !params.output_corrupt
                && !params.show_all;
            if params.pic_output_flag && !no_output {
                frame.flags = FrameFlags::OUTPUT;
            }
            frame.flags.insert(FrameFlags::SHORT_REF);
            frame.poc = params.poc;
        }
        self.cur_frame = Some(slot);
        self.collocated_ref = None;
        Ok(slot)
    }

    /// FFmpeg's `find_ref_idx()`.
    ///
    /// With `use_msb == false` only the low `log2_max_poc_lsb` bits are
    /// compared, and the current picture is excluded.
    pub fn find_ref_idx(
        &self,
        poc: i32,
        use_msb: bool,
        log2_max_poc_lsb: u32,
        cur_poc: i32,
    ) -> Option<usize> {
        let mask = if use_msb {
            !0
        } else {
            (1i32 << log2_max_poc_lsb) - 1
        };
        self.frames.iter().position(|maybe| {
            maybe
                .as_ref()
                .is_some_and(|f| (f.poc & mask) == poc && (use_msb || f.poc != cur_poc))
        })
    }

    /// FFmpeg's `mark_ref()`.
    pub fn mark_ref(&mut self, slot: usize, flag: FrameFlags) {
        if let Some(frame) = self.frame_mut(slot) {
            frame
                .flags
                .remove(FrameFlags::LONG_REF.union(FrameFlags::SHORT_REF));
            frame.flags.insert(flag);
        }
    }

    /// FFmpeg's `generate_missing_ref()`: an unavailable reference picture is
    /// synthesized (in FFmpeg its samples are filled with neutral grey).
    pub fn generate_missing_ref(&mut self, poc: i32) -> Result<usize> {
        let slot = self.alloc_frame()?;
        self.frames[slot].as_mut().ok_or(Error::Bug)?.poc = poc;
        self.frames[slot].as_mut().ok_or(Error::Bug)?.flags = FrameFlags::UNAVAILABLE;
        Ok(slot)
    }

    /// FFmpeg's `add_candidate_ref()`.
    // Mirrors FFmpeg's eleven-argument helper exactly; splitting it would
    // obscure the direct correspondence with `refs.c`.
    #[allow(clippy::too_many_arguments)]
    pub fn add_candidate_ref(
        &mut self,
        list_idx: RpsType,
        poc: i32,
        ref_flag: FrameFlags,
        use_msb: bool,
        log2_max_poc_lsb: u32,
        cur_poc: i32,
        nal_unit_type: NalUnitType,
        recovery_poc: i32,
        output_corrupt: bool,
        show_all: bool,
    ) -> Result<()> {
        let mut slot = self.find_ref_idx(poc, use_msb, log2_max_poc_lsb, cur_poc);

        if self.cur_frame.is_some_and(|cur| Some(cur) == slot)
            || self.rps[list_idx as usize].nb_refs >= MAX_DPB_SIZE
        {
            return Err(Error::InvalidData);
        }

        let is_irap = nal_unit_type.is_irap();
        let following = matches!(list_idx, RpsType::StFoll | RpsType::LtFoll);
        if !is_irap && !following {
            let ref_corrupt = slot.is_none_or(|s| {
                self.frame(s).is_some_and(|f| {
                    f.flags
                        .intersects(FrameFlags::CORRUPT | FrameFlags::UNAVAILABLE)
                })
            });
            let recovering = recovery_poc != i32::MAX && recovery_poc != i32::MIN;
            if ref_corrupt && !recovering {
                if !output_corrupt && !show_all {
                    return Err(Error::InvalidData);
                }
                if let Some(cur) = self.cur_frame {
                    self.frame_mut(cur)
                        .ok_or(Error::Bug)?
                        .flags
                        .insert(FrameFlags::CORRUPT);
                }
            }
        }

        if slot.is_none() {
            slot = Some(self.generate_missing_ref(poc)?);
        }
        let slot = slot.ok_or(Error::Bug)?;
        let (id, actual_poc) = {
            let frame = self.frame(slot).ok_or(Error::Bug)?;
            (frame.id, frame.poc)
        };
        let list = &mut self.rps[list_idx as usize];
        let dst = list.nb_refs;
        list.list[dst] = actual_poc;
        list.r#ref[dst] = Some(FrameRef::new(slot, id, actual_poc));
        list.nb_refs += 1;

        self.mark_ref(slot, ref_flag);
        Ok(())
    }

    /// FFmpeg's `ff_hevc_frame_rps()`.
    pub fn frame_rps(&mut self, params: &FrameRpsParams<'_>) -> Result<()> {
        let cur_slot = self.cur_frame;

        // unref_missing_refs()
        let missing: Vec<usize> = self
            .frames
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                f.as_ref()
                    .is_some_and(|f| f.flags.contains(FrameFlags::UNAVAILABLE))
                    .then_some(i)
            })
            .collect();
        for slot in missing {
            self.frames[slot] = None;
        }

        // Clear the reference flags on all frames except the current one.
        for slot in 0..self.frames.len() {
            if Some(slot) == cur_slot {
                continue;
            }
            if let Some(frame) = self.frames[slot].as_mut() {
                frame
                    .flags
                    .remove(FrameFlags::LONG_REF.union(FrameFlags::SHORT_REF));
            }
        }

        for r in &mut self.rps {
            r.nb_refs = 0;
        }

        let short_rps = params.sh.short_term_rps.clone();
        let long_rps = params.sh.long_term_rps.clone();

        if let Some(short_rps) = short_rps {
            // add the short refs
            for i in 0..short_rps.num_delta_pocs as usize {
                let poc = params.poc.wrapping_add(short_rps.delta_poc[i]);
                let list = if short_rps.used & (1 << i) == 0 {
                    RpsType::StFoll
                } else if i < short_rps.num_negative_pics as usize {
                    RpsType::StCurrBef
                } else {
                    RpsType::StCurrAft
                };
                self.add_candidate_ref(
                    list,
                    poc,
                    FrameFlags::SHORT_REF,
                    true,
                    params.log2_max_poc_lsb,
                    params.poc,
                    params.nal_unit_type,
                    params.recovery_poc,
                    params.output_corrupt,
                    params.show_all,
                )?;
            }
        }

        // add the long refs
        for i in 0..long_rps.nb_refs.min(long_rps.poc.len()) {
            let poc = long_rps.poc[i];
            let list = if long_rps.used[i] {
                RpsType::LtCurr
            } else {
                RpsType::LtFoll
            };
            let use_msb = long_rps.poc_msb_present[i];
            self.add_candidate_ref(
                list,
                poc,
                FrameFlags::LONG_REF,
                use_msb,
                params.log2_max_poc_lsb,
                params.poc,
                params.nal_unit_type,
                params.recovery_poc,
                params.output_corrupt,
                params.show_all,
            )?;
        }

        // add inter-layer refs
        if let (true, Some(poc)) = (params.sh.inter_layer_pred, params.inter_layer_poc) {
            self.add_candidate_ref(
                RpsType::InterLayer0,
                poc,
                FrameFlags::SHORT_REF,
                true,
                params.log2_max_poc_lsb,
                params.poc,
                params.nal_unit_type,
                params.recovery_poc,
                params.output_corrupt,
                params.show_all,
            )?;
        }

        // release any frames that are now unused
        for slot in 0..self.frames.len() {
            if let Some(frame) = self.frames[slot].as_mut() {
                frame.unref(FrameFlags::default());
            }
        }

        Ok(())
    }

    /// FFmpeg's `ff_hevc_clear_refs()`.
    pub fn clear_refs(&mut self) {
        for frame in self.frames.iter_mut().flatten() {
            frame.unref(FrameFlags::SHORT_REF.union(FrameFlags::LONG_REF));
        }
    }

    /// FFmpeg's `ff_hevc_flush_dpb()`.
    pub fn flush(&mut self) {
        for frame in self.frames.iter_mut() {
            *frame = None;
        }
        self.cur_frame = None;
        self.collocated_ref = None;
        for r in &mut self.rps {
            *r = RefPicList::default();
        }
    }

    /// FFmpeg's `ff_hevc_output_frames()`: output pending frames in POC order
    /// while the output/DPB limits are exceeded. Returns the frames that
    /// should be emitted (empty when `discard` is set).
    pub fn output_frames(
        &mut self,
        max_output: usize,
        max_dpb: usize,
        discard: bool,
    ) -> Result<Vec<FrameRef>> {
        let mut outputs = Vec::new();
        loop {
            let mut nb_output = 0usize;
            let mut nb_dpb = 0usize;
            let mut min_poc = i32::MAX;
            let mut min_slot = None;

            for (slot, frame) in self.frames.iter().enumerate() {
                let Some(frame) = frame else { continue };
                if frame.flags.contains(FrameFlags::OUTPUT) {
                    nb_output += 1;
                    if min_slot.is_none_or(|_| frame.poc < min_poc) {
                        min_poc = frame.poc;
                        min_slot = Some(slot);
                    }
                }
                if !frame.flags.is_empty() {
                    nb_dpb += 1;
                }
            }

            if nb_output <= max_output && (nb_output == 0 || nb_dpb <= max_dpb) {
                return Ok(outputs);
            }

            let slot = min_slot.ok_or(Error::Bug)?;
            let frame = self.frame(slot).ok_or(Error::Bug)?;
            let handle = FrameRef::new(slot, frame.id, frame.poc);
            if !discard {
                outputs.push(handle);
            }
            self.frame_mut(slot)
                .ok_or(Error::Bug)?
                .flags
                .remove(FrameFlags::OUTPUT);
        }
    }
}

impl FrameFlags {
    /// Union of two flag sets (FFmpeg's `|`).
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}
