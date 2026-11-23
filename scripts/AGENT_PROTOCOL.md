# Subagent protocol (crate factory)

Every parallel subagent works on **one crate**. The crate is already
pre-registered in the workspace, the facade, the CLI/player link lists, and the
FATE harness. You do not edit any of those files.

## Ownership

- **Yours, exclusively:** your crate directory (see `scripts/work_units.toml`) and
  `tests/fate/tests/fate_<your_crate>.rs`.
- **Never touch:** root `Cargo.toml`, `Cargo.lock`, `crates/wedeo*/**`,
  `bins/**`, `crates/wedeo-core/**`, `crates/wedeo-codec/**`,
  `crates/wedeo-format/**`, `.github/**`, `test_suites/baselines/**`,
  `TODO.md`, `PROGRESS.md`, `DIVERGENCES.md`, `README.md`, another agent's
  crate, another agent's FATE test.
- Need a shared interface change? Write it in your final report. The integrator
  handles it between waves.

## Setup

```bash
git worktree add -b agent/<id> /var/tmp/wedeo-agent-<id> main
cd /var/tmp/wedeo-agent-<id>
source scripts/agent_env.sh <id>
```

`agent_env.sh` sets a shared `CARGO_TARGET_DIR` (disk is limited), a private
`TMPDIR` (FATE tests use fixed temp filenames), and a private snapshot dir.
Conformance assets in `/var/cache/zstd5/wedeo-ref/` are read-only.

## Implementation contract

1. Read the FFmpeg C reference listed in `scripts/work_units.toml` from
   `~/Projects/FFmpeg/` **before writing Rust**. Copy FFmpeg comments verbatim.
2. Implement the whole work unit. No partial codecs: decoder/format plus
   conformance tests must land together, or nothing lands.
3. Register via `inventory::submit!` using the exact pattern from an existing
   crate (`codecs/wedeo-codec-pcm/src/lib.rs`, `formats/wedeo-format-ivf/src/lib.rs`).
4. Use `Result<T, E>` + `?`, `wrapping_*` for C overflow parity, safe Rust
   (`// SAFETY:` for the rare `unsafe`), `Vec<(String, String)>` for metadata.
5. No `#[allow(clippy::*)]` without an explanatory comment. No `eprintln!`
   in decoders; use `tracing` macros.

## Verification (all required)

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test -p wedeo-<your-crate> -p wedeo-fate
python3 scripts/framecrc_compare.py <conformance-vector> --pixel-detail
python3 scripts/suite_runner.py --suite <your-shard> --check-snapshot \
  --snapshot-dir test_suites/baselines
```

Never pass `--save-snapshot` with `test_suites/baselines` from an agent tree.
If you need to record new passes, write to `$WEDEO_SNAPSHOT_DIR`.

## Deliverable

Commit on `agent/<id>` (Conventional Commit + `Assisted-by:` trailer). Report:

- crate name and C reference
- conformance numbers (bitexact / total, or SNR)
- files changed (must be only your crate + your FATE test)
- any new divergence, reported but not written to docs

The integrator merges sequentially, runs the full gate, updates baselines via
the manual workflow, and removes the worktree.
