# Credits and third-party code

Wedeo is a pure-Rust rewrite of FFmpeg. Most code is translated directly from
FFmpeg's C sources (LGPL-2.1-or-later) by this project's contributors.

## OxiMedia derived code

The crate `crates/wedeo-limits/` contains code derived from OxiMedia:

- Source: `crates/oximedia-codec/src/util/limits.rs` at
  https://github.com/cool-japan/oximedia
- Copyright 2024-2026 COOLJAPAN OU (Team Kitasan)
- Licensed under the Apache License, Version 2.0
  (http://www.apache.org/licenses/LICENSE-2.0)
- A copy of the Apache 2.0 text is included at
  `crates/wedeo-limits/LICENSE-APACHE`.
- The originating file is attributed in `crates/wedeo-limits/src/lib.rs`.

Modifications made in Wedeo: the API returns `wedeo_core::error::Result`
instead of `Result<_, String>`, and the crate declares `#![forbid(unsafe_code)]`.
The safety guards and their arithmetic are otherwise unchanged.

## FFmpeg

Most codec, format, filter and bitstream code is translated directly from
FFmpeg's C sources under `LGPL-2.1-or-later`. Copyright belongs to the FFmpeg
contributors; see the `FFmpeg/` source tree for the full list. Each ported
Rust module names the C file it follows. Wedeo is distributed under the same
`LGPL-2.1-or-later` license (`LICENSE`, `COPYING.LGPLv2.1`).

## Third-party crates used by Wedeo

No third-party Rust source is copied into this repository: Wedeo depends on
these projects through Cargo. Attribution is recorded here because their
algorithms and implementations are essential to the rewrite.

| Project | Used for | License | Integration |
|---|---|---|---|
| [rusty_h264-encoder](https://www.mata.network/) / `rusty_h264-common` / `rusty_h264-accel` 0.17.4 | Pure-Rust H.264 encoding and SIMD fast path | BSD-2-Clause | `adapters/wedeo-h264-rusty` (dependency only; `std` + `asm` features, no global allocator) |
| [oxideav-h265](https://crates.io/crates/oxideav-h265) 0.0.18 | Pure-Rust HEVC decoding and encoding | MIT | `adapters/wedeo-h265-oxideav` (dependency only; adapter glue is LGPL-2.1-or-later) |
| [oxideav-core](https://crates.io/crates/oxideav-core) 0.1.43 | Shared frame, packet and codec traits used by the oxideav adapters | MIT | `adapters/wedeo-h265-oxideav`, `adapters/wedeo-aac-oxideav` (dependency only) |
| [oxideav-aac](https://crates.io/crates/oxideav-aac) 0.1.7 | Pure-Rust AAC decoding and encoding | MIT | `adapters/wedeo-aac-oxideav` (dependency only; adapter glue is LGPL-2.1-or-later) |
| [oxideav-ac3](https://crates.io/crates/oxideav-ac3) 0.0.12 | Pure-Rust AC-3 and E-AC-3 decoding and encoding | MIT | `adapters/wedeo-ac3-oxideav` (dependency only) |
| [oxideav-prores](https://crates.io/crates/oxideav-prores) 0.1.3 | Pure-Rust ProRes decoding and encoding | MIT | `adapters/wedeo-prores-oxideav` (dependency only) |
| [oxideav-mpeg4video](https://crates.io/crates/oxideav-mpeg4video) 0.1.11 | Pure-Rust MPEG-4 Part 2 decoding and encoding | MIT | `adapters/wedeo-mpeg4-oxideav` (dependency only) |
| [oxideav-ffv1](https://crates.io/crates/oxideav-ffv1) 0.0.11 | Pure-Rust FFV1 lossless decoding and encoding | MIT | `adapters/wedeo-ffv1-oxideav` (dependency only) |
| [vorbis-enc](https://crates.io/crates/vorbis-enc) 0.1.1 | Pure-Rust Vorbis encoding (aoTuV b6.03 port); see its `NOTICE` for aoTuV/libvorbis credits | BSD-3-Clause | `adapters/wedeo-vorbis-enc` (dependency only; adapter glue is LGPL-2.1-or-later) |
| [Symphonia](https://github.com/pdeljanov/Symphonia) 0.5.5 | Audio demuxing and decoding (AAC, MP3, Vorbis, FLAC, ALAC, WavPack, ADPCM, PCM) | MPL-2.0 | `adapters/wedeo-symphonia` |
| [rav1d](https://github.com/memorysafety/rav1d) (git, commit `9d78516`) | AV1 decoding; pure-Rust port of dav1d | BSD-2-Clause | `adapters/wedeo-rav1d`. Its `COPYING` credits VideoLAN, the dav1d authors and the Internet Security Research Group; retain that notice in distributions |
| [av-bitstream](https://github.com/rust-av/rust-av) 0.2.1 | Exp-Golomb / bit-reader primitives (Luca Barbato, rust-av) | MIT | `crates/wedeo-codec/src/bitstream.rs` |
| [aligned-vec](https://github.com/sarah-ek/aligned-vec/) 0.6.4 | Aligned picture/sample buffers (`AVec`) | MIT | `crates/wedeo-core` |
| [rav1e](https://github.com/xiph/rav1e/) 0.8 | AV1 encoding | BSD-2-Clause | `adapters/wedeo-rav1e` |
| [Rusopus / opus-decoder](https://github.com/TadeuszWolfGang/Rusopus) 0.1.1 | Opus decoding | MIT OR Apache-2.0 | `adapters/wedeo-symphonia/src/opus.rs` |
| [rubato](https://github.com/HEnquist/rubato) 1.0.1 | Audio resampling | MIT | `crates/wedeo-resample` |
| [dcv-color-primitives](https://github.com/aws/dcv-color-primitives) 1.0.0 | I420/NV12 to RGB pixel conversion | MIT-0 | `crates/wedeo-scale` |
| [adler2](https://github.com/oyvindln/adler2) 2.0.1 | FFmpeg-style non-standard Adler-32 in framecrc | 0BSD OR MIT OR Apache-2.0 | `tests/fate` harness |
| [md-5](https://github.com/RustCrypto/hashes) 0.10.6 | MD5 for conformance vectors | MIT OR Apache-2.0 | `tests/fate` harness |
| [bitflags](https://github.com/bitflags/bitflags) 2, [inventory](https://github.com/dtolnay/inventory) 0.3, [tracing](https://github.com/tokio-rs/tracing) 0.1 / `tracing-subscriber` 0.3 | Flags, registry and diagnostics | MIT or MIT OR Apache-2.0 | workspace-wide |

Platform/utility crates (`cpal`, `wgpu`, `winit`, `pollster`, `ringbuf`,
`cc`, `libc`, `divan`) are used by the player, CLI and build harness; their
licenses are recorded in `Cargo.lock` and in each crate's published manifest.

## Upstream Wedeo project

The repository was forked from [sharifhsn/wedeo](https://github.com/sharifhsn/wedeo),
which was originally dual-licensed `MIT OR Apache-2.0`. Core traits, the
workspace layout and the verification harness inherit from that project. The
fork was later relicensed to `LGPL-2.1-or-later` to match the FFmpeg-derived
majority of the code (`LICENSE`); `README.md` also credits the upstream authors.

## License compatibility note

Wedeo's workspace is licensed `LGPL-2.1-or-later`. Apache 2.0 is not compatible
with LGPL-2.1-only, but it is compatible with GPLv3/LGPLv3. Wedeo therefore
exercises its "or later" option: any distribution that includes Apache 2.0
derived code is distributed under LGPL-3.0-or-later for the combined work,
while each third-party source file keeps its original Apache 2.0 notice.

References: GNU license list, entries for Apache License 2.0 and LGPL 2.1
(https://www.gnu.org/licenses/license-list.html).
