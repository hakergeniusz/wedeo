#!/usr/bin/env python3
# /// script
# dependencies = []
# ///
"""Download JVT H.264 conformance vectors from ITU zip archives.

Reads manifest JSON files from test_suites/h264/ and downloads + extracts
the input bitstreams into jvt-vectors/<suite>/.

Usage:
    python3 scripts/fetch_jvt.py                        # both suites
    python3 scripts/fetch_jvt.py --suite JVT-AVC_V1     # one suite
    python3 scripts/fetch_jvt.py --dry-run               # show what would download
    python3 scripts/fetch_jvt.py --jobs 8                # parallel downloads
    python3 scripts/fetch_jvt.py --dest /shared/jvt-vectors  # shared staging path
Environment:
    WEDEO_REF_DIR stages into "$WEDEO_REF_DIR/jvt-vectors" unless --dest is given.
"""

import argparse
import hashlib
import json
import os
import shutil
import sys
import tempfile
import urllib.request
import zipfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MANIFEST_DIR = ROOT / "test_suites" / "h264"
VECTORS_DIR = ROOT / "jvt-vectors"


def load_manifest(path: Path) -> dict:
    with open(path) as f:
        return json.load(f)


def discover_suites() -> dict[str, Path]:
    """Return {suite_name: manifest_path} for all JSON manifests."""
    suites = {}
    if MANIFEST_DIR.is_dir():
        for p in sorted(MANIFEST_DIR.glob("*.json")):
            data = load_manifest(p)
            suites[data["name"]] = p
    return suites


def resolve_vectors_dir(dest_arg: str | None) -> Path:
    """Destination: --dest > WEDEO_REF_DIR/jvt-vectors > jvt-vectors/."""
    if dest_arg:
        return Path(dest_arg).expanduser().resolve()
    ref_dir = os.environ.get("WEDEO_REF_DIR")
    if ref_dir:
        return Path(ref_dir).expanduser().resolve() / "jvt-vectors"
    return VECTORS_DIR


def display_path(path: Path) -> str:
    """Show a path relative to the checkout when possible."""
    try:
        return str(path.relative_to(ROOT))
    except ValueError:
        return str(path)


def target_path(suite_name: str, input_file: str, dest_dir: Path) -> Path:
    """Compute the local path for a vector's input file."""
    # input_file may be nested (e.g. "subdir/file.264") — flatten to just filename
    return dest_dir / suite_name / Path(input_file).name


def fetch_one(suite_name: str, vector: dict, dry_run: bool, dest_dir: Path,
              force: bool = False) -> str:
    """Download and extract one vector. Returns a status string."""
    name = vector["name"]
    input_file = vector["input_file"]
    dest = target_path(suite_name, input_file, dest_dir)

    if dest.exists() and not force:
        return f"  SKIP  {name} (exists)"

    if dry_run:
        return f"  FETCH {name} -> {display_path(dest)}"

    source_url = vector["source"]
    expected_md5 = vector.get("source_checksum", "")

    # Create temp file outside try so tmp_path is always bound for finally
    with tempfile.NamedTemporaryFile(suffix=".zip", delete=False) as tmp:
        tmp_path = Path(tmp.name)

    try:
        urllib.request.urlretrieve(source_url, tmp_path)

        # Verify MD5 if provided
        if expected_md5:
            actual_md5 = hashlib.md5(tmp_path.read_bytes()).hexdigest()
            if actual_md5 != expected_md5:
                return f"  FAIL  {name} (md5 mismatch: expected {expected_md5}, got {actual_md5})"

        # Extract the target file from the zip
        dest.parent.mkdir(parents=True, exist_ok=True)
        target_name = Path(input_file).name

        with zipfile.ZipFile(tmp_path) as zf:
            # Find the matching file in the archive (may be nested)
            # Prefer exact path match, then fall back to basename match
            match = None
            for info in zf.infolist():
                if info.is_dir():
                    continue
                if info.filename == input_file:
                    match = info
                    break
                if match is None and Path(info.filename).name == target_name:
                    match = info

            if match is None:
                members = [i.filename for i in zf.infolist() if not i.is_dir()]
                return f"  FAIL  {name} ('{target_name}' not in zip: {members[:5]})"

            # Extract to temp location then move
            with zf.open(match) as src, open(dest, "wb") as dst:
                shutil.copyfileobj(src, dst)

        return f"  OK    {name}"
    except Exception as e:
        return f"  ERROR {name}: {e}"
    finally:
        tmp_path.unlink(missing_ok=True)


def fetch_suite(suite_name: str, manifest_path: Path, dry_run: bool, jobs: int,
                dest_dir: Path, force: bool = False) -> tuple[int, int, int]:
    """Fetch all vectors for a suite. Returns (ok, skip, fail) counts."""
    data = load_manifest(manifest_path)
    vectors = data["test_vectors"]

    print(f"\nSuite: {suite_name} ({len(vectors)} vectors) -> {dest_dir}")
    if dry_run:
        print("  (dry run — no downloads)")

    ok = skip = fail = 0

    if jobs > 1 and not dry_run:
        with ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = {
                pool.submit(fetch_one, suite_name, v, dry_run, dest_dir, force): v
                for v in vectors
            }
            for future in as_completed(futures):
                msg = future.result()
                print(msg)
                if msg.startswith("  OK"):
                    ok += 1
                elif msg.startswith("  SKIP"):
                    skip += 1
                else:
                    fail += 1
    else:
        for v in vectors:
            msg = fetch_one(suite_name, v, dry_run, dest_dir, force)
            print(msg)
            if msg.startswith("  OK") or msg.startswith("  FETCH"):
                ok += 1
            elif msg.startswith("  SKIP"):
                skip += 1
            else:
                fail += 1

    return ok, skip, fail


def main():
    parser = argparse.ArgumentParser(description="Download JVT conformance vectors")
    parser.add_argument("--suite", help="Suite name (e.g. JVT-AVC_V1). Default: all suites.")
    parser.add_argument("--dry-run", action="store_true", help="Show what would be downloaded")
    parser.add_argument("--jobs", type=int, default=4, help="Parallel downloads (default: 4)")
    parser.add_argument("--dest", help="Destination directory (default: $WEDEO_REF_DIR/jvt-vectors or jvt-vectors/)")
    parser.add_argument("--force", action="store_true", help="Re-download vectors that already exist")
    args = parser.parse_args()

    dest_dir = resolve_vectors_dir(args.dest)

    suites = discover_suites()
    if not suites:
        print(f"No manifest files found in {MANIFEST_DIR}", file=sys.stderr)
        sys.exit(1)

    if args.suite:
        if args.suite not in suites:
            print(f"Unknown suite '{args.suite}'. Available: {', '.join(suites)}", file=sys.stderr)
            sys.exit(1)
        selected = {args.suite: suites[args.suite]}
    else:
        selected = suites

    total_ok = total_skip = total_fail = 0
    for name, path in selected.items():
        ok, skip, fail = fetch_suite(
            name, path, args.dry_run, args.jobs, dest_dir, args.force
        )
        total_ok += ok
        total_skip += skip
        total_fail += fail

    print(f"\nTotal: {total_ok} downloaded, {total_skip} skipped, {total_fail} failed")
    sys.exit(1 if total_fail > 0 else 0)


if __name__ == "__main__":
    main()
