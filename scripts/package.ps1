# Builds a runnable, redistributable folder: dist\BondSymphonic\.
#
#   .\scripts\package.ps1 [-Version 0.1.0] [-SkipDaemon] [-NoDaemon] [-SkipZip]
#
# It contains the release IDE executable, the Qt runtime `windeployqt` says that
# executable needs, the Visual C++ runtime that both the exe and Qt6Gui import
# statically, the Linux daemon binary the IDE installs into the WSL distro on
# first run, the two WSL provisioning scripts, and a generated `install.ps1`
# that provisions the distro and then starts the IDE.
#
# -Version defaults to the workspace version in Cargo.toml, so the zip is named
# after the build rather than after whatever was typed.
# -SkipDaemon reuses the binary already in target\daemon\ (a WSL build takes
# minutes and rarely changes while the packaging itself is being worked on).
# -NoDaemon builds a Windows-only package with no daemon in it and no zip, for a
# CI runner that has Qt but no WSL. The completeness assertions still run, so
# the Qt and CRT deployment is checked exactly as it is in a real package; the
# result can run `--version` and the offscreen smoke, but not a real workspace.
# -SkipZip stops after the folder, for iterating on the script itself.
#
# Requires a dev shell: dot-source scripts\env.ps1 first, so QMAKE points at the
# Qt installation whose windeployqt and DLLs go into the package.
param([string]$Version, [switch]$SkipDaemon, [switch]$NoDaemon, [switch]$SkipZip)
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
if ($NoDaemon) { Write-Warning "package: -NoDaemon, the package will contain no daemon and no zip. It can run --version and the offscreen smoke, not a real workspace." }

# Native tools write progress to stderr, which Windows PowerShell turns into
# error records; every call is checked through $LASTEXITCODE instead.
$ErrorActionPreference = "Continue"

Write-Host "package: building the IDE (release)"
& cargo build -p bondsymphonic-ide --release --manifest-path (Join-Path $root "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "cargo build -p bondsymphonic-ide --release failed" }

if ($NoDaemon) {
  Write-Host "package: -NoDaemon, not building the daemon"
} elseif ($SkipDaemon) {
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
if ((-not $NoDaemon) -and (-not (Test-Path $builtDaemon))) {
  throw "$builtDaemon is missing. Run scripts\build-daemon.ps1 (or drop -SkipDaemon)."
}

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

# windeployqt brings these in behind Qt6Gui, and they are 27 MB of a 76 MB
# folder. Their only consumer is QtGui's D3D12 QRhi backend, which is reached
# through QRhi clients -- QtQuick, QRhiWidget, an OpenGL/RHI QGraphicsView
# viewport. BondSymphonic is Qt Widgets on the raster paint engine with the
# `windows` platform plugin and no QML, so the backend is never instantiated;
# Qt6Gui.dll imports d3d11/dxgi/d3d12 statically but loads dxcompiler on demand,
# and a failed load degrades rather than crashes. The completeness assertion at
# the end of this script is what will catch a future QtQuick dependency.
foreach ($unused in @("dxcompiler.dll", "dxil.dll")) {
  $path = Join-Path $stage $unused
  if (Test-Path $path) { Remove-Item -Force $path; Write-Host "package: dropped $unused (D3D12 QRhi only)" }
}

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

# The Visual C++ runtime. `bondsymphonic-ide.exe` imports MSVCP140 and
# VCRUNTIME140, and Qt6Gui.dll adds MSVCP140_1, MSVCP140_2 and VCRUNTIME140_1 --
# all static imports, so the loader fails the process before `main` on a machine
# without the redistributable. (The `api-ms-win-crt-*` half is the UCRT, which
# ships with Windows 10 and later.)
#
# Located through vswhere rather than by setting VCINSTALLDIR and asking
# windeployqt: the newest Visual Studio instance on a machine is not necessarily
# the one carrying the redistributable, and windeployqt answers a bad guess with
# a warning and no files -- which is exactly the silent failure that shipped a
# package nobody could start. Copied explicitly instead, and asserted below.
Write-Host "package: deploying the Visual C++ runtime"
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe not found at $vswhere; the Visual C++ runtime cannot be located" }
$ErrorActionPreference = "Continue"
$crtDlls = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Redist.14.Latest -find "VC\Redist\MSVC\*\x64\Microsoft.VC143.CRT\*.dll"
$ErrorActionPreference = "Stop"
if (-not $crtDlls) {
  throw "no Visual C++ redistributable found. Install the 'C++ Redistributable MSMs / Redist' component in the Visual Studio Installer, then run this again."
}

# Several toolsets can be installed side by side. Prefer the one the exe was
# actually linked with, read out of its PE optional header (MajorLinkerVersion,
# MinorLinkerVersion) -- a newer CRT than the binaries were built against
# usually works, but "usually" is not something to ship.
$peBytes = [System.IO.File]::ReadAllBytes((Join-Path $stage $exeName))
$optionalHeader = [BitConverter]::ToInt32($peBytes, 0x3C) + 4 + 20
$linked = "{0}.{1:00}" -f $peBytes[$optionalHeader + 2], $peBytes[$optionalHeader + 3]
$byToolset = $crtDlls | Group-Object { ($_ -split '\\Redist\\MSVC\\')[1] -replace '\\.*$', '' }
$chosen = $byToolset | Where-Object { $_.Name.StartsWith("$linked.") -or $_.Name -eq $linked }
if (-not $chosen) {
  $chosen = $byToolset | Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
  Write-Warning "no VC++ redistributable for toolset $linked; using $($chosen.Name), which the exe was not linked against"
} else {
  $chosen = @($chosen)[0]
}
Write-Host "package: VC++ runtime $($chosen.Name) (exe linked with $linked), $($chosen.Count) DLLs"
$chosen.Group | ForEach-Object { Copy-Item $_ $stage }

if ($NoDaemon) {
  Write-Host "package: -NoDaemon, no $daemonName in the package"
} else {
  # The Linux ELF the IDE's launcher hashes and installs into the distro. It sits
  # beside the exe, which is the first place launcher::resolve_daemon_binary looks.
  Copy-Item $builtDaemon (Join-Path $stage $daemonName)
}

Copy-Item (Join-Path $PSScriptRoot "setup-wsl.ps1") (Join-Path $stage "setup-wsl.ps1")
Copy-Item (Join-Path $PSScriptRoot "setup-wsl.sh") (Join-Path $stage "setup-wsl.sh")

$installer = @'
# Prepares this machine to run BondSymphonic, then starts it.
#
# Run once after unzipping. It provisions the `bondsymphonic` WSL distro --
# Ubuntu 24.04 with git, bubblewrap, python3, Claude Code and gh, and no Rust
# toolchain, because the daemon ships as a binary in this folder -- creating the
# distro first if it is not already there. Provisioning is idempotent, so
# running this again on a machine that is already set up re-checks every step
# and repairs a distro that was half-provisioned by an interrupted first run.
#
#   -NoStart   provision only; do not start the IDE.
#   -WhatIf    print what would be done and change nothing.
param([switch]$NoStart, [switch]$WhatIf)
$ErrorActionPreference = "Stop"
$here = $PSScriptRoot
$Distro = "bondsymphonic"

# `wsl.exe` is in System32 on stock Windows 11 whether or not the WSL feature is
# installed, so its presence proves nothing; `--status` failing is the real test.
if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
  throw 'wsl.exe was not found. BondSymphonic needs WSL2: run "wsl --install" in an elevated PowerShell, reboot, and run this script again.'
}
$ErrorActionPreference = "Continue"
& wsl.exe --status | Out-Null
$wslReady = ($LASTEXITCODE -eq 0)
$ErrorActionPreference = "Stop"
if (-not $wslReady) {
  throw 'WSL is not installed or not working ("wsl --status" failed). Run "wsl --install" in an elevated PowerShell, reboot, and run this script again.'
}

$existing = (wsl -l -q) -replace "`0","" | Where-Object { $_.Trim() -eq $Distro }
if ($existing) {
  Write-Host "install: the '$Distro' distro is already here; re-checking its provisioning"
} else {
  Write-Host "install: creating the '$Distro' distro (this downloads about a gigabyte and takes a few minutes)"
}
$setup = Join-Path $here "setup-wsl.ps1"
if (-not (Test-Path $setup)) { throw "setup-wsl.ps1 is not beside this script" }
if ($WhatIf) { & $setup -Runtime -WhatIf } else { & $setup -Runtime }

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

# One LF, no BOM and no CR: this file is read on the Linux side too, and
# Set-Content -Encoding ascii would end it with CRLF.
[System.IO.File]::WriteAllText((Join-Path $stage "VERSION"), "$Version`n")

# Everything the package cannot start without, checked before anyone is told it
# is done. The Visual C++ gap that shipped in the first version of this script
# survived because a windeployqt warning scrolled past; a throw does not scroll.
$required = @(
  $exeName,
  "Qt6Core.dll", "Qt6Gui.dll", "Qt6Widgets.dll",
  "vcruntime140.dll", "vcruntime140_1.dll",
  "msvcp140.dll", "msvcp140_1.dll", "msvcp140_2.dll",
  "platforms\qwindows.dll", "platforms\qoffscreen.dll",
  "install.ps1", "setup-wsl.ps1", "setup-wsl.sh", "VERSION"
)
if (-not $NoDaemon) { $required += $daemonName }
$missing = $required | Where-Object { -not (Test-Path (Join-Path $stage $_)) }
if ($missing) { throw "package is incomplete: $($missing -join ', ') missing from $stage" }
# The other half of the claim: what was dropped on purpose stayed dropped. A Qt
# upgrade that starts needing the D3D12 shader compiler must fail here loudly
# rather than have it silently deleted out from under the running application.
$dropped = @("dxcompiler.dll", "dxil.dll") | Where-Object { Test-Path (Join-Path $stage $_) }
if ($dropped) { throw "package still contains $($dropped -join ', '); the drop step did not run" }
Write-Host "package: completeness check passed ($($required.Count) required files)"

$bytes = (Get-ChildItem -Recurse -File $stage | Measure-Object -Property Length -Sum).Sum
$files = (Get-ChildItem -Recurse -File $stage | Measure-Object).Count
$mb = [math]::Round($bytes / 1MB, 1)
Write-Host "package: $stage is $mb MB across $files files"

if ($SkipZip -or $NoDaemon) {
  Write-Host "package: no archive written"
} else {
  $zip = Join-Path $dist "BondSymphonic-$Version-win64.zip"
  if (Test-Path $zip) { Remove-Item -Force $zip }
  Compress-Archive -Path $stage -DestinationPath $zip
  $zipMb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
  Write-Host "package: $zip is $zipMb MB"
}

Write-Host "package: done. Test it with"
Write-Host "    `$env:BS_PACKAGED_EXE = `"$stage\$exeName`"; cargo test -p bondsymphonic-ide --test packaged_smoke"
