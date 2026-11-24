//! Decoded picture buffers.
//!
//! FFmpeg reference: `hevcdec.h` (`HEVCFrame`, `pic_arrays_init`) and
//! `hevcdec.c` (`pic_arrays_init` / `set_sps`). Samples are stored as `u16`
//! at the picture's bit depth with `PAD` samples of padding on every side, so
//! the DSP routines (which read up to three samples outside a block) never
//! need edge emulation.

use std::collections::HashMap;

use wedeo_codec_hevc_dsp::sao::SaoParams;
use wedeo_codec_hevc_refs::{FrameRef, MvFieldTable};

/// Padding around each plane, in samples. The luma 8-tap filter needs three
/// samples on each side; `PAD = 16` leaves headroom for deblocking and SAO.
pub const PAD: usize = 16;

/// A decoded picture: sample planes plus FFmpeg's per-CTB decoding tables.
#[derive(Debug, Clone)]
pub struct Picture {
    pub poc: i32,
    /// Coded luma width/height (not cropped).
    pub width: usize,
    pub height: usize,
    pub bit_depth: u8,
    pub chroma_format_idc: u32,
    /// Plane samples, indexed as `plane[(y + PAD) * stride + x + PAD]`.
    pub planes: [Vec<u16>; 3],
    /// Per-plane stride in samples.
    pub stride: [usize; 3],
    /// Per-plane sample width/height (without padding).
    pub plane_size: [(usize, usize); 3],

    // Per-frame tables, mirroring FFmpeg's `HEVCLayerContext` arrays.
    pub qp_y_tab: Vec<i32>,
    pub min_cb_width: usize,
    pub cbf_luma: Vec<u8>,
    pub min_tb_width: usize,
    pub tab_ct_depth: Vec<u8>,
    pub skip_flag: Vec<u8>,
    pub bs_h: Vec<u8>,
    pub bs_v: Vec<u8>,
    pub bs_width: usize,
    pub is_pcm: Vec<u8>,
    pub min_pu_width: usize,
    /// Per-PU motion-vector fields (FFmpeg's `tab_mvf`).
    pub tab_mvf: MvFieldTable,
    pub tab_slice_address: Vec<i32>,
    pub sao: Vec<SaoParams>,
    pub deblock_tc: Vec<i32>,
    pub deblock_beta: Vec<i32>,
    pub filter_slice_edges: Vec<bool>,
}

impl Picture {
    /// Allocate a picture for the given coded geometry.
    #[allow(clippy::too_many_arguments)] // mirrors FFmpeg's HEVCSPS-derived geometry
    pub fn new(
        poc: i32,
        width: usize,
        height: usize,
        bit_depth: u8,
        chroma_format_idc: u32,
        min_cb_width: usize,
        min_cb_height: usize,
        min_tb_width: usize,
        min_tb_height: usize,
        min_pu_width: usize,
        min_pu_height: usize,
        ctb_count: usize,
    ) -> Self {
        let sub_w = if chroma_format_idc == 2 || chroma_format_idc == 1 {
            2
        } else {
            1
        };
        let sub_h = if chroma_format_idc == 1 { 2 } else { 1 };
        let plane_size = [
            (width, height),
            (width / sub_w, height / sub_h),
            (width / sub_w, height / sub_h),
        ];
        let mut stride = [0; 3];
        let mut planes = [Vec::new(), Vec::new(), Vec::new()];
        for c in 0..3 {
            if chroma_format_idc == 0 && c > 0 {
                continue;
            }
            let (pw, ph) = plane_size[c];
            stride[c] = pw + 2 * PAD;
            planes[c].resize(stride[c] * (ph + 2 * PAD), 1 << (bit_depth - 1));
        }
        let bs_width = width.div_ceil(4) + 4;
        let bs_height = height.div_ceil(4) + 4;
        Self {
            poc,
            width,
            height,
            bit_depth,
            chroma_format_idc,
            planes,
            stride,
            plane_size,
            qp_y_tab: vec![0; min_cb_width * min_cb_height],
            min_cb_width,
            cbf_luma: vec![0; min_tb_width * min_tb_height],
            min_tb_width,
            tab_ct_depth: vec![0; min_cb_width * min_cb_height],
            skip_flag: vec![0; min_cb_width * min_cb_height],
            bs_h: vec![0; bs_width * bs_height],
            bs_v: vec![0; bs_width * bs_height],
            bs_width,
            is_pcm: vec![0; (min_pu_width + 1) * (min_pu_height + 1)],
            min_pu_width,
            tab_mvf: MvFieldTable::new(min_pu_width, min_pu_height),
            tab_slice_address: vec![-1; ctb_count],
            sao: vec![SaoParams::default(); ctb_count],
            deblock_tc: vec![0; ctb_count],
            deblock_beta: vec![0; ctb_count],
            filter_slice_edges: vec![false; ctb_count],
        }
    }

    /// Linear index of sample `(x, y)` in plane `c` (plane coordinates).
    #[inline]
    pub fn index(&self, c: usize, x: usize, y: usize) -> usize {
        (y + PAD) * self.stride[c] + x + PAD
    }

    /// Linear index of a luma sample at luma coordinates, with chroma shift.
    #[inline]
    pub fn index_shifted(&self, c: usize, x: usize, y: usize, hshift: u32, vshift: u32) -> usize {
        self.index(c, x >> hshift, y >> vshift)
    }

    /// Read a sample at plane coordinates.
    #[inline]
    pub fn sample(&self, c: usize, x: usize, y: usize) -> u16 {
        self.planes[c][self.index(c, x, y)]
    }

    /// Write a sample at plane coordinates.
    #[inline]
    pub fn set_sample(&mut self, c: usize, x: usize, y: usize, value: u16) {
        let i = self.index(c, x, y);
        self.planes[c][i] = value;
    }

    /// Return the bounds-checked horizontal BS entry `(x, y)` in luma coords.
    pub fn bs_h_mut(&mut self, x: usize, y: usize) -> Option<&mut u8> {
        self.bs_h.get_mut((y / 4) * self.bs_width + x / 4)
    }
    pub fn bs_h_at(&self, x: usize, y: usize) -> u8 {
        self.bs_h[(y / 4) * self.bs_width + x / 4]
    }
    pub fn bs_v_mut(&mut self, x: usize, y: usize) -> Option<&mut u8> {
        self.bs_v.get_mut((y / 4) * self.bs_width + x / 4)
    }
    pub fn bs_v_at(&self, x: usize, y: usize) -> u8 {
        self.bs_v[(y / 4) * self.bs_width + x / 4]
    }

    /// Fill the entire picture with neutral grey (FFmpeg's `generate_missing_ref`).
    pub fn fill_grey(&mut self) {
        let grey = 1 << (self.bit_depth - 1);
        for c in 0..3 {
            self.planes[c].fill(grey);
        }
    }

    /// Extract the cropped luma/chroma planes for a `wedeo_core::Frame`.
    pub fn cropped_planes(
        &self,
        crop_left: u32,
        crop_top: u32,
        crop_right: u32,
        crop_bottom: u32,
    ) -> (usize, usize, Vec<(Vec<u8>, usize)>) {
        let w = self.width.saturating_sub((crop_left + crop_right) as usize);
        let h = self
            .height
            .saturating_sub((crop_top + crop_bottom) as usize);
        let mut out = Vec::with_capacity(3);
        for c in 0..3 {
            if self.chroma_format_idc == 0 && c > 0 {
                out.push((Vec::new(), 0));
                continue;
            }
            let (pw, ph) = self.plane_size[c];
            let cs = if c == 0 { 1 } else { 2 };
            let cw = pw.saturating_sub((crop_left as usize / cs) + (crop_right as usize / cs));
            let ch = ph.saturating_sub((crop_top as usize / cs) + (crop_bottom as usize / cs));
            let x0 = PAD + crop_left as usize / cs;
            let y0 = PAD + crop_top as usize / cs;
            let bytes_per_sample = usize::from(self.bit_depth > 8) + 1;
            let mut plane = Vec::with_capacity(cw * ch * bytes_per_sample);
            for y in 0..ch {
                for x in 0..cw {
                    let v = self.planes[c][(y0 + y) * self.stride[c] + x0 + x];
                    if bytes_per_sample == 1 {
                        plane.push(v as u8);
                    } else {
                        plane.extend_from_slice(&v.to_le_bytes());
                    }
                }
            }
            out.push((plane, cw * bytes_per_sample));
        }
        (w, h, out)
    }
}

/// Sample buffers for DPB frames, keyed by the stable `FrameRef` id.
#[derive(Debug, Default)]
pub struct PictureStore {
    pictures: HashMap<u64, Picture>,
}

impl PictureStore {
    pub fn insert(&mut self, handle: FrameRef, picture: Picture) {
        self.pictures.insert(handle.id, picture);
    }

    pub fn get(&self, handle: FrameRef) -> Option<&Picture> {
        self.pictures.get(&handle.id)
    }

    pub fn get_mut(&mut self, handle: FrameRef) -> Option<&mut Picture> {
        self.pictures.get_mut(&handle.id)
    }

    pub fn remove(&mut self, handle: FrameRef) -> Option<Picture> {
        self.pictures.remove(&handle.id)
    }

    pub fn clear(&mut self) {
        self.pictures.clear();
    }
}
