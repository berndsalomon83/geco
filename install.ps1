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
            # gh is authenticated, so it avoids the anonymous API rate limit.
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
# Windows locks a RUNNING exe's image against overwrite (Copy-Item throws
# a sharing violation) but allows renaming it — and geco serve is a
# long-lived process an open MCP client keeps alive, so a plain overwrite
# aborted every upgrade attempted while a client was open, with a raw
# IOException under ErrorActionPreference=Stop. Rename-then-copy, the
# Windows analogue of install.sh's remove-then-copy.
if (Test-Path $Dest) {
    $Stale = "$Dest.old.$PID"
    try {
        Move-Item $Dest $Stale -Force
        Copy-Item $Bin $Dest -Force
        Remove-Item $Stale -Force -ErrorAction SilentlyContinue  # fails while still running: cleaned next install
    } catch {
        # Put the working binary back rather than leaving nothing installed.
        if (-not (Test-Path $Dest) -and (Test-Path $Stale)) { Move-Item $Stale $Dest -Force }
        Die "cannot replace $Dest ($($_.Exception.Message)) - close MCP clients using geco and retry"
    }
} else {
    Copy-Item $Bin $Dest -Force
}
# Leftover from a previous in-use upgrade, deletable now that its process is gone.
Get-ChildItem -Path $DestDir -Filter "geco.exe.old.*" -ErrorAction SilentlyContinue |
    Remove-Item -Force -ErrorAction SilentlyContinue
Say "Installed to $Dest"

# Add to PATH if not already there.
#
# Read and written through the REGISTRY, not [Environment]:: — the .NET
# getter expands REG_EXPAND_SZ (returning %JAVA_HOME%\bin as its current
# expansion) and the setter writes plain REG_SZ, so the round-trip froze
# every variable-based PATH entry at its install-time value and downgraded
# the value type. Silent corruption of user configuration, discovered long
# after. GetValue with DoNotExpandEnvironmentNames + SetValue as
# ExpandString preserves both the %VAR% text and the type.
$PathDirs = $env:PATH -split ";"
if ($DestDir -notin $PathDirs) {
    Say "Adding $DestDir to user PATH"
    $EnvKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey("Environment", $true)
    try {
        $UserPath = $EnvKey.GetValue("PATH", "",
            [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        if ($DestDir -notin ($UserPath -split ";")) {
            $NewPath = if ([string]::IsNullOrEmpty($UserPath)) { $DestDir } else { "$UserPath;$DestDir" }
            $EnvKey.SetValue("PATH", $NewPath,
                [Microsoft.Win32.RegistryValueKind]::ExpandString)
            $env:PATH = "$env:PATH;$DestDir"
            # The [Environment]:: setter broadcast WM_SETTINGCHANGE so new
            # shells see the change before the next login; a raw registry
            # write does not. Best effort — a failed broadcast just means
            # the change waits for re-login, same as no broadcast at all.
            try {
                $Sig = '[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Auto)]' +
                    'public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);'
                $SM = Add-Type -MemberDefinition $Sig -Name "EnvBroadcast" -Namespace "Win32" -PassThru
                [UIntPtr]$Out = [UIntPtr]::Zero
                # 0xffff = HWND_BROADCAST, 0x1A = WM_SETTINGCHANGE, 2 = SMTO_ABORTIFHUNG
                $SM::SendMessageTimeout([IntPtr]0xffff, 0x1A, [UIntPtr]::Zero,
                    "Environment", 2, 5000, [ref]$Out) | Out-Null
            } catch { }
        }
    } finally {
        $EnvKey.Close()
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

# The picker assets are deliberately NOT scaffolded here. pick.sh is a bash
# script that discovers an opencode session model through sqlite3; neither is
# present on a stock Windows box, so copying it would leave a config pointing
# at something that cannot run. The example config says as much and tells
# Windows users to point `picker.command` at their own wrapper instead. This
# comment exists so the omission reads as a decision, not an oversight.

# Generate editor schema.
Say "Generating config schema..."
# NOT Set-Content: in PowerShell 5.1 it writes the system ANSI codepage, and
# the schema carries the config doc comments verbatim: em-dashes and curly
# quotes included. The result is a file that is not valid UTF-8, so the YAML
# language server fails to load it and config autocomplete silently does not
# work, on Windows only. `geco schema` from a shell looks fine, which makes it
# read as an editor bug. UTF8Encoding($false) is UTF-8 with no BOM, which is
# what a JSON consumer expects.
# The DECODE matters as much as the encode: PowerShell reads a native
# command's stdout through [Console]::OutputEncoding, which on a stock 5.1
# console is the OEM codepage. Writing valid UTF-8 afterwards would faithfully
# preserve characters that were already mangled on the way in. The schema
# would load, and every em-dash in the config hover text would be mojibake.
# `-join` rather than Out-String: no trailing CRLF, and no LF->CRLF rewrite.
$prevOut = [Console]::OutputEncoding
try {
    [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false
    $schemaText = (& $Dest schema) -join "`n"
} finally {
    [Console]::OutputEncoding = $prevOut
}
[System.IO.File]::WriteAllText(
    (Join-Path $CfgDir "geco.schema.json"),
    $schemaText,
    (New-Object System.Text.UTF8Encoding $false))

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
# Checked BY NAME, not by count; see the matching comment in install.sh. The
# old `-eq 8` went stale when the surface dropped to 6 and then warned on
# every install, which teaches the user to ignore this line entirely. Only
# search and execute are required: the browse tools are served under
# `proxy.surface: full` alone, so requiring them warned on every default
# (lean) install. @() so a single result still counts as a collection.
$names = @(($result | Where-Object { $_.id -eq 2 }).result.tools.name)
$missing = @(@("search", "execute") |
    Where-Object { $names -notcontains $_ })
if ($missing.Count -eq 0) {
    Say "Proxy OK - meta-tools: $($names -join ', ')"
} else {
    Warn "proxy did not serve: $($missing -join ', ')"
}

Say "Done. Add servers to $Cfg and restart your agent."