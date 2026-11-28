# Wedeo Progress

## Current Status (2026-11-28)

Measured at HEAD `bfa97e3` with a freshly built binary. See [STATUS.md](STATUS.md)
for the authoritative compact table and the evidence links.

- H.264, four suites: **199/283** vectors BITEXACT
- H.264 FATE CAVLC: **50/52** (two FMO vectors produce no frames in either decoder)
- H.264 FATE CABAC: **27/27** progressive conformance files BITEXACT
- H.264 JVT-AVC_V1: **78/135**; JVT-FR-EXT: **44/69**
- H.264 interlaced: PAFF field pairing and field-view MC landed as a bounded step
  with documented trade-offs. Full MBAFF/PAFF pixel parity remains open; see
  `PAFF_RESEARCH.md` Appendices Q and S.
- HEVC, native port: **32/227** conformance vectors BITEXACT
- VP9: **222/226** vectors BITEXACT (four CRC differences: one resize, one 8-bit
  4:2:2, two high-depth 4:2:2; zero zero-frame vectors and zero errors). Wave 10
  added the high-depth cat6 extension-bit fix, lifting 220/226 to 222/226.
- WAV/PCM pipeline: byte-identical to FFmpeg 8.0.1 across all FATE suite samples
- Audio via symphonia: 28 decoders, 10 demuxers, SNR-verified lossy codecs
- **Video player:** GPU-accelerated via wgpu+winit, 24fps 0-drop 1080p playback,
  ffplay-style A/V sync (measured on v0.1.1; the workspace is now v0.2.0)

Workspace gates pass: `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and 1236 workspace tests.
CI is green on `bfa97e3`. Running suites requires both `TMPDIR=/var/cache/zstd5/tmp`
and `CARGO_TARGET_DIR=/var/cache/zstd5/wedeo-target`; see `AGENTS.md`.

## VP9 Decoder (historical snapshot, 2026-04-05)

The implementation notes below are from the dated session; current parity is in
`STATUS.md` and `VP9_GAPS.md`. The crate now has ~21K Rust lines across 19 modules.

- Native Rust, 12 modules, ~6K lines at that snapshot
- IVF + WebM demuxers
- Bool arithmetic decoder, frame header parsing, block-level partition/mode/coefficient decoding
- Full intra prediction (10 modes + 5 fallbacks), IDCT (4/8/16/32 + IADST), loop filter
- Inter prediction: MV parsing/prediction (spatial+temporal), 8-tap subpel MC, compound prediction, reference frame management
- 24 bugs fixed across 4 debugging sessions (2026-04-03 through 2026-04-05)
- Bool decoder symbol counts verified identical to FFmpeg across all 927 inter-frame blocks
- Deferred at that snapshot: probability adaptation, scaled prediction, odd-size
  frames, 10/12-bit, superframe parsing, multi-tile parallelism, SIMD. Most of
  these are now implemented; see `STATUS.md` and `TODO.md`.

### VP9 Bugs Fixed (2026-04-05 session)

1. **Inter-frame mode context indexing** - y_mode arrays used keyframe 2-per-8x8 indexing for inter frames; FFmpeg uses 1-per-8x8. Fixed reads in decode_inter_mode (sub-8x8, >=8x8, filter ctx) and writes in update_ctx.
2. **end_x_y/end_y_y edge clamping** - coefficient decode edge clamp doubled remaining count; `cols_4x4 - col` is already in NNZ units, no `* 2` needed.
3. **MV clamping bounds** (prior session) - `BWH_TAB[1]` -> `BWH_TAB[0]` for bw4/bh4.
4. **MC emu_edge offset** (prior session) - removed erroneous +3 column/row offset from emu buffer src pointer.

## Threading Benchmark (historical, Phase 7 - BBB 1080p 10s, 10 runs)

These numbers are from the 2026-04-05 measurement, not a current benchmark.

| Config | Wall | User | System |
|--------|------|------|--------|
| wedeo single-threaded | 18.1s +/-1.2s | 16.7s | 0.59s |
| wedeo Phase 7 (auto) | 12.8s +/-1.4s | 20.0s | 0.56s |
| FFmpeg -threads 1 | 1.50s +/-0.09s | 1.74s | 0.04s |
| FFmpeg -threads 0 | 0.80s +/-0.11s | 3.75s | 0.20s |

Threading speedup: wedeo **1.42x**, FFmpeg **1.87x**.
Single-threaded gap: wedeo is **12x slower** than FFmpeg (scalar Rust vs C+NEON).

### Profile Breakdown (active CPU, BBB 1080p)

| Subsystem | % CPU | Notes |
|-----------|-------|-------|
| Motion Compensation | ~26.6% (2026-11-27 profile) | `extract_ref_block` 1.0%, lowpass ~8.2%, chroma MC 6.72%. An SSE2 lowpass patch was bit-exact but only 1.006x median and was reverted. See `PERF_PROFILE.md`. |
| Memory alloc/free | 0% (2026-11-27 profile) | No allocator symbols in the top 76 self-time entries. Do not revisit allocations. |
| Deblocking | 18.2% (2026-11-27 profile) | `filter_mb_edge_luma::<u8>` alone is 13.59%; a bounded hoist candidate is documented in `PERF_PROFILE.md` and produced no reproducible wall-clock win. |
| MB decode + recon | 11.7% | apply_mc_bi_partition 6.8% |
| Deblocking | 10.8% (older profile) | filter_mb_edge_luma 3.3% |
| memcpy/memmove | 5.8% | System-optimized |
| CABAC | 1.8% | Inherently serial |

## Next Steps (open gaps)

- **H.264 interlaced (84 of 283 vectors still non-matching).** All 25 remaining
  JVT-FR-EXT failures are field-coded (`field_order=tt`). Three measured
  clusters: even-row frame-mode intra with only the first MB column written
  (`FREXT02_JVC_C`, traced to `zeros_left < 0` and a missing field/frame left
  remap), odd-row bottom-field inter (`HCAFF1_HHI_B`, mvd `[1,0]` vs `[1,-2]`),
  and high-depth 4:2:2 residual. See `STATUS.md` and `PAFF_RESEARCH.md`.
- **HEVC (195 of 227 vectors still non-matching).** Classified clusters: 77
  inter prediction / MV / reference propagation, 74 bitstream / DPB / output
  format, 24 deblock + SAO. Temporal candidates are now aligned after the MV
  table fix; cross-component prediction needs more measured refinement before
  any gate change. See `HEVC_GAPS.md`.
- **VP9 (four CRC differences).** `vp90-2-05-resize.ivf` needs both restored
  scaled MC and FFmpeg n9.1's swscale graph backend; a legacy bicubic prototype
  was close but not bit-exact. The remaining three are 8-bit and high-depth
  4:2:2 chroma cases. See `VP9_GAPS.md`.
- **VP9 probability adaptation** - `adapt_probs` is still not wired up between frames.
- **Player seek** - removed (was buggy), needs reimplementation with ffplay's serial mechanism.
- **Speed** - MC and deblock dominate; the SSE2 lowpass and deblock hoist experiments
  showed no reproducible wall-clock win. See `PERF_PROFILE.md`.
- **Infrastructure** - interruptible I/O, error context, chapter/program metadata,
  `find_stream_info` equivalent; see `TODO.md`.
