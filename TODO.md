# Wedeo TODO

## Audio gaps (short-term)

- [x] **Test ALAC decode** — tested via FFmpeg-generated M4A. Symphonia decodes to S32; verified against FFmpeg.
- [x] **Test ADPCM decode** — ADPCM IMA WAV and ADPCM MS tests added. Symphonia requires "frames per block" metadata; tests gracefully skip when unavailable.
- [ ] **Improve opus-decoder SILK quality** - the pure-Rust opus-decoder crate still differs from libopus in SILK/hybrid modes. Monitor upstream for improvements. CELT mode is fine (~48 dB). Wedeo applies the Opus ID-header pre-skip itself (Symphonia ignores Matroska CodecDelay and Wedeo's Ogg demuxer emits unconsumed SkipSamples side data); this raised measured SILK SNR on `opus/silk-lbrr-mono.mka` from -4.05 dB to 9.79 dB. The remaining gap is the upstream resampler phase and tail DiscardPadding, see `DIVERGENCES.md`.
- [x] **MP3 gapless trimming** — enabled symphonia's `enable_gapless` mode. Trim fields (`trim_start`/`trim_end`) added to Packet and applied in decoder wrapper. Sample count diff vs FFmpeg bounded by ~3 MP3 frames.
- [x] **AAC priming trim** — implemented iTunSMPB metadata parsing for M4A containers. Trim applied via packet `trim_start`/`trim_end` fields. Sample count diff bounded by ~5 AAC frames.
- [x] **Audio resampling** — integrated `rubato` 1.x behind `wedeo-resample`. Supports Fast/Normal/High quality modes. Deinterleave/reinterleave, chunked processing, flush with zero-padding.

## Video (medium-term, the big effort)

### Prerequisites
- [x] **Expand Frame for video** — added ColorPrimaries, ColorTransferCharacteristic, ChromaLocation enums (matching FFmpeg pixfmt.h). VideoFrameData has color_primaries, color_trc, chroma_location, crop_top/bottom/left/right. Frame has pkt_dts, best_effort_timestamp, repeat_pict.
- [x] **Bitstream reader** — exp-golomb parsing utilities (get_ue_golomb, get_se_golomb, get_te_golomb, get_te0_golomb) in wedeo-codec::bitstream, built on av-bitstream 0.2.1. Ports of FFmpeg's golomb.h functions.
- [x] **Pixel format conversion** — wedeo-scale now wraps dcv-color-primitives for I420/NV12↔RGB24/BGR24/RGBA/BGRA conversions. Converter struct with metadata preservation. 11 unit tests.

### Video codecs (native Rust, no existing crate covers these)
- [x] **H.264 decoder** - 77/77 decodable progressive BITEXACT (50 CAVLC + 27 CABAC; two FMO CAVLC vectors produce no frames in either decoder), 44/69 JVT FRext; four-suite total 199/283. See `H264.md` for detailed status.
  - [ ] FMO (Flexible Macroblock Ordering) - do not implement: FFmpeg w62 rejects it with Patchwelcome and emits zero frames, while Wedeo already rejects it with the same effect. Measured evidence in `H264_FMO_NOTES.md`. The FM1_BT_B failure-path hang was diagnosed and fixed at `d86b9d2`; it was not an unbounded `wait_for_row()` Condvar or panic-unsafe thread-pool join.
  - [ ] Interlaced (MBAFF/PAFF) - field-aware DPB metadata (`ref_mask`, `field_poc`) and field-aware ref-list construction are in place (parity-carrying `RefListEntry` / `RefPic`). PAFF top/bottom field pairing and field-view MC are now enabled as a bounded step (commit `e61b4af`); the top-field first-row double-deblock was fixed at `056ffec`, and high-depth bottom fields now run `post_decode_mb_hires` with field semantics (`9636f08`). `mark_complete()` remains a temporary guard that a per-field row-progress model should replace. Full interlaced parity remains open; MBAFF pixel reconstruction is partial. See `PAFF_RESEARCH.md` Appendices Q and S. The older CABAC-bin divergence in Appendix L was fixed at `e2ccefa` and is historical.
  - [ ] 10-bit / 4:2:2 - progressive 4:2:2 8/10-bit and Hi422FR1-4,6-9,10-15 are BITEXACT; 10-bit 4:2:0 FREH10-1 is 360/360 frames bit-exact after widening the u16 4x4 IDCT to i32 intermediates (commit 4a0c0ea). FREH10-2 is now 358/358 frames bit-exact after the i32 coefficient precision fix (commit b46b0fc); see `H264_HIGHDEPTH_GAPS.md`. Interlaced vectors still have field-handling diffs.
- [x] **AV1 decoder via rav1d** — `adapters/wedeo-rav1d/` wraps rav1d behind wedeo `Decoder` trait.
- [x] **Video player with audio** — `bins/wedeo-play/` has full A/V playback: wgpu+winit GPU rendering, cpal audio output, ffplay-style A/V sync (pts_drift audio clock), resampling via wedeo-resample, 5.1→stereo downmix, volume control, pause. Supports H.264/AV1 video + symphonia audio codecs.
  - [ ] Seek — removed (was buggy), needs proper reimplementation with ffplay serial mechanism
- [x] **VP9 decoder** — native Rust, IVF/WebM demuxer, bool decoder, intra/inter prediction, IDCT, loop filter. ~5K lines across 12 modules.
  - [x] VP9 inter prediction — MV parsing/prediction (spatial+temporal), 8-tap subpel MC, compound prediction, reference frame management. 21 bugs fixed 2026-04-03.
  - [x] VP9 keyframe pixel conformance — 64/64 standard-size keyframes match FFmpeg bit-for-bit
  - [x] VP9 inter-frame bool decoder parity — symbol counts verified identical to FFmpeg (927 blocks). 4 bugs fixed: mode ctx indexing (1-per-8×8 vs 2-per-8×8), end_x_y edge clamping, MV clamping bounds, MC emu_edge offset (2026-04-05).
  - [ ] VP9 inter frame pixel conformance — 222/226 vectors BITEXACT, 4 CRC differences (one resize, one 8-bit 4:2:2, two high-depth 4:2:2), 0 zero-frame vectors and 0 errors. Commit `d7ac5ba` fixed clipped NNZ edge-context merging, resolving `210x198`, `210x202` and `segkey_adpq`. No vectors panic today.
  - [x] VP9 odd-size OOB panics — the 74 historical `intra_pred.rs` panics no longer reproduce; odd-size vectors decode with correct geometry.
  - [ ] VP9 probability adaptation — `adapt_probs` not yet wired up after each frame decode
  - [ ] VP9 scaled prediction — silently skipped when ref/current dimensions differ
  - [ ] VP9 10/12-bit support - stage 1 complete at commit `61398bf`: FFmpeg-numbered pixel formats, output format selection and 4:2:2/4:4:4 chroma allocation. The 8-bit gate stays until the u16 sample path is bit-exact; staged design in `VP9_GAPS.md`.
  - [ ] VP9 superframe parsing
- [x] **HEVC decoder and encoder** - `adapters/wedeo-h265-oxideav` wraps pure-Rust `oxideav-h265` (MIT): full decode and I/P/B encode registered as `hevc_oxideav`. MP4 (hvcC + length-prefixed samples) and MPEG-TS (Annex B) output verified against FFmpeg. The native HEVC port remains partial.
  - [ ] VP9 8-bit odd-size - `vp90-2-03-size-210x202.webm`: frame 0 exact, frame 1 first inter divergence at x=157, y=203 (154 vs 155). Measured with gdb: the row filter inputs q0/q1 already differ by 1 before filtering, so the loop filter is faithful and the root cause is upstream. Measured further to the initial NNZ coefficient context of a 32x32 block; see `VP9_NNZ_GAPS.md`.
  - [ ] HEVC pixel parity - measured 2026-11-28: **32/227** vectors bit-exact against FFmpeg (including `ipcm_B_NEC_2.bit` and four other IPCM vectors; two are trivial 384-byte constant frame-packing vectors). `wedeo-framecrc` links only the native decoder (priority 100), not the adapter. A DPB slot-freeing fix (`fb4f2cd`) restored matching frame counts on 165/224 failing vectors but pixel hashes still differ. Commit `86c1ccc` made the luma of `ipcm_B_NEC_2.bit` byte-exact by reading I_PCM chroma and skipping PCM bytes in CABAC; commit `9f0d242` then made chroma byte-exact by fixing the TU width passed to neighbour availability, lifting parity to 8/227; the later inter-TU internal-edge deblock fix (`f32250c`) raised it to 18/227, and the CU part_mode timing fix (`8073af8`) raised it to 32/227. Details and next targets in `HEVC_GAPS.md`.
  - [ ] HEVC CABAC ground truth (`/tmp/ai-agent/HEVC_444_GROUND_TRUTH.md`, `HEVC_CABAC_GROUND_TRUTH.md`, `HEVC_BINTRACE_GROUND_TRUTH.md`): first divergence is a missing cross-component prediction decode. FFmpeg decodes `log2_res_scale_abs` (ctx 167) between luma and Cb residual; Wedeo had no cross-pred path. Adding it made Y and Cb of `cbf_cr_cb_TUDepth_4_circle.h265` bit-exact (Cr still off by 2) but regressed `QMATRIX_A_RExt_Sony_1.bit` (20 -> 5 frames), so it was reverted. FFmpeg decodes 32448 cross-pred calls for that vector vs ~10320 in Wedeo: the gate needs more measured refinement (per-PU chroma mode / cbf_luma provenance). An odd-pointer init-branch experiment also made parity worse and was reverted.

### Video muxers
- [x] **MP4/MOV muxer** — `formats/wedeo-format-mp4/src/muxer.rs` (H.264/HEVC/AAC, hvcC + length-prefixed samples, verified against FFmpeg).
- [x] **MKV/WebM muxer** — `formats/wedeo-format-matroska-mux/`; full MKV transcode works in wedeo alone.
- [x] **Ogg muxer** — `formats/wedeo-format-ogg/src/muxer.rs` (Vorbis/Opus, FFmpeg-style granules and page duration). Remaining: Theora/FLAC/Ogm mappings.

## Player performance

**Target: smooth 1080p24 playback with A/V sync ≤ ±40ms. ACHIEVED.**

Measured on player v0.1.1 (wgpu+winit, ffplay pts_drift clock; the workspace is now v0.2.0):
- 24.0fps steady, 0 frame drops on 1080p H.264 @ 23.976fps
- A/V sync drift: <40ms (within ffplay's correction threshold)
- GPU YUV→RGB via WGSL shader (BT.601/709, MPEG/JPEG range)
- 3-thread architecture matching ffplay (read, video decode, audio decode)

Remaining optimizations:
- [ ] SIMD for MC lowpass filters (NEON on Apple Silicon) - 3x slower at qpel(2,2) vs (0,0)
- [x] Per-MB chroma DC trace heap allocation removed (`189ec4a`). Measured median wall-clock change was 1.9-2.5%, within run-to-run noise, so no speedup is claimed.
- [ ] The MC path now uses preallocated `McScratch` and `bi_scratch_y/u/v`; remaining allocation cost needs a fresh profile before changing more code.

## Infrastructure (ongoing)

- [x] **Filter graph data flow** — `filters/wedeo-filter-graph` has pad/link validation, format negotiation (`query_formats` intersection) and per-link frame queues with a `run` path. Remaining: automatic insertion of conversion filters when the negotiation result requires a rescale (currently an explicit `format` filter is needed).
- [ ] **Interruptible I/O** — needed for network streams and cancellation.
- [x] **Buffer pool** — `BufferPool` for `PictureBuffer` reuse via `SharedPicture::Drop` reclaim.
- [ ] **Error context** — add "where/why" info to errors. Currently errors are flat enums with no call-site context.
- [ ] **Demuxer read_close** — the Demuxer trait has no cleanup method. Not critical (Rust's Drop handles most cases) but FFmpeg has it.
- [ ] **Chapters/programs** — the DemuxerHeader has no chapter or program info. Needed for Matroska/MP4 chapter support.
- [ ] **find_stream_info equivalent** — for formats that need to read some packets before stream params are fully known.

## Crates to evaluate later

| Crate | Version | What for | Notes |
|-------|---------|----------|-------|
| `rubato` | 1.0.1 | Audio resampling (wedeo-resample) | Pure Rust, SIMD, 1.0 stable. Not FATE-exact vs libswresample. |
| `dcv-color-primitives` | 1.0.0 | Pixel format conversion (wedeo-scale) | **Integrated.** Amazon. I420/NV12/RGB. No 10-bit, no BT.2020. |
| `rav1e` | 0.8.1 | AV1 encoding | Production-ready, 20M downloads. Encoding only, not decoding. |
| `matroska` | 0.30.0 | MKV demuxing (alternative to symphonia) | By tuffy, not rust-av. 143K downloads. Demux only. |
| `av-data` | 0.4.4 | Multimedia data types | 1.2M downloads. Overlaps with wedeo-core's Frame/Packet. Not adopting — would require core rewrite. |
| `av-codec` | 0.3.1 | Codec trait abstractions | 18K downloads. Similar to wedeo-codec's traits. Confirms our design but not worth adopting. |

## Testing gaps

- [x] **ALAC decode test** — tested via FFmpeg-generated M4A
- [x] **ADPCM decode test** — tests added (gracefully skip on symphonia metadata limitation)
- [ ] **Opus multistream (surround)** — opus-decoder has `OpusMultistreamDecoder` but we only support mono/stereo
- [ ] **Big-endian PCM through symphonia** — untested (we have no BE test files running through symphonia path)
- [ ] **MKV with multiple audio tracks** — test stream_index handling in symphonia demuxer
- [x] **Seek correctness** — seek tests added for WAV, FLAC, and MP3 via symphonia demuxer

## Performance experiments with no measured win

- SSE2/AVX2 H.264 MC lowpass (`h_lowpass_direct`, `v_lowpass_direct`,
  `hv_lowpass_direct`, `avg_pixels`) was implemented with runtime dispatch and
  verified bit-exact on 9 vectors (329 release tests). Alternating A/B on a
  720x480 inter-heavy vector gave a 1.006x median speedup with individual runs
  from 0.886x to 1.101x, i.e. within noise. The scalar loops are already
  auto-vectorized by LLVM. Do not retry this without a new profile showing a
  real bottleneck. Evidence: /tmp/ai-agent/ (ephemeral).
