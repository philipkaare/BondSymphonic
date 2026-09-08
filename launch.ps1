# One-command launch for BondSymphonic.
#
#   .\launch.ps1              build the daemon (release) and start the IDE
#   .\launch.ps1 -Debug       debug daemon build
#   .\launch.ps1 -Release     release IDE build
#   .\launch.ps1 -SkipDaemon  reuse the last daemon binary in target\daemon\
#   .\launch.ps1 -NoSetup     fail instead of installing missing prerequisites
#
# Missing prerequisites are installed by the setup scripts: scripts\setup-windows.ps1
# (elevated; Rust, VS Build Tools, CMake, Ninja, Python, Qt) and scripts\setup-wsl.ps1
# (the `bondsymphonic` WSL distro with git, bubblewrap, rustup, Claude Code, gh).
param(
  [switch]$Debug,
  [switch]$SkipDaemon,
  [switch]$Release,
  [switch]$NoSetup
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
