# geco uninstaller for Windows
#   Removes the binary, agent config entries, and (with -Purge) config + cache.
#
#   irm https://raw.githubusercontent.com/berndsalomon83/geco/main/uninstall.ps1 | iex
#
#   with -Purge, piped:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/berndsalomon83/geco/main/uninstall.ps1))) -Purge
#
#   or from a clone:  powershell -ExecutionPolicy Bypass -File uninstall.ps1 [-Purge]
#
# One script block and no param() block, for the same reason as install.ps1:
# piped through iex this runs in the caller's session, and nothing it sets
# may stay behind there.
& {
    param($ScriptPath, [string[]]$Opts)

    $ErrorActionPreference = "Stop"

    $Purge = $false
    foreach ($o in $Opts) {
        switch ($o) {
            "-Purge" { $Purge = $true }
            default {
                Write-Host "ERROR unknown option: $o (option: -Purge)" -ForegroundColor Red
                if ($ScriptPath) { exit 1 } else { return }
            }
        }
    }

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

    # Remove the picker agent. It lives in opencode's tree rather than geco's, so
    # -Purge would never have reached it.
    foreach ($d in @("agents", "agent")) {
        $Agent = Join-Path $env:USERPROFILE ".config\opencode\$d\picker.md"
        if (Test-Path $Agent) {
            Remove-Item $Agent -Force
            Say "Removed picker agent ($d/)"
        }
    }

    # Remove from agent configs.
    foreach ($p in @(
        (Join-Path $env:USERPROFILE ".config\opencode\opencode.json"),
        (Join-Path $env:APPDATA "Claude\claude_desktop_config.json")
    )) {
        if (-not (Test-Path $p)) { continue }
        try {
            # -Encoding UTF8 on the READ as well as the write. PowerShell 5.1
            # decodes as ANSI by default, so without this the round-trip mangles
            # every non-ASCII character elsewhere in the user's agent config (an
            # accented path, a project name) while removing the geco entry
            # correctly. The file stays valid JSON, so nothing errors and the
            # damage is found much later. (A read hint only; it adds no BOM.)
            #
            # These comments stay ASCII on purpose: a .ps1 containing non-ASCII
            # without a BOM is itself decoded as ANSI by 5.1, which is the very
            # trap being fixed here. PSScriptAnalyzer enforces it in CI.
            $cfg = Get-Content $p -Raw -Encoding UTF8 | ConvertFrom-Json
            $changed = $false
            foreach ($key in @("mcp", "mcpServers")) {
                if ($cfg.$key -and $cfg.$key.PSObject.Properties["geco"]) {
                    $cfg.$key.PSObject.Properties.Remove("geco")
                    $changed = $true
                }
            }
            if ($changed) {
                Copy-Item $p "$p.bak"
                # NOT Set-Content: PowerShell 5.1 writes the system ANSI codepage,
                # which would mangle any non-ASCII already in the user's agent
                # config (a project name, an accented path) while rewriting it.
                # UTF8Encoding($false) is UTF-8 with no BOM, which is what a JSON
                # parser expects. Same trap as the schema write in install.ps1.
                [System.IO.File]::WriteAllText(
                    $p,
                    ($cfg | ConvertTo-Json -Depth 10),
                    (New-Object System.Text.UTF8Encoding $false))
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
        Say "Config kept at ~/.config/geco (run with -Purge to delete it too)"
    }

    Say "Done. Restart your agents."
} $MyInvocation.MyCommand.Path $args
