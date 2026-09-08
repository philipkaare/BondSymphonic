# Installs everything needed to build bondsymphonic-ide on Windows.
# Idempotent: re-running skips what is present. Run from an elevated PowerShell.
$ErrorActionPreference = "Stop"
$QtVersion = "6.9.2"          # latest 6.9.x at time of writing; adjust if aqt reports it missing
$QtRoot = "C:\Qt"
$QtDir = "$QtRoot\$QtVersion\msvc2022_64"

function Have($cmd) { return [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
function Get-VcToolsPath {
  if (-not (Test-Path $vswhere)) { return $null }
  return & $vswhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
}

# 1. Rust
if (-not (Have "cargo")) {
  winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
  $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
}
rustup toolchain install stable --profile default
rustup default stable

# 2. VS 2022 Build Tools with C++ workload (Qt's msvc2022_64 binaries target v143)
if (-not (Get-VcToolsPath)) {
  winget install --id Microsoft.VisualStudio.2022.BuildTools -e --accept-source-agreements --accept-package-agreements `
    --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
}
if (-not (Get-VcToolsPath)) {
  throw "MSVC C++ toolset (VC.Tools.x86.x64) not found via vswhere after install attempt. This install must run elevated: re-run 'powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1' from an ELEVATED PowerShell (winget needs admin rights to write the HKLM registry keys the VS installer touches; a non-elevated run fails silently on those writes)."
}

# 3. CMake, Ninja, Python
if (-not (Have "cmake")) { winget install --id Kitware.CMake -e --accept-source-agreements --accept-package-agreements }
if (-not (Have "ninja")) { winget install --id Ninja-build.Ninja -e --accept-source-agreements --accept-package-agreements }
$py = Get-Command python -ErrorAction SilentlyContinue
if (-not $py -or $py.Source -like "*WindowsApps*") {
  winget install --id Python.Python.3.12 -e --accept-source-agreements --accept-package-agreements
}
# refresh PATH for this session
$env:PATH = [System.Environment]::GetEnvironmentVariable("PATH","Machine") + ";" + [System.Environment]::GetEnvironmentVariable("PATH","User")

# 4. Qt via aqtinstall
python -m pip install --upgrade aqtinstall
if (-not (Test-Path "$QtDir\bin\qmake.exe")) {
  python -m aqt install-qt windows desktop $QtVersion win64_msvc2022_64 -O $QtRoot
}
if (-not (Test-Path "$QtDir\bin\qmake.exe")) { throw "Qt install failed: $QtDir\bin\qmake.exe not found" }

# 5. Write env.ps1
$envFile = Join-Path $PSScriptRoot "env.ps1"
@"
`$env:QT_DIR = "$QtDir"
`$env:QMAKE = "$QtDir\bin\qmake.exe"
`$env:PATH = "$QtDir\bin;`$env:USERPROFILE\.cargo\bin;`$env:PATH"
"@ | Set-Content -Encoding ascii $envFile

# 6. Verify
. $envFile
cargo --version
cmake --version | Select-Object -First 1
ninja --version
& $env:QMAKE --version
$vcPath = Get-VcToolsPath
if (-not $vcPath) { throw "MSVC C++ toolset (VC.Tools.x86.x64) not found via vswhere. The 'cc' crate needs this to build; setup is NOT complete." }
Write-Host "MSVC found at: $vcPath"
Write-Host "setup-windows: OK"
