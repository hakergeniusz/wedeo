//! Picture and shared macroblock-context structures.
//!
//! These mirror the subset of FFmpeg's `MPVPicture` / `MpegEncContext`
//! (`mpegvideo.h`, `mpegpicture.h`) needed by MPEG-1/2/4 decoders. Every field
//! is public: this crate is a toolkit, and the consumer owns all state
//! transitions.

use crate::headers::{ChromaFormat, PictureStructure, PictureType};
use crate::mvpred::MotionField;
use crate::quant::QuantMatrices;
use crate::scan::ScanTable;

/// A decoded or reference picture with three planar components.
#[derive(Debug, Clone)]
pub struct Picture {
    /// Y, U, V planes (row-major, each `linesize[c]` bytes per row).
    pub data: [Vec<u8>; 3],
    pub linesize: [usize; 3],
    pub width: usize,
    pub height: usize,
    pub pict_type: PictureType,
    /// Whether this picture is used as a reference.
    pub reference: bool,
    /// Set for an allocated dummy reference frame.
    pub dummy: bool,
    /// Repeat-picture count (`repeat_first_field` handling).
    pub repeat_pict: i32,
}

impl Picture {
    /// Allocate a zeroed YUV420 picture.
    pub fn new(width: usize, height: usize) -> Self {
        let cw = width / 2;
        let ch = height / 2;
        Self {
            data: [vec![0; width * height], vec![0; cw * ch], vec![0; cw * ch]],
            linesize: [width, cw, cw],
            width,
            height,
            pict_type: PictureType::Unknown,
            reference: false,
            dummy: false,
            repeat_pict: 0,
        }
    }

    /// Fill luma with `luma` and chroma with 128 (FFmpeg's `color_frame`).
    pub fn color(&mut self, luma: u8) {
        self.data[0].fill(luma);
        self.data[1].fill(128);
        self.data[2].fill(128);
    }
}

/// Motion-vector direction flags (FFmpeg `MV_DIR_*`).
pub const MV_DIR_FORWARD: i32 = 1;
pub const MV_DIR_BACKWARD: i32 = 2;
/// Bidirectional mode where the difference equals the last P/S/I vector.
pub const MV_DIRECT: i32 = 4;

/// Motion-vector type (FFmpeg `MV_TYPE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MvType {
    #[default]
    /// One vector for the whole macroblock.
    Mv16X16 = 0,
    /// Four vectors (H.263 / MPEG-4 4MV).
    Mv8X8 = 1,
    /// Two vectors, one per 16x8 block.
    Mv16X8 = 2,
    /// Two vectors, one per field.
    Field = 3,
    /// Two vectors, MPEG-2 dual-prime.
    Dmv = 4,
}

/// Shared macroblock decoding context (FFmpeg `MpegEncContext`).
#[derive(Debug, Clone)]
pub struct MpegEncContext {
    // --- sequence parameters ---
    pub width: usize,
    pub height: usize,
    pub mb_width: usize,
    pub mb_height: usize,
    /// `mb_width + 1`, allowing simple left/top addressing.
    pub mb_stride: usize,
    /// `2 * mb_width + 1`.
    pub b8_stride: usize,
    pub codec_id: i32,
    pub out_format: i32,

    // --- quantizer matrices and scans ---
    pub intra_matrix: [u16; 64],
    pub chroma_intra_matrix: [u16; 64],
    pub inter_matrix: [u16; 64],
    pub chroma_inter_matrix: [u16; 64],
    pub intra_scantable: ScanTable,
    pub inter_scantable: ScanTable,

    // --- quantizer state ---
    pub qscale: i32,
    pub chroma_qscale: i32,
    pub y_dc_scale: i32,
    pub c_dc_scale: i32,
    /// Last non-zero coefficient index for each of the 12 blocks.
    pub block_last_index: [i32; 12],
    /// Last DC values for Y, Cb, Cr.
    pub last_dc: [i32; 3],
    pub ac_pred: bool,
    pub h263_aic: bool,

    // --- MPEG-2 / interlacing ---
    pub progressive_sequence: bool,
    pub progressive_frame: bool,
    pub mpeg_f_code: [[i32; 2]; 2],
    pub picture_structure: PictureStructure,
    pub intra_dc_precision: i32,
    pub frame_pred_frame_dct: bool,
    pub top_field_first: bool,
    pub concealment_motion_vectors: bool,
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    pub repeat_first_field: bool,
    pub chroma_420_type: bool,
    pub chroma_format: ChromaFormat,
    pub full_pel: [i32; 2],
    pub interlaced_dct: bool,
    /// 1 when decoding the first field of a field picture.
    pub first_field: bool,

    // --- macroblock state ---
    pub mb_x: usize,
    pub mb_y: usize,
    pub mb_intra: bool,
    pub mb_skipped: bool,
    pub pict_type: PictureType,
    pub mv_dir: i32,
    pub mv_type: MvType,
    /// Motion vectors: `[direction][block][x/y]`.
    pub mv: [[[i32; 2]; 4]; 2],
    pub last_mv: [[[i32; 2]; 2]; 2],
    pub field_select: [[i32; 2]; 2],
    /// Indexes into the motion-vector/chroma arrays with edge padding;
    /// `i32` matches FFmpeg's `int` and can be negative for border blocks.
    pub block_index: [i32; 6],
    pub first_slice_line: bool,
    pub resync_mb_x: usize,
    pub resync_mb_y: usize,
    pub no_rounding: bool,
    /// 1 for H.263-style prediction (MPEG-4 / H.263).
    pub h263_pred: bool,

    // --- pictures and motion-vector fields ---
    pub cur_pic: Picture,
    pub last_pic: Picture,
    pub next_pic: Picture,
    /// Motion-vector field for forward (0) and backward (1) references.
    pub motion_val: [MotionField; 2],
}

impl Default for MpegEncContext {
    fn default() -> Self {
        Self::new(0, 0)
    }
}

impl MpegEncContext {
    /// Create a context with FFmpeg's common defaults.
    pub fn new(width: usize, height: usize) -> Self {
        let mb_width = width / 16;
        let mb_height = height / 16;
        let q = QuantMatrices::new();
        Self {
            width,
            height,
            mb_width,
            mb_height,
            mb_stride: mb_width + 1,
            b8_stride: 2 * mb_width + 1,
            codec_id: 0,
            out_format: 0,
            intra_matrix: q.intra_matrix,
            chroma_intra_matrix: q.chroma_intra_matrix,
            inter_matrix: q.inter_matrix,
            chroma_inter_matrix: q.chroma_inter_matrix,
            intra_scantable: crate::scan::init_scantable_identity(&crate::tables::ZIGZAG_DIRECT),
            inter_scantable: crate::scan::init_scantable_identity(&crate::tables::ZIGZAG_DIRECT),
            qscale: 1,
            chroma_qscale: 1,
            y_dc_scale: 8,
            c_dc_scale: 8,
            block_last_index: [-1; 12],
            last_dc: [128; 3],
            ac_pred: false,
            h263_aic: false,
            progressive_sequence: true,
            progressive_frame: true,
            mpeg_f_code: [[1, 1], [1, 1]],
            picture_structure: PictureStructure::Frame,
            intra_dc_precision: 0,
            frame_pred_frame_dct: true,
            top_field_first: false,
            concealment_motion_vectors: false,
            q_scale_type: false,
            intra_vlc_format: false,
            alternate_scan: false,
            repeat_first_field: false,
            chroma_420_type: false,
            chroma_format: ChromaFormat::Chroma420,
            full_pel: [0, 0],
            interlaced_dct: false,
            first_field: false,
            mb_x: 0,
            mb_y: 0,
            mb_intra: false,
            mb_skipped: false,
            pict_type: PictureType::Unknown,
            mv_dir: 0,
            mv_type: MvType::Mv16X16,
            mv: [[[0; 2]; 4]; 2],
            last_mv: [[[0; 2]; 2]; 2],
            field_select: [[0; 2]; 2],
            block_index: [0; 6],
            first_slice_line: false,
            resync_mb_x: 0,
            resync_mb_y: 0,
            no_rounding: false,
            h263_pred: false,
            cur_pic: Picture::new(width, height),
            last_pic: Picture::new(width, height),
            next_pic: Picture::new(width, height),
            motion_val: [
                MotionField::new(mb_width, mb_height),
                MotionField::new(mb_width, mb_height),
            ],
        }
    }

    /// Recompute derived stride fields after a resolution change.
    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.mb_width = width / 16;
        self.mb_height = height / 16;
        self.mb_stride = self.mb_width + 1;
        self.b8_stride = 2 * self.mb_width + 1;
        self.cur_pic = Picture::new(width, height);
        self.last_pic = Picture::new(width, height);
        self.next_pic = Picture::new(width, height);
        self.motion_val = [
            MotionField::new(self.mb_width, self.mb_height),
            MotionField::new(self.mb_width, self.mb_height),
        ];
    }

    /// Recompute `block_index` for the current macroblock
    /// (FFmpeg `ff_init_block_index` from `mpegvideo.c`).
    ///
    /// Like FFmpeg, `mb_x`/`mb_y` are expected to be 1-based (the first
    /// macroblock is at `(1, 1)`), which is why border indices can be -2.
    pub fn init_block_index(&mut self) {
        let my = self.mb_y as i32;
        let mx = self.mb_x as i32;
        let b8 = self.b8_stride as i32;
        let ms = self.mb_stride as i32;
        let mh = self.mb_height as i32;
        self.block_index[0] = b8 * (my * 2) - 2 + mx * 2;
        self.block_index[1] = b8 * (my * 2) - 1 + mx * 2;
        self.block_index[2] = b8 * (my * 2 + 1) - 2 + mx * 2;
        self.block_index[3] = b8 * (my * 2 + 1) - 1 + mx * 2;
        self.block_index[4] = ms * (my + 1) + b8 * mh * 2 + mx - 1;
        self.block_index[5] = ms * (my + mh + 2) + b8 * mh * 2 + mx - 1;
    }

    /// Copy the quantizer matrices from `matrices`.
    pub fn set_matrices(&mut self, matrices: &QuantMatrices) {
        self.intra_matrix = matrices.intra_matrix;
        self.chroma_intra_matrix = matrices.chroma_intra_matrix;
        self.inter_matrix = matrices.inter_matrix;
        self.chroma_inter_matrix = matrices.chroma_inter_matrix;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_defaults() {
        let c = MpegEncContext::new(352, 288);
        assert_eq!(c.mb_width, 22);
        assert_eq!(c.mb_height, 18);
        assert_eq!(c.mb_stride, 23);
        assert_eq!(c.b8_stride, 45);
        assert_eq!(c.last_dc, [128, 128, 128]);
        assert_eq!(c.block_last_index, [-1; 12]);
    }

    #[test]
    fn block_index_layout() {
        let mut c = MpegEncContext::new(32, 32);
        c.mb_x = 1;
        c.mb_y = 1;
        c.init_block_index();
        // FFmpeg formula: b8_stride = 5, mb_stride = 3, mb_height = 2.
        assert_eq!(c.block_index[0], 5 * 2 - 2 + 2);
        assert_eq!(c.block_index[1], 5 * 2 - 1 + 2);
        assert_eq!(c.block_index[2], 5 * 3 - 2 + 2);
        assert_eq!(c.block_index[3], 5 * 3 - 1 + 2);
        assert_eq!(c.block_index[4], 3 * 2 + 5 * 2 * 2 + 1 - 1);
        assert_eq!(c.block_index[5], 3 * (1 + 2 + 2) + 5 * 2 * 2 + 1 - 1);
    }

    #[test]
    fn picture_color() {
        let mut p = Picture::new(16, 16);
        p.color(0);
        assert!(p.data[0].iter().all(|&v| v == 0));
        assert!(p.data[1].iter().all(|&v| v == 128));
    }
}
