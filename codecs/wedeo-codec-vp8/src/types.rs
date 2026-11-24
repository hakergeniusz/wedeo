// Shared VP8 decoder types, mirroring FFmpeg's vp8.h enumerations.
// LGPL-2.1-or-later, same as FFmpeg.

/// Reference frame slots (FFmpeg `VP8FrameType`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FrameType {
    #[default]
    Current = 0,
    Previous = 1,
    Golden = 2,
    AltRef = 3,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Previous,
            2 => Self::Golden,
            3 => Self::AltRef,
            _ => Self::Current,
        }
    }

    /// FFmpeg uses `VP8_FRAME_NONE == -1` to signal "no update".
    pub fn index(self) -> usize {
        self as usize
    }
}

/// Sentinel used in header parsing for `VP8_FRAME_NONE` (-1).
pub const FRAME_NONE: i32 = -1;

// Macroblock modes (vp8.h). MODE_I4x4 is also used to signal intra 4x4.
pub const MODE_I4x4: u8 = 4;
pub const MVMODE_ZERO: u8 = MODE_I4x4 + 1; // 5
pub const MVMODE_MV: u8 = 6;
pub const MVMODE_SPLIT: u8 = 7;

// Motion-vector partition modes (vp8.h).
pub const SPLITMVMODE_16x8: u8 = 0;
pub const SPLITMVMODE_8x16: u8 = 1;
pub const SPLITMVMODE_8x8: u8 = 2;
pub const SPLITMVMODE_4x4: u8 = 3;
pub const SPLITMVMODE_NONE: u8 = 4;

// Intra prediction mode identifiers (FFmpeg h264pred.h, 8-bit values).
pub const VERT_PRED: u8 = 0;
pub const HOR_PRED: u8 = 1;
pub const DC_PRED: u8 = 2;
pub const DIAG_DOWN_LEFT_PRED: u8 = 3;
pub const DIAG_DOWN_RIGHT_PRED: u8 = 4;
pub const VERT_RIGHT_PRED: u8 = 5;
pub const HOR_DOWN_PRED: u8 = 6;
pub const VERT_LEFT_PRED: u8 = 7;
pub const HOR_UP_PRED: u8 = 8;
pub const TM_VP8_PRED: u8 = 9;
pub const VERT_VP8_PRED: u8 = 10;
pub const DC_127_PRED: u8 = 12;
pub const DC_129_PRED: u8 = 13;
pub const HOR_VP8_PRED: u8 = 14;

// 16x16 / 8x8 (chroma) prediction modes.
pub const DC_PRED8x8: u8 = 0;
pub const HOR_PRED8x8: u8 = 1;
pub const VERT_PRED8x8: u8 = 2;
pub const PLANE_PRED8x8: u8 = 3;
pub const LEFT_DC_PRED8x8: u8 = 4;
pub const TOP_DC_PRED8x8: u8 = 5;
pub const DC_128_PRED8x8: u8 = 6;
pub const DC_127_PRED8x8: u8 = 7;
pub const DC_129_PRED8x8: u8 = 8;

/// A motion vector (FFmpeg `VP8mv`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mv {
    pub x: i16,
    pub y: i16,
}

impl Mv {
    pub fn new(x: i16, y: i16) -> Self {
        Self { x, y }
    }

    pub fn is_zero(self) -> bool {
        self.x == 0 && self.y == 0
    }
}

/// Motion-vector clamping bounds (FFmpeg `VP8mvbounds`).
#[derive(Clone, Copy, Debug)]
pub struct MvBounds {
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
}

/// Per-macroblock loop-filter strength (FFmpeg `VP8FilterStrength`).
#[derive(Clone, Copy, Debug, Default)]
pub struct FilterStrength {
    pub filter_level: u8,
    pub inner_limit: u8,
    pub inner_filter: bool,
}

/// All updatable binary probabilities (FFmpeg `VP8Context.prob`).
#[derive(Clone)]
pub struct Probabilities {
    pub segmentid: [u8; 3],
    pub mbskip: u8,
    pub intra: u8,
    pub last: u8,
    pub golden: u8,
    pub pred16x16: [u8; 4],
    pub pred8x8c: [u8; 3],
    /// [type][coeff_index][context][token]
    pub token: [[[[u8; 11]; 3]; 16]; 4],
    pub mvc: [[u8; 19]; 2],
    pub scan: [u8; 16],
}

impl Default for Probabilities {
    fn default() -> Self {
        Self {
            segmentid: [255; 3],
            mbskip: 255,
            intra: 255,
            last: 255,
            golden: 255,
            pred16x16: [255; 4],
            pred8x8c: [255; 3],
            token: [[[[255; 11]; 3]; 16]; 4],
            mvc: [[255; 19]; 2],
            scan: crate::data::ZIGZAG_SCAN,
        }
    }
}

/// Per-segment quantiser multipliers (FFmpeg `VP8Context.qmat[seg]`).
#[derive(Clone, Copy, Debug, Default)]
pub struct QMat {
    /// [0] DC qmul, [1] AC qmul.
    pub luma_qmul: [i32; 2],
    /// [0] DC-only luma DC qmul, [1] AC qmul.
    pub luma_dc_qmul: [i32; 2],
    pub chroma_qmul: [i32; 2],
}

/// Segmentation state (FFmpeg `VP8Context.segmentation`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Segmentation {
    pub enabled: bool,
    pub absolute_vals: bool,
    pub update_map: bool,
    pub update_feature_data: bool,
    pub base_quant: [i8; 4],
    pub filter_level: [i8; 4],
}

/// Loop-filter deltas (FFmpeg `VP8Context.lf_delta`).
#[derive(Clone, Copy, Debug, Default)]
pub struct LfDelta {
    pub enabled: bool,
    pub update: bool,
    /// Indexed by macroblock mode (only MODE_I4x4..MVMODE_SPLIT used).
    pub mode: [i8; 8],
    /// Indexed by reference frame.
    pub ref_delta: [i8; 4],
}
