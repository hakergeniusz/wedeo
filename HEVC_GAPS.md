# HEVC remaining gaps

## I_PCM sample decoding

Fixed 2026-11-27 at commit `86c1ccc` (cherry-picked from agent worktree).

`decode_pcm()` previously read only the luma samples and then called a plain
`Cabac::reinit()`. FFmpeg's `hls_pcm_sample()` (`hevcdec.c:1682`) reads luma,
then Cb and Cr, from a byte-aligned bit reader, and then calls
`skip_bytes(&lc->sub.cc, (length + 7) >> 3)` to restart CABAC past all PCM
bytes. Wedeo now follows that order and `Cabac::reinit_skipping(n)` models
`skip_bytes`.

Measured on `hevc-conformance/ipcm_B_NEC_2.bit` (1 frame, 416x240, yuv420p):
before the fix, 38133 luma bytes differed and the PCM area was shifted garbage;
after that fix, the luma plane matched FFmpeg byte for byte, including the
16x16 PCM block at x=128, y=128. At that commit the chroma planes still
differed, first at Cb x=160, y=0; commit `9f0d242` later made the vector fully
bit-exact (see "Next measured targets" below).

Side effect: vectors that previously emitted wrong frames from broken PCM
input, such as `DBLK_D_VIXS_1.bit`, now return `InvalidData` early. This is
intentional: it is better to reject a stream than to emit silently wrong
pixels.

## Overall parity

Current at `d86b9d2`: 9/227 bit-exact vectors. The progression below is
recorded at the commits named in each step.

Baseline at `f6d844c` was 3/227 bit-exact vectors: one real match,
`cbf_cr_cb_TUDepth_4_circle.h265`, and two trivial 384-byte constant
frame-packing vectors. The PCM fix does not change that count because its
target vector still has chroma differences. A full 227-vector framecrc sweep
after the PCM fix completed: 3/227 MATCH, 222 FAIL, 2 ERROR (no regression).
Commit `9f0d242` then fixed the chroma TU width passed to neighbour
availability, lifting parity to 8/227 (five IPCM vectors newly matching, the
original three retained).

`wedeo-framecrc` links only the native decoder (priority 100), not the
`oxideav` adapter, so HEVC parity numbers describe the native port.

## Next measured targets

- `ipcm_B_NEC_2.bit` is now fully bit-exact: framecrc `0xc80d43f8`, with Y,
  Cb and Cr byte-identical. The chroma bug was not in PCM sample reading:
  `hls_transform_unit()` passed the luma TU width `size << hshift` to
  `predict_chroma_tu()` / `set_neighbour_available()`. FFmpeg `hevcdec.c:1428`
  uses the chroma width `1 << (log2_trafo_size_c + hshift)`. Four call sites
  now pass `size_c << hshift`.
- `cbf_cr_cb_TUDepth_4_circle.h265` is already fully bit-exact at HEAD: Y 0,
  Cb 0 and Cr 0 differing bytes, framecrc `0x0127a0d9` on both decoders. Older
  notes claiming a residual +2 Cr offset were stale; `1973a7d` (cross-component
  prediction gate plus no Cb-to-Cr SAO copy) already fixed it. Do not spend
  another pass on that vector.
- `hevc-conformance/MERGE_E_TI_3.bit`: the Cb/Cr(67,0) divergence was fixed by
  commit `9f0d242`; the first luma difference moved to Y(384,6). Historical
  ground truth and the update are in `HEVC_MERGE_TRIAGE.md`.
- Do not reapply the odd-pointer CABAC init experiment; it made parity worse.

---

## RESOLVED: inter TU internal deblock edges (commit f32250c, 2026-11-27)

Vector `hevc-conformance/CIP_C_Panasonic_2.bit`: frame 0 exact, frame 1 had
1713 differing luma bytes, first at x=271,y=0 (Wedeo 152, FFmpeg 150); chroma
matched. gdb on both decoders showed identical MC output (162) and residual
coefficient c[15]=-12, exonerating motion compensation and residual coding.

FFmpeg `filter.c:551` measured `bs0=bs1=0` at x=264/272, y=0..24; its
`ff_hevc_deblocking_boundary_strengths()` compares only MvField for internal
edges, while `cbf_luma` is used only for a TU's own top/left edges. Wedeo's
`edge_bs()` used cbf on internal edges (`pcbf=qcbf=1` gave bs=1) and filtered
150,159 into 152,157 at x=272. A new `internal_edge_bs()` skips cbf in the two
internal loops of `boundary_strengths()`.

Verified: both frames byte-exact on all planes; full 227-vector sweep 8/227 ->
9/227 with no regressions (`DSLICE_B_HHI_5.bit` only load-timeout, already
failing before).
