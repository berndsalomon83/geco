# Porting geco to a new machine

This bundle carries the full repo: all branches, tags, and history. The new
machine gets the same code, verified green, with a path to a running setup.

## What's in the bundle

- **`geco-port.bundle`** (18 MB) — full git history, all 4 local branches
  (`main`, `feat/embeddings-done-right`, `fix/search-generalization`,
  `fix/search-quality`, `v1.0-dispatch`), tag `v0.7.0`, and all remotes.
- This **`PORTING.md`** — the steps below.

## 1. Unpack the repo

```bash
git clone geco-port.bundle geco      # creates a working clone with all history
cd geco
git fetch origin --prune             # drop the bundle's remote refs (optional)
git remote -v                       # will show origin = the bundle file
```

The bundle's `origin` points at the bundle file path. Repoint it to the
GitHub remote (or keep both — `origin` for the bundle, `github` for upstream):

```bash
git remote set-url origin https://github.com/berndsalomon83/geco.git
git fetch origin                     # pulls the latest if the new box has net
git checkout main && git pull        # lands on the freshly committed main
```

## 2. Install Rust (only if building from source)

The prebuilt binaries need no toolchain. If the new machine has no matching
binary, or you want to build:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# geco pins rust-version = 1.88 (the floor rmcp's macros need); rustup gives you latest stable, which is >= 1.88
```

## 3. Build and verify

```bash
cargo build --release                    # ~90s on a fast box; 8 MB static binary
cargo test --all-targets                 # 172 tests, should all pass
cargo clippy --all-targets               # clean
cargo fmt --check                        # clean (just committed)
bash scripts/lint.sh                     # mirrors CI's lint job (some tools may be missing on a fresh box — it prints which ones it skipped)
```

The binary lands at `target/release/geco`. Install it:

```bash
bash install.sh                          # installs the binary, scaffolds ~/.config/geco/, builds first snapshot
# or, if you already built it:
cp target/release/geco /opt/homebrew/bin/geco   # macOS arm64; adjust path for your platform
```

## 4. Restore your config

Your live config (`~/.config/geco/config.yaml`) and skill library
(`~/.config/opencode/skills-library/`) are **not in the repo** — they live in
your home directory. To port them:

```bash
# On the OLD machine, tarball the config and skills:
tar czf geco-config-port.tar.gz \
  ~/.config/geco/ \
  ~/.config/opencode/skills-library/

# Copy the tarball to the new machine, then:
tar xzf geco-config-port.tar.gz -C /
```

What this carries:
- `~/.config/geco/config.yaml` — your server definitions, picker, skills config
- `~/.config/geco/geco.schema.json` — editor autocomplete (regenerable via `geco schema`)
- `~/.config/geco/picker/pick.sh` + `opencode.json` — the picker wrapper
- `~/.config/opencode/skills-library/` — your skill library (the long tail)

What it does **not** carry (and why):
- **Keychain secrets** — OS keychain entries (e.g. `github_token`) don't tarball.
  Recreate them on the new machine (see README → Secrets).
- **The `~/.config/opencode/skills/` native dir** — should stay empty; geco
  absorbs it into the library on every scan.
- **`~/.config/opencode/opencode.json`** — your agent config; the geco entry
  (`"geco": { "type": "local", "command": ["geco", "serve"], "enabled": true }`)
  is added by `install.sh` if missing.

## 5. Wire it into your agent

In `~/.config/opencode/opencode.json`:

```jsonc
{
  "mcp": {
    "geco": { "type": "local", "command": ["geco", "serve"], "enabled": true }
  }
}
```

Restart opencode. Verify: ask the model to `search("*")` — you should see
your server catalogue, one entry per backend with tool counts.

## 6. First-run checks

```bash
geco health         # what's broken right now (exit 1 on errors)
geco inspect        # the cached snapshot — servers, tool counts, refresh status
geco stats          # runtime metrics (calls, errors, latency, search effectiveness)
geco validate       # config for common mistakes
```

If a backend fails to spawn, the most common cause is a command not on geco's
`PATH` (version managers like nvm/fnm only edit shell rc, not the GUI-launched
PATH). Use an absolute path in `command`, or launch the agent from a shell
where it works. See README → Troubleshooting.

## Debug sweep result (2026-08-05, commit bad5a92)

- **172 tests pass, 0 failures, 0 ignored**
- **clippy clean** on all targets
- **cargo fmt clean** (fixed drift in main.rs, picker.rs, server.rs this commit)
- **release build**: 8.0 MB Mach-O arm64 static binary
- **No code smells**: all `unwrap`/`expect` confined to `#[cfg(test)]`; no
  `unsafe`, no `todo!`/`unimplemented!`, no locks held across `.await`, no
  blocking calls in async context, no `TODO`/`FIXME`/`HACK` comments in `src/`.
- **Open item**: the opencode TUI toast plugin (`opencode-plugin/geco-tui.ts`)
  is parked — the plugin loads but hooks never fire in current opencode. The
  health system itself (`health.json`) works perfectly; only the in-app toast
  is affected. Workaround: `geco health` and `geco stats` on the CLI. See
  `opencode-plugin/TODO.md`.

## What to read first on the new machine

1. **README.md → "The lean way"** — the 5-line whole loop.
2. **README.md → "How it works"** — the three meta-tools and what they do.
3. **`docs/HANDOVER.md`** — state of the repo, what's measured, what's next,
   and the traps that already cost time. Read this before touching search
   ranking — the rejection table in `docs/search-ranking.md` exists so dead
   ends stay dead.
4. **CHANGELOG.md** — every fix and addition, with the reasoning.

## Environment notes

- The release binary is **arm64** (built on this Mac). The new machine, if
  also arm64 macOS, can use it directly. For x86_64 or Linux, either download
  a prebuilt binary via `install.sh`, or `cargo build --release` from source.
- `scripts/lint.sh` will report 6 tools as NOT CHECKED on a fresh machine
  (shellcheck, actionlint, typos, gitleaks, PSScriptAnalyzer, pwsh) — install
  them with Homebrew (`brew install shellcheck actionlint typos gitleaks`)
  to match CI locally. PSScriptAnalyzer needs PowerShell, macOS-only CI
  handles it.