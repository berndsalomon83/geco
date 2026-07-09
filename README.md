# ondemand-mcp

An MCP proxy that keeps your servers' tool schemas **out of the model's context until the moment they're needed**. Put N servers behind it; the agent sees five tiny meta-tools (~2.2 KB total) instead of every schema on every request.

Rust, single static binary, no runtime dependencies. Design inspired by [lazy-mcp](https://github.com/voicetreelab/lazy-mcp) (MIT) — reimplemented from scratch on the official [rmcp](https://crates.io/crates/rmcp) SDK, with search, self-refreshing snapshots, allowlists, and status built in.

## Why

Every MCP server you connect injects its full tool catalogue into every model call. A handful of servers easily costs 20–50k tokens before the conversation starts — and measurably degrades tool selection. ondemand-mcp flattens that to a constant ~2.2 KB, no matter how many tools hide behind it.

## How it works

The agent sees exactly five tools:

| Meta-tool | Purpose |
|---|---|
| `list_servers` | Name, description, tool count, running state of each backend |
| `get_tools` | A server's tools (name + description); with `tool`: the full input schema |
| `search_tools` | Keyword search across *all* servers' tools in one call |
| `execute_tool` | Run a backend tool — the backend process starts on first use |
| `refresh_tools` | Re-scan backends after installing/reconfiguring one |

Tool catalogues come from a **snapshot** cached on disk. The proxy refreshes it automatically at startup when your config changes (hash-based), or on demand — backends stay cold the rest of the time. A backend process starts only when `execute_tool` first touches it, and is reused afterwards.

## Quickstart

```bash
cargo build --release
cp target/release/ondemand-mcp ~/.local/bin/   # or use install.sh
```

Create `~/.config/ondemand-mcp/config.json` (see `ondemand-mcp.example.json`) — entries are copy-paste compatible with both Claude Desktop and OpenCode shapes:

```jsonc
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${GITHUB_TOKEN}" },
      "description": "GitHub: issues, PRs, repos",
      "deny": ["delete_*"]
    },
    "filesystem": {
      "command": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "${HOME}/Documents"],
      "description": "Files under ~/Documents"
    }
  }
}
```

Then register the **proxy** (instead of the individual servers) in your agent:

```jsonc
// OpenCode: ~/.config/opencode/opencode.json
"mcp": {
  "ondemand": { "type": "local", "command": ["ondemand-mcp", "serve"], "enabled": true }
}
```

```json
// Claude Desktop: claude_desktop_config.json
"mcpServers": {
  "ondemand": { "command": "/path/to/ondemand-mcp", "args": ["serve"] }
}
```

## Config reference

Per server: `command` (string **or** array), `args`, `env` (with `${VAR}` expansion), `description` (shown in `list_servers` — write a good one, it's what the agent decides by), `allow` / `deny` (glob patterns, deny wins; enforced at listing **and** execution), `enabled`.

`proxy` section: `call_timeout_secs` (default 120), `startup_timeout_secs` (30), `refresh_on_start` (true).

### Secrets

Don't put tokens in the config or your shell rc. `${keychain:SERVICE}` resolves from the OS keychain **at backend spawn time** — the secret exists only in that child process's environment:

```jsonc
"env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${keychain:github_token}" }
```

Store it once: `security add-generic-password -a "$USER" -s github_token -w '<token>'` (macOS Keychain; Linux uses `secret-tool` / libsecret). A missing keychain item fails the spawn loudly — plain `${VAR}` env references still expand to "" like shell. There is deliberately no `${cmd:...}`: arbitrary command execution from a config file is an injection surface.

## CLI

```bash
ondemand-mcp serve [--refresh|--no-refresh]   # run the proxy (default command)
ondemand-mcp refresh                          # rebuild the snapshot and exit
ondemand-mcp inspect                          # show the cached snapshot
```

`RUST_LOG=debug` for verbose logging (always stderr — stdout is the protocol).

## Which servers belong behind the proxy?

Servers you use *sometimes*. A server whose tools you want the model to reach for proactively on every task (e.g. a memory store like [icm-slim](https://github.com/rtk-ai/icm)) can stay directly connected — visibility is the point there. Everything else is a candidate.

## License

MIT. Design derived from lazy-mcp (© 2025 tbxark, MIT); this implementation © 2026 Bernd Salomon.
