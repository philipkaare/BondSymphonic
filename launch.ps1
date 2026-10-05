# One-command launch for BondSymphonic.
#
#   .\launch.ps1              build the daemon (release) and start the IDE
#   .\launch.ps1 -Debug       debug daemon build
#   .\launch.ps1 -Release     release IDE build
#   .\launch.ps1 -SkipDaemon  reuse the last daemon binary in target\daemon\
#   .\launch.ps1 -NoSetup     fail instead of installing missing prerequisites
#   .\launch.ps1 -NoCleanup   keep stale Cargo build caches in the WSL distro
#   .\launch.ps1 -Compact     also shrink the distro's ext4.vhdx (shuts down ALL WSL
#                             distros and asks for elevation)
#
# Missing prerequisites are installed by the setup scripts: scripts\setup-windows.ps1
# (elevated; Rust, VS Build Tools, CMake, Ninja, Python, Qt) and scripts\setup-wsl.ps1
# (the `bondsymphonic` WSL distro with git, bubblewrap, rustup, Claude Code, gh).
param(
  [switch]$Debug,
  [switch]$SkipDaemon,
  [switch]$Release,
  [switch]$NoSetup,
  [switch]$NoCleanup,
  [switch]$Compact
)
$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
$scripts = Join-Path $root "scripts"
$Distro = "bondsymphonic"

function Have($cmd) { return [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

function Test-IsAdmin {
  $id = [Security.Principal.WindowsIdentity]::GetCurrent()
  return (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-VcToolsPath {
  $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
  if (-not (Test-Path $vswhere)) { return $null }
  $p = & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
  if ($p) { return ($p | Select-Object -First 1) } else { return $null }
}

function Refresh-Path {
  $machine = [Environment]::GetEnvironmentVariable("PATH", "Machine")
  $user = [Environment]::GetEnvironmentVariable("PATH", "User")
  $env:PATH = "$machine;$user;$env:PATH"
  . (Join-Path $scripts "env.ps1")
}

function Test-Distro {
  $names = (& wsl -l -q) -replace "`0", "" | ForEach-Object { $_.Trim() }
  return ($names -contains $Distro)
}

function Test-DistroHealthy {
  $prev = $ErrorActionPreference
  $ErrorActionPreference = "Continue"
  & wsl -d $Distro -- bash -lc "command -v cargo >/dev/null && command -v bwrap >/dev/null && command -v git >/dev/null" 2>$null | Out-Null
  $ok = ($LASTEXITCODE -eq 0)
  $ErrorActionPreference = $prev
  return $ok
}

function Get-DistroVhdPath {
  $entry = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss\*" -ErrorAction SilentlyContinue |
    Where-Object { $_.DistributionName -eq $Distro } | Select-Object -First 1
  if (-not $entry -or -not $entry.BasePath) { return $null }
  $name = if ($entry.VhdFileName) { $entry.VhdFileName } else { "ext4.vhdx" }
  $path = Join-Path ($entry.BasePath -replace '^\\\\\?\\', '') $name
  if (Test-Path $path) { return $path } else { return $null }
}

# Failures here are warnings: the launch carries on with the disk as it was.
function Compress-DistroVhd {
  $vhd = Get-DistroVhdPath
  if (-not $vhd) { Write-Warning "Compact skipped: could not locate the virtual disk of '$Distro'."; return }
  $before = (Get-Item $vhd).Length

  $prev = $ErrorActionPreference
  $ErrorActionPreference = "Continue"
  try {
    & wsl -d $Distro -u root -- fstrim -v /
    if ($LASTEXITCODE -ne 0) { Write-Warning "fstrim failed with exit code $LASTEXITCODE; compacting anyway." }

    Write-Host "Shutting down WSL to compact $vhd ..."
    & wsl --shutdown
    if ($LASTEXITCODE -ne 0) { Write-Warning "Compact skipped: wsl --shutdown failed with exit code $LASTEXITCODE"; return }

    $dpScript = Join-Path $env:TEMP "bondsymphonic-compact-vhd.txt"
    Set-Content -Path $dpScript -Encoding ASCII -Value @(
      "select vdisk file=`"$vhd`"",
      "attach vdisk readonly",
      "compact vdisk",
      "detach vdisk"
    )
    try {
      if (Test-IsAdmin) {
        & diskpart /s $dpScript
        $code = $LASTEXITCODE
      } else {
        Write-Host "Running diskpart in an elevated window (accept the UAC prompt) ..."
        $p = Start-Process -FilePath "diskpart" -Verb RunAs -Wait -PassThru -ArgumentList "/s", "`"$dpScript`""
        $code = $p.ExitCode
      }
    } catch {
      Write-Warning "Compact failed: $($_.Exception.Message)"
      return
    } finally {
      Remove-Item $dpScript -ErrorAction SilentlyContinue
    }
    if ($code -ne 0) { Write-Warning "diskpart failed with exit code $code"; return }

    $after = (Get-Item $vhd).Length
    Write-Host ("Compacted {0}: {1:N1} GB -> {2:N1} GB" -f (Split-Path $vhd -Leaf), ($before / 1GB), ($after / 1GB))
  } finally {
    $ErrorActionPreference = $prev
  }
}

# ---- 1. Environment -------------------------------------------------------------
Refresh-Path

# ---- 2. Windows toolchain -------------------------------------------------------
$missing = @()
if (-not (Have "cargo")) { $missing += "cargo (Rust)" }
if (-not (Test-Path $env:QMAKE)) { $missing += "Qt ($env:QMAKE)" }
if (-not (Get-VcToolsPath)) { $missing += "MSVC C++ toolset" }
if (-not (Have "cmake")) { $missing += "cmake" }
if (-not (Have "ninja")) { $missing += "ninja" }

if ($missing.Count -gt 0) {
  Write-Host "Windows toolchain incomplete: $($missing -join ', ')"
  if ($NoSetup) { throw "Run scripts\setup-windows.ps1 from an elevated PowerShell, then retry." }
  $setup = Join-Path $scripts "setup-windows.ps1"
  if (Test-IsAdmin) {
    Write-Host "Running $setup ..."
    & powershell -NoProfile -ExecutionPolicy Bypass -File $setup
    if ($LASTEXITCODE -ne 0) { throw "setup-windows.ps1 failed with exit code $LASTEXITCODE" }
  } else {
    Write-Host "Running $setup in an elevated window (accept the UAC prompt) ..."
    $p = Start-Process -FilePath "powershell" -Verb RunAs -Wait -PassThru `
      -ArgumentList "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$setup`""
    if ($p.ExitCode -ne 0) { throw "setup-windows.ps1 failed with exit code $($p.ExitCode)" }
  }
  Refresh-Path
  if (-not (Have "cargo") -or -not (Test-Path $env:QMAKE) -or -not (Get-VcToolsPath)) {
    throw "Toolchain still incomplete after setup. Open a new terminal and retry; if it persists, run scripts\setup-windows.ps1 manually."
  }
}

# ---- 3. WSL distro --------------------------------------------------------------
if (-not (Have "wsl")) { throw "wsl.exe not found. Install WSL2 (wsl --install) and retry." }
if (-not (Test-Distro) -or -not (Test-DistroHealthy)) {
  Write-Host "WSL distro '$Distro' missing or incomplete."
  if ($NoSetup) { throw "Run scripts\setup-wsl.ps1, then retry." }
  $setup = Join-Path $scripts "setup-wsl.ps1"
  Write-Host "Running $setup (this takes several minutes the first time) ..."
  & powershell -NoProfile -ExecutionPolicy Bypass -File $setup
  if ($LASTEXITCODE -ne 0) { throw "setup-wsl.ps1 failed with exit code $LASTEXITCODE" }
  if (-not (Test-DistroHealthy)) { throw "Distro '$Distro' still unhealthy after setup." }
}

# ---- 3a. Reclaim stale Rust build caches ---------------------------------------
# Keep the profile of the daemon cache that build-daemon.ps1 is about to use. Its
# other profile, and every other Cargo target directory in the dedicated distro
# home, is disposable once none of its files have changed for 24 hours; the helper
# also skips cleanup while Cargo/rustc is active. A failed cleanup never blocks
# the launch.
if (-not $NoCleanup) {
  $keepProfile = if ($Debug) { "debug" } else { "release" }
  $cleanupScript = Join-Path $scripts "cleanup-wsl-builds.sh"
  $prev = $ErrorActionPreference
  $ErrorActionPreference = "Continue"
  $wslCleanupScript = (& wsl -d $Distro -- wslpath -a ($cleanupScript -replace "\\", "/") | Out-String).Trim()
  if ($LASTEXITCODE -ne 0 -or -not $wslCleanupScript) {
    Write-Warning "WSL build-cache cleanup skipped: wslpath failed for $cleanupScript"
  } else {
    & wsl -d $Distro -- bash $wslCleanupScript 24 $keepProfile
    if ($LASTEXITCODE -ne 0) { Write-Warning "WSL build-cache cleanup failed with exit code $LASTEXITCODE" }
  }
  $ErrorActionPreference = $prev
}

# ---- 3b. Compact the distro's virtual disk (-Compact) ---------------------------
# Space freed inside the distro stays allocated in ext4.vhdx until the file is
# compacted, which needs every WSL distro stopped and an elevated diskpart.
if ($Compact) { Compress-DistroVhd }

# ---- 4. Daemon ------------------------------------------------------------------
if (-not $SkipDaemon) {
  $bd = @{}
  if ($Debug) { $bd.Debug = $true }
  & (Join-Path $scripts "build-daemon.ps1") @bd
} elseif (-not (Test-Path (Join-Path $root "target\daemon\bondsymphonic-daemon"))) {
  throw "-SkipDaemon given but target\daemon\bondsymphonic-daemon does not exist. Run once without -SkipDaemon."
}

# ---- 5. IDE ---------------------------------------------------------------------
# cargo writes build progress to stderr, which Windows PowerShell would otherwise
# turn into terminating errors; relax the preference and use the exit code instead.
$ErrorActionPreference = "Continue"
$cargoArgs = @("run", "-p", "bondsymphonic-ide")
if ($Release) { $cargoArgs += "--release" }
Push-Location $root
try {
  & cargo @cargoArgs
  $code = $LASTEXITCODE
} finally {
  Pop-Location
}
exit $code
