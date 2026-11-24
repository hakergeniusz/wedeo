//! MPEG-1 / MPEG-2 video decoder.
//!
//! FFmpeg reference: `libavcodec/mpeg12dec.c` (with `mpegvideo_dec.c`,
//! `mpegvideo_motion.c`, `mpegvideo_unquantize.c`, `mpeg12data.c` and
//! `simple_idct_template.c`).

pub mod bitreader;
pub mod decoder;
pub mod idct;
pub mod tables;

pub use decoder::Mpeg12Decoder;
