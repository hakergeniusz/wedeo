# Wedeo - FFmpeg Rust Rewrite

## Project Overview

Rust rewrite of FFmpeg, translated directly from FFmpeg's C source. **No bindgen,
no c2rust, no FFI.** Pure Rust. Licensed under **LGPL-2.1-or-later** (same as FFmpeg).
Verification target: **bit-for-bit output parity** with FFmpeg's FATE test suite.
See `H264.md` for decoder architecture, module map, and conformance status.

This repo is a fork of `sharifhsn/wedeo` (`origin`). It stays on the 0.2.0 line;
the upstream v3 merge was rejected because of 52 conflicts. Only verified main
integration commits get pushed to `fork`; subagents do not push.

Measured state at HEAD `bfa97e3` (2026-11-28), from `STATUS.md`:

| Area | Result |
|---|---|
| H.264, four suites combined | 200/283 (CAVLC 50/52, CABAC 27/27, JVT-AVC_V1 79/135, JVT-FR-EXT 44/69) |
| HEVC, native port | 32/227 |
| VP9 | 222/226 (four CRC differences, no zero-frame or error vectors) |
| Opus SILK, `silk-lbrr-mono.mka` | 9.79 dB SNR |

Workspace gates pass: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, 1236 workspace tests.

## Philosophy

- **"Make it work, make it right, make it fast."** Correctness first.
- Reproduce C bugs if needed for behavioral parity.
- Copy FFmpeg comments verbatim. Don't invent docs where FFmpeg has none.

## Build & Verify

- `cargo clippy` + `cargo fmt` before considering code complete.
- `cargo nextest run` for tests (process isolation, leak detection).
- FATE: `FATE_SUITE=./fate-suite cargo nextest run --profile fate -p wedeo-fate`.
- Video cross-validate: `cargo run --bin wedeo-framecrc -- <file>` vs
  `ffmpeg -bitexact -i <file> -f framecrc -`.
- Conformance: `python3 scripts/conformance_full.py` (full report),
  `scripts/regression_check.py` (quick check).

**Always set both temp and target dirs for any suite or framecrc run:**

```sh
export TMPDIR=/var/cache/zstd5/tmp
export CARGO_TARGET_DIR=/var/cache/zstd5/wedeo-target
```

Without `TMPDIR`, the run hits `Disk quota exceeded` and reports a false 197/283.
Without `CARGO_TARGET_DIR`, `wedeo-framecrc` is missing and the runner fails with
`FileNotFoundError`.

Full H.264 suite (the gate for any H.264 change):

```sh
env TMPDIR=/var/cache/zstd5/tmp CARGO_TARGET_DIR=/var/cache/zstd5/wedeo-target \
  JVT_VECTORS=/var/cache/zstd5/wedeo-ref/jvt-vectors \
  FATE_SUITE=/var/cache/zstd5/wedeo-ref/fate-suite \
  python3 scripts/suite_runner.py --suite all
```

Reference assets: FATE vectors in `/var/cache/zstd5/wedeo-ref/fate-suite`, JVT
vectors in `/var/cache/zstd5/wedeo-ref/jvt-vectors`, debug FFmpeg at
`/var/cache/zstd5/ffmpeg-src-w62/ffmpeg_g`.

## Verification gates before any push

1. Rebuild a fresh binary with the current tree. Never trust a stale binary or a
   stale subagent report.
2. `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`.
3. Full workspace tests plus the relevant full suite (not just a triage subset).
4. Confirm git state directly: `git log`, `git status`, `git ls-remote fork refs/heads/main`.
5. Push only when local gates and CI are green. Revert patches with no measurable
   gate improvement.

## Debugging H.264 Differences

**Conformance workflow (do this in order):**

1. `scripts/conformance_full.py` - full report. `--save-snapshot` to baseline.
2. `scripts/regression_check.py` - quick check against snapshot. Run after every change.
3. `scripts/framecrc_compare.py --pixel-detail <file>` - triage specific files with per-plane pixel diffs.

**Key rules:**

- **Read the FFmpeg C code FIRST.** Key files: `h264_cavlc.c`, `h264_cabac.c`, `h264idct_template.c`, `h264_mb.c`, `h264_mb_template.c`, `h264_ps.c`, `h264_mvpred.h`.
- **HARD RULE:** After 2 failed hypotheses, **STOP theorizing**. Extract values from BOTH decoders. Find WHERE values diverge before explaining WHY. **5-minute backstop** without ground-truth extraction - use lldb.
- **When formulas look identical, the bug is in the INPUTS.** One lldb extraction > any algebraic analysis.
- **Never infer intermediate values from outputs.** Measure via lldb: `breakpoint set -f file.c -l N` - `expression`.
- **Never manually count entries in C arrays.** Write a parser or use lldb to extract table contents.
- **Build debug FFmpeg:** `cd FFmpeg && ./configure --disable-optimizations --enable-debug=3 --disable-stripping --disable-asm && make -j$(sysctl -n hw.ncpu) ffmpeg`. `--disable-asm` is critical on ARM64.
- **Tracing is always available.** `RUST_LOG=wedeo_codec_h264::mb=trace`. Never use `eprintln!` - always `tracing` macros.
- **When existing logs don't reveal the divergence, get better logs.** (1) add `trace!()`, (2) FFmpeg `-loglevel debug`, (3) lldb.
- **CABAC cat=5 (8x8 luma) has NO coded_block_flag.** The CBP check is the only gate.
- **CABAC context offsets are 2D for MBAFF** - `significant_coeff_flag_offset[MB_FIELD][cat]` uses field vs frame tables.
- **MBAFF field-mode above-neighbor stride** - `top_xy = mb_xy - (mb_stride << MB_FIELD)`.
- **MBAFF `left_block_options`** - FFmpeg's `fill_decode_neighbors` (h264_mvpred.h:491-538) selects 1 of 4 remapping variants for NNZ/CBP/intra4x4 mode context when there's a field/frame mismatch with the left neighbor. For CABAC state comparison, breakpoint at h264_cabac.c:1966 (after field flag decode).
- **CABAC neighbor cache:** FFmpeg's `fill_decode_caches` (h264_mvpred.h:576) is the canonical reference for how neighbor values populate the CABAC context. For inter/skip MBs, `intra4x4_pred_mode` = 2 (DC) without `constrained_intra_pred`, -1 with it. Wedeo has two parallel storage systems (`NeighborContext` + `CabacNeighborCtx`) - both must store consistent values.
- **Pipeline-stage tracing tags:** SLICE -> PPS_SCALING -> DEQUANT_TABLES -> COEFF -> DEQUANT -> MB_RECON -> MB_DEBLOCK.

## Code Quality

- Fix all clippy warnings. No `#[allow(clippy::*)]` without a comment.
- Prefer safe Rust. `unsafe` requires `// SAFETY:` comment.
- `Result<T, E>` + `?` for errors, not panics.
- `wrapping_add`/`wrapping_neg`/`wrapping_mul` for C overflow parity.

## Architecture

- Bottom-up: wedeo-core -> wedeo-codec/wedeo-format -> implementations -> CLI.
- Each FFmpeg library -> one Rust crate. Codecs/formats registered via `inventory`.
- Traits: `Decoder`, `Demuxer`, `Muxer`, `Filter`. Builder pattern for contexts.
- `Vec<(String, String)>` for metadata (NOT `HashMap` - deterministic ordering for FATE parity).

## Critical Technical Requirements

- SIMD padding: `vec.resize(len + 64, 0)` (matches `AV_INPUT_BUFFER_PADDING_SIZE`).
- Endianness: `u32::from_be_bytes()` / `u32::from_le_bytes()`.
- Custom `Rational` with FFmpeg-compatible rounding (not `num-rational`).
- Adler-32 in framecrc: FFmpeg's non-standard init (`s1=0, s2=0`).
- Packet sizes: `bitrate / 8 / 10 / block_align`, rounded down to power of 2.

## Working with subagents

- Subagents reply in Simplified Chinese. Code, files, filenames, and commit
  messages stay in English.
- Every patch needs measured ground truth from both decoders. No hypothesis-only
  patches, no declaring success from an unrelated unit test.
- Serialize heavy builds with `flock /var/cache/zstd5/.build-slot.lock`; at most
  two concurrent heavy builds machine-wide. Run them under `nice -n 19 ionice -c 3`.
- Report commits and pushes as measured facts, not as intentions. An agent message
  is not evidence; git and the suite logs are.

## Reference

- FFmpeg source: `./FFmpeg/` - read C code before writing Rust equivalents.
- FATE samples: `./fate-suite/`.
- `STATUS.md` - compact measured-state table (authoritative current numbers).
- `PROGRESS.md` - progress history and next steps.
- `H264.md` - decoder status; `PAFF_RESEARCH.md` - interlaced findings.
- `HEVC_GAPS.md` - HEVC failure map; `VP9_GAPS.md` - VP9 pipeline and gaps.
- `PERF_PROFILE.md` - measured hotspots and rejected speed experiments.
- `TODO.md` - tasks; `DIVERGENCES.md` - known behavioral differences.
- `CONTRIBUTING.md` - adding codecs/formats.
