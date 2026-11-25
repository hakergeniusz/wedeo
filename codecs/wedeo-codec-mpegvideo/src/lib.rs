#![forbid(unsafe_code)]
//! Shared MPEG-1/2/4 building blocks for Wedeo.
//!
//! This crate is a **reusable library**, not a decoder registration. It ports
//! the common DCT-based hybrid video machinery from FFmpeg's `libavcodec`
//! (`mpegvideo*`, `mpeg12data.c`, `mpeg12dec.c`) into pure Rust so that codec
//! crates such as `wedeo-codec-mpeg12` can build on a single, shared set of
//! tables and primitives.
//!
//! The modules exposed here are:
//!
//! - [`tables`] - quantizer matrices, zigzag/alternate scans, non-linear
//!   quantizer scales, DC scale tables and the raw VLC/RL code tables.
//! - [`scan`] - [`ScanTable`] / [`init_scantable`] (FFmpeg `ff_init_scantable`).
//! - [`vlc`] - a small canonical VLC decoder plus helpers for the MPEG-1/2
//!   DC, motion-vector and run/level codebooks.
//! - [`headers`] - sequence, sequence-extension, picture-coding-extension,
//!   display-extension and GOP header structs and bitstream parsers.
//! - [`quant`] - quantizer-matrix loading helpers.
//! - [`mvpred`] - median prediction and MPEG/H.263 motion-vector prediction.
//! - [`coeff`] - DC/AC block coefficient decoding helpers.
//! - [`unquant`] - block dequantization primitives (MPEG-1/2 and H.263).
//! - [`idct`] - 8x8 block IDCT primitives.
//! - [`picture`] - picture and macroblock/context structs shared by decoders.
//!
//! All public struct fields are `pub`: this crate is deliberately a toolkit,
//! and consumers are expected to read and mutate context state directly.

pub mod coeff;
pub mod headers;
pub mod idct;
pub mod mvpred;
pub mod picture;
pub mod quant;
pub mod scan;
pub mod tables;
pub mod unquant;
pub mod vlc;

pub use scan::{ScanTable, init_scantable, permute_scantable};
