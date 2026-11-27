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
| HEVC, native port | 10/227 | [HEVC_GAPS.md](HEVC_GAPS.md), [HEVC_MERGE_TRIAGE.md](HEVC_MERGE_TRIAGE.md) |
| VP9 | 215/226 | Three CRC differences, eight high-depth zero-frame vectors; [VP9_GAPS.md](VP9_GAPS.md), [VP9_NNZ_GAPS.md](VP9_NNZ_GAPS.md) |
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
