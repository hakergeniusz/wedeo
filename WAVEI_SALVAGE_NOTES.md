# Wave I salvage / recovery notes (2026-11-24)

## State (historical snapshot, 2026-11-24)

These numbers describe that snapshot only. Current measured parity is in
`STATUS.md` (H.264 four-suite 199/283, VP9 215/226, HEVC 10/227).

- `main` = `ddbbd7d` + H.264 encoder work (not yet pushed). VP8 bit-exact (3364/3364), w72 VP9 SIMD, w78 CLI multi-input merged, Theora duration fix, Ogg cover-art/w79, H264 MBAFF termination alignment, w85 structured framecrc comparison, VP9 sub-8x8 MV/context alignment (`13ccb4f`).
- H.264 encoder: new `adapters/wedeo-h264-rusty` wraps pure-Rust `rusty_h264-encoder` 0.17.4 (BSD-2-Clause dependency, adapter glue LGPL). Adds `Encoder::extradata()` so the CLI can hand avcC to the MP4 muxer. `wedeo-cli -i av1.mp4 -c:v h264 out.mp4` verified: 30-frame output decodes cleanly under ffmpeg.
- Gate at that snapshot: `cargo fmt --all -- --check` clean, `cargo clippy --workspace --all-targets -- -D warnings` clean (only pre-existing manifest warnings), full `wedeo-fate` suite green. The then-current JVT-AVC_V1 78/78 MATCH, JVT-FR-EXT 24/24 MATCH, H264 CAVLC FATE 52/52 BITEXACT and VP9 corpus 166/225 files bitexact (1435/2027 frames) after `13ccb4f` sub-8x8 MV/context extent alignment are superseded by the counts in `STATUS.md`. Remaining VP9 gaps: 10/12-bit and 4:2:2/4:4:4 profiles, plus ±1/±2 luma rounding in `03-size` vectors.
- Disk: ~33 GB free on `/`. Guard in `scripts/agent_env.sh` aborts below 3 GB.
- Subagent provider: **free usage limit reached** - no new subagents until user fixes provider. Main agent still works.

## Upstream / dependabot (2026-11-24)
- `origin/main` has no new commits (main is 171 ahead). `origin/v2` gained one commit `d36f966` (dav1d FFI isolation in `ffi_guard.rs` + v2-only e2e harness). That commit depends on v2's core API (`ExternalCpuBuffer`, `BufferPool`, `VideoFrame`, 8/10/12-bit CICP), so it is NOT mergeable into main without the v2 architecture. Decision: not merged; the rav1d adapter on main is unchanged and the v2 work stays on `origin/v2`.
- Dependabot branches deleted from the fork (`dependabot/cargo/rand-0.9.4`, `dependabot/cargo/rust-deps-cb49e41e27`, `dependabot/github_actions/actions/cache-5`, `dependabot/github_actions/actions/checkout-7`, `dependabot/github_actions/actions/upload-artifact-7`). The same branches still exist on upstream `origin` (cannot delete upstream); they show in `git branch -r` as `origin/dependabot/...` and are harmless. Local remote-tracking refs pruned for the fork. Dependabot config remains in `.github/dependabot.yml`; if the noise is unwanted, disable the app in repo settings rather than via git.
- Note: dependabot PRs (#1, #3, #7, #12, #13 on upstream) are open against upstream; none are applied to main.

## Salvaged work (Wave I, agents 69-86)
- All agents based on local `main`, isolated worktrees `/var/tmp/wedeo-agent-<id>`, branches `agent/w<id>`.
- Completed + merged: **w70** (muxer options/metadata hooks, `f5d39b1`), **w71** (encoder `thread_count`, `34a8a67`), **w73** (VP8 bit-exact inter prediction, `39cfdb3`).
- w72 (VP9 SIMD) and w78 (CLI multi-input) are NOT yet in main (rebase + verify before merging). All other agents errored/cancelled; their uncommitted changes were committed on their `agent/w<id>` branches (`wip: salvage agent w<id> ...`) and copied to `/var/tmp/wedeo-salvage-<id>` as extra backup.
- Cover-art gap **resolved** (2026-11-25): `METADATA_BLOCK_PICTURE` base64 decoding, FLAC picture block parsing, attached-picture `Stream` creation and packet emission in the Ogg demuxer. `cover_art/ogg_vorbiscomment_cover.opus` now emits 102 packets bitexact and w79's stricter Ogg test passes. **w79 can now merge** (re-apply only Ogg/test files, not the branch wholesale).
- Per-agent status is in `SALVAGE_STATUS.md`.

## Per-branch inventory (review before merging)
| branch | diff vs main | notes |
|---|---|---|
| agent/w72 | merged | VP9 SIMD (dsp/mc/simd). |
| agent/w73 | merged | VP8 bit-exact inter prediction. |
| agent/w77 | skipped | Seek scaffolding only: adds generation/serial fields without keybinding, `Command::Seek`, flush or `ctx.seek()` call sites; does not compile as-is. Reimplement fresh if seek is wanted. |
| agent/w78 | merged | CLI multi-input + filter_complex. |
| agent/w79 | merged | Ogg tests + cover-art support (102 packets bitexact), re-applied as `83549e8`/`133fbd9`. |
| agent/w80 | 5 files, +148/-395 | FLV demux tests. Old base; keep only FLV/test additions. |
| agent/w81 | 5 files, +176/-395 | AVI demux tests. Old base; keep only AVI/test additions. |
| agent/w85 | merged | Structured field-level framecrc comparison (`tests/fate/src/lib.rs`, `75fa04c`), adapted with lifetime + fixture + clippy fixes. |

**Do not merge any branch wholesale.** For pre-w70/w71 branches, re-apply only the files under the agent's ownership; the framework "deletions" are already in `main`.

## How to resume
1. For each `agent/w<id>` with commits ahead of `main`, review the diff: `git log --oneline main..agent/w<id>`, `git diff main...agent/w<id>`.
2. Keep changes that are complete and verified; re-run `scripts/agent_env.sh <id>` + fmt/clippy/test for the touched crate.
3. Drop or finish partial work; never merge unverified code into `main`.
4. Once merged, delete the worktree and branch, then push `main` to `fork`.

## Ownership map (disjoint)
- w69 MP4 demuxer ctts/PTS; w72 VP9 dsp/mc/simd; w73 VP8; w74 H264 cavlc/dequant/hires; w75 H264 mc/idct/intra/deblock; w76 all HEVC; w77 wedeo-play; w78 wedeo-cli; w79 Ogg; w80 FLV; w81 AVI; w82 MPEG-TS; w83 status docs; w84 VP9 decoder unsafe audit; w85 FATE harness; w86 MP4 muxer faststart.
- w82 was cancelled while a diagnostic shell about `h264_intra_first-small.ts` was running; its diff may be empty.
