# geco — Install, Set up, Verify, Uninstall

## Requirements

**No Rust toolchain needed** for the platforms with published binaries: macOS (arm64, x86_64), Linux (x86_64 static/musl, aarch64), Windows (x86_64). Anything else builds from source and needs rustup.rs.

On macOS/Linux: `curl`, and `python3` for the final self-check (skipped with a note when absent). Backends need whatever they always needed (`npx`, `uvx`, binaries…) — the proxy just spawns them later instead of your agent spawning them at startup.

## Install

### macOS / Linux

```bash
curl -fsSL https://raw.githubusercontent.com/berndsalomon83/geco/main/install.sh | bash
```

Downloads the prebuilt binary for your platform, verifies its SHA-256 against the release's `SHA256SUMS`, and installs it (existing location → `/opt/homebrew/bin` → `~/.local/bin`; remove-then-copy so macOS signing stays valid). Then scaffolds `~/.config/geco/config.yaml` from the example if missing, builds the first tool snapshot, and verifies the proxy answers with the gateway meta-tools.

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/berndsalomon83/geco/main/install.ps1 | iex
```

Same flow, installing to `~/.local/bin/geco.exe` and adding it to your user PATH (new terminals pick it up). Piped like this, no script file runs, so the execution policy does not get in the way.

### From a clone, or another platform

```bash
git clone https://github.com/berndsalomon83/geco.git && cd geco
bash install.sh --from-source
```

On Windows: `powershell -ExecutionPolicy Bypass -File install.ps1 -FromSource`. Without the flag, a clone install still prefers the prebuilt binary and falls back to building only when none matches your platform or the download fails. Windows on ARM64 always builds from source — the published Windows binary is x86_64 and would otherwise run under emulation.

The same scripts ship inside every release archive: extract one and run `bash install.sh` (or the `.ps1`) there to install that archive's binary without downloading anything.

### Notes on downloads

- **No account needed.** Release assets are public, so plain `curl` works. If `gh` is installed and logged in, the installers use it instead, which avoids GitHub's anonymous API rate limit.
- **macOS Gatekeeper.** The binaries are unsigned, so a downloaded geco may be quarantined. If macOS refuses to run it: `xattr -d com.apple.quarantine "$(command -v geco)"`. The installer already re-signs ad-hoc as a fallback.
- **Verifying by hand:** every release carries `SHA256SUMS`; `sha256sum -c SHA256SUMS` in the download directory checks all of them.

## Move servers behind the proxy

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
  "geco": { "type": "local", "command": ["geco", "serve"], "enabled": true }
}
```

`~/.config/geco/config.yaml`:
```yaml
mcpServers:
  github:
    command: [github-mcp-server, stdio]
    env:
      GITHUB_PERSONAL_ACCESS_TOKEN: "${keychain:github_token}"
    description: "GitHub: issues, PRs, repos"
  playwright:
    command: [npx, "@playwright/mcp@latest"]
    description: "Browser automation"
```

(JSON configs keep working — handy when copy-pasting entries straight from `opencode.json`. The installer drops an editor schema next to the config; regenerate anytime with `geco schema`.)

The entries are the same shape — copy, paste, add a `description` (that's what the agent sees when deciding). Then:

```bash
geco refresh     # snapshot the moved servers once
```

Restart the agent. Context cost for those servers drops to ~2.5 KB flat.

> **Moving a server that reads a token from your shell? Read this.**
> A backend does **not** inherit geco's environment. A server that worked
> directly because `GITHUB_TOKEN` was exported in your shell will start
> without it once it sits behind geco. Nothing fails at startup — it fails at
> the first `execute`, with whatever opaque auth error the backend produces.
>
> Name what it needs:
> ```yaml
> github:
>   command: [github-mcp-server, stdio]
>   pass_env: ["GITHUB_TOKEN"]        # hand THIS server that one variable
> ```
> Better still, take it out of the shell entirely with
> `${keychain:github_token}` as above. `proxy.inherit_env: true` hands every
> backend geco's whole environment, but then every server can read every secret you
> have — see the README for why that is the thing being fixed.

**Rule of thumb:** keep proactive-use servers (memory, core workflow) direct; gate everything you use occasionally.

## Verify

```bash
geco inspect                  # snapshot: servers, tool counts, refresh status
RUST_LOG=debug geco serve     # verbose logs on stderr (Ctrl-C to stop)
```

In the agent: ask it to `search` for something you know is behind the proxy, then execute it. First execution on a cold backend takes the backend's startup time (npx downloads on very first use); after that it's warm.

## Config knobs

The full reference lives in **[README → Config reference](README.md#config-reference)** — every `proxy` key with its default, the per-server keys, environment isolation and secrets. It is deliberately not repeated here: this file had a copy that fell three keys behind the code, which is what a second copy always does eventually.

The two you are most likely to want while installing:

- `proxy.startup_timeout_secs` — raise it if a first `npx`/`uvx` run downloads a large package.
- `pass_env` on a server — the thing to reach for when a backend you moved behind geco can't authenticate (see the callout above).

## Uninstall

### macOS / Linux

```bash
curl -fsSL https://raw.githubusercontent.com/berndsalomon83/geco/main/uninstall.sh | bash                 # binary + geco entries in agent configs (.bak backups)
curl -fsSL https://raw.githubusercontent.com/berndsalomon83/geco/main/uninstall.sh | bash -s -- --purge   # …and config + snapshot cache
```

### Windows

```powershell
irm https://raw.githubusercontent.com/berndsalomon83/geco/main/uninstall.ps1 | iex                                   # binary + geco entries in agent configs
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/berndsalomon83/geco/main/uninstall.ps1))) -Purge   # …and config + cache
```

From a clone, `bash uninstall.sh [--purge]` and `powershell -ExecutionPolicy Bypass -File uninstall.ps1 [-Purge]` do the same.

Re-add any servers you still want directly to your agent config afterwards.

## Troubleshooting

Install-specific issues:

- **Backend shows `error:` in `inspect`** — run the backend's command by hand; usually a missing binary, or a variable it does not inherit (see the callout under *Move servers behind the proxy*). Fix, then `geco refresh`.
- **First `execute` times out** — cold `npx`/`uvx` fetch. Raise `startup_timeout_secs`, or pre-warm with `geco refresh`.
- **"no prebuilt binary for this machine"** — the installer couldn't fetch a release asset: no network, no prebuilt for your platform, or GitHub's anonymous API rate limit (`gh auth login` avoids that). Build from a clone instead (see *From a clone, or another platform*).

Everything else — PATH differences under a GUI-launched agent, keychain misses, "the agent can't find a tool", stale tool lists, macOS quarantine — is in **[README → Troubleshooting](README.md#troubleshooting)**.

---

## Skills on demand

Move your skill library behind the gateway (zero session cost until used):

```bash
mkdir -p ~/.config/opencode/skills-library
mv ~/.config/opencode/skills/* ~/.config/opencode/skills-library/
```

`~/.config/geco/config.yaml`:

```yaml
skills:
  dirs: ["~/.config/opencode/skills-library"]
```

(`geco refresh` sweeps any configured `skills.absorb` dirs into the library before it rebuilds the snapshot — it moves files, not only cache entries.)

Restart the agent. `search("*", kind="skills")` lists the whole catalogue (`list_skills` too, under `proxy.surface: full`); `search` with `kind="skills"` finds playbooks by intent; `read_skill` loads the full SKILL.md + bundled file inventory. `geco refresh` re-scans the library (the gateway also watches it and picks up changes on its own). Keep a handful of daily-driver skills in the native dir if you want opencode's built-in skill UX for them — the gateway serves the long tail.

### The absorb model (nothing is ever native)

The library is the single home for every skill, and the gateway enforces
it: at startup and on every refresh, any skill found in the
configured `absorb` dirs is MOVED into the library. Wherever a model, an
installer, or a playbook drops a skill, it gets swept behind the gate —
the native auto-loaded dir stays empty forever, so no skill ever costs
always-on context:

```yaml
skills:
  dirs: ["~/.config/opencode/skills-library"]
  absorb: ["~/.config/opencode/skills"]
```

Name collisions are skipped and logged for the human; non-skill files are
left untouched. Agents are additionally instructed (AGENTS.md) to create
skills in the library directly; the gateway's watcher picks them up, or run `geco refresh`.

---

## Health toasts in opencode

The gateway runs headless — when a backend endpoint is down or a refresh half-fails, that pain would otherwise sit in an unread log. `install.sh` drops a companion TUI plugin at `~/.config/opencode/plugins/geco-tui.ts` that polls the gateway's `health.json` and raises an opencode toast when a problem appears (error/warning) or clears (success). Restart opencode to load it. *Currently parked:* recent opencode versions load the plugin but never fire its hooks, so no toast appears — see `opencode-plugin/TODO.md`. From a shell, `geco health` reports the failing backends and why, and exits 1 when something is wrong. Asking the model works too: every `search` response carries an `issues` key whenever the gateway has any (and so does `list_mcp_servers`, under `proxy.surface: full`). (There is deliberately no `health` meta-tool: it would cost every session's context to answer a question only ever asked once something is already broken.)
