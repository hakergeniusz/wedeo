#![forbid(unsafe_code)]
//! Native pure-Rust FLAC decoder.
//!
//! FFmpeg reference: `libavcodec/flacdec.c` (with `flac.c`, `flacdata.c`,
//! `flacdsp.c`, `flacdsp_template.c`).
//!
//! Ported directly from FFmpeg's C source: MSB-first bitstream parsing,
//! STREAMINFO/metadata handling, CONSTANT/VERBATIM/FIXED/LPC subframes with
//! Rice residuals, stereo decorrelation (including the 33 bps path), and
//! packed S16/S32 sample output.

mod bitstream;
mod crc;
mod data;
mod decoder;
mod streaminfo;

pub use decoder::FlacDecoder;
pub use streaminfo::StreamInfo;

use wedeo_codec as _;
use wedeo_core as _;
