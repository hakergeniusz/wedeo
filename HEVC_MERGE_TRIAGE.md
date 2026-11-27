# HEVC MERGE_E_TI_3.bit first-divergence ground truth

Measured 2026-11-27 at HEAD `5e47eb4`, no source change committed.

## Vector

`/var/cache/zstd5/wedeo-ref/fate-suite/hevc-conformance/MERGE_E_TI_3.bit`
(15971 bytes, 8 frames, 416x240, yuv420p, Main profile). Both decoders emit 8
frames of 149760 bytes, so this is a pixel divergence, not a frame-count issue.

## First divergence (frame 0)

| plane | differing bytes | first differing sample |
|---|---|---|
| Y | 0 / 99840 | none |
| Cb | 19769 / 49920 | x=67, y=0: Wedeo 169, FFmpeg 156 |
| Cr | 20460 / 49920 | x=67, y=0: Wedeo 120, FFmpeg 111 |

Frame 0 luma is byte-exact but chroma is not. Later frames also diverge in
luma (frame 1 first luma diff at x=0, y=0: Wedeo 139, FFmpeg 138), consistent
with a reconstructed-reference error that propagates through inter prediction.

## Hypotheses rejected with extracted values

- Chroma QP / dequant mismatch: rejected. At the chroma TU x0=128, y0=0,
  log2_trafo_size=2, c_idx=1, FFmpeg reports qp_y=32 and all QP offsets zero.
  Its nonzero levels are `720, -720, -1440, 720, 720, -2160, 2160`; Wedeo's
  `decode_residual` returns the identical set for the same block.
- Chroma motion interpolation as the first cause: rejected. The first
  divergent sample is inside an intra CTU. FFmpeg reports `MODE_INTRA`,
  planar mode 2, cand flags up=0/left=1/up_left=0/up_right=0/bottom_left=1,
  top and left samples 102, and first prediction samples 102, 102, 102, 101.

FFmpeg SAO parameters at that CTB (c=1) were extracted as `eo=3` and offsets
`0/2/0/0/0`, but Wedeo's corresponding values were not captured before the
time budget ended, so SAO cannot yet be blamed or cleared.

## Next step

Capture Wedeo's SAO parameters for the same CTB and compare with the FFmpeg
values above. If they match, extract the post-prediction/pre-SAO chroma sample
at (67,0) in both decoders. Do not patch on these partial results.

All gdb logs and scripts are under `/tmp/ai-agent/HEVC_TRIAGE2_*` (ephemeral).

---

## Update at HEAD ce02fc5 (2026-11-27): stale Cb/Cr(67,0) claim

Measured again with temporary tracing and gdb; probes were reverted and no
source change was committed. Commit `9f0d242` (chroma TU width passed to
neighbour availability) already fixed the Cb/Cr(67,0) divergence described
above, so this file's original "first divergence" is historical.

- SAO parameters match exactly: c1 Edge eo=3 offsets `0/2/0/0/0`, c2 Edge
  eo=3 offsets `0/0/0/-1/-2` on both decoders.
- Sample (67,0) also matches: FFmpeg prediction 101 + residual 55 = 156,
  Wedeo identical; deblock and SAO leave it at 156 (EO=3 top row uses
  `offset_val[0]=0`).
- The first luma difference at current HEAD moved to Y(384,6).

A clean release sweep in that worktree and in the main checkout both measured
8/227 at `ce02fc5`, with no regression. Do not chase Cb/Cr(67,0) again.
