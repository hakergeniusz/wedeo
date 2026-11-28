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

Current at HEAD `58d2c0c`: 32/227 bit-exact vectors. The progression below is
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
32/227 with no regressions (`DSLICE_B_HHI_5.bit` only load-timeout, already
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
unchanged at 32/227 with no new errors.

Resolved later by `e7a65a6`: FFmpeg's CIP sample replacement macros
(`EXTEND_LEFT_CIP`, `EXTEND_DOWN_CIP`, `EXTEND_UP_CIP`, `EXTEND_RIGHT_CIP`) are
now applied after neighbour collection. `CIP_A_Panasonic_3.bit` became
byte-exact (2809 -> 0 differing bytes) and HEVC parity rose to 32/227.

## Intra prediction exonerated on IPRED/CIP vectors (2026-11-27)

`IPRED_C_Mitsubishi_3.bit`: first frame diff Y(384,31) Wedeo 139 vs FFmpeg 140,
Cb(176,0) 133 vs 136. gdb at `pred_template.c:383` captured four luma TUs
covering the point; FFmpeg predictions 154, 83, 158, 60 for the respective
(x0,y0,log2,mode) tuples. Wedeo tracing produced the identical four values
with matching neighbor-availability flags on the same TUs and modes.

`cip_B_NEC_2.bit`: frame 0 exact, frame 1 first diff Y(360,32) 97 vs 95.
gdb on an 8x8 planar TU (x0=360,y0=32,mode=0) gave FFmpeg prediction samples
82 77 75 77 81 84 83 81 ...; all 64 Wedeo samples matched byte-for-byte.

`IPRED_C_Mitsubishi_2.bit` and `IPRED_A_docomo_2.bit` show only small +/-1 to
+/-3 edge differences, consistent with deblock/SAO rather than prediction.

Conclusion: intra prediction is not the cause for these vectors. Do not repeat
this investigation; focus on post-prediction deblock/SAO stages. No patch.

## RESOLVED: SAO edge padding at CTB boundaries (commit d8531ba, 2026-11-27)

`filter.rs` used `x0 + xx.saturating_sub(1)` for SAO edge fill; with `usize`
and `xx=0` that picked the current column instead of `x-1`. Image-edge
padding also clamped to a copy of the first row/column, while FFmpeg's
`sao_filter_CTB()` leaves zeros in `edge_emu_buffer`.

Paired trace on IPRED_B_Nokia_3 CTB(5,0): deblock produced the same values as
FFmpeg (Y(320,1) 28->29, Cb(160,0) 151->152). Wedeo's SAO left neighbour was
152 (duplicated first column) while FFmpeg's was 156. With Cb offsets
`{0,1,1,0,0}`, `cmp(152,156)=-1` omitted the `+1`, giving 152 instead of 153.

After the fix: Cb(160,0)=153, Cr(160,0)=123, both first chroma diffs gone.
IPRED differing bytes 5901 -> 5577 (Y 170->54). Full 227-vector sweep stays
32/227 with no new errors; the DSLICE_B_HHI_5 timeout disappeared (errors
3 -> 2). CIP_A_Panasonic_3 was unchanged (2809 diffs) at that point; the later
CIP sample-replacement fix `e7a65a6` made it byte-exact (0 diffs).

## MERGE_E_TI_3 current first divergence (2026-11-27, no patch)

At current HEAD, `Y(384,6)` is no longer a divergence: both decoders output 141
after deblock and SAO. The new first difference is frame 0 `Y(192,63)`:
Wedo 95 vs FFmpeg 97; Cb(336,0) 109 vs 108, Cr(240,15) 138 vs 140; 757
differing bytes in frame 0.

Paired values at CTB(3,0): FFmpeg gdb shows pre-deblock Y(192,63)=97,
post-deblock=97, SAO type 2/2/2, luma EO class 3, offsets `0,2,0,-1,-2`,
`lfase=1`, `restore=0`, post-SAO=97. Wedeo tracing shows pre-filter 94,
post-deblock 97 (matching FFmpeg), then SAO changes it to 95. The bottom-left
padding sample read 0 from the left CTB's `sao_v` while the picture/diagonal
sample was 116.

An experimental SAO corner-padding reorder with a diagonal-CTB applied check
changed the padding value to 174 but left all 757 differences unchanged, so
that hypothesis is incomplete and was reverted. Next step: reproduce FFmpeg's
one-CTB-delayed interleaved deblock/SAO ordering, then pair corner padding
again. Evidence: `/tmp/ai-agent/merge_next_*` (ephemeral).

---

## SAO edge fixes combined result (2026-11-27)

Two complementary SAO fixes were merged at HEAD: `b07857b` selects the
diagonal neighbour CTB for padding corners with correct `height+1`/`width+1`
bounds, and `1c1693d` allocates separate edge buffers per component (FFmpeg
`hevcdec.c:150-166`). Their conflict in `filter.rs` was resolved by keeping
both the diagonal-CTB selection and the per-component indexing.

Full 227-vector sweep: 10/227 -> **32/227 MATCH** with zero regressions. Newly
matching vectors: `EXT_A_ericsson_4.bit`, `POC_A_Bossen_3.bit`,
`IPRED_A_docomo_2.bit`, `IPRED_B_Nokia_3.bit`, `IPRED_C_Mitsubishi_2.bit`,
`IPRED_C_Mitsubishi_3.bit`, `MAXBINS_A_TI_4.bit`, `OPFLAG_B_Qualcomm_1.bit`.
Both ERROR entries remain vectors where neither decoder emits frames.

## Sony/RAP cluster triage (2026-11-27, no patch)

- `RAP_A_docomo_4.bit`: Wedeo emits 0 frames vs FFmpeg 86;
  `RAP_B_Bossen_1.bit`: 64 vs 80 frames. Both are frame-count issues, not
  pixel-level divergence targets.
- No file actually starts with `Sony_`; the Sony family uses `*_Sony_*` /
  `*_SONY_*` and all currently fail.
- `INITQP_A_Sony_1.bit`: the apparent frame 0 difference came from an older
  binary; with a current HEAD build frame 0 is exact. gdb and Wedeo tracing
  at CTB(5,1) matched through deblock and SAO, so that location is cleared.
- `SLIST_A_Sony_4.bit`: frame 0 exact; frame 1 differs massively
  (Y 381005/399360 bytes). Paired trace shows the divergence already at
  `filter_picture()` entry for decode-order frame 4: Wedeo Y(1,0)=156,
  Y(80,0)=152 vs FFmpeg 151/135. The cause is reconstruction/reference-frame
  selection or slice reordering, not deblock/SAO.

No measured single cause in budget; probes reverted, no patch. Evidence:
`/tmp/ai-agent/HEVC_CLUSTER_B_EVIDENCE.md` (ephemeral).

## RESOLVED: CU part_mode timing (commit 8073af8, 2026-11-28)

Wedeo assigned `self.part_mode = part_mode` after decoding PUs, so merge
candidate derivation used the previous CU's partition. FFmpeg stores
`lc->cu.part_mode` before PU decode. On `VPSID_A_VIDYO_1.bit` frame 1 PU(104,0),
the stale `Part2Nx2N` made `is_available_a1` accept a left candidate that should
have been rejected, yielding MV(10,1) instead of FFmpeg's MV(0,0). This was the
shared `f1 Y(102,0)` signature seen across seven differently named vectors.

Fix: set `self.part_mode` immediately after computing `intra_split`, before PU
decode. Verified `VPSID_A_VIDYO_1` frames 0-2 byte-exact, first diff moved from
frame 1 to frame 3, divergent bytes 233619 -> 45422. Full sweep 18/227 ->
32/227 with `cip_B_NEC_2.bit` and `cip_B_NEC_3.bit` newly matching, all prior
matches preserved, no new errors.

## Largest remaining inter signature: 11-vector temporal merge (2026-11-28)

After commit 8073af8, the largest exact shared signature among the 77 inter
vectors is 11 vectors with `f1 Y(0,0) Wedeo 139 / FFmpeg 138`:
`MERGE_A/B/C/D_TI_3`, `PMERGE_A/B/C/D/E_TI_3`, `RPS_A_docomo_4`,
`RPS_F_docomo_1`. Next largest groups have only 5 and 3 members.

Paired measurements at POC1 PU(0,0), 32x32:
- FFmpeg skip merge candidate `BI`, ref=(0,0), mv=(-3,0)/(3,0), with all
  `lc->na` neighbour flags zero, so the candidate must be temporal
  (collocated). FFmpeg collocated: `colPic=2`, temporal `pf=L0`,
  `colmv=(-6,-1)`, POC-scaled to `(-3,0)`.
- Wedeo's merge call (gdb hit 257, x0=0,y0=0,32x32) returned `pf=BI`,
  mv=(0,0)/(0,0), i.e. a zero temporal candidate. `temporal_luma_motion_vector`
  was called twice (L0 and L1) in that call.

The shared root cause is the temporal merge candidate for the first PU of the
second decoded picture. The exact failing sub-step (collocated picture choice,
tab_mvf sample, long-term/POC check, or scaling) was NOT pinned: the instrumented
build needed a temporary `tracing` dependency in `wedeo-codec-hevc-refs` and did
not finish behind the shared build lock within budget. No patch, gates not run.
Evidence: `/tmp/ai-agent/partnext_evidence.md`, `/tmp/ai-agent/partnext_fastsig.log`.
Next: pair inputs of `make_temporal` / `derive_temporal_colocated_mvs` at that
call using source-level tracing.

## 12-bit 4:4:4 output format (commit 7fb422c, 2026-11-28)

Six vectors failed in `receive_frame` with `Not yet implemented` because
`map_pixel_format()` lacked `(12, 3) => Yuv444p12le` and `decoder.rs` returned
`PatchwelcomeNotImplemented`. FFmpeg decodes all six to `yuv444p12le` with
1/1/1/2/2/8 frames. Added the format mapping in `sps.rs`, the decoder fallback,
and explicit framecrc plane geometry. Three vectors are now byte-exact
(`12bit-444-qp74/75`, `IPCM_B_RExt_NEC`) with CRCs `0xe49d741e`, `0xd93c96fd`,
`0x1f808406`. Three more have correct frame count/format but still differ in
PRA/SAO RExt stages. Full sweep 20/227 -> 32/227 with all prior matches kept.

Also: 11 vectors with `Not yet implemented` in `decode_packet` share a
multi-layer/multi-view VPS extension parse limitation (no single simple gate);
18 `Invalid data found` vectors are a mix of WPP/RPS/PCM/422-10bit causes.

## RESOLVED: decoded-picture MV table for temporal candidates (commit ab1fa7e, 2026-11-28)

FFmpeg keeps one `tab_mvf` per frame at `min_pu` granularity and writes MVs into
it while decoding. Wedeo wrote MVs into `Picture::tab_mvf` but temporal merge/
AMVP candidates read a different table from the collocated DPB frame, allocated
at `min_cb` granularity and never populated, so temporal candidates were always
zero. Representative `VPSID_A_VIDYO_1.bit` first diverged at f3 Y(64,0) 143/144
with 37655 differing bytes; gdb showed FFmpeg reading collocated MV (19,2) and
scaling to (9,1) while Wedeo read the initial INTRA value.

Fix: use the decoded picture's MV table for temporal candidates and add the
missing `slice_temporal_mvp_enabled_flag` check for AMVP. Verified twice with a
freshly built binary: full sweep 23/227 -> **32/227** with nine new matches and
no regressions. Both ERROR entries remain vectors where neither decoder emits
frames.

## Temporal candidates aligned after commit ab1fa7e (2026-11-28, no patch)

At HEAD b632d28, paired ffmpeg_g gdb and Wedeo source-level traces on frame 1
(POC 1):

- MERGE_A_TI_3.bit first PU(0,0): both decoders have temp_col L0/ref0
  (-6,-1), colpoc=2, collist=L1, current RPL L0=[0,2,4]/L1=[2,4,0],
  collocated RPL L0=[0,4]/L1=[4,0], scaled BI(-3,0)/(3,0). The old
  f1 Y(0,0) 139/138 signature is gone. Current first diff is Y(204,14),
  PU(196,0), with both decoders using L0 ref1 (18,6) from collocated (36,12).
- PMERGE_A_TI_3.bit first diffs near Y(271,0): PUs (256,0), (256,4), (240,0)
  have identical temporal candidates and final MVs.
- MERGE_B_TI_3.bit and RPS_A_docomo_4.bit first-diff PUs also show no missing,
  zero or POC-scaling-divergent temporal candidates.

Conclusion: collocated picture selection, tab_mvf sampling, long-term/POC
checks and scaling are all correct after the MV-table fix. Residual pixel
differences are downstream of MV derivation (MC/deblock/SAO). No patch, gates
not run, tree clean. Evidence: /tmp/ai-agent/w10hetmp_evidence.md (ephemeral).
