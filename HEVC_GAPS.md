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
after the fix, the luma plane matches FFmpeg byte for byte, including the
16x16 PCM block at x=128, y=128. The chroma planes still differ, first at
Cb x=160, y=0, so the vector is not yet bit-exact overall.

Side effect: vectors that previously emitted wrong frames from broken PCM
input, such as `DBLK_D_VIXS_1.bit`, now return `InvalidData` early. This is
intentional: it is better to reject a stream than to emit silently wrong
pixels.

## Overall parity

Baseline at `f6d844c` was 3/227 bit-exact vectors: one real match,
`cbf_cr_cb_TUDepth_4_circle.h265`, and two trivial 384-byte constant
frame-packing vectors. The PCM fix does not change that count because its
target vector still has chroma differences. A full 227-vector framecrc sweep
after the fix completed: 3/227 MATCH, 222 FAIL, 2 ERROR (both vectors where neither decoder emits frames), so there was no regression.

`wedeo-framecrc` links only the native decoder (priority 100), not the
`oxideav` adapter, so HEVC parity numbers describe the native port.

## Next measured targets

- Chroma difference at Cb x=160, y=0 in `ipcm_B_NEC_2.bit` after correct PCM
  sample reads. Extract the sample source with gdb before changing code.
- `cbf_cr_cb_TUDepth_4_circle.h265` is already fully bit-exact at HEAD: Y 0,
  Cb 0 and Cr 0 differing bytes, framecrc `0x0127a0d9` on both decoders. Older
  notes claiming a residual +2 Cr offset were stale; `1973a7d` (cross-component
  prediction gate plus no Cb-to-Cr SAO copy) already fixed it. Do not spend
  another pass on that vector.
- `hevc-conformance/MERGE_E_TI_3.bit`: frame 0 luma is byte-exact but Cb/Cr
  differ from x=67, y=0. Chroma QP/dequant and intra prediction were ruled out
  by gdb extraction; SAO parameters are the next measured comparison. See
  `HEVC_MERGE_TRIAGE.md`.
- Do not reapply the odd-pointer CABAC init experiment; it made parity worse.
