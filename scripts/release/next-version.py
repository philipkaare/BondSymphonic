#!/usr/bin/env python3
"""Prints the version the next release gets.

<major>.<minor> comes from [workspace.package] in Cargo.toml; the patch is one
past the highest existing v<major>.<minor>.<patch> tag, or 1 when there is
none. Bumping the minor (or major) in Cargo.toml therefore starts a new series
at .1, and every other release just counts up.

    scripts/release/next-version.py            # e.g. 0.1.14
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def workspace_version() -> str:
    in_package = False
    for line in (ROOT / "Cargo.toml").read_text().splitlines():
        if re.match(r"^\s*\[workspace\.package\]\s*$", line):
            in_package = True
            continue
        if in_package and re.match(r"^\s*\[", line):
            break
        m = re.match(r'^\s*version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', line) if in_package else None
        if m:
            return f"{m.group(1)}.{m.group(2)}"
    sys.exit("next-version: no version under [workspace.package] in Cargo.toml")


def main() -> None:
    series = workspace_version()
    tags = subprocess.run(
        ["git", "tag", "--list", f"v{series}.*"], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.split()
    patches = [int(t.rsplit(".", 1)[1]) for t in tags if re.fullmatch(rf"v{re.escape(series)}\.\d+", t)]
    print(f"{series}.{max(patches, default=0) + 1}")


if __name__ == "__main__":
    main()
