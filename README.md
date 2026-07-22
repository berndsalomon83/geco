# geco

*the generic connector*

geco is an MCP gateway that keeps your servers' tool schemas **out of the model's context until the moment they're needed**. Put N servers behind it; the agent sees nine small meta-tools (~3.6 KB total) instead of every schema on every request.

Rust, single static binary, no runtime dependencies. Design inspired by [lazy-mcp](https://github.com/voicetreelab/lazy-mcp) (MIT) — reimplemented from scratch on the official [rmcp](https://crates.io/crates/rmcp) SDK, with search, self-refreshing snapshots, allowlists, and status built in.

## Why

Every MCP server you connect injects its full tool catalogue into every model call. A handful of servers easily costs 20–50k tokens before the conversation starts — and measurably degrades tool selection. geco flattens that to a constant ~3.6 KB, no matter how many tools hide behind it.

## How it works

The agent sees exactly nine meta-tools:

| Meta-tool | Purpose |
|---|---|
| `list_servers` | Name, description, tool count, running state of each backend |
| `get_tools` | A server's tools (name + description); with `tool`: the full input schema |
| `search_tools` | Keyword search across *all* servers' tools in one call |
| `execute_tool` | Run a backend tool — the backend process starts on first use |
| `refresh_tools` | Re-scan backends after installing/reconfiguring one |
| `health` | Report which backends failed their last refresh and why |
| `list_skills` | Every skill in the library (name + one-line description) |
| `search_skills` | Find a skill (playbook + bundled scripts) by intent |
| `read_skill` | Load a skill's SKILL.md + file inventory (runs bundled scripts via your shell) |

Tool catalogues come from a **snapshot** cached on disk. The proxy refreshes it automatically at startup when your config changes (hash-based), or on demand — backends stay cold the rest of the time. A backend process starts only when `execute_tool` first touches it, and is reused afterwards.

## Quickstart

```bash
cargo build --release
cp target/release/geco ~/.local/bin/   # or use install.sh
```

Create `~/.config/geco/config.yaml` (see `geco.example.yaml`; JSON is equally accepted) — entries are copy-paste compatible with both Claude Desktop and OpenCode shapes:

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
  "geco": { "type": "local", "command": ["geco", "serve"], "enabled": true }
}
```

```json
// Claude Desktop: claude_desktop_config.json
"mcpServers": {
  "geco": { "command": "/path/to/geco", "args": ["serve"] }
}
```

## Skills on demand

Agent skills have the same context disease as tools: every skill's name and description ride in the system prompt of every session (measured live: 36 skills ≈ 2,300 tokens, used or not). Point the gateway at a skill library instead:

```yaml
skills:
  dirs: ["~/.config/opencode/skills-library"]
```

Three meta-tools appear: `list_skills` (the whole catalogue), `search_skills` (find a playbook by intent) and `read_skill` (full SKILL.md + bundled file paths — the agent runs scripts via its shell exactly as a native skill would instruct). Migration: move skills out of the auto-loaded dir into the library; keep your few daily drivers native if you like:

```bash
mkdir -p ~/.config/opencode/skills-library
mv ~/.config/opencode/skills/* ~/.config/opencode/skills-library/
```

Add one line to your global AGENTS.md so models reach for it: *"Before specialized work, search_skills → read_skill and follow the playbook."*

## Config reference

**Format:** YAML is the standard (`config.yaml`); JSON works identically for paste-compat with agent configs. A JSON Schema — generated from the binary's own structs, so it can't drift — powers editor validation and autocomplete: `geco schema > ~/.config/geco/geco.schema.json`, referenced from the YAML via the `# yaml-language-server: $schema=…` header (see the example file).

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
geco serve [--refresh|--no-refresh]   # run the proxy (default command)
geco refresh                          # rebuild the snapshot and exit
geco inspect                          # show the cached snapshot
geco schema                           # print the config JSON Schema (YAML + JSON)
```

`RUST_LOG=debug` for verbose logging (always stderr — stdout is the protocol).

## Hot reload

The gateway watches its config file and auto-refreshes when it changes — add a server, edit a denylist, update a description, save, and the agent sees it within seconds. No restart, no manual `refresh_tools` call. Removed backends are shut down; new ones are snapshotted and available immediately.

## Self-healing backends

Warm backends that crash silently (child process exits) are detected by a background health check every 30 seconds (configurable via `health_check_secs`, 0 to disable). The dead connection is reset so the next `execute_tool` reconnects fresh instead of erroring — the agent never sees a stale failure.

## Health — failures are loud, not logged

The gateway is headless, so a dead backend or a partial refresh would otherwise live only in a stderr log. Instead it writes its current problem set to `<data-dir>/health.json` (level-triggered: empty = healthy, tagged with the MCP client name). Two ways to see it:

- **The `health` meta-tool** — ask the agent "is the gateway healthy?" and it returns the failing backends and reasons.
- **The opencode toast plugin** (`opencode-plugin/geco-tui.ts`, installed automatically when opencode is present) — polls `health.json` and raises an in-app toast when a problem appears or clears. A persistent problem toasts once, not every tick.

## Which servers belong behind the proxy?

Servers you use *sometimes*. A server whose tools you want the model to reach for proactively on every task (e.g. a memory store like [icm-slim](https://github.com/rtk-ai/icm)) can stay directly connected — visibility is the point there. Everything else is a candidate.

## License

MIT. Design derived from lazy-mcp (© 2025 tbxark, MIT); this implementation © 2026 Bernd Salomon.
