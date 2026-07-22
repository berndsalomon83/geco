# geco installer for Windows
#   1. cargo build --release
#   2. install the binary to a directory on PATH
#   3. scaffold ~/.config/geco/config.yaml if missing
#   4. build the first tool snapshot + verify the meta-tool surface
param([switch]$Force)

$ErrorActionPreference = "Stop"

$RepoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$CfgDir = Join-Path $env:USERPROFILE ".config\geco"
$Cfg = Join-Path $CfgDir "config.yaml"

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "WARN $msg" -ForegroundColor Yellow }
function Die($msg)  { Write-Host "ERROR $msg" -ForegroundColor Red; exit 1 }

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Die "cargo not found - install Rust first: https://rustup.rs"
}
if (-not (Test-Path "$RepoDir\Cargo.toml")) {
    Die "run this from the geco checkout"
}

Say "Building geco (release)..."
Push-Location $RepoDir
cargo build --release
if ($LASTEXITCODE -ne 0) { Die "cargo build failed" }
Pop-Location

# Install the binary.
$Bin = Join-Path $RepoDir "target\release\geco.exe"
if (-not (Test-Path $Bin)) { Die "binary not found at $Bin" }

$DestDir = Join-Path $env:USERPROFILE ".local\bin"
if (-not (Test-Path $DestDir)) { New-Item -ItemType Directory -Path $DestDir -Force | Out-Null }
$Dest = Join-Path $DestDir "geco.exe"

if ((Test-Path $Dest) -and -not $Force) {
    $existing = & $Dest --version 2>$null
    Say "Existing install: $existing"
}
Copy-Item $Bin $Dest -Force
Say "Installed to $Dest"

# Add to PATH if not already there.
$PathDirs = $env:PATH -split ";"
if ($DestDir -notin $PathDirs) {
    Say "Adding $DestDir to user PATH"
    $UserPath = [Environment]::GetEnvironmentVariable("PATH", "User")
    if ($DestDir -notin ($UserPath -split ";")) {
        [Environment]::SetEnvironmentVariable("PATH", "$UserPath;$DestDir", "User")
        $env:PATH = "$env:PATH;$DestDir"
    }
}

# Scaffold config.
if (-not (Test-Path $CfgDir)) { New-Item -ItemType Directory -Path $CfgDir -Force | Out-Null }
if (-not (Test-Path $Cfg)) {
    Say "Scaffolding $Cfg"
    Copy-Item (Join-Path $RepoDir "geco.example.yaml") $Cfg
} else {
    Say "Config already exists at $Cfg (kept)"
}

# Generate editor schema.
Say "Generating config schema..."
& $Dest schema | Set-Content (Join-Path $CfgDir "geco.schema.json")

# Build snapshot + verify.
Say "Building initial snapshot..."
& $Dest --config $Cfg refresh 2>&1 | ForEach-Object { Write-Host $_ }

Say "Verifying meta-tool surface..."
$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"install","version":"0"}}}'
$tools = '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
$msgs = "$init`n$tools"
$result = $msgs | & $Dest --config $Cfg serve --no-refresh 2>$null | ConvertFrom-Json
$toolCount = ($result | Where-Object { $_.id -eq 2 }).result.tools.Count
if ($toolCount -eq 8) {
    Say "Proxy OK: 8 meta-tools"
} else {
    Warn "expected 8 meta-tools, got: $toolCount"
}

Say "Done. Add servers to $Cfg and restart your agent."