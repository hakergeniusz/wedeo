# wedeo

wedeo is a pure-Rust rewrite of FFmpeg. It decodes and muxes real media, verifies its output bit-for-bit against FFmpeg's FATE suite, and runs in a video player built with wgpu. This fork is a development experiment in AI-assisted code translation and parallel agent orchestration.

This codebase is derived from two upstream projects:

- [FFmpeg](https://github.com/FFmpeg/FFmpeg): its C source is read first and translated module by module into safe Rust. No bindgen, no c2rust, no FFI.
- [the original wedeo project](https://github.com/sharifhsn/wedeo): the workspace layout, core traits, registry pattern, and verification harness came from there. This fork lives at [hakergeniusz/wedeo](https://github.com/hakergeniusz/wedeo).

Essentially all code in this fork was written by **OpenCode (Hy4 flash Free)**, directed by a human.

See [STATUS.md](STATUS.md) for a one-page snapshot of the current measured state and links to the detailed gap docs.

## Highlights

- **Bit-exact verification:** 199/283 vectors match FFmpeg across four H.264 suites (50/52 FATE CAVLC, 27/27 FATE CABAC, 78/135 JVT-AVC_V1, 44/69 JVT-FR-EXT). Two CAVLC FMO vectors produce no frames in either decoder; see `H264.md` for details.
- **Native video decoders:** H.264 (77/77 decodable progressive FATE vectors bitexact), VP9 (220/226 vectors bit-exact), VP8, MPEG-1/2, FLAC, and HEVC (32/227 vectors bit-exact against FFmpeg; native port still in progress).
- **Containers:** demuxers for WAV, IVF, MP4/MOV, Matroska/WebM, MPEG-TS, Ogg, FLV, AVI, and raw H.264/HEVC. Muxers for WAV, IVF, MP4/MOV, Matroska/WebM, MPEG-TS, and FLV.
- **Bitstream filters:** H.264 and HEVC MP4-to-Annex B, and AAC ADTS-to-ASC, with registry-driven initialization.
- **Filters:** a DAG-based filter graph with FFmpeg 8 style format negotiation, options, and EOF flush, plus crop, copy, format, fps, flip, pad, and transpose filters.
- **Adapters:** AV1 decoding through pure-Rust `rav1d`; audio through `symphonia`; AV1 encoding through `rav1e`.
- **Player:** `wedeo-play` renders video with wgpu and uses ffplay-style A/V sync.

## Quick start

Requires Rust 1.95+ (the repo pins stable with rustfmt and clippy). `nasm` is needed for the `rav1d` assembly. System `ffmpeg` is needed only for conformance checks.

```bash
git clone https://github.com/hakergeniusz/wedeo.git
cd wedeo
cargo build --workspace
cargo run --bin wedeo-cli -- -codecs
cargo run --bin wedeo-framecrc -p wedeo-fate -- tests/fate/data/vp9_keyframe_64x64.ivf
```

`-codecs` prints the FFmpeg-style codec registry. The framecrc command prints FFmpeg-compatible per-frame checksums; for the bundled 64x64 VP9 keyframe the first data line is:

```text
0,          0,          0,        1,     6144, 0xd025647b
```

Other useful commands:

```bash
cargo run --bin wedeo-cli -- -i input.mp4          # probe, like ffprobe
cargo run --bin wedeo-cli -- -i input.mp4 out.wav  # transcode audio
cargo run --bin wedeo-play -- video.mp4            # play a file
```

## Verification

Every change is expected to pass these checks:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
python3 scripts/suite_runner.py --suite all --format yuv420p \
  --check-snapshot --snapshot-dir test_suites/baselines
```

For FATE tests that read external vectors, set the read-only asset paths first:

```bash
export FATE_SUITE=/var/cache/zstd5/wedeo-ref/fate-suite
export JVT_VECTORS=/var/cache/zstd5/wedeo-ref/jvt-vectors
```

More focused helpers live in `scripts/`: `conformance_full.py`, `framecrc_compare.py`, `regression_check.py`, `fate_cross_validate.py`, and `fetch_jvt.py`.

## Architecture

```
crates/          framework: core, codec, format, filter, resample, scale, limits
codecs/          native codecs and bitstream filters (h264, vp9, vp8, flac, hevc, mpeg12, ...)
formats/         demuxers and muxers (mp4, wav, ivf, matroska, mpegts, ogg, flv, avi, hevc, ...)
adapters/        pure-Rust external crates (rav1d, rav1e, symphonia)
filters/         filter graph and individual filters
bins/            wedeo-cli and wedeo-play
tests/fate/      framecrc and FATE/JVT cross-validation harness
scripts/         conformance runners, agent orchestration, crate factory
```

Codecs, formats, filters, and bitstream filters register themselves with `inventory::submit!`, so there is no central codec enum to edit. Each work unit owns one crate. `scripts/new_crate.sh` creates a new work-unit crate, and `scripts/AGENT_PROTOCOL.md` documents the parallel-agent workflow used to build this fork.

## Limitations

This is a development fork, not a drop-in FFmpeg replacement.

- Pixel parity is incomplete for HEVC, H.264 High 10 4:2:0 and interlaced vectors, and parts of VP8 and VP9.
- Encoding is limited to adapters such as `rav1e`; there are no native H.264 or HEVC encoders.
- Network protocols, hardware decoding, subtitles, and seeking in the player are not implemented.
- Performance is well behind C FFmpeg, especially without SIMD on non-aarch64 targets.

The unsafe-code inventory and remaining risks are documented in `docs/SAFETY_AUDIT.md`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Run the verification commands above before opening a pull request. Commit messages follow Conventional Commits and carry an `Assisted-by:` trailer for AI-authored work.

## License

The project is licensed under `LGPL-2.1-or-later`; see [COPYING.LGPLv2.1](COPYING.LGPLv2.1). Code derived from OxiMedia in `crates/wedeo-limits` is Apache-2.0, with attribution in [CREDITS.md](CREDITS.md) and the license text at `crates/wedeo-limits/LICENSE-APACHE`. Distributions including that code exercise the "or later" option, so the combined work is LGPL-3.0-or-later while the third-party file keeps its Apache notice.

## Credits

- FFmpeg contributors: the original C implementations and FATE reference behavior.
- Original wedeo contributors: the workspace design and initial Rust translation.
- OxiMedia / COOLJAPAN OU (Team Kitasan): allocation guard helpers in `crates/wedeo-limits`.
- All development in this fork: OpenCode (Hy4 flash Free).
