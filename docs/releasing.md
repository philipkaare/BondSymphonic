# Releasing and auto-update

Every commit on `main` that passes CI is released automatically, and installed
copies of BondSymphonic update themselves from those releases.

## What happens on a push to main

1. `ci` runs as usual. It also tests the updater crate, compiles the Windows
   installer script, and builds and packages the macOS app (ad-hoc signed).
2. When `ci` succeeds, `release` (`.github/workflows/release.yml`) starts:
   - **prepare** chooses the version. The `major.minor` comes from
     `[workspace.package]` in `Cargo.toml`, and the patch is one past the
     highest `v<major>.<minor>.*` tag (`scripts/release/next-version.py`).
     Only the commit that is still the tip of `main` is released. A commit
     that already has a tag is skipped.
   - **daemon** builds the Linux daemon for x86_64 (WSL) and aarch64 (Lima on
     Apple Silicon).
   - **windows** stamps the version into the build, runs `scripts/package.ps1`,
     and wraps the result in the Inno Setup installer
     (`installer/windows/BondSymphonic.iss`).
   - **macos** runs `scripts/package-macos.sh`, which builds
     `BondSymphonic.app` with the daemon, Lima and the provisioning script
     inside. It signs the app with the Developer ID, notarizes it, staples the
     ticket, and produces a `.dmg` and a `.zip`.
   - **publish** writes `latest.json` (the version, plus each platform
     package's URL, size and SHA-256) and signs it with minisign. It then
     checks the signature against `crates/updater/update-key.pub` and creates
     the GitHub release `v<version>` with all of it attached.

The version stamp is never committed. The tag is the record of which commit
became which version. To start a new series, bump the minor (or major) in
`Cargo.toml`; the next release is then `<new>.1`.

## How installed copies update

On start, and every 6 hours after that, the IDE fetches
`https://github.com/philipkaare/BondSymphonic/releases/latest/download/latest.json`
and its `.minisig`. These are two plain downloads, with no GitHub API calls
and no token. It then:

1. Verifies the signature with the public key compiled into it. A manifest
   that fails the check is ignored.
2. Compares versions. Only a strictly newer release is taken, so a replayed
   old manifest can never downgrade an install.
3. Checks that the package URL is under this repository's release for that
   version. It downloads the file, and keeps it only if the size and SHA-256
   match the signed manifest.
4. Prepares the install. On macOS this means extracting the bundle and
   checking its code signature and Team ID against the running app.
5. Installs when the IDE quits. A helper process waits for the IDE to exit:
   - **Windows**: it runs the installer with `/VERYSILENT`.
   - **macOS**: it swaps the app bundle.

   **Restart to update** (in the status bar and in Settings) does the same
   thing now and then starts the new version.

**Settings > General > Updates** has a switch between automatic and manual
mode, and a **Check now** button:

- **Automatic**: checks, downloads and installs on quit.
- **Manual**: does nothing until you click **Check now**.

A development build, or a package unzipped by hand, checks for updates but
never installs one, because it wasn't installed by the installer. On Windows
the IDE recognises an installed copy by Inno Setup's `unins000.exe`; on macOS,
by running from an `.app` bundle.

If an update did not take, read the installer's log:

- **Windows**: `%LOCALAPPDATA%\BondSymphonic\update-install.log`
- **macOS**: `~/Library/Caches/BondSymphonic/update-install.log`

## One-time setup: secrets

Until these exist, `release` runs but publishes nothing. Its summary shows a
warning naming what is missing.

### Update signing key (required)

The key needs [minisign](https://jedisct1.github.io/minisign/)
(`brew install minisign`, or `winget install minisign`). Generate it once,
outside the repository:

```sh
minisign -G -W -p crates/updater/update-key.pub -s ~/bondsymphonic-release.key
gh secret set MINISIGN_SECRET_KEY < ~/bondsymphonic-release.key
```

- `-W` writes the secret key without a password. CI cannot type one, and the
  GitHub secret is what protects it.
- Commit the new `crates/updater/update-key.pub`. It **replaces** the
  placeholder; the file must contain only the two lines minisign wrote.
- Keep `~/bondsymphonic-release.key` somewhere safe and offline.
- If the key is lost, installed copies can no longer verify updates. Users
  would then have to install a build carrying the new public key by hand.

### Apple: Developer ID and notarization (for macOS packages)

1. **Signing certificate.** In Xcode (Settings > Accounts > Manage
   Certificates) or on developer.apple.com, create a **Developer ID
   Application** certificate. Export it from Keychain Access as a `.p12` with
   a password, then:

   ```sh
   base64 -i DeveloperID.p12 | gh secret set MACOS_CERTIFICATE_P12
   gh secret set MACOS_CERTIFICATE_PASSWORD
   ```

2. **Notary API key.** In App Store Connect, go to Users and Access >
   Integrations > App Store Connect API and create a Team key with the
   Developer role. Download the `.p8`, then:

   ```sh
   gh secret set APPLE_API_KEY_P8 < AuthKey_XXXXXXXXXX.p8
   gh secret set APPLE_API_KEY_ID      # the key's ID
   gh secret set APPLE_API_ISSUER_ID   # the issuer ID shown above the key list
   ```

Without these, the release still ships for Windows, and the macOS app is
built but not published.

### Windows code signing (not set up)

The Windows installer is unsigned, so SmartScreen warns about an "unknown
publisher" on a first install downloaded with a browser. Updates are fetched
by the IDE itself, carry no mark-of-the-web, and do not show the warning.
Azure Trusted Signing is the cheapest way to sign later: add a signing step
to the `windows` job before the upload.

## Local packaging

- Windows: `.\scripts\package.ps1`, then
  `iscc /DAppVersion=0.1.0 installer\windows\BondSymphonic.iss`
- macOS: `scripts/build-daemon.sh && QMAKE=$(brew --prefix)/opt/qt/bin/qmake scripts/package-macos.sh`.
  This is ad-hoc signed unless `MACOS_SIGN_IDENTITY` is set; see the script's
  header for the notarization variables.
