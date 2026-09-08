# Builds the Linux daemon inside the WSL distro and copies it to target\daemon\.
# The IDE's launcher picks the binary up from there and installs it into the distro.
param([switch]$Debug)
$ErrorActionPreference = "Stop"
$Distro = "bondsymphonic"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$profileFlag = if ($Debug) { "" } else { "--release" }
$profileDir = if ($Debug) { "debug" } else { "release" }

# Windows PowerShell turns a native program's stderr into error records, and cargo
# writes its build progress there, so wsl.exe runs with the preference relaxed and
# every call is checked through $LASTEXITCODE instead.
$ErrorActionPreference = "Continue"

$wslRoot = (& wsl -d $Distro -- wslpath -a ($root -replace "\\", "/") | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or -not $wslRoot) { throw "wslpath failed inside $Distro" }

& wsl -d $Distro -- bash -lc "cd '$wslRoot' && CARGO_TARGET_DIR=~/.bondsymphonic/target cargo build -p bondsymphonic-daemon $profileFlag"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed inside $Distro" }

New-Item -ItemType Directory -Force (Join-Path $root "target\daemon") -ErrorAction Stop | Out-Null

& wsl -d $Distro -- bash -lc "cp ~/.bondsymphonic/target/$profileDir/bondsymphonic-daemon '$wslRoot/target/daemon/bondsymphonic-daemon'"
if ($LASTEXITCODE -ne 0) { throw "copying the daemon out of $Distro failed" }

Write-Host "daemon built: target\daemon\bondsymphonic-daemon"
