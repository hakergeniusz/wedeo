#![forbid(unsafe_code)]
//! Native HEVC / H.265 decoder.
//!
//! FFmpeg reference: `libavcodec/hevc/hevcdec.c` (plus `filter.c`, `cabac.c`,
//! `mvs.c`, `refs.c`, `ps.c` and `pred_template.c`).
//!
//! This crate owns the picture/slice/CTU control flow: it wires the four HEVC
//! library crates together and drives them in the same order as FFmpeg's
//! decoder. All syntax is pure Rust; there is no FFI, bindgen or c2rust.

pub mod coefficients;
pub mod decoder;
pub mod filter;
pub mod intra;
pub mod picture;
pub mod residual;
pub mod tables;

pub use decoder::HevcDecoder;
