#!/usr/bin/env bash
# Bundled into the daemon. Download candidates only; the Rust updater validates
# protocol compatibility and atomically selects the complete release bundle.
set -euo pipefail
exec python3 - "$@" <<'PY'
import argparse
import base64
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

CLAUDE_REPO = "https://downloads.claude.ai/claude-code-releases"
CLAUDE_KEY = "https://downloads.claude.ai/keys/claude-code.asc"
CLAUDE_SIGNER = "31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE"
MAX_BINARY = 768 * 1024 * 1024
MAX_METADATA = 8 * 1024 * 1024


def fetch(url, destination=None):
    """HTTPS only, bounded redirects, size, and wall time; never execute downloads."""
    if not url.startswith("https://"):
        raise ValueError("HTTPS is required")
    command = ["curl", "--fail", "--silent", "--show-error", "--location",
               "--proto", "=https", "--proto-redir", "=https", "--tlsv1.2",
               "--max-redirs", "3", "--connect-timeout", "10", "--max-time", "90",
               "--max-filesize", str(MAX_BINARY if destination else MAX_METADATA),
               "--user-agent", "BondSymphonic-harness-updater"]
    # Always stream to disk; malicious or broken servers cannot grow captured stdout.
    with tempfile.TemporaryDirectory(prefix="bs-fetch-") as scratch:
        target = Path(destination) if destination else Path(scratch) / "metadata"
        result = subprocess.run(command + ["--output", str(target), url],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=95, check=False)
        if result.returncode:
            raise ValueError("Release download failed; existing installation retained")
        if target.stat().st_size > (MAX_BINARY if destination else MAX_METADATA):
            raise ValueError("Release download exceeds size limit")
        return None if destination else target.read_bytes()


def stable_version(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", value):
        raise ValueError("Release must have a stable numeric version")
    return value


def no_update(version, current):
    return current is not None and tuple(map(int, version.split("."))) <= tuple(
        map(int, stable_version(current).split(".")))


def checksum(path, expected):
    if not isinstance(expected, str) or not re.fullmatch(r"[a-fA-F0-9]{64}", expected):
        raise ValueError("Release SHA256 checksum missing or malformed")
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != expected.lower():
        raise ValueError("Release checksum mismatch")


def verify_claude_manifest(manifest, signature, key):
    """Verify in an isolated keyring, pinned to Anthropic's published fingerprint."""
    with tempfile.TemporaryDirectory(prefix="bs-signature-") as scratch:
        root = Path(scratch)
        root.chmod(0o700)
        # gpgv ships with the distro's apt trust tooling. Decode ASCII armor
        # ourselves so verification does not require the full gnupg package.
        armor = key.decode("ascii").replace("\r\n", "\n")
        match = re.fullmatch(r"\s*-----BEGIN PGP PUBLIC KEY BLOCK-----\n(?:[^\n]+\n)*?\n([A-Za-z0-9+/=\n]+)-----END PGP PUBLIC KEY BLOCK-----\s*", armor)
        if not match:
            raise ValueError("Malformed Claude signing key")
        payload = "".join(line for line in match[1].splitlines() if not line.startswith("="))
        (root / "trusted.gpg").write_bytes(base64.b64decode(payload, validate=True))
        (root / "manifest.json").write_bytes(manifest)
        (root / "manifest.sig").write_bytes(signature)
        base = ["gpgv", "--homedir", str(root)]
        result = subprocess.run(base + ["--keyring", str(root / "trusted.gpg"), "--status-fd", "1",
                                str(root / "manifest.sig"), str(root / "manifest.json")],
                                check=True, timeout=10, capture_output=True, text=True)
        valid = [line.split() for line in result.stdout.splitlines()
                 if line.startswith("[GNUPG:] VALIDSIG ")]
        if not any(parts[2] == CLAUDE_SIGNER or parts[-1] == CLAUDE_SIGNER for parts in valid):
            raise ValueError("Claude release manifest signer did not match Anthropic")


def extract_binary(archive, expected, destination):
    found = False
    unpacked_size = 0
    with tarfile.open(archive, "r|gz") as bundle:
        for index, entry in enumerate(bundle):
            unpacked_size += entry.size
            if entry.size < 0 or unpacked_size > MAX_BINARY:
                raise ValueError("Release archive exceeds decompressed size limit")
            path = PurePosixPath(entry.name)
            if index > 10000 or path.is_absolute() or ".." in path.parts or "\\" in entry.name:
                raise ValueError("Unsafe release archive member")
            if not (entry.isfile() or entry.isdir()):
                raise ValueError("Links and special files are forbidden in release archives")
            if str(path) != expected:
                continue
            if found or not entry.isfile() or not 0 < entry.size <= MAX_BINARY:
                raise ValueError("Invalid or duplicate executable in release archive")
            with bundle.extractfile(entry) as source, destination.open("xb") as output:
                shutil.copyfileobj(source, output, 1024 * 1024)
            found = True
    if not found:
        raise ValueError("Expected executable missing from release archive")
    destination.chmod(0o755)


def github_release(harness, arch, destination, download, current):
    repo = {"codex": "openai/codex", "opencode": "anomalyco/opencode"}[harness]
    release = json.loads(download(f"https://api.github.com/repos/{repo}/releases/latest"))
    if release.get("draft") is not False or release.get("prerelease") is not False:
        raise ValueError("Only stable published releases can be installed")
    prefix = "rust-v" if harness == "codex" else "v"
    tag = release.get("tag_name", "")
    if not tag.startswith(prefix):
        raise ValueError("Unexpected release tag")
    version = stable_version(tag[len(prefix):])
    if no_update(version, current):
        return {"version": version, "unchanged": True}
    if harness == "codex":
        triple = {"x64": "x86_64", "arm64": "aarch64"}[arch] + "-unknown-linux-musl"
        names = [(name, name + "-" + triple) for name in ("codex", "codex-code-mode-host")]
    else:
        names = [("opencode", "opencode-linux-" + arch)]
    result = {"version": version, "binary": str(destination / harness)}
    for name, asset_base in names:
        asset_name = asset_base + ".tar.gz"
        matches = [asset for asset in release.get("assets", []) if asset.get("name") == asset_name]
        if len(matches) != 1:
            raise ValueError("Required release artifact missing or duplicated")
        asset = matches[0]
        expected_url = f"https://github.com/{repo}/releases/download/{tag}/{asset_name}"
        if asset.get("browser_download_url") != expected_url:
            raise ValueError("Unexpected release artifact URL")
        digest = asset.get("digest")
        if not isinstance(digest, str) or not re.fullmatch(r"sha256:[a-fA-F0-9]{64}", digest):
            raise ValueError("Release SHA256 checksum unavailable")
        archive = destination / (name + ".tar.gz")
        try:
            download(expected_url, archive)
            checksum(archive, digest[7:])
            extract_binary(archive, asset_base if harness == "codex" else "opencode", destination / name)
        finally:
            archive.unlink(missing_ok=True)
        if name == "codex-code-mode-host":
            result["helper"] = str(destination / name)
    return result


def stage(harness, destination, machine, download=fetch, current=None):
    destination = Path(destination)
    if not destination.is_absolute() or destination.is_symlink() or not destination.is_dir() or any(destination.iterdir()):
        raise ValueError("Stage directory must be an existing empty absolute directory")
    arch = {"x86_64": "x64", "aarch64": "arm64", "arm64": "arm64"}.get(machine)
    if arch is None:
        raise ValueError("Unsupported Linux architecture")
    if harness in ("codex", "opencode"):
        return github_release(harness, arch, destination, download, current)
    if harness != "claude":
        raise ValueError("Unknown harness")
    version = stable_version(download(CLAUDE_REPO + "/latest").decode().strip())
    if no_update(version, current):
        return {"version": version, "unchanged": True}
    base = CLAUDE_REPO + "/" + version
    manifest_bytes = download(base + "/manifest.json")
    verify_claude_manifest(manifest_bytes, download(base + "/manifest.json.sig"), download(CLAUDE_KEY))
    manifest = json.loads(manifest_bytes)
    if manifest.get("version") != version:
        raise ValueError("Claude manifest version mismatch")
    target = "linux-" + arch
    expected = manifest["platforms"][target]["checksum"]
    binary = destination / "claude"
    download(base + "/" + target + "/claude", binary)
    checksum(binary, expected)
    binary.chmod(0o755)
    return {"version": version, "binary": str(binary)}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("harness", choices=("claude", "codex", "opencode"))
    parser.add_argument("--stage-dir", required=True)
    parser.add_argument("--current-version")
    args = parser.parse_args()
    try:
        if platform.system() != "Linux":
            raise ValueError("Harness staging is supported inside Linux/WSL only")
        result = stage(args.harness, args.stage_dir, platform.machine(), current=args.current_version)
        print(json.dumps(result), flush=True)
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError, subprocess.SubprocessError) as error:
        # Avoid emitting downloaded data or inherited proxy/auth configuration.
        if type(error) is ValueError:
            reason = str(error)
        elif isinstance(error, FileNotFoundError):
            reason = "Required download or signature-verification tool is unavailable"
        elif isinstance(error, subprocess.TimeoutExpired):
            reason = "Download or signature verification timed out"
        elif isinstance(error, subprocess.CalledProcessError):
            reason = "Release signature verification failed"
        else:
            reason = "Release data or local staging operation was invalid"
        print("Harness staging failed: " + reason + "; existing installation retained", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
PY
