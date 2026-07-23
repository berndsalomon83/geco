# geco uninstaller for Windows
#   Removes the binary, config (optionally), and agent config entries.
#   Usage: .\uninstall.ps1 [-Purge]
param([switch]$Purge)

$ErrorActionPreference = "Stop"

function Say($msg)  { Write-Host "==> $msg" -ForegroundColor Green }
function Warn($msg) { Write-Host "WARN $msg" -ForegroundColor Yellow }

$DestDir = Join-Path $env:USERPROFILE ".local\bin"
$Dest = Join-Path $DestDir "geco.exe"

# Remove binary.
if (Test-Path $Dest) {
    Remove-Item $Dest -Force
    Say "Removed $Dest"
} else {
    Say "Binary not found (already removed?)"
}

# Remove opencode health plugin (check both plugins/ and plugin/ dirs).
foreach ($d in @("plugins", "plugin")) {
    $Plugin = Join-Path $env:USERPROFILE ".config\opencode\$d\geco-tui.ts"
    if (Test-Path $Plugin) {
        Remove-Item $Plugin -Force
        Say "Removed opencode health plugin ($d/)"
    }
}

# Remove from agent configs.
foreach ($p in @(
    (Join-Path $env:USERPROFILE ".config\opencode\opencode.json"),
    (Join-Path $env:APPDATA "Claude\claude_desktop_config.json")
)) {
    if (-not (Test-Path $p)) { continue }
    try {
        $cfg = Get-Content $p -Raw | ConvertFrom-Json
        $changed = $false
        foreach ($key in @("mcp", "mcpServers")) {
            if ($cfg.$key -and $cfg.$key.PSObject.Properties["geco"]) {
                $cfg.$key.PSObject.Properties.Remove("geco")
                $changed = $true
            }
        }
        if ($changed) {
            Copy-Item $p "$p.bak"
            $cfg | ConvertTo-Json -Depth 10 | Set-Content $p
            Say "Removed geco from $p (backup: $p.bak)"
        }
    } catch {
        Warn "Could not parse $p - skip"
    }
}

# Purge config + cache.
if ($Purge) {
    $paths = @(
        (Join-Path $env:USERPROFILE ".config\geco"),
        (Join-Path $env:LOCALAPPDATA "geco\geco"),
        (Join-Path $env:APPDATA "geco\geco")
    )
    foreach ($p in $paths) {
        if (Test-Path $p) {
            Remove-Item $p -Recurse -Force
            Say "Purged $p"
        }
    }
} else {
    Say "Config kept at ~/.config/geco (re-run with -Purge to delete)"
}

Say "Done. Restart your agents."