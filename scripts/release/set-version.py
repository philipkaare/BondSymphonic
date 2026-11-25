#!/usr/bin/env python3
"""Stamps a release version into [workspace.package] in Cargo.toml.

    scripts/release/set-version.py 0.1.14

Every crate inherits that version, so `env!("CARGO_PKG_VERSION")` -- what the
IDE reports, compares updates against and shows in Settings -- is the release
number. Only the release workflow runs this, on its own checkout; the change
is never committed, and the git tag is the record of which commit became
which version.
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def main() -> None:
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
        sys.exit("usage: set-version.py <major.minor.patch>")
    version = sys.argv[1]
    path = ROOT / "Cargo.toml"
    lines = path.read_text().splitlines(keepends=True)
    in_package = False
    for i, line in enumerate(lines):
        if re.match(r"^\s*\[workspace\.package\]\s*$", line):
            in_package = True
            continue
        if in_package and re.match(r"^\s*\[", line):
            break
        if in_package and re.match(r'^\s*version\s*=\s*"', line):
            lines[i] = re.sub(r'"[^"]*"', f'"{version}"', line, count=1)
            path.write_text("".join(lines))
            print(f"set-version: {version}")
            return
    sys.exit("set-version: no version under [workspace.package] in Cargo.toml")


if __name__ == "__main__":
    main()
