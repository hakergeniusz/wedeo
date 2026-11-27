# Wedeo measured status

Snapshot at commit `d86b9d2` (2026-11-27). Counts are bit-exact framecrc matches
against FFmpeg unless an SNR is stated. Research appendices keep their original
dates and commits; the numbers below are the current ones.

| Area | Measured result | Details / gaps |
|---|---|---|
| H.264, four suites combined | **198/283** | [H264.md](H264.md) |
| FATE CAVLC | 50/52 | Two FMO vectors yield no frames in either decoder; [H264_FMO_NOTES.md](H264_FMO_NOTES.md) |
| FATE CABAC | 27/27 | [H264.md](H264.md) |
| JVT-AVC_V1 | 78/135 | [H264.md](H264.md) |
| JVT-FR-EXT (FRext) | 43/69 | [H264_HIGHDEPTH_GAPS.md](H264_HIGHDEPTH_GAPS.md), [PAFF_RESEARCH.md](PAFF_RESEARCH.md) |
| HEVC, native port | 9/227 | [HEVC_GAPS.md](HEVC_GAPS.md), [HEVC_MERGE_TRIAGE.md](HEVC_MERGE_TRIAGE.md) |
| VP9 | 214/226 | Four CRC differences, eight high-depth zero-frame vectors; [VP9_GAPS.md](VP9_GAPS.md), [VP9_NNZ_GAPS.md](VP9_NNZ_GAPS.md) |
| Opus SILK, `silk-lbrr-mono.mka` | 9.79 dB SNR (was -4.05 dB before pre-skip) | [DIVERGENCES.md](DIVERGENCES.md) |

## Current caveats

- PAFF top/bottom field pairing and field-view motion compensation are
  implemented as a bounded step (commit `e61b4af`), with documented trade-offs:
  the top-field first row loses byte-exactness from paired-row deblocking,
  high-depth bottom fields skip `post_decode_mb_hires`, and `mark_complete()`
  is a temporary guard for self-reference waits. Full interlaced parity is
  still open. See `PAFF_RESEARCH.md` Appendix Q.
- FMO is deliberately rejected: FFmpeg w62 rejects it too and emits zero
  frames. The FM1_BT_B rejection-path hang was fixed at `d86b9d2`.
- Older docs recording 3/227, 8/227, 211/226, 42/69, 197/283 or "PAFF not
  implemented" describe earlier measurements or states; use the linked gap
  docs for the measured progression.
