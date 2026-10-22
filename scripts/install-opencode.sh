#!/usr/bin/env bash
set -euo pipefail
# Source-inspection candidate; no claim of live sandbox compatibility.
version=1.18.33
case "$(uname -m)" in x86_64) arch=x64;; aarch64|arm64) arch=arm64;; *) echo 'Unsupported OpenCode architecture'; exit 1;; esac
stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
curl -fsSL "https://api.github.com/repos/anomalyco/opencode/releases/tags/v$version" -o "$stage/release.json"
# Install only a release asset with GitHub's published SHA256 digest.
python3 - "$stage/release.json" "$arch" "$stage/asset" <<'PY'
import json,sys
release=json.load(open(sys.argv[1]))
name='opencode-linux-'+sys.argv[2]+'.tar.gz'
asset=next((a for a in release['assets'] if a['name']==name),None)
if not asset or not str(asset.get('digest','')).startswith('sha256:'):
    raise SystemExit('Release artifact or SHA256 digest unavailable; installation refused')
open(sys.argv[3],'w').write(asset['browser_download_url']+'\n'+asset['digest'][7:]+'\n')
PY
mapfile -t asset < "$stage/asset"
curl -fsSL "${asset[0]}" -o "$stage/opencode.tar.gz"
printf '%s  %s\n' "${asset[1]}" "$stage/opencode.tar.gz" | sha256sum -c -
tar -xzf "$stage/opencode.tar.gz" -C "$stage"
"$stage/opencode" --version | grep -Fx "$version"
mkdir -p "$HOME/.local/bin"
install -m 0755 "$stage/opencode" "$HOME/.local/bin/opencode"
echo "Installed OpenCode $version source candidate. ACP/provider dependencies and sandbox acceptance remain to be validated."
