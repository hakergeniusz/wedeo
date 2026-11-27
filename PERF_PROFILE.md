# H.264 decoder CPU profile (2026-11-27)

Measured with `perf 7.3.5-2`, `WEDEO_THREADS=1`, release `wedeo-framecrc`,
vector `src19td.IBP.264` (720x480, 257 frames). Single decode about 5.13 s;
8 rounds, about 41 s total. `perf record -g --call-graph=dwarf -F 999`
captured 54,249 samples with zero losses. The hybrid CPU produced two PMU
tables (`cpu_atom`, `cpu_core`) that agree within 0.3 percentage points; the
table below uses `cpu_core/cycles/P`.

## Top 15 self-time symbols

| Self time | Symbol |
|---:|---|
| 13.59% | `deblock::filter_mb_edge_luma::<u8>` |
| 11.32% | `cabac::CabacReader::get_cabac` |
| 6.72% | `mc::chroma_mc_scalar` |
| 4.85% | `deblock::deblock_mb_if_enabled` |
| 4.54% | `mc::h_lowpass_direct` |
| 4.18% | `cabac::decode_cabac_residual` |
| 3.83% | `mc::avg_pixels` |
| 3.80% | `deblock::filter_mb_edge_chroma::<u8>` |
| 3.32% | `mc::v_lowpass_direct` |
| 3.18% | `cabac::decode_mb_cabac` |
| 2.76% | `mb::apply_mc_partition` |
| 2.75% | `mb::decode_inter_mb` |
| 2.64% | `idct::idct4x4_add_impl::<u8>` |
| 2.51% | `mvpred::MvContext::get_neighbors_impl` |
| 2.36% | `cabac::decode_cabac_mb_mvd_component` |

## Comparison with the old BBB 1080p profile

The old profile was measured on a different vector, so only directional
comparison is meaningful:

- Motion compensation: 47.8% then, about 26.6% now. Still a hotspot, but
  `extract_ref_block` fell from 14.8% to 1.0% and the lowpass filters from
  17% to about 8.2%. Chroma MC at 6.72% is now the largest single MC item.
- Allocator symbols: 16.2% then, 0% now. None of the 76 self-time symbols
  contains allocator, `malloc`, `free` or `Vec`. Do not spend another pass on
  per-call allocations.
- Deblocking and CABAC are now the top two. The old 1.8% CABAC number is
  stale for this CABAC-heavy vector.

## One bounded candidate (not implemented)

`filter_mb_edge_luma::<u8>` has 13.59% self time. Hoist
`get_tc0(qp, alpha_offset, cur_bs)` and the shift out of the 4-pixel `d` loop
(they depend only on edge-level parameters) and remove two per-pixel
`trace!()` calls in the inner loop. Expected effect if those costs are at
least 40% of the function's self time: wall clock 5.13 s -> <= 4.86 s
(more than 5%).

Acceptance gate: alternating A/B/A/B/A/B runs with a >5% median improvement,
bit-identical framecrc, and baseline SHA-256
`c5bccd8f86322171ab6c04a85eb9ab2620df78fe747e7b2e4aaf3a70c2c4e4ab`
(257 frames). No A/B was run, so no patch was committed.

Raw report: `/tmp/ai-agent/PERF_FINAL.md` (ephemeral).
