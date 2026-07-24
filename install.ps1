# geco installer for Windows
#   1. download a prebuilt binary (or -FromSource / fall back to cargo build)
#   2. install the binary to a directory on PATH
#   3. scaffold ~/.config/geco/config.yaml if missing
#   4. build the first tool snapshot + verify the meta-tool surface
param([switch]$Force, [switch]$FromSource)

$ErrorActionPreference = "Stop"

$RepoDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$GhRepo = "berndsalomon83/geco"
$CfgDir = Join-Path $env:USERPROFILE ".config\geco"
$Cfg = Join-Path $CfgDir "config.yaml"

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "WARN $msg" -ForegroundColor Yellow }
function Die($msg)  { Write-Host "ERROR $msg" -ForegroundColor Red; exit 1 }

# Returns the path to an extracted prebuilt geco.exe, or $null. Never fatal
# except on a checksum mismatch: everything else falls back to a source build.
function Get-Prebuilt {
    $target = "x86_64-pc-windows-msvc"
    if ($env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
        # No ARM64 Windows target is published yet; the x64 build runs under
        # emulation, so prefer a source build for a native binary.
        return $null
    }
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("geco-dl-" + [System.Guid]::NewGuid().ToString("N").Substring(0,8))
    New-Item -ItemType Directory -Force $tmp | Out-Null
    try {
        $gh = Get-Command gh -ErrorAction SilentlyContinue
        if ($gh) {
            # gh carries credentials, which a private repo needs.
            & $gh.Source release download --repo $GhRepo --pattern "*$target*" --pattern "SHA256SUMS" --dir $tmp 2>$null | Out-Null
        }
        if (-not (Get-ChildItem $tmp -Filter *.zip -ErrorAction SilentlyContinue)) {
            [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
            $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$GhRepo/releases/latest" `
                                     -Headers @{ "User-Agent" = "geco-installer" }
            $asset = $rel.assets | Where-Object { $_.name -like "*$target*.zip" } | Select-Object -First 1
            if (-not $asset) { return $null }
            Invoke-WebRequest -Uri $asset.browser_download_url -OutFile (Join-Path $tmp $asset.name) -UseBasicParsing
            $sums = $rel.assets | Where-Object { $_.name -eq "SHA256SUMS" } | Select-Object -First 1
            if ($sums) { Invoke-WebRequest -Uri $sums.browser_download_url -OutFile (Join-Path $tmp "SHA256SUMS") -UseBasicParsing }
        }
        $zip = Get-ChildItem $tmp -Filter *.zip | Select-Object -First 1
        if (-not $zip) { return $null }

        $sumFile = Join-Path $tmp "SHA256SUMS"
        if (Test-Path $sumFile) {
            $line = Select-String -Path $sumFile -Pattern ([regex]::Escape($zip.Name)) | Select-Object -First 1
            if ($line) {
                $want = ($line.Line -split '\s+')[0].ToLower()
                $got = (Get-FileHash $zip.FullName -Algorithm SHA256).Hash.ToLower()
                if ($want -ne $got) { Die "checksum mismatch on $($zip.Name) - refusing to install" }
            }
        }

        Expand-Archive -Path $zip.FullName -DestinationPath $tmp -Force
        $exe = Get-ChildItem $tmp -Recurse -Filter geco.exe | Select-Object -First 1
        if ($exe) { return $exe.FullName }
        return $null
    } catch {
        return $null
    }
}

$Bin = $null
if (-not $FromSource) {
    Say "Looking for a prebuilt geco..."
    $Bin = Get-Prebuilt
    if ($Bin) { Say "Using prebuilt binary (no Rust toolchain needed)" }
}

if (-not $Bin) {
    if (-not $FromSource) { Warn "no prebuilt binary available - falling back to a source build" }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Die "cargo not found - install Rust first: https://rustup.rs"
    }
    if (-not (Test-Path "$RepoDir\Cargo.toml")) {
        Die "run this from the geco checkout"
    }
    Say "Building geco (release)..."
    Push-Location $RepoDir
    cargo build --release
    if ($LASTEXITCODE -ne 0) { Pop-Location; Die "cargo build failed" }
    Pop-Location
    $Bin = Join-Path $RepoDir "target\release\geco.exe"
}
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

# Install opencode health plugin if opencode is present.
$OpencodePluginDir = Join-Path $env:USERPROFILE ".config\opencode\plugins"
if (Test-Path (Split-Path $OpencodePluginDir -Parent)) {
    if (-not (Test-Path $OpencodePluginDir)) { New-Item -ItemType Directory -Path $OpencodePluginDir -Force | Out-Null }
    Copy-Item (Join-Path $RepoDir "opencode-plugin\geco-tui.ts") $OpencodePluginDir -Force
    Say "Installed opencode health plugin"
}

# Build snapshot + verify.
Say "Building initial snapshot..."
# NOT `2>&1`: PowerShell 5.1 wraps each stderr line from a native command in an
# ErrorRecord, and with $ErrorActionPreference = "Stop" that is terminating -
# so the installer aborted here the moment any backend logged a warning. geco
# logs everything to stderr by design (stdout belongs to the MCP protocol), and
# a first refresh with a missing `npx` or keychain item is the common case, not
# the exception. Let stderr flow to the console untouched and judge the result
# by the exit code, which is what it is for.
& $Dest --config $Cfg refresh
if ($LASTEXITCODE -ne 0) {
    Warn "refresh reported issues (fix the config, then: geco refresh)"
}

Say "Verifying meta-tool surface..."
$init = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"install","version":"0"}}}'
$tools = '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
$msgs = "$init`n$tools"
# The same PowerShell 5.1 trap as the refresh above, and it bites even with
# `2>$null`: stderr from a native command still becomes ErrorRecords, which
# are terminating under "Stop". geco announces itself on stderr at startup,
# so this aborted every install. Relax the preference for just this call.
$prevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$raw = $msgs | & $Dest --config $Cfg serve --no-refresh 2>$null
$ErrorActionPreference = $prevEap
$result = $raw | ConvertFrom-Json
$toolCount = ($result | Where-Object { $_.id -eq 2 }).result.tools.Count
if ($toolCount -eq 8) {
    Say "Proxy OK: 8 meta-tools"
} else {
    Warn "expected 8 meta-tools, got: $toolCount"
}

Say "Done. Add servers to $Cfg and restart your agent."