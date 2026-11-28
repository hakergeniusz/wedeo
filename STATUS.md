# Wedeo measured status

Verified at HEAD `bfa97e3` (2026-11-28) with a freshly built binary.
Counts are bit-exact framecrc matches against FFmpeg, except the JVT rows,
which are MD5 matches against the ITU reference YUV, and rows stating an SNR.
Research appendices keep their original dates and commits; the numbers below
are the current ones.

| Area | Measured result | Details / gaps |
|---|---|---|
| H.264, four suites combined | **199/283** | [H264.md](H264.md) |
| FATE CAVLC | 50/52 | Two FMO vectors yield no frames in either decoder; [H264_FMO_NOTES.md](H264_FMO_NOTES.md) |
| FATE CABAC | 27/27 | [H264.md](H264.md) |
| JVT-AVC_V1 | 78/135 | [H264.md](H264.md) |
| JVT-FR-EXT (FRext) | 44/69 | [H264_HIGHDEPTH_GAPS.md](H264_HIGHDEPTH_GAPS.md), [PAFF_RESEARCH.md](PAFF_RESEARCH.md) |
| HEVC, native port | 32/227 | [HEVC_GAPS.md](HEVC_GAPS.md), [HEVC_MERGE_TRIAGE.md](HEVC_MERGE_TRIAGE.md) |
| VP9 | 222/226 | Four CRC differences (one resize, one 8-bit 4:2:2, two high-depth 4:2:2); no zero-frame or error vectors; [VP9_GAPS.md](VP9_GAPS.md), [VP9_NNZ_GAPS.md](VP9_NNZ_GAPS.md) |
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
400 frames (MD5 `8f1fe866b567fe41a8327c68805c1352` on both). The suite's
expected hash `ca0b3580f448cc1e07f81a4756273b59` is not stale: it is exactly
the MD5 of `sp1_bt_a.yuv` in the official ITU archive
`SP1_BT_A.zip` (archive MD5 `23c47c4535e838cb2ac4e98eb05c9238`, matching
`source_checksum`; extracted `sp1_bt_a.h264` MD5 `5d48ba43dc52819ab3ebfd3df6e07e36`
matches the local vector). The ITU reference YUV is byte-identical to FFmpeg's
output for frames 0-9, then diverges from frame 10 (first difference at frame
10, Y(0,0): ITU 27 vs FFmpeg 33; 390/400 frames differ, no frame shift).
Therefore this FAIL is a real divergence from the ITU reference output, not a
reference-hash error; the manifest hash must not be rewritten to FFmpeg's MD5.
FFmpeg w61, w62 and system n9.1 all produce `8f1fe866...`, so no pinned-version
difference explains the mismatch. `sp2_bt_b` has no vector file in the reference
directory.

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

## HEVC failure map at 32/227 (classified 2026-11-27, rechecked 2026-11-28)

At 18/227 this covered 209 failing vectors; at the current 32/227, 195 remain
(193 CRC diffs + 2 neither-decoder frame vectors). Measured: frame CRCs,
YUV first-diff coordinates/values, and
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

## FREXT02_JVC_C first-column clue resolved (2026-11-28, no patch)

The huge zero-filled areas in Wedeo's frame 0 (100864/101376 zero luma samples)
are NOT a slice start address, missing slice, context reset or row-progress
gate. FFmpeg gdb shows a single slice with `first_mb=0` and normal MBAFF MB
order. Wedeo trace hit `CAVLC error site 532`: `zeros_left < 0` aborts at the
second MB pair, leaving the rest of the frame at zero.

FFmpeg's nC sequence at MB(1,0) diverges from Wedeo's starting at the third
residual block, matching a missing `left_block_options[3]` field/frame left
neighbor remapping. A candidate remap was tried but failed the gates: zero
samples only fell 100864 -> 100359/100608, FRext dropped 44/69 -> 42/69 with
two new errors, and the failure moved to `chroma_pred_mode`. A correct fix needs
top-neighbor and other paths together, beyond a single-cause patch.
Evidence: `/tmp/ai-agent/firstcol_evidence.md` (ephemeral).

## SP1_BT_A closed (2026-11-28, no Wedeo action)

The ITU official golden YUV MD5 ca0b3580... is correct and must not be changed. FFmpeg w62 debug (`ffmpeg_g -threads 1 -bitexact`) produces MD5 8f1fe866... byte-identical to Wedeo for all 400 frames. ITU diverges from frame 10 onward (first diff Y(0,0): ITU 27 vs FFmpeg/Wedo 33; 390/400 frames differ). Every 10th frame is an SP slice (slice_type=3) with sp_for_switch_flag=0.

Root cause is in the reference decoder: H.264 8.6.1 requires SP slices with sp_for_switch_flag=0 to re-quantize in the transform domain, but FFmpeg `h264_slice.c:1915-1919` parses and discards sp_for_switch_flag/slice_qs_delta, and `slice_type_nos = slice_type & 3` demotes SP (6) to P (2). gdb confirmed `slice_type=6 slice_type_nos=2`; the CAVLC path still has `//FIXME SP correct ?`. Wedeo likewise decodes SP as P, so it correctly matches FFmpeg. Implementing full 8.6.1 would deliberately diverge from the verification target and is not a single-cause fix. Evidence: /tmp/ai-agent/SP1_BT_A_no_fix_evidence.md (ephemeral).
