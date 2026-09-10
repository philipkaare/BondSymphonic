# Builds a runnable, redistributable folder: dist\BondSymphonic\.
#
#   .\scripts\package.ps1 [-Version 0.1.0] [-SkipDaemon] [-SkipZip]
#
# It contains the release IDE executable, the Qt runtime `windeployqt` says that
# executable needs, the Linux daemon binary the IDE installs into the WSL distro
# on first run, the two WSL provisioning scripts, and a generated `install.ps1`
# that creates the distro when it is missing and then starts the IDE.
#
# -Version defaults to the workspace version in Cargo.toml, so the zip is named
# after the build rather than after whatever was typed. -SkipDaemon reuses the
# binary already in target\daemon\ (a WSL build takes minutes and rarely changes
# while the packaging itself is being worked on).
#
# Requires a dev shell: dot-source scripts\env.ps1 first, so QMAKE points at the
# Qt installation whose windeployqt and DLLs go into the package.
param([string]$Version, [switch]$SkipDaemon, [switch]$SkipZip)
$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$dist = Join-Path $root "dist"
$stage = Join-Path $dist "BondSymphonic"
$exeName = "bondsymphonic-ide.exe"
$daemonName = "bondsymphonic-daemon"

if (-not $Version) {
  # The one `version = "x.y.z"` under [workspace.package]; every crate inherits it.
  $manifest = Get-Content (Join-Path $root "Cargo.toml")
  $inPackage = $false
  foreach ($line in $manifest) {
    if ($line -match '^\s*\[workspace\.package\]\s*$') { $inPackage = $true; continue }
    if ($inPackage -and $line -match '^\s*\[') { break }
    if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') { $Version = $Matches[1]; break }
  }
}
if (-not $Version) { throw "no version given and none found under [workspace.package] in Cargo.toml" }

$qmake = $env:QMAKE
if (-not $qmake) { throw "QMAKE is unset. Dot-source scripts\env.ps1 first: windeployqt and the Qt DLLs come from that installation." }
if (-not (Test-Path $qmake)) { throw "QMAKE=$qmake does not exist" }
$windeployqt = Join-Path (Split-Path $qmake -Parent) "windeployqt.exe"
if (-not (Test-Path $windeployqt)) { throw "windeployqt.exe not found beside $qmake" }

Write-Host "package: BondSymphonic $Version"

# Native tools write progress to stderr, which Windows PowerShell turns into
# error records; every call is checked through $LASTEXITCODE instead.
$ErrorActionPreference = "Continue"

Write-Host "package: building the IDE (release)"
& cargo build -p bondsymphonic-ide --release --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "cargo build -p bondsymphonic-ide --release failed" }

if ($SkipDaemon) {
  Write-Host "package: -SkipDaemon, reusing target\daemon\$daemonName"
} else {
  Write-Host "package: building the daemon inside WSL (release)"
  & (Join-Path $PSScriptRoot "build-daemon.ps1")
  if ($LASTEXITCODE -ne 0) { throw "build-daemon.ps1 failed" }
}

$ErrorActionPreference = "Stop"

$builtExe = Join-Path $root "target\release\$exeName"
if (-not (Test-Path $builtExe)) { throw "$builtExe was not built" }
$builtDaemon = Join-Path $root "target\daemon\$daemonName"
if (-not (Test-Path $builtDaemon)) { throw "$builtDaemon is missing. Run scripts\build-daemon.ps1 (or drop -SkipDaemon)." }

# A fresh staging folder every time: windeployqt only ever adds files, so a
# folder left over from an older Qt would ship both versions of a DLL.
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item $builtExe (Join-Path $stage $exeName)

$ErrorActionPreference = "Continue"
Write-Host "package: deploying the Qt runtime"
# --no-translations, --no-system-d3d-compiler, --no-opengl-sw and
# --no-quick-import: BondSymphonic is English-only Qt Widgets with no QML, and
# the software OpenGL fallback alone is ~20 MB the widgets never touch.
& $windeployqt --release --no-translations --no-system-d3d-compiler --no-opengl-sw --no-quick-import (Join-Path $stage $exeName)
if ($LASTEXITCODE -ne 0) { throw "windeployqt failed with exit code $LASTEXITCODE" }
$ErrorActionPreference = "Stop"

# windeployqt deploys only the platform plugin a desktop run needs, `qwindows`.
# The offscreen plugin goes in beside it so the package can be started headless:
# that is how crates\ide\tests\packaged_smoke.rs proves the deployed Qt loads,
# and how CI or a support session can start the package on a machine with no
# session at all. 110 KB, and nothing selects it unless QT_QPA_PLATFORM does.
$offscreen = Join-Path (Split-Path (Split-Path $qmake -Parent) -Parent) "plugins\platforms\qoffscreen.dll"
if (Test-Path $offscreen) {
  Copy-Item $offscreen (Join-Path $stage "platforms\qoffscreen.dll")
} else {
  Write-Warning "qoffscreen.dll was not found at $offscreen; the package cannot be started headless"
}

# The Linux ELF the IDE's launcher hashes and installs into the distro. It sits
# beside the exe, which is the first place launcher::resolve_daemon_binary looks.
Copy-Item $builtDaemon (Join-Path $stage $daemonName)

Copy-Item (Join-Path $PSScriptRoot "setup-wsl.ps1") (Join-Path $stage "setup-wsl.ps1")
Copy-Item (Join-Path $PSScriptRoot "setup-wsl.sh") (Join-Path $stage "setup-wsl.sh")

$installer = @'
# Prepares this machine to run BondSymphonic, then starts it.
#
# Run once after unzipping. It creates the `bondsymphonic` WSL distro if it is
# not already there -- Ubuntu 24.04 with git, bubblewrap, python3, Claude Code
# and gh, and no Rust toolchain, because the daemon ships as a binary in this
# folder. Running it again on a machine that already has the distro re-checks
# the provisioning and changes nothing else.
#
#   -NoStart   provision only; do not start the IDE.
#   -WhatIf    print what would be done and change nothing.
param([switch]$NoStart, [switch]$WhatIf)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$Distro = "bondsymphonic"

if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
  throw 'wsl.exe was not found. BondSymphonic needs WSL2: run "wsl --install" in an elevated PowerShell, reboot, and run this script again.' 
}

$existing = (wsl -l -q) -replace "`0","" | Where-Object { $_.Trim() -eq $Distro }
if ($existing) {
  Write-Host "install: the '$Distro' distro is already here"
} else {
  Write-Host "install: creating the '$Distro' distro (this downloads about a gigabyte and takes a few minutes)"
  $setup = Join-Path $here "setup-wsl.ps1"
  if ($WhatIf) { & $setup -Runtime -WhatIf } else { & $setup -Runtime }
}

if ($NoStart) { Write-Host "install: -NoStart, not launching"; return }
$exe = Join-Path $here "bondsymphonic-ide.exe"
if (-not (Test-Path $exe)) { throw "bondsymphonic-ide.exe is not beside this script" }
if ($WhatIf) {
  Write-Host "would run: $exe"
} else {
  Write-Host "install: starting BondSymphonic"
  Start-Process $exe -WorkingDirectory $here
}
'@
Set-Content -Path (Join-Path $stage "install.ps1") -Value $installer -Encoding utf8

# ASCII, so the file has no byte-order mark for a reader to trip over.
Set-Content -Path (Join-Path $stage "VERSION") -Value $Version -Encoding ascii

$bytes = (Get-ChildItem -Recurse -File $stage | Measure-Object -Property Length -Sum).Sum
$files = (Get-ChildItem -Recurse -File $stage | Measure-Object).Count
$mb = [math]::Round($bytes / 1MB, 1)
Write-Host "package: $stage is $mb MB across $files files"

if ($SkipZip) {
  Write-Host "package: -SkipZip, no archive written"
} else {
  $zip = Join-Path $dist "BondSymphonic-$Version-win64.zip"
  if (Test-Path $zip) { Remove-Item -Force $zip }
  Compress-Archive -Path $stage -DestinationPath $zip
  $zipMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
  Write-Host "package: $zip is $zipMb MB"
}

Write-Host "package: done. Test it with"
Write-Host "    `$env:BS_PACKAGED_EXE = `"$stage\$exeName`"; cargo test -p bondsymphonic-ide --test packaged_smoke"
