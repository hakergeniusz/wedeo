#!/usr/bin/env python3
"""Pre-commit conformance guard for H.264 changes.

Runs baseline/CABAC/FRext regression checks against known-passing snapshots.
Exits non-zero if any known-passing file regresses.

Usage:
    python3 scripts/h264_precommit.py          # all three suites (~7s)
    python3 scripts/h264_precommit.py --quick   # baseline CAVLC only (~1s)
    python3 scripts/h264_precommit.py --frext   # FRext only (~1s)

Exit codes:
    0 = all known-passing files still pass
    1 = regression detected
    2 = missing snapshot or setup error
"""
# /// script
# requires-python = ">=3.10"
# ///

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from wedeo_utils import find_wedeo_binary
from framecrc_compare import compare_one

SCRIPTS_DIR = Path(__file__).resolve().parent
ROOT = SCRIPTS_DIR.parent
SNAPSHOT_DIR = Path(os.environ.get("WEDEO_SNAPSHOT_DIR", SCRIPTS_DIR))
BASELINE_SNAPSHOT = SNAPSHOT_DIR / ".conformance_snapshot.json"
CABAC_SNAPSHOT = SNAPSHOT_DIR / ".conformance_cabac_snapshot.json"
FREXT_SNAPSHOT_DIR = ROOT / "test_suites" / "baselines"
FREXT_SNAPSHOT = FREXT_SNAPSHOT_DIR / ".suite_jvt-fr-ext_snapshot.json"


def warn_missing_snapshot(label: str, path: Path, command: str) -> None:
    """Warn loudly instead of silently passing when a baseline is missing."""
    print(f"WARNING: {label} snapshot missing at {path}; {label} check skipped.",
          file=sys.stderr)
    print(f"  Create it with: {command}", file=sys.stderr)


def run_frext_check() -> tuple[int, int]:
    """Check FRext via suite_runner. Returns (regressed, setup_error)."""
    save_cmd = (
        "python3 scripts/suite_runner.py --suite jvt-fr-ext --save-snapshot "
        f"--snapshot-dir {FREXT_SNAPSHOT_DIR.relative_to(ROOT)}"
    )
    if not FREXT_SNAPSHOT.exists():
        warn_missing_snapshot("FRext", FREXT_SNAPSHOT, save_cmd)
        return 0, 1
    try:
        data = json.loads(FREXT_SNAPSHOT.read_text())
        if not data.get("passing"):
            print(f"WARNING: FRext snapshot {FREXT_SNAPSHOT} has no passing vectors; "
                  "check skipped.", file=sys.stderr)
            return 0, 1
    except (OSError, ValueError) as e:
        print(f"WARNING: cannot read FRext snapshot {FREXT_SNAPSHOT}: {e}; "
              "check skipped.", file=sys.stderr)
        return 0, 1

    cmd = [
        sys.executable, str(SCRIPTS_DIR / "suite_runner.py"),
        "--suite", "jvt-fr-ext",
        "--check-snapshot",
        "--snapshot-dir", str(FREXT_SNAPSHOT_DIR),
    ]
    result = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    if result.stdout:
        print(result.stdout, end="")
    if result.stderr:
        print(result.stderr, end="", file=sys.stderr)
    if result.returncode > 1:
        print("WARNING: FRext suite_runner could not run (see output above); "
              "check skipped.", file=sys.stderr)
        return 0, 1
    if "no vectors tested" in result.stdout:
        print("WARNING: FRext suite_runner tested no vectors (missing JVT vectors?); "
              "check skipped.", file=sys.stderr)
        return 0, 1
    return (1 if result.returncode == 1 else 0), 0


def load_snapshot(path: Path) -> list[str]:
    """Load list of known-passing files from a snapshot."""
    if not path.exists():
        return []
    with open(path) as f:
        data = json.load(f)
    return sorted(data.get("passing", []))


def check_files(
    files: list[str], fate_dir: str, wedeo_bin: str, label: str
) -> tuple[int, int]:
    """Check a list of files. Returns (passed, regressed)."""
    passed = 0
    regressed = 0
    for name in files:
        file_path = Path(fate_dir) / name
        if not file_path.exists():
            print(f"  SKIP  {name} (file not found)")
            continue
        try:
            # compare_one returns (total, matching, diff_indices, plane_info)
            total, matching, _diffs, _planes = compare_one(str(file_path), wedeo_bin)
        except Exception as e:
            print(f"  FAIL  {name} ({e})")
            regressed += 1
            continue
        if matching == total:
            passed += 1
        else:
            print(f"  REGRESSED  {name} ({matching}/{total})")
            regressed += 1
    return passed, regressed


def main():
    parser = argparse.ArgumentParser(description="H.264 pre-commit conformance guard")
    parser.add_argument(
        "--quick", action="store_true", help="Baseline regression only (~1s)"
    )
    parser.add_argument(
        "--frext", action="store_true", help="FRext regression only (~4s)"
    )
    parser.add_argument(
        "--fate-dir",
        default="fate-suite/h264-conformance",
        help="Path to conformance files",
    )
    args = parser.parse_args()

    run_baseline = not args.frext
    run_cabac = not args.quick and not args.frext
    run_frext = not args.quick

    wedeo_bin = find_wedeo_binary()
    if not wedeo_bin:
        print("ERROR: wedeo-framecrc binary not found. Run: cargo build --release -p wedeo-fate", file=sys.stderr)
        sys.exit(2)

    start = time.monotonic()
    total_passed = 0
    total_regressed = 0
    setup_errors = 0

    if run_baseline:
        baseline_files = load_snapshot(BASELINE_SNAPSHOT)
        if not baseline_files:
            warn_missing_snapshot(
                "baseline", BASELINE_SNAPSHOT,
                "python3 scripts/conformance_full.py --save-snapshot",
            )
            setup_errors += 1
        else:
            print(f"Baseline: checking {len(baseline_files)} known-passing files...")
            p, r = check_files(baseline_files, args.fate_dir, wedeo_bin, "baseline")
            total_passed += p
            total_regressed += r
            if r == 0:
                print(f"  OK ({p} files)")

    if run_cabac:
        cabac_files = load_snapshot(CABAC_SNAPSHOT)
        if not cabac_files:
            warn_missing_snapshot(
                "CABAC", CABAC_SNAPSHOT,
                "python3 scripts/conformance_full.py --cabac --save-snapshot",
            )
            setup_errors += 1
        else:
            print(f"CABAC: checking {len(cabac_files)} known-passing files...")
            p, r = check_files(cabac_files, args.fate_dir, wedeo_bin, "CABAC")
            total_passed += p
            total_regressed += r
            if r == 0:
                print(f"  OK ({p} files)")

    if run_frext:
        print("FRext: checking known-passing JVT vectors via suite_runner.py...")
        fr, fe = run_frext_check()
        total_regressed += fr
        setup_errors += fe
        if fr == 0 and fe == 0:
            print("  OK (FRext snapshot)")

    elapsed = time.monotonic() - start
    status = "PASS" if not total_regressed and not setup_errors else (
        "FAIL" if total_regressed else "SETUP ERROR"
    )
    print(f"\n{status}: {total_passed} passed, {total_regressed} regressed ({elapsed:.1f}s)")
    sys.exit(1 if total_regressed > 0 else (2 if setup_errors else 0))


if __name__ == "__main__":
    main()
