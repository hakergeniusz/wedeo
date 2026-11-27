# H.264 high-depth gaps

## FREH10: localized high-depth intra 4x4 reconstruction

Measured 2026-11-27 at HEAD `5e47eb4`. No source change was committed.

The FREH10 vectors named in `test_suites/h264/JVT-FR-EXT.json` are not in the
FATE `FRext/` directory. They live in
`/var/cache/zstd5/wedeo-ref/jvt-vectors/JVT-FR-EXT/`:

- FREH10-1: `WB_10bit_QP21_I-Only_1920x1088.264` (High 10, yuv420p10le,
  1920x1088, 360 frames, one slice per frame, `transform_8x8_mode=false`,
  `slice_qp=33`)
- FREH10-2: `WB_10bit_QP21_1920x1088.264` (High 10, 358 frames)

`freh10.264` in FATE is a different 8-bit High-profile vector, not the FREH10
high-depth case.

### Measured divergence

Frame 0 first differing sample: luma (x=607, y=964), FFmpeg 468, Wedeo 0.
2137 differing bytes, all luma, confined to one four-row band y=964..967,
x=607..939 (MB row 60, columns 37..58). U and V are byte-identical, and rows
outside the band are identical. FREH10-1 has 20 differing frames in the first
194 (0, 60, 75, 97, 98, ...), so this is a localized per-frame defect, not a
global dequant or output-scale error.

### Ruled out by measurement

- Deblocking: disabling it on both decoders leaves the exact same first byte,
  byte count and delta.
- Motion compensation: frame 0 is an IDR I-slice, all affected MBs are
  `intra4x4`; no I_PCM MBs occur in frame 0.
- Output packing/cropping: U/V match byte for byte and only one luma MB-row
  band differs.
- Bitstream parsing: Wedeo trace and FFmpeg gdb agree at MB(37,60) on QP 33,
  MB type I_4x4 and CBP 35; FFmpeg also reports NNZ 14 and intra prediction
  mode 4 for that block.

### What remains

The distinction between high-depth `intra_pred::predict_4x4` and
`dequant_4x4`/`idct4x4_add` on the `u16` plane is not yet measured. Wedeo's
`INTRA_FINAL_ROWS` trace prints the `u8` plane, which the high-depth path does
not write, so those all-zero values are not evidence.

Next measured steps: gdb FFmpeg frame 0 MB(37,60) block 7 with
`sl->poc == 0` and a u16 row stride of 1920 (`sl->linesize` is in pixels),
dumping the prediction before IDCT and the 16 dequantized coefficients before
`ff_h264_idct_add_10_c`; add a temporary `trace!()` in
`codecs/wedeo-codec-h264/src/hires.rs::decode_intra4x4_hires` for the same
block to print u16 prediction, coefficients and post-IDCT rows. Patch only the
kernel that actually diverges, then verify all 20 frames and FRext >= 42/69.
