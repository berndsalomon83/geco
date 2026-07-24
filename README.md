<img align="right" src="geco_logo.png" alt="geco" width="35%" style="margin-left: 24px;">

# geco

*the <span style="color: #B7410E; font-weight: bold;">ge</span>neric <span style="color: #B7410E; font-weight: bold;">co</span>nnector*

## Why it exists
The more mcp tools you expose to your llm, the more the context gets prefilled with ballast that most likely is not even used and reduces the available space you have to work with.
The same is true for skills. Skills are great recipies to have but they also land as descriptive text in the context of your llm and yet again, more skills means lesser remaining context for the actual task to be done.

So here comes this rusty reptile geco that helps you to tackle this. It sits between your IDE and the skills and mcp tools that you have and gives your llm the same 8 tools always, no matter how much you have in your tool shed!

<span style="color: #B7410E; font-weight: bold;">geco is an MCP and SKILL gateway </span> that keeps your servers' tool schemas **out of the model's context until the moment they're needed**. Put N servers behind it; the agent sees eight small meta-tools instead of every schema on every request.

Built with Rust as single static binary, no runtime dependencies. macOS, Linux, and Windows. Design inspired by [lazy-mcp](https://github.com/voicetreelab/lazy-mcp) (MIT) — reimplemented from scratch on the official [rmcp](https://crates.io/crates/rmcp) SDK, with search, self-refreshing snapshots, allowlists, and status built in.

## How it works

The agent sees exactly eight meta-tools:

| Meta-tool | Purpose |
|---|---|
| `list_mcp_servers` | Name, description, tool count, running state of each backend |
| `list_tools` | A server's tools (name + description); with `tool`: the full input schema |
| `refresh_tools` | Re-scan backends after installing/reconfiguring one |
| `list_skills` | Every skill in the library (name + one-line description) |
| `read_skill` | Load a skill's SKILL.md + file inventory (runs bundled scripts via your shell) |
| `search` | Find tools AND skills by keywords in one call; `include_schema=true` for inline schemas; `kind="tools"` or `"skills"` to filter |
| `execute` | Run 1–10 tool calls in one round-trip; cross-backend calls run in parallel; results in input order |
| `health` | Report which backends failed their last refresh and why |


Tool catalogues come from a **snapshot** cached on disk. The proxy refreshes it automatically at startup when your config changes (hash-based), or on demand — backends stay cold the rest of the time. A backend process starts only when `execute` first touches it, and is reused afterwards.

## Install

### Build & install (macOS / Linux)

```bash
git clone <repo> && cd geco
bash install.sh
```

### Build & install (Windows)

```powershell
git clone <repo>; cd geco
.\install.ps1
```

### Wire it into your agent

Move the MCP servers you want behind the proxy into `~/.config/geco/config.yaml` (copy-paste from your existing agent config — the `mcpServers` format is identical). Then register the **proxy** instead of the individual servers:

**OpenCode** (`~/.config/opencode/opencode.json`):
```jsonc
{
  "mcp": {
    "geco": { "type": "local", "command": ["geco", "serve"], "enabled": true }
  }
}
```

**Claude Desktop** (`claude_desktop_config.json`):
```json
{
  "mcpServers": {
    "geco": { "command": "/path/to/geco", "args": ["serve"] }
  }
}
```

Remove the individual server entries you moved behind geco — restart the agent and it sees only the 8 meta-tools. See [INSTALL.md](INSTALL.md) for the full guide.

## Quick start

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

## Skills on demand

Agent skills have the same context disease as tools: every skill's name and description ride in the system prompt of every session (measured live: 36 skills ≈ 2,300 tokens, used or not). Point the gateway at a skill library instead:

```yaml
skills:
  dirs: ["~/.config/opencode/skills-library"]
```

Three meta-tools appear: `list_skills` (the whole catalogue), `search` (with `kind="skills"` to find a playbook by intent) and `read_skill` (full SKILL.md + bundled file paths — the agent runs scripts via its shell exactly as a native skill would instruct). Migration: move skills out of the auto-loaded dir into the library; keep your few daily drivers native if you like:

```bash
mkdir -p ~/.config/opencode/skills-library
mv ~/.config/opencode/skills/* ~/.config/opencode/skills-library/
```

Add one line to your global AGENTS.md so models reach for it: *Before specialized work, search (kind="skills") → read_skill and follow the playbook.*

## Config reference

**Format:** YAML is the standard (`config.yaml`); JSON works identically for paste-compat with agent configs. A JSON Schema — generated from the binary's own structs, so it can't drift — powers editor validation and autocomplete: `geco schema > ~/.config/geco/geco.schema.json`, referenced from the YAML via the `# yaml-language-server: $schema=…` header (see the example file).

Per server: `command` (string **or** array), `args`, `env` (with `${VAR}` expansion), `pass_env` (hand this server a variable from geco's own environment), `description` (shown in `list_mcp_servers` — write a good one, it's what the agent decides by), `allow` / `deny` (glob patterns, deny wins; enforced at listing **and** execution), `enabled`.

### What geco proxies (and what it doesn't)

geco proxies **tools**. Backend *resources* and *prompts* are not exposed
through the gateway, and because geco terminates the client side of each
backend session, a backend cannot use sampling or elicitation through it. If
a server's value is mostly resources or prompts, wire it up directly rather
than behind geco.

Tool lists stay current on their own: geco listens for
`notifications/tools/list_changed` and re-scans that one backend when it
fires, so a server that adds or removes tools at runtime is picked up without
a `refresh_tools` call. Servers that don't send the notification are still
snapshot-based — refresh them manually or let a config change do it.

### Backend environment isolation

Since 0.7, a backend does **not** inherit geco's environment. Each child starts clean and receives only an OS/runtime baseline — `PATH`, `HOME`/`USERPROFILE`, temp dirs, locale, TLS trust (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, …) and proxy settings (`HTTP_PROXY` and friends) — plus whatever its own `env` and `pass_env` declare.

The point: a `GITHUB_TOKEN` exported in your shell for one server is no longer handed to every other server you run. Aggregating many backends behind one gateway means one compromised or careless server otherwise sees every secret in the environment.

If a server needs an ambient variable, name it: `pass_env: ["GITHUB_TOKEN"]`. For non-secret tool configuration wanted by *every* backend, use `proxy.env_passthrough: ["NODE_*"]` (glob patterns). To restore the old behaviour wholesale, `proxy.inherit_env: true` — but then any server can read anything.

`proxy` section: `call_timeout_secs` (default 120), `startup_timeout_secs` (30), `refresh_on_start` (true), `health_check_secs` (30; 0 disables dead-backend detection).

### Secrets

Don't put tokens in the config or your shell rc. `${keychain:SERVICE}` resolves from the OS keychain **at backend spawn time** — the secret exists only in that child process's environment:

```jsonc
"env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "${keychain:github_token}" }
```

Store it once:
- **macOS:** `security add-generic-password -a "$USER" -s github_token -w '<token>'`
- **Linux:** `secret-tool store --label='geco' service github_token` (libsecret / `secret-tool`)
- **Windows:** `cmdkey /generic:github_token /user:geco /pass:<token>` (Credential Manager via PowerShell CredReadW)

A missing keychain item fails the spawn loudly — plain `${VAR}` env references still expand to "" like shell. There is deliberately no `${cmd:...}`: arbitrary command execution from a config file is an injection surface.

## CLI

```bash
geco serve [--refresh|--no-refresh]   # run the proxy (default command)
geco refresh                          # rebuild the snapshot and exit
geco inspect                          # show the cached snapshot
geco stats                            # show runtime metrics (calls, errors, latency)
geco validate                         # check config for common mistakes
geco schema                           # print the config JSON Schema (YAML + JSON)
```

`RUST_LOG=debug` for verbose logging (always stderr — stdout is the protocol).

## Hot reload

The gateway watches its config file and auto-refreshes when it changes — add a server, edit a denylist, update a description, save, and the agent sees it within seconds. No restart, no manual `refresh_tools` call. Removed backends are shut down; new ones are snapshotted and available immediately.

## Self-healing backends

Warm backends that crash silently (child process exits) are detected by a background health check every 30 seconds (configurable via `health_check_secs`, 0 to disable). The dead connection is reset so the next `execute` reconnects fresh instead of erroring — the agent never sees a stale failure.

## Health — failures are loud, not logged

The gateway is headless, so a dead backend or a partial refresh would otherwise live only in a stderr log. Instead it writes its current problem set to `<data-dir>/health.json` (level-triggered: empty = healthy, tagged with the MCP client name). Two ways to see it:

- **The `health` meta-tool** — ask the agent "is the gateway healthy?" and it returns the failing backends and reasons.
- **The opencode toast plugin** (`opencode-plugin/geco-tui.ts`, installed automatically when opencode is present) — polls `health.json` and raises an in-app toast when a problem appears or clears. A persistent problem toasts once, not every tick.

## Which servers belong behind the proxy?

Servers you use *sometimes*. A server whose tools you want the model to reach for proactively on every task (e.g. a memory store like [icm-slim](https://github.com/rtk-ai/icm)) can stay directly connected — visibility is the point there. Everything else is a candidate.

## License

MIT. Design derived from lazy-mcp (© 2025 tbxark, MIT); this implementation © 2026 Bernd Salomon.
