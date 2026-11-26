#!/usr/bin/env bash
# Builds the macOS app bundle and its two distributables:
#
#   dist/BondSymphonic.app
#   dist/BondSymphonic-<version>-macos-arm64.dmg   first install (drag to Applications)
#   dist/BondSymphonic-<version>-macos-arm64.zip   what the IDE's updater downloads
#
#   scripts/package-macos.sh [--version X.Y.Z] [--skip-build] [--no-daemon]
#
# The bundle holds the release IDE, the Qt frameworks macdeployqt says it needs,
# and in Contents/Resources: the aarch64 Linux daemon (target/daemon/, from
# scripts/build-daemon.sh or the release workflow), setup-wsl.sh (the VM's
# provisioning script), and Lima -- limactl, its Linux guest agent and its
# templates -- so a user needs neither Homebrew nor a checkout. The IDE creates
# and provisions the VM itself on first start.
#
# Signing is optional and driven by the environment:
#   MACOS_SIGN_IDENTITY   a "Developer ID Application: ..." identity in the
#                         keychain. Unset: ad-hoc signed, runs only on this Mac.
#   NOTARY_KEY_PATH, NOTARY_KEY_ID, NOTARY_ISSUER
#                         an App Store Connect API key. All three set (and an
#                         identity): the zip and the dmg are notarized and the
#                         tickets stapled.
#
# --no-daemon packages without a daemon, for CI checking the packaging itself
# on a runner that cannot build the Linux binary.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"
APP="$DIST/BondSymphonic.app"
BUNDLE_ID="io.github.philipkaare.bondsymphonic"

# Pinned: the bundle ships this exact Lima. Bumping it means a new hash from
# the release's SHA256SUMS.
LIMA_VERSION="2.3.0"
LIMA_SHA256="b7af444bd8f91d3c307477e562f2cf3bd7595d62b9429185e4fa841563578cfc"

version="" skip_build=0 no_daemon=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="$2"; shift ;;
    --skip-build) skip_build=1 ;;
    --no-daemon) no_daemon=1 ;;
    -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "package-macos: unknown option '$1'" >&2; exit 2 ;;
  esac
  shift
done

[ "$(uname -s)" = "Darwin" ] || { echo "package-macos: run this on macOS" >&2; exit 1; }
[ "$(uname -m)" = "arm64" ] || { echo "package-macos: releases are Apple Silicon only" >&2; exit 1; }
: "${QMAKE:?QMAKE must point at the Qt installation to deploy (its macdeployqt is used)}"
MACDEPLOYQT="$(dirname "$QMAKE")/macdeployqt"
[ -x "$MACDEPLOYQT" ] || { echo "package-macos: $MACDEPLOYQT not found" >&2; exit 1; }

if [ -z "$version" ]; then
  version="$(awk '/^\[workspace\.package\]/{p=1;next} /^\[/{p=0} p && /^version *=/{gsub(/.*= *"|".*/,""); print; exit}' "$ROOT/Cargo.toml")"
fi
echo "package-macos: BondSymphonic $version"

IDENTITY="${MACOS_SIGN_IDENTITY:-}"
notarize=0
if [ -n "$IDENTITY" ] && [ -n "${NOTARY_KEY_PATH:-}" ] && [ -n "${NOTARY_KEY_ID:-}" ] && [ -n "${NOTARY_ISSUER:-}" ]; then
  notarize=1
fi

# ---- 1. Build ---------------------------------------------------------------------
if [ "$skip_build" = 0 ]; then
  cargo build -p bondsymphonic-ide --release --manifest-path "$ROOT/Cargo.toml"
fi
EXE="$ROOT/target/release/bondsymphonic-ide"
[ -x "$EXE" ] || { echo "package-macos: $EXE was not built" >&2; exit 1; }
DAEMON="$ROOT/target/daemon/bondsymphonic-daemon"
if [ "$no_daemon" = 0 ]; then
  [ -f "$DAEMON" ] || { echo "package-macos: $DAEMON is missing (scripts/build-daemon.sh)" >&2; exit 1; }
  file "$DAEMON" | grep -q 'ELF 64-bit.*aarch64' || {
    echo "package-macos: $DAEMON is not an aarch64 Linux binary: $(file -b "$DAEMON")" >&2; exit 1; }
fi

# ---- 2. Lima ----------------------------------------------------------------------
lima_dir="$ROOT/target/lima-$LIMA_VERSION"
lima_tgz="$lima_dir.tar.gz"
if [ ! -x "$lima_dir/bin/limactl" ]; then
  curl -fsSL -o "$lima_tgz" \
    "https://github.com/lima-vm/lima/releases/download/v$LIMA_VERSION/lima-$LIMA_VERSION-Darwin-arm64.tar.gz"
  echo "$LIMA_SHA256  $lima_tgz" | shasum -a 256 -c -
  rm -rf "$lima_dir" && mkdir -p "$lima_dir"
  tar -xzf "$lima_tgz" -C "$lima_dir"
fi

# ---- 3. Bundle --------------------------------------------------------------------
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources/lima/bin" "$APP/Contents/Resources/lima/share/lima"
cp "$EXE" "$APP/Contents/MacOS/bondsymphonic-ide"
[ "$no_daemon" = 1 ] || install -m 755 "$DAEMON" "$APP/Contents/Resources/bondsymphonic-daemon"
cp "$ROOT/scripts/setup-wsl.sh" "$APP/Contents/Resources/setup-wsl.sh"
# Only what `limactl create/start/shell` of a Linux VM needs: the Linux guest
# agent and the templates. The rest of the tarball (the macOS guest agent, the
# krunkit driver, the menu bar app) is code nobody runs here, and every Mach-O
# in the bundle is one more thing to sign and notarize.
cp "$lima_dir/bin/limactl" "$APP/Contents/Resources/lima/bin/limactl"
cp "$lima_dir/share/lima/lima-guestagent.Linux-aarch64.gz" "$APP/Contents/Resources/lima/share/lima/"
cp -R "$lima_dir/share/lima/templates" "$APP/Contents/Resources/lima/share/lima/templates"
cp "$lima_dir/share/doc/lima/LICENSE" "$APP/Contents/Resources/lima/LICENSE"

iconset="$DIST/logo.iconset"
rm -rf "$iconset" && mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$ROOT/assets/logo.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  # assets/logo.png is 512 px, so there is no 512@2x.
  if [ $double -le 512 ]; then
    sips -z $double $double "$ROOT/assets/logo.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
  fi
done
iconutil -c icns "$iconset" -o "$APP/Contents/Resources/logo.icns"
rm -rf "$iconset"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>BondSymphonic</string>
    <key>CFBundleDisplayName</key><string>BondSymphonic</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleExecutable</key><string>bondsymphonic-ide</string>
    <key>CFBundleIconFile</key><string>logo</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

"$MACDEPLOYQT" "$APP" -always-overwrite -verbose=1

# ---- 4. Sign ----------------------------------------------------------------------
# Inside out: every nested dylib, framework and executable before the bundle
# that seals them. `--deep` is not used for signing (Apple advises against it);
# it is only used to verify.
sign() {
  if [ -n "$IDENTITY" ]; then
    codesign --force --timestamp --options runtime --sign "$IDENTITY" "$@"
  else
    codesign --force --sign - "$@"
  fi
}
while IFS= read -r -d '' lib; do sign "$lib"; done < <(find "$APP/Contents/PlugIns" "$APP/Contents/Frameworks" -name '*.dylib' -type f -print0 2>/dev/null)
while IFS= read -r -d '' fw; do sign "$fw"; done < <(find "$APP/Contents/Frameworks" -maxdepth 1 -name '*.framework' -print0 2>/dev/null)
sign --entitlements "$ROOT/installer/macos/lima.entitlements" "$APP/Contents/Resources/lima/bin/limactl"
sign "$APP"
codesign --verify --deep --strict --verbose=2 "$APP"

# ---- 5. Notarize, zip, dmg --------------------------------------------------------
ZIP="$DIST/BondSymphonic-$version-macos-arm64.zip"
DMG="$DIST/BondSymphonic-$version-macos-arm64.dmg"
notarize_file() {
  local out id status
  out="$(xcrun notarytool submit "$1" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" \
    --issuer "$NOTARY_ISSUER" --wait --timeout 60m --output-format json)"
  echo "$out"
  id="$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("id",""))')"
  status="$(printf '%s' "$out" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("status",""))')"
  if [ "$status" != "Accepted" ]; then
    [ -n "$id" ] && xcrun notarytool log "$id" --key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER" || true
    echo "package-macos: notarization of $1 ended '$status'" >&2
    exit 1
  fi
}

rm -f "$ZIP" "$DMG"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
if [ "$notarize" = 1 ]; then
  notarize_file "$ZIP"
  xcrun stapler staple "$APP"
  # Re-zipped, so what the updater installs carries its ticket offline.
  rm -f "$ZIP"
  ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
fi

dmg_stage="$DIST/dmg"
rm -rf "$dmg_stage" && mkdir -p "$dmg_stage"
cp -R "$APP" "$dmg_stage/"
ln -s /Applications "$dmg_stage/Applications"
hdiutil create -volname "BondSymphonic $version" -srcfolder "$dmg_stage" -ov -format UDZO "$DMG"
rm -rf "$dmg_stage"
if [ -n "$IDENTITY" ]; then
  codesign --force --timestamp --sign "$IDENTITY" "$DMG"
fi
if [ "$notarize" = 1 ]; then
  notarize_file "$DMG"
  xcrun stapler staple "$DMG"
fi

echo "package-macos: done"
ls -lh "$ZIP" "$DMG"
[ "$notarize" = 1 ] || echo "package-macos: NOT notarized; other Macs will refuse to open it"
