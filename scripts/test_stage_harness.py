"""Offline artifact-staging tests: python -m unittest discover -s scripts -p test_stage_harness.py."""
import hashlib
import io
import json
import os
from pathlib import Path
import tarfile
import tempfile
import types
import unittest
import shutil
import subprocess
from unittest.mock import patch


def archive(name, content=b"executable", kind=tarfile.REGTYPE):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as tar:
        entry = tarfile.TarInfo(name)
        entry.type = kind
        entry.size = len(content) if kind == tarfile.REGTYPE else 0
        entry.linkname = "/outside"
        tar.addfile(entry, io.BytesIO(content) if entry.size else None)
    return buffer.getvalue()


class StageTests(unittest.TestCase):
    def setUp(self):
        source = Path(__file__).with_name("stage-harness.sh")
        self.assertTrue(source.exists(), "staging implementation must exist")
        self.stage = types.ModuleType("stage_harness")
        body = source.read_text(encoding="utf-8").split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
        exec(compile(body, str(source), "exec"), self.stage.__dict__)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.destination = Path(self.temp.name) / "candidate"
        self.destination.mkdir()
        self.responses = {}
        self.calls = []

    def fetch(self, url, destination=None):
        self.calls.append(url)
        data = self.responses[url]
        if destination is not None:
            Path(destination).write_bytes(data)
        return data

    def release(self, harness, entries):
        repo = {"codex": "openai/codex", "opencode": "anomalyco/opencode"}[harness]
        tag = "rust-v1.2.3" if harness == "codex" else "v1.2.3"
        assets = []
        for name, data in entries.items():
            url = f"https://github.com/{repo}/releases/download/{tag}/{name}"
            assets.append({"name": name, "browser_download_url": url,
                           "digest": "sha256:" + hashlib.sha256(data).hexdigest()})
            self.responses[url] = data
        release = {"tag_name": tag, "draft": False, "prerelease": False, "assets": assets}
        url = f"https://api.github.com/repos/{repo}/releases/latest"
        self.responses[url] = json.dumps(release).encode()
        return release, url

    def test_codex_stages_matching_pair_without_activating(self):
        suffix = "-x86_64-unknown-linux-musl"
        self.release("codex", {name + suffix + ".tar.gz": archive(name + suffix)
                               for name in ("codex", "codex-code-mode-host")})
        result = self.stage.stage("codex", self.destination, "x86_64", self.fetch)
        self.assertEqual(result["version"], "1.2.3")
        self.assertEqual(Path(result["helper"]).read_bytes(), b"executable")
        self.assertEqual(Path(result["binary"]).parent, self.destination)
        self.assertFalse((self.destination.parent / "current.json").exists())

    def test_opencode_exact_asset_and_digest(self):
        self.release("opencode", {"opencode-linux-arm64.tar.gz": archive("opencode")})
        result = self.stage.stage("opencode", self.destination, "aarch64", self.fetch)
        self.assertEqual(Path(result["binary"]).read_bytes(), b"executable")

    def test_prerelease_or_draft_refused_before_download(self):
        release, url = self.release("opencode", {})
        for field in ("draft", "prerelease"):
            with self.subTest(field=field):
                value = dict(release, **{field: True})
                self.responses[url] = json.dumps(value).encode()
                with self.assertRaisesRegex(ValueError, "stable"):
                    self.stage.stage("opencode", self.destination, "x86_64", self.fetch)
        self.assertEqual(set(self.calls), {url})

    def test_missing_digest_and_foreign_asset_url_refused(self):
        release, url = self.release("opencode", {"opencode-linux-x64.tar.gz": archive("opencode")})
        for change in ({"digest": None}, {"browser_download_url": "https://example.com/payload"}):
            with self.subTest(change=change):
                value = dict(release, assets=[dict(release["assets"][0], **change)])
                self.responses[url] = json.dumps(value).encode()
                with self.assertRaises(ValueError):
                    self.stage.stage("opencode", self.destination, "x86_64", self.fetch)
        self.assertEqual(set(self.calls), {url})

    def test_corrupt_archive_never_produces_executable(self):
        release, url = self.release("opencode", {"opencode-linux-x64.tar.gz": archive("opencode")})
        self.responses[release["assets"][0]["browser_download_url"]] = b"corrupt"
        with self.assertRaisesRegex(ValueError, "checksum"):
            self.stage.stage("opencode", self.destination, "x86_64", self.fetch)
        self.assertFalse((self.destination / "opencode").exists())

    def test_archive_links_and_traversal_not_extracted(self):
        for name, kind in (("../opencode", tarfile.REGTYPE), ("opencode", tarfile.SYMTYPE)):
            with self.subTest(name=name, kind=kind):
                self.release("opencode", {"opencode-linux-x64.tar.gz": archive(name, kind=kind)})
                with self.assertRaises(ValueError):
                    self.stage.stage("opencode", self.destination, "x86_64", self.fetch)
                self.assertFalse((self.destination / "opencode").exists())
                self.assertFalse((self.destination.parent / "opencode").exists())

    def test_claude_manifest_checksum_and_stable_version(self):
        base = "https://downloads.claude.ai/claude-code-releases"
        self.responses[base + "/latest"] = b"2.1.280\n"
        self.responses[base + "/2.1.280/manifest.json"] = json.dumps({"version": "2.1.280", "platforms": {
            "linux-x64": {"checksum": hashlib.sha256(b"claude").hexdigest()}}}).encode()
        self.responses[base + "/2.1.280/linux-x64/claude"] = b"claude"
        self.responses[base + "/2.1.280/manifest.json.sig"] = b"signature"
        self.responses[self.stage.CLAUDE_KEY] = b"key"
        with patch.object(self.stage, "verify_claude_manifest") as verify:
            result = self.stage.stage("claude", self.destination, "x86_64", self.fetch)
            verify.assert_called_once()
        self.assertEqual(result["version"], "2.1.280")
        self.assertEqual(Path(result["binary"]).read_bytes(), b"claude")
        Path(result["binary"]).unlink()
        self.responses[base + "/latest"] = b"2.1.281-beta.1"
        with self.assertRaisesRegex(ValueError, "stable"):
            self.stage.stage("claude", self.destination, "x86_64", self.fetch)

    def test_current_or_newer_installed_version_skips_artifacts(self):
        for harness in ("claude", "codex", "opencode"):
            with self.subTest(harness=harness):
                self.calls.clear()
                if harness == "claude":
                    self.responses[self.stage.CLAUDE_REPO + "/latest"] = b"1.2.3"
                else:
                    self.release(harness, {})
                result = self.stage.stage(harness, self.destination, "x86_64", self.fetch, current="1.2.4")
                self.assertTrue(result["unchanged"])
                self.assertEqual(len(self.calls), 1)
                self.assertEqual(list(self.destination.iterdir()), [])

    def test_oversized_ignored_member_is_rejected(self):
        self.release("opencode", {"opencode-linux-x64.tar.gz": archive("ignored", b"12345")})
        with patch.object(self.stage, "MAX_BINARY", 4):
            with self.assertRaisesRegex(ValueError, "decompressed size"):
                self.stage.stage("opencode", self.destination, "x86_64", self.fetch)

    @unittest.skipUnless(shutil.which("gpg") and shutil.which("gpgv"), "GnuPG fixture tools unavailable")
    def test_real_signature_accepts_pinned_key_and_rejects_tampering(self):
        if os.name == "nt" and "/git/usr/bin/" in shutil.which("gpg").replace("\\", "/").lower():
            # Git's MSYS GnuPG expects POSIX paths even when called by Python.
            original_run = subprocess.run
            def msys_run(args, **kwargs):
                args = ["/" + arg[0].lower() + "/" + arg[3:].replace("\\", "/")
                        if isinstance(arg, str) and len(arg) > 2 and arg[1:3] in (":\\", ":/") else arg
                        for arg in args]
                return original_run(args, **kwargs)
            self.addCleanup(patch.stopall)
            patch.object(subprocess, "run", side_effect=msys_run).start()
        key_home = self.destination / "keys"
        key_home.mkdir(mode=0o700)
        command = ["gpg", "--batch", "--no-options", "--homedir", str(key_home),
                   "--pinentry-mode", "loopback", "--passphrase", ""]
        def run(*args):
            result = subprocess.run(command + list(args), check=False, capture_output=True, timeout=20)
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
            return result.stdout
        try:
            run("--quick-generate-key", "Updater fixture <fixture@example.invalid>", "ed25519", "sign", "0")
            fingerprint = next(line.split(":")[9] for line in run("--with-colons", "--list-keys").decode().splitlines()
                               if line.startswith("fpr:"))
            key = run("--armor", "--export", fingerprint)
            manifest = key_home / "manifest.json"
            manifest.write_bytes(b'{"version":"1.2.3"}')
            run("--detach-sign", str(manifest))
            signature = manifest.with_suffix(".json.sig").read_bytes()
            with patch.object(self.stage, "CLAUDE_SIGNER", fingerprint):
                self.stage.verify_claude_manifest(manifest.read_bytes(), signature, key)
                with self.assertRaises(subprocess.CalledProcessError):
                    self.stage.verify_claude_manifest(b"tampered", signature, key)
            with self.assertRaisesRegex(ValueError, "signer"):
                self.stage.verify_claude_manifest(manifest.read_bytes(), signature, key)
        finally:
            if shutil.which("gpgconf"):
                subprocess.run(["gpgconf", "--homedir", str(key_home), "--kill", "all"],
                               check=False, capture_output=True, timeout=10)

    def test_signature_must_match_pinned_anthropic_fingerprint(self):
        key = b"-----BEGIN PGP PUBLIC KEY BLOCK-----\n\na2V5\n-----END PGP PUBLIC KEY BLOCK-----\n"
        result = types.SimpleNamespace(stdout="[GNUPG:] VALIDSIG " + "A" * 40 + " 0 0 0\n")
        with patch.object(self.stage.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(ValueError, "signer"):
                self.stage.verify_claude_manifest(b"manifest", b"signature", key)
        result.stdout = "[GNUPG:] VALIDSIG " + self.stage.CLAUDE_SIGNER + " 0 0 0\n"
        with patch.object(self.stage.subprocess, "run", return_value=result):
            self.stage.verify_claude_manifest(b"manifest", b"signature", key)

    def test_nonempty_or_relative_destination_refused(self):
        for destination in (Path("relative"), self.destination):
            (self.destination / "keep").write_text("old")
            with self.assertRaises(ValueError):
                self.stage.stage("claude", destination, "x86_64", self.fetch)
        self.assertEqual(self.calls, [])
        self.assertEqual((self.destination / "keep").read_text(), "old")


if __name__ == "__main__":
    unittest.main()
