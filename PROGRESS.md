# Wedeo Progress

## Current Status (2026-11-27)

See [STATUS.md](STATUS.md) for the compact measured-state table.

- H.264, four suites: **199/283** vectors BITEXACT
- H.264 FATE CAVLC: **50/52** (two FMO vectors produce no frames in either decoder)
- H.264 FATE CABAC: **27/27** progressive conformance files BITEXACT
- H.264 JVT-AVC_V1: **78/135**; JVT-FR-EXT: **44/69**
- H.264 interlaced: PAFF field pairing and field-view MC landed as a bounded step with documented trade-offs; full MBAFF/PAFF pixel parity remains open. See `PAFF_RESEARCH.md` Appendix Q.
- VP9: **214/226** vectors BITEXACT (4 CRC differences, 8 high-depth zero-frame vectors)
- HEVC, native port: **9/227** conformance vectors BITEXACT
- WAV/PCM pipeline: byte-identical to FFmpeg 8.0.1 across all FATE suite samples
- Audio via symphonia: 28 decoders, 10 demuxers, SNR-verified lossy codecs
- **Video player (v0.1.1):** GPU-accelerated via wgpu+winit, 24fps 0-drop 1080p playback, ffplay-style A/V sync

## VP9 Decoder (historical snapshot, 2026-04-05)

The implementation notes below are from the dated session; current parity is in
`STATUS.md` and `VP9_GAPS.md`.

- Native Rust, 12 modules, ~6K lines
- IVF + WebM demuxers
- Bool arithmetic decoder, frame header parsing, block-level partition/mode/coefficient decoding
- Full intra prediction (10 modes + 5 fallbacks), IDCT (4/8/16/32 + IADST), loop filter
- Inter prediction: MV parsing/prediction (spatial+temporal), 8-tap subpel MC, compound prediction, reference frame management
- 24 bugs fixed across 4 debugging sessions (2026-04-03 through 2026-04-05)
- Bool decoder symbol counts verified identical to FFmpeg across all 927 inter-frame blocks
- Deferred: probability adaptation (adapt_probs), scaled prediction, odd-size frames, 10/12-bit, superframe parsing, multi-tile parallelism, SIMD

### VP9 Bugs Fixed (2026-04-05 session)

1. **Inter-frame mode context indexing** — y_mode arrays used keyframe 2-per-8×8 indexing for inter frames; FFmpeg uses 1-per-8×8. Fixed reads in decode_inter_mode (sub-8x8, >=8x8, filter ctx) and writes in update_ctx.
2. **end_x_y/end_y_y edge clamping** — coefficient decode edge clamp doubled remaining count; `cols_4x4 - col` is already in NNZ units, no `* 2` needed.
3. **MV clamping bounds** (prior session) — `BWH_TAB[1]` → `BWH_TAB[0]` for bw4/bh4.
4. **MC emu_edge offset** (prior session) — removed erroneous +3 column/row offset from emu buffer src pointer.

## Threading Benchmark (historical, Phase 7 - BBB 1080p 10s, 10 runs)

These numbers are from the 2026-04-05 measurement, not a current benchmark.

| Config | Wall | User | System |
|--------|------|------|--------|
| wedeo single-threaded | 18.1s ±1.2s | 16.7s | 0.59s |
| wedeo Phase 7 (auto) | 12.8s ±1.4s | 20.0s | 0.56s |
| FFmpeg -threads 1 | 1.50s ±0.09s | 1.74s | 0.04s |
| FFmpeg -threads 0 | 0.80s ±0.11s | 3.75s | 0.20s |

Threading speedup: wedeo **1.42x**, FFmpeg **1.87x**.
Single-threaded gap: wedeo is **12x slower** than FFmpeg (scalar Rust vs C+NEON).

### Profile Breakdown (active CPU, BBB 1080p)

| Subsystem | % CPU | Notes |
|-----------|-------|-------|
| Motion Compensation | ~26.6% (2026-11-27 profile) | `extract_ref_block` 1.0%, lowpass ~8.2%, chroma MC 6.72%. An SSE2 lowpass patch was bit-exact but only 1.006x median and was reverted. See `PERF_PROFILE.md`. |
| Memory alloc/free | 0% (2026-11-27 profile) | No allocator symbols in the top 76 self-time entries. Do not revisit allocations. |
| Deblocking | 18.2% (2026-11-27 profile) | `filter_mb_edge_luma::<u8>` alone is 13.59%; a bounded hoist candidate is documented in `PERF_PROFILE.md`. |
| MB decode + recon | 11.7% | apply_mc_bi_partition 6.8% |
| Deblocking | 10.8% | filter_mb_edge_luma 3.3% |
| memcpy/memmove | 5.8% | System-optimized |
| CABAC | 1.8% | Inherently serial |

## Next Steps (open gaps)

- VP9 inter frame conformance - 4 CRC differences remain; 8 high-depth vectors emit zero frames (see `VP9_GAPS.md`)
- VP9 probability adaptation - `adapt_probs` not yet wired up between frames
- Seek reimplementation - ffplay serial mechanism for stale frame discard
- SIMD for MC lowpass filters (NEON) - primary target, 3x slower at qpel(2,2) vs (0,0)
- Full interlaced (MBAFF/PAFF) pixel parity - field pairing and field-view MC are in place; remaining work is in `PAFF_RESEARCH.md` Appendix Q
- HEVC native pixel parity - 9/227; next targets in `HEVC_GAPS.md`
