# Creates the `bondsymphonic` WSL distro (Ubuntu 24.04) and provisions it.
#
#   -Runtime   Provision a machine that only *runs* the daemon: git, bubblewrap,
#              python3, Claude Code and gh, and no Rust toolchain. This is what
#              a packaged install does -- end users never build the daemon, and a
#              rustup install is most of the download and most of the disk.
#   -WhatIf    Print every command this script would run and change nothing.
#              The distro is never created, provisioned or terminated.
#
# Both modes are idempotent, and neither touches a distro that already exists
# beyond re-running the provisioning script inside it.
param([switch]$Runtime, [switch]$WhatIf)
$ErrorActionPreference = "Stop"
$Distro = "bondsymphonic"
$mode = "dev"
if ($Runtime) { $mode = "runtime" }

# Windows PowerShell turns a native program's stderr into error records, and both
# wsl.exe and apt write progress there, so the preference is relaxed and every
# call is checked through $LASTEXITCODE instead.
$ErrorActionPreference = "Continue"

function Invoke-Wsl {
  param([string[]]$Arguments, [string]$What, [string]$WhatIfResult = "")
  if ($WhatIf) {
    Write-Host "would run: wsl $($Arguments -join ' ')"
    return $WhatIfResult
  }
  $out = & wsl @Arguments
  if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
  return $out
}

Write-Host "setup-wsl: distro '$Distro', mode '$mode'"

# Listing distros changes nothing, so it runs even under -WhatIf: knowing
# whether the distro is already there is what makes the preview accurate.
$existing = (wsl -l -q) -replace "`0","" | Where-Object { $_.Trim() -eq $Distro }
if (-not $existing) {
  # wsl 2.4+ supports --name; the machine has 2.6.1
  Invoke-Wsl -Arguments @("--install", "Ubuntu-24.04", "--name", $Distro, "--no-launch") -What "wsl --install" | Out-Null
} else {
  Write-Host "setup-wsl: distro '$Distro' already exists; provisioning it in place"
}

$sh = (Resolve-Path (Join-Path $PSScriptRoot "setup-wsl.sh")).Path
$wslPath = (Invoke-Wsl -Arguments @("-d", $Distro, "-u", "root", "--", "wslpath", "-a", ($sh -replace "\\","/")) -What "wslpath" -WhatIfResult "/mnt/<setup-wsl.sh>").Trim()
Invoke-Wsl -Arguments @("-d", $Distro, "-u", "root", "--", "bash", "-c", "sed -i 's/\r`$//' '$wslPath' && bash '$wslPath' bs $mode") -What "setup-wsl.sh provisioning" | Out-Null
Invoke-Wsl -Arguments @("--terminate", $Distro) -What "wsl --terminate" | Out-Null   # so /etc/wsl.conf default user takes effect

# What the daemon needs at run time, and -- in dev mode only -- what building it needs.
$verify = "whoami; git --version; bwrap --version; python3 --version; claude --version; gh --version | head -1"
if (-not $Runtime) { $verify = "whoami; git --version; bwrap --version; python3 --version; cargo --version; claude --version; gh --version | head -1" }
Invoke-Wsl -Arguments @("-d", $Distro, "--", "bash", "-lc", $verify) -What "verification command"

if ($WhatIf) {
  Write-Host "setup-wsl: -WhatIf, nothing was changed"
} else {
  Write-Host "setup-wsl: distro '$Distro' ready ($mode)"
}
