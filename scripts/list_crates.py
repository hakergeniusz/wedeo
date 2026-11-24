#!/usr/bin/env python3
"""List workspace work-unit crates for CI sharding and agent dispatch.

Usage:
    python3 scripts/list_crates.py                 # path per line
    python3 scripts/list_crates.py --format json   # JSON array of objects
    python3 scripts/list_crates.py --kind filter   # only one kind
    python3 scripts/list_crates.py --filter-wedeo  # exclude legacy framework crates
"""

import argparse
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KINDS = ("codec", "format", "filter", "bsf", "adapter", "crate", "bin")
DIR_KIND = {
    "codecs": "codec",
    "formats": "format",
    "filters": "filter",
    "adapters": "adapter",
    "crates": "crate",
    "bins": "bin",
}


def workspace_members() -> list[str]:
    text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r"members\s*=\s*\[(.*?)\n\]", text, re.S)
    if not match:
        raise RuntimeError("no [workspace] members array in root Cargo.toml")
    return re.findall(r'"([^"]+)"', match.group(1))


def crate_name(path: Path) -> str:
    match = re.search(r'^name\s*=\s*"([^"]+)"', (path / "Cargo.toml").read_text(), re.M)
    if not match:
        raise RuntimeError(f"no package name in {path}/Cargo.toml")
    return match.group(1)


def list_crates() -> list[dict]:
    crates = []
    for member in workspace_members():
        path = ROOT / member
        parts = member.split("/")
        kind = DIR_KIND.get(parts[0], parts[0] if parts[0] in KINDS else "other")
        crates.append({"name": crate_name(path), "path": member, "kind": kind})
    return crates


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--format", choices=["lines", "json"], default="lines")
    parser.add_argument("--kind", choices=KINDS + ("work-unit",))
    parser.add_argument("--filter-wedeo", action="store_true",
                        help="only work-unit crates (codec/format/filter/bsf)")
    args = parser.parse_args()

    crates = list_crates()
    if args.kind == "work-unit":
        crates = [c for c in crates if c["kind"] in ("codec", "format", "filter", "bsf")]
    elif args.kind:
        crates = [c for c in crates if c["kind"] == args.kind]
    if args.filter_wedeo:
        crates = [c for c in crates if c["kind"] in ("codec", "format", "filter", "bsf")]

    if args.format == "json":
        print(json.dumps(crates, indent=2))
    else:
        for c in crates:
            print(c["name"])


if __name__ == "__main__":
    main()
