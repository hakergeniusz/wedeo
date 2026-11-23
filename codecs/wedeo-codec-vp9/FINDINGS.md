# VP9 conformance root-cause findings (agent/w55)

Ground-truth extraction notes for the remaining FATE VP9 failures. All numbers
were measured against the 226-vector `vp9-test-vectors` corpus with
`wedeo-framecrc` vs `ffmpeg -bitexact` (CRC field only, since wedeo emits a
1/1000 time base and FFmpeg 1001/30000).

Baseline (unchanged by this investigation): **165/226 files bitexact,
1408/1871 frames matching.**

## Verified root causes

### 1. Demuxer mis-routes `vp90-2-09-lf_deltas.webm` (out of ownership)

`wedeo_format::context` opens this VP9 WebM with `format="h264"`, so the H.264
decoder is selected and the vector yields 0 frames. Verified with
`RUST_LOG=wedeo_format::context=debug`:

```
... InputContext opened path=".../vp90-2-09-lf_deltas.webm" format="h264" streams=1
```

Other vectors (e.g. `vp90-2-00-quantizer-00.webm`) open as `format="webm"`.
This is a WebM demuxer format-detection bug, not a VP9 decoder bug. Fix belongs
in `wedeo-format-webm` / `wedeo-format::context` (not owned by this agent).

### 2. Invisible inter frames with segmentation: wrong tile offset

Vectors `trac4359`, `segkey_adpq`, `tiling-pedestrian` decode frame 0 then
fail with `InvalidData`. Tracing (temporary, reverted) showed:

- frame 1 header parses as `InterFrame, show_frame=false, size_from_ref=0`,
  `refresh_ref_mask=4`, `ref_idx=[0,1,2]`, `frame_ctx_id=1`,
  `filter_level=11`, `base_q_idx=52`, `seg_enabled=true`;
- `tile_cols_log2/tile_rows_log2` decode as 1 col x 4 rows and the computed
  `tile_data_offset=181`;
- at offset 181 the first "tile size" is `0xafc61f5d` = 2.9 GB, i.e. the
  offset lands in compressed-header data, so the tile size OOB guard fires.

The uncompressed header parser was re-verified field-by-field against
`libavcodec/vp9.c:534-911` (profile bits, `intra_only` gating at vp9.c:587,
`resetctx`, refresh mask, ref idx/sign-bias interleaving, size-from-ref flags,
loopfilter 6/3-bit fields, `get_sbits_inv` = n+1 bits, and all segmentation
features) and matches. The divergence therefore originates in the **compressed
header arithmetic decode** (probability context slot 1, populated from frame 0)
or in superframe splitting for these altref-style streams; resolving it needs
per-symbol ground truth (lldb on a VP9-enabled debug FFmpeg build).

### 3. Sub-pixel / transform rounding deltas (size-03 vectors)

`vp90-2-03-size-*` vectors show only +/-1 luma pixel diffs (6-44 pixels per
frame) that accumulate from inter frame 2 onward; chroma is always exact.
Example `vp90-2-03-size-198x198.webm` frame 2: 6 Y pixels +/-1 at
(x=97..100, y=188 and 195). Frame 0 (keyframe) of `196x198` has 14 Y pixels
+/-1 concentrated in 8x8 **skip** (prediction-only) blocks near the superblock
corner at (128,192). Verified faithful ports (no bug found): `itxfm_add_nxn`
rounding vs `vp9dsp_template.c:1155-1192`, `idct4/8/16/32`, dequant truncation
to int16 vs `vp9block.c:905-917`, DC/AC qlookup tables (all 256 entries match
FFmpeg), `dc_pred`/`dc_left`/`dc_top`/`hor_pred`/`tm_pred`, `mask_edges`
(vp9block.c:1142-1262), `filter4/8/16`, `build_filter_luts`, and the 8-tap
filter coefficients/rounding in `vp9dsp_template.c:2026-2113`.

### 4. `vp90-2-16-intra-only.webm` (3/7 frames)

Its first packets are INTRA_ONLY / invisible altref frames (verified from raw
bitstream: packet 0 byte0=0x84, show_frame=0, intra_only bit=1). The shown
frames that reference them differ completely (mean abs diff ~32/plane), i.e. a
wrong reference image rather than a rounding issue. Requires reference-slot
ground truth to resolve.

## Experiments that did NOT help (reverted)

- Disabling the loop filter: only 12/226 files bitexact (LF is required).
- Removing the loop-filter edge bounds check: frame total dropped 1871 -> 1851
  (OOB accesses cause decode errors); reverted.
- MC edge clamping with current- vs reference-frame dimensions: FFmpeg uses
  reference-frame dims (`vp9_mc_template.c:41`), so wedeo is already correct.

No source changes remain; the worktree is clean. No conformance regression.
