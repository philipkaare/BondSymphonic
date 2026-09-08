. (Join-Path $PSScriptRoot "env.ps1")
Push-Location (Join-Path $PSScriptRoot "..")
cargo run -p bondsymphonic-ide @args
Pop-Location
