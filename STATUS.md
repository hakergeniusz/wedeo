# Wedeo measured status

Snapshot at commit `58d2c0c` (2026-11-27). Counts are bit-exact framecrc matches
against FFmpeg unless an SNR is stated. Research appendices keep their original
dates and commits; the numbers below are the current ones.

| Area | Measured result | Details / gaps |
|---|---|---|
| H.264, four suites combined | **199/283** | [H264.md](H264.md) |
| FATE CAVLC | 50/52 | Two FMO vectors yield no frames in either decoder; [H264_FMO_NOTES.md](H264_FMO_NOTES.md) |
| FATE CABAC | 27/27 | [H264.md](H264.md) |
| JVT-AVC_V1 | 78/135 | [H264.md](H264.md) |
| JVT-FR-EXT (FRext) | 44/69 | [H264_HIGHDEPTH_GAPS.md](H264_HIGHDEPTH_GAPS.md), [PAFF_RESEARCH.md](PAFF_RESEARCH.md) |
| HEVC, native port | 20/227 | [HEVC_GAPS.md](HEVC_GAPS.md), [HEVC_MERGE_TRIAGE.md](HEVC_MERGE_TRIAGE.md) |
| VP9 | 216/226 | Two CRC differences, eight high-depth zero-frame vectors; [VP9_GAPS.md](VP9_GAPS.md), [VP9_NNZ_GAPS.md](VP9_NNZ_GAPS.md) |
| Opus SILK, `silk-lbrr-mono.mka` | 9.79 dB SNR (was -4.05 dB before pre-skip) | [DIVERGENCES.md](DIVERGENCES.md) |

## Current caveats

- PAFF top/bottom field pairing and field-view motion compensation are
  implemented as a bounded step (commit `e61b4af`), with documented trade-offs:
  the top-field first row was double-deblocked (fixed at 056ffec),
  high-depth bottom fields now run `post_decode_mb_hires` with field semantics
  (9636f08), and `mark_complete()` is a temporary guard for self-reference
  waits. Full interlaced parity is still open. See `PAFF_RESEARCH.md` Appendices
  Q and S.
- FMO is deliberately rejected: FFmpeg w62 rejects it too and emits zero
  frames. The FM1_BT_B rejection-path hang was fixed at `d86b9d2`.
- Older docs recording 3/227, 8/227, 211/226, 42/69, 197/283 or "PAFF not
  implemented" describe earlier measurements or states; use the linked gap
  docs for the measured progression.

## JVT-AVC_V1 failure classification (2026-11-27)

Of the 55 JVT-AVC_V1 FAIL entries, only `FM1_BT_B`, `FM1_FT_E` (FMO, which
FFmpeg w62 also rejects) and `SP1_BT_A` are nominally progressive. `SP1_BT_A`
was measured: Wedeo and the debug FFmpeg produce byte-identical raw YUV for all
400 frames (MD5 `8f1fe866...` on both). The suite's expected hash `ca0b3580...`
differs from FFmpeg's own output, so this FAIL is a stale reference hash, not a
decoding divergence. `sp2_bt_b` has no vector file in the reference directory.

`CABREF3_Sand_D` was inspected and is PAFF (`frame_mbs_only_flag=0,
mb_adaptive_frame_field_flag=0`), so it belongs to the interlaced workstream.

Evidence: `/tmp/ai-agent/H264_JVT_TRIAGE_GT.md` (ephemeral).

## FRext remaining 25 failures (2026-11-27)

All 25 failing JVT-FR-EXT vectors report `field_order=tt` under ffprobe, which
means `frame_mbs_only_flag=0` (PAFF/MBAFF interlaced). Pixel comparison shows
they diverge from frame 0 near the top-left Y plane with roughly 150k to 600k
differing bytes in a single frame, i.e. a global field reconstruction/deblock
effect rather than a localized single-cause bug. They belong to the interlaced
workstream, not a progressive fixable target. Previously timing-out FREH10-1/2
now pass.

Evidence: `/tmp/ai-agent/frext_no_fix_evidence.md` (ephemeral).

## FRext interlaced failure clusters (2026-11-27, all 25 measured)

Every remaining FRext failure diverges on frame 0 in the Y plane. Deblocking is
not the root cause for 23/25 (disabling it changes frame-0 differences by under
1.3%). Entropy is mixed: 17 CABAC and 8 CAVLC vectors, so a CABAC-only fix
cannot solve the group. A key observation: Wedeo's frame 0 has huge zero-filled
areas, e.g. `FREXT02_JVC_C` has 100864/101376 zero luma samples (only the first
MB column was written), suggesting missing macroblock coverage rather than
arithmetic error.

Three clusters:
1. Even-row / frame-mode intra reconstruction (13 vectors). Representative
   `FREXT02_JVC_C`. First step: trace why only the first MB column is written.
2. Odd-row / bottom-field inter reconstruction (8 vectors). Representative
   `HCAFF1_HHI_B`, with a measured bottom MB(1,1) mvd divergence [1,0] vs
   [1,-2] (PAFF_RESEARCH.md Appendix U).
3. High-depth 4:2:2 residual (4 vectors, yuv422p10le), representative
   `Hi422FREXT16_SONY_A`, deltas -1023 to +622.

Report: `/tmp/ai-agent/H264_FREXT_CLASSIFICATION.md` (ephemeral; 25/25 vectors
measured, caveat: used a prebuilt binary containing all current H.264 fixes).

## HEVC failure map at 20/227 (2026-11-27)

Classification of all 209 failing vectors (207 CRC diffs + 2 neither-decoder
frame vectors). Measured: frame CRCs, YUV first-diff coordinates/values, and
FFmpeg no-loop-filter comparisons. Stage attribution is inferred from
signatures/names/published gdb values, not Wedeo internal stage dumps.

1. **Inter prediction / MV / reference propagation (77 vectors).** 74 have an
   exact frame 0 and first differences in frames 1-4. Strongest signal: seven
   differently named vectors (`AMVP_A_MTK_4`, `SLPPLP_A/B`, `VPSID_A/B`, ...)
   share the identical signature `f1 Y(102,0) Wedeo 132 / FFmpeg 133`, plus
   groups of 5 and 4 repeated signatures - likely one shared root cause.
   Representative: `AMVP_A_MTK_4.bit`. Caveat: an exact frame 0 could also be
   a filter that rewrote reference frames after output; needs paired gdb.
2. **Bitstream / DPB / output format (74 vectors).** 35 Wedeo-only errors
   (18 `Invalid data found`, 17 `Not yet implemented`), 2 neither-decoder
   frame vectors, 36 frame-count/geometry mismatches. Representative:
   `12bit-444-qp74.hevc` hits the 10-bit+ output gate at `decoder.rs:2205`.
3. **Filter stage, deblock + SAO (24 vectors).** Boundary signatures with
   +/-1 to +/-4 pixel deltas. Representative `ipcm_D_NEC_2.bit`.

Important: `-skip_loop_filter all` cannot separate deblock from SAO. Report:
`/tmp/ai-agent/HEVC_CLASSIFICATION.md` (ephemeral).
