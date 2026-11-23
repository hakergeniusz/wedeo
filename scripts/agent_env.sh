#!/usr/bin/env bash
# Isolated environment for one parallel subagent.
#
# Usage:  source scripts/agent_env.sh <agent-id>
#         source scripts/agent_env.sh 7   # WEDEO_AGENT_ID=7, branch agent/7
#
# All agents share one Cargo target dir (disk is limited: /home is ~96% full).
# TMPDIR and snapshot dirs are per-agent so fixed filenames in FATE tests and
# non-atomic snapshot writers cannot collide across processes.

if [ -z "${1:-}" ] && [ -z "${WEDEO_AGENT_ID:-}" ]; then
    echo "usage: source scripts/agent_env.sh <agent-id>" >&2
    return 1 2>/dev/null || exit 1
fi

export WEDEO_AGENT_ID="${1:-$WEDEO_AGENT_ID}"
export WEDEO_AGENT_BRANCH="agent/$WEDEO_AGENT_ID"

# Shared build cache. Cargo serializes concurrent builds via its file lock;
# this is intentional, per-agent target dirs would exhaust the disk.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/var/cache/zstd5/wedeo-target}"
mkdir -p "$CARGO_TARGET_DIR"

# Per-agent scratch. std::env::temp_dir() honours TMPDIR, so Rust FATE tests
# with fixed filenames (asynth-44100-2.wav etc.) stay isolated.
export TMPDIR="/tmp/ai-agent/wedeo-agent-$WEDEO_AGENT_ID-tmp"
export WEDEO_SNAPSHOT_DIR="/tmp/ai-agent/wedeo-agent-$WEDEO_AGENT_ID-snapshots"
mkdir -p "$TMPDIR" "$WEDEO_SNAPSHOT_DIR"

# Shared, read-only conformance assets. Fetched once by the assets agent.
export FATE_SUITE="${FATE_SUITE:-/var/cache/zstd5/wedeo-ref/fate-suite}"
export JVT_VECTORS="${JVT_VECTORS:-/var/cache/zstd5/wedeo-ref/jvt-vectors}"

if [ "${WEDEO_AGENT_QUIET:-0}" != "1" ]; then
    echo "agent=$WEDEO_AGENT_ID branch=$WEDEO_AGENT_BRANCH"
    echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
    echo "TMPDIR=$TMPDIR"
    echo "snapshots=$WEDEO_SNAPSHOT_DIR"
    echo "FATE_SUITE=$FATE_SUITE"
fi
