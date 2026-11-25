#!/usr/bin/env python3
"""Writes latest.json, the release manifest the IDE's updater reads.

    make-manifest.py --version 0.1.14 --repo owner/name --commit <sha> \
        --asset windows-x86_64=dist/BondSymphonic-0.1.14-windows-x64-setup.exe \
        --asset macos-aarch64=dist/BondSymphonic-0.1.14-macos-arm64.zip \
        --out dist/latest.json

The format is crates/updater/src/manifest.rs's `Manifest`. Each asset's URL
is where `gh release create` will put the file, so the hash and size here are
of exactly the bytes users download. The workflow then signs this file with
minisign; the signature is what makes any of it trustworthy.
"""
import argparse
import hashlib
import json
from pathlib import Path


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--version", required=True)
    p.add_argument("--repo", required=True)
    p.add_argument("--commit", required=True)
    p.add_argument("--asset", action="append", default=[], metavar="PLATFORM=PATH")
    p.add_argument("--out", required=True)
    args = p.parse_args()

    tag = f"v{args.version}"
    assets = {}
    for spec in args.asset:
        platform, _, path = spec.partition("=")
        f = Path(path)
        digest = hashlib.sha256()
        with f.open("rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                digest.update(chunk)
        assets[platform] = {
            "url": f"https://github.com/{args.repo}/releases/download/{tag}/{f.name}",
            "sha256": digest.hexdigest(),
            "size": f.stat().st_size,
        }
    if not assets:
        raise SystemExit("make-manifest: no assets")
    manifest = {
        "version": args.version,
        "commit": args.commit,
        "notes_url": f"https://github.com/{args.repo}/releases/tag/{tag}",
        "assets": assets,
    }
    Path(args.out).write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
