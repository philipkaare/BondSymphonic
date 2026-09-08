# Creates the `bondsymphonic` WSL distro (Ubuntu 24.04) and provisions it.
$ErrorActionPreference = "Stop"
$Distro = "bondsymphonic"
$existing = (wsl -l -q) -replace "`0","" | Where-Object { $_.Trim() -eq $Distro }
if (-not $existing) {
  # wsl 2.4+ supports --name; the machine has 2.6.1
  wsl --install Ubuntu-24.04 --name $Distro --no-launch
  if ($LASTEXITCODE -ne 0) { throw "wsl --install failed with exit code $LASTEXITCODE" }
}
$sh = (Resolve-Path (Join-Path $PSScriptRoot "setup-wsl.sh")).Path
$wslPath = (wsl -d $Distro -u root -- wslpath -a ($sh -replace "\\","/")).Trim()
wsl -d $Distro -u root -- bash -c "sed -i 's/\r$//' '$wslPath' && bash '$wslPath' bs"
if ($LASTEXITCODE -ne 0) { throw "setup-wsl.sh provisioning failed with exit code $LASTEXITCODE" }
wsl --terminate $Distro     # so /etc/wsl.conf default user takes effect
wsl -d $Distro -- bash -lc "whoami; git --version; bwrap --version; cargo --version; claude --version; gh --version | head -1"
if ($LASTEXITCODE -ne 0) { throw "verification command failed with exit code $LASTEXITCODE" }
Write-Host "setup-wsl: distro '$Distro' ready"
