# Runs the proto and daemon test suites inside the WSL distro.
$ErrorActionPreference = "Stop"
$Distro = "bondsymphonic"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

# Windows PowerShell turns a native program's stderr into error records, and cargo
# writes its build progress there, so wsl.exe runs with the preference relaxed and
# every call is checked through $LASTEXITCODE instead.
$ErrorActionPreference = "Continue"

$wslRoot = (& wsl -d $Distro -- wslpath -a ($root -replace "\\", "/") | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or -not $wslRoot) { throw "wslpath failed inside $Distro" }

# The sandbox test runs `sandbox-init` from the daemon binary inside bwrap, so
# the binary has to exist and be named explicitly: under `cargo test` the running
# executable is the test harness, not the daemon.
& wsl -d $Distro -- bash -lc "cd '$wslRoot' && CARGO_TARGET_DIR=~/.bondsymphonic/target cargo build -p bondsymphonic-daemon"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed inside $Distro" }

$testArgs = $args -join " "
& wsl -d $Distro -- bash -lc "cd '$wslRoot' && CARGO_TARGET_DIR=~/.bondsymphonic/target BS_DAEMON_EXE=~/.bondsymphonic/target/debug/bondsymphonic-daemon cargo test -p bondsymphonic-proto -p bondsymphonic-daemon $testArgs"
if ($LASTEXITCODE -ne 0) { throw "cargo test failed inside $Distro" }

Write-Host "daemon tests passed"
