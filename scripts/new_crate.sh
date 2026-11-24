#!/usr/bin/env bash
# Create a new work-unit crate. One crate = one subagent = no shared-file edits.
#
# Usage: scripts/new_crate.sh <codec|format|filter|bsf> <name> [ffmpeg-ref]
# Example: scripts/new_crate.sh codec mpeg4 libavcodec/mpeg4videodec.c
#
# This script only creates files inside the new crate directory and its FATE
# test stub. Root Cargo.toml, the facade, and the binary link lists are owned
# by the integrator; the script prints the exact lines to add.

set -euo pipefail

KIND="${1:-}"
NAME="${2:-}"
REF="${3:-TBD}"
case "$KIND" in
    codec|format|filter|bsf) ;;
    *) echo "usage: $0 <codec|format|filter|bsf> <name> [ffmpeg-ref]" >&2; exit 2 ;;
esac
[ -n "$NAME" ] || { echo "error: missing name" >&2; exit 2; }

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="wedeo-$KIND-$NAME"
case "$KIND" in
    codec|bsf) DIR="codecs/$CRATE" ;;
    format)    DIR="formats/$CRATE" ;;
    filter)    DIR="filters/$CRATE" ;;
esac

if [ -e "$ROOT/$DIR" ]; then
    echo "error: $DIR already exists" >&2
    exit 1
fi

FW="wedeo-codec"
[ "$KIND" = format ] && FW="wedeo-format"
[ "$KIND" = filter ] && FW="wedeo-filter"
TRAIT=codec
[ "$KIND" = format ] && TRAIT="demuxer/muxer"
[ "$KIND" = filter ] && TRAIT=filter
[ "$KIND" = bsf ] && TRAIT=bitstream_filter
TEST="fate_${CRATE#wedeo-}"
TEST="${TEST//-/_}"

mkdir -p "$ROOT/$DIR/src"
cat > "$ROOT/$DIR/Cargo.toml" <<EOF
[package]
name = "$CRATE"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Work unit for $REF (scaffolding, not yet implemented)"
repository.workspace = true
publish = false

[dependencies]
wedeo-core = { workspace = true }
$FW = { workspace = true }
inventory = { workspace = true }

[lints]
workspace = true
EOF
cat > "$ROOT/$DIR/src/lib.rs" <<EOF
//! Work unit: \`$CRATE\`.
//!
//! FFmpeg reference: \`$REF\`.
//!
//! Scaffolding created by scripts/new_crate.sh. A single subagent owns this
//! directory; replace this stub with the full $TRAIT implementation, register
//! it via \`inventory::submit!\`, and add conformance tests in
//! \`tests/fate/tests/$TEST.rs\`.
EOF
cat > "$ROOT/$DIR/README.md" <<EOF
# \`$CRATE\`

- Kind: $KIND
- FFmpeg reference: \`~/Projects/FFmpeg/$REF\`
- Owner: one subagent, this crate directory only
- Status: scaffolding (no implementation yet)

Do not put this work unit in another crate. Conformance tests belong in
\`tests/fate/tests/$TEST.rs\`.
EOF
cat > "$ROOT/tests/fate/tests/$TEST.rs" <<EOF
//! Conformance tests for \`$CRATE\`.
//!
//! Owned by the subagent implementing this work unit. Replace with real
//! framecrc / registration tests before the crate is merged.
EOF
mkdir -p "$ROOT/scripts/work_units"
cat > "$ROOT/scripts/work_units/$CRATE.md" <<EOF
# Task: $CRATE

- Kind: $KIND
- FFmpeg reference: \`~/Projects/FFmpeg/$REF\`
- Owned files: \`$DIR/**\`, \`tests/fate/tests/$TEST.rs\`
- Forbidden files: root Cargo.toml, Cargo.lock, core traits, baselines, status docs
- Done when: implementation + FATE tests pass, bitexact or documented SNR
EOF

echo "created $DIR"
echo "integrator must add to root Cargo.toml:"
echo "    members:        \"$DIR\","
echo "    dependencies:   $CRATE = { path = \"$DIR\", version = \"0.2.0\" }"
echo "    link line:      use ${CRATE//-/_} as _;"
