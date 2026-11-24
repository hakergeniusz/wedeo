#!/bin/bash
# Download FFmpeg FATE test samples.
# Usage: ./scripts/fetch-fate-suite.sh [--dest DIR] [--force] [destination]
# Environment: WEDEO_REF_DIR stages into "$WEDEO_REF_DIR/fate-suite".
set -euo pipefail

DEST=""
FORCE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            DEST="${2:?--dest requires a directory}"
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [--dest DIR] [--force] [destination]"
            echo "  --dest DIR  stage into DIR (default: \$WEDEO_REF_DIR/fate-suite or fate-suite/)"
            echo "  --force     re-sync even if a completed marker is present"
            exit 0
            ;;
        *)
            if [ -n "$DEST" ]; then
                echo "unexpected argument: $1" >&2
                exit 1
            fi
            DEST="$1"
            shift
            ;;
    esac
done

if [ -z "$DEST" ]; then
    if [ -n "${WEDEO_REF_DIR:-}" ]; then
        DEST="$WEDEO_REF_DIR/fate-suite"
    else
        DEST="fate-suite"
    fi
fi

MARKER="$DEST/.wedeo-fate-suite-complete"
if [ -f "$MARKER" ] && [ "$FORCE" -eq 0 ]; then
    echo "FATE suite already staged at $DEST/ (marker present); skipping. Use --force to re-sync."
    exit 0
fi

mkdir -p "$DEST"
echo "Syncing FATE suite to $DEST/ ..."
rsync -avP rsync://fate-suite.ffmpeg.org/fate-suite/ "$DEST/"
touch "$MARKER"
echo "Done. Set FATE_SUITE=$DEST to run FATE tests."
