# ondemand-mcp — Install, Migrate, Verify, Uninstall

## Requirements

Rust toolchain (rustup.rs), `python3` (used by the scripts for JSON handling). Backends need whatever they always needed (`npx`, `uvx`, binaries…) — the proxy just spawns them later instead of your agent spawning them at startup.

## Install

```bash
bash install.sh
```

Builds the release binary, installs it (existing location → `/opt/homebrew/bin` → `~/.local/bin`; remove-then-copy so macOS signing stays valid), scaffolds `~/.config/ondemand-mcp/config.json` from the example if missing, builds the first tool snapshot, and verifies the proxy answers with exactly 5 meta-tools.

## Migrate servers behind the proxy

Take the occasional servers out of your agent config and move them into the proxy config. Example, OpenCode:

**Before** (`~/.config/opencode/opencode.json`):
```jsonc
"mcp": {
  "icm":        { "type": "local", "command": ["icm", "serve"], "enabled": true },
  "github":     { "type": "local", "command": ["npx", "-y", "@modelcontextprotocol/server-github"], "enabled": true },
  "playwright": { "type": "local", "command": ["npx", "@playwright/mcp@latest"], "enabled": true }
}
```

**After** — keep always-on servers direct, gate the rest:
```jsonc
"mcp": {
  "icm":      { "type": "local", "command": ["icm", "serve"], "enabled": true },
  "ondemand": { "type": "local", "command": ["ondemand-mcp", "serve"], "enabled": true }
}
```

`~/.config/ondemand-mcp/config.json`:
```jsonc
{
  "mcpServers": {
    "github":     { "command": ["npx", "-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" },
                    "description": "GitHub: issues, PRs, repos" },
    "playwright": { "command": ["npx", "@playwright/mcp@latest"],
                    "description": "Browser automation" }
  }
}
```

The entries are the same shape — copy, paste, add a `description` (that's what the agent sees when deciding). Then:

```bash
ondemand-mcp refresh     # snapshot the moved servers once
```

Restart the agent. Context cost for those servers drops to ~2.2 KB flat.

**Rule of thumb:** keep proactive-use servers (memory, core workflow) direct; gate everything you use occasionally.

## Verify

```bash
ondemand-mcp inspect                  # snapshot: servers, tool counts, refresh status
RUST_LOG=debug ondemand-mcp serve     # verbose logs on stderr (Ctrl-C to stop)
```

In the agent: ask it to `search_tools` for something you know is behind the proxy, then execute it. First execution on a cold backend takes the backend's startup time (npx downloads on very first use); after that it's warm.

## Config knobs

`proxy.call_timeout_secs` (120), `proxy.startup_timeout_secs` (30 — raise for slow `npx` first-runs), `proxy.refresh_on_start` (true; refresh only happens when the config hash changed). Per server: `allow`/`deny` glob lists (deny wins, enforced at listing *and* execution), `enabled`, `${VAR}` expansion in `command`/`args`/`env`.

## Uninstall

```bash
bash uninstall.sh          # removes binary + 'ondemand' entries from agent configs (with .bak backups)
bash uninstall.sh --purge  # …and config + snapshot cache
```

Re-add any servers you still want directly to your agent config afterwards.

## Troubleshooting

- **Backend shows `error:` in `inspect`** — run the backend's command by hand; usually a missing env var or binary. Fix, then `ondemand-mcp refresh`.
- **First `execute_tool` times out** — cold `npx`/`uvx` fetch. Raise `startup_timeout_secs`, or pre-warm: `ondemand-mcp refresh`.
- **Agent can't see a newly installed backend tool** — `refresh_tools` meta-tool, or `ondemand-mcp refresh`.
- **Where are logs?** stderr of the proxy process (your agent's MCP log view), `RUST_LOG=debug` for detail.
