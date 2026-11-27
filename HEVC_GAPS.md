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

Current at `8e13c1e`: 9/227 bit-exact vectors. The progression below is
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

## CIP_A_Panasonic_3.bit first divergence (2026-11-27, no patch)

Vector: `hevc-conformance/CIP_A_Panasonic_3.bit` (11957 bytes, 2 frames,
416x240 yuv420p, Main). Frame 0 bit-exact (`0x6e1f16d0`), frame 1 differs:
Wedeo `0x3e35865a`, FFmpeg `0x5be5bde9`. Frame 1 diffs: Y 2353/99840,
Cb 300/24960, Cr 634/24960. First luma difference at (80,61): Wedeo 124,
FFmpeg 123; large full-8x8-block differences start at CTU (1,1)/(1,2), e.g.
(64,64) 122 vs 116 and (66,64) 120 vs 95.

FFmpeg gdb values at `deblocking_filter_CTB()` entry for CTB (64,64), POC 1:
Y(80,61)=123, Y(64,64)=111, Y(66,64)=101, Y(70,70)=98, Y(80,64)=121,
Y(96,80)=124, Y(64,128)=64, Y(80,128)=80. Most internal edges are bs=2
(intra blocks), with zeros at x=120 and x=72 for y=80/88. SAO was not
triggered at that CTB but was not directly confirmed to be disabled.

No Wedeo-side intermediate values were captured: the isolated build was
blocked by 13 concurrent cargo processes / rav1d nasm scripts and exceeded the
time budget. Deblocking, SAO and reconstruction remain unexonerated. Next step:
pair the Wedeo pre-deblock samples at the coordinates above with the FFmpeg
values. Evidence: `/tmp/ai-agent/HEVC_C_GT.md` (ephemeral).

## IPRED_B_Nokia_3.bit first divergence (2026-11-27, no patch)

Vector: `hevc-conformance/IPRED_B_Nokia_3.bit` (29168 bytes, 1 frame,
1920x1080 yuv420p). FFmpeg framecrc `0x11ef5ddd`, Wedeo `0xe0e6497a`.
First luma difference (320,1): Wedeo 30, FFmpeg 29. First chroma differences
(160,0): Cb 152 vs 153, Cr 124 vs 123. All differences are +/-1 at CTB
boundaries (multiples of 64), pointing at deblock or SAO edge handling.

FFmpeg gdb measurements for CTB(5,0): all three components use SAO_EDGE with
EO_HORIZ, offsets Y `{0,1,0,0,-1}`, Cb `{0,1,1,0,0}`, Cr `{0,0,1,0,-1}`.
Vertical deblock: luma bs=2, qp=22, tc=1; Cb bs=2, c_tc=1. Y(320,1) goes
28 -> 29 from deblock with SAO unchanged; Cb(160,0) goes 151 -> 152 from
deblock then +1 to 153 from SAO. FFmpeg pads the row above the first line
with zeros, not a copy of row 0.

No Wedeo-side staged values were captured: the isolated trace build was
blocked by cargo registry locks and concurrent rav1d/rav1e NASM scripts,
exceeding the 20-minute budget. Probes were reverted; tree clean at d86b9d2.
Next step: add Wedeo deblock/SAO traces at the CTB(5,0) boundaries and pair
with the FFmpeg values above. Evidence: `/tmp/ai-agent/HEVC_D_GT.md`.

## RESOLVED: CIP neighbour availability per PU (commit 39c6b34, 2026-11-27)

On `CIP_A_Panasonic_3.bit` frame 1, both decoders reconstructed 128 and
vertical deblock produced 123, but Wedeo's horizontal deblock at (80,64) made
it 124 because the q-side samples differed. Tracing back to TU(64,64) 32x32
intra planar: FFmpeg predicted 122/121/120/119 while Wedeo predicted 128
because it treated all reference samples as unavailable.

gdb values: FFmpeg `cip=1` with cand_up/left/up_right/bottom_left all 1;
Wedeo's per-pixel AND check made all four false. FFmpeg
`pred_template.c:167-207` samples every 2 pixels per PU and ORs the intra
flags, including an `on_pu_edge` condition. Wedeo now follows that rule.

Verified: target vector luma diffs 2353 -> 1867, total differing bytes
3287 -> 2809, first diff moved from (80,61) to (72,62). Full 227-vector sweep
unchanged at 9/227 with no new errors.

Remaining: FFmpeg's CIP sample replacement macros (`EXTEND_LEFT_CIP`,
`EXTEND_DOWN_CIP`, `EXTEND_UP_CIP`, `EXTEND_RIGHT_CIP`) are not implemented;
reference samples still differ slightly (t15 114 vs 112, t31 128 vs 130).
