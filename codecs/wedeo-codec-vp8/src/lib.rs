// VP8 video codec for wedeo.
//
// Pure-Rust translation of FFmpeg's VP8 decoder (libavcodec/vp8.c,
// vp8data.c, vp8dsp.c). No FFI, no bindgen. LGPL-2.1-or-later.

// Constant names mirror FFmpeg's C macro identifiers (e.g. MODE_I4x4,
// DC_PRED8x8) to keep a direct correspondence with the reference source.
#![allow(non_upper_case_globals)]

pub mod bool_decoder;
pub mod coeff;
pub mod data;
pub mod decoder;
pub mod header;
pub mod idct;
pub mod intra_pred;
pub mod loopfilter;
pub mod mb;
pub mod mc;
pub mod recon;
pub mod types;

pub use decoder::Vp8Decoder;
