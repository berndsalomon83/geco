#!/usr/bin/env bash
# geco installer
#   1. cargo build --release
#   2. install the binary (existing install > /opt/homebrew/bin > ~/.local/bin)
#      — remove-then-copy, never overwrite in place (macOS signature cache)
#   3. scaffold ~/.config/geco/config.yaml if missing
#   4. build the first tool snapshot + verify the meta-tool surface
set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CFG_DIR="$HOME/.config/geco"
CFG="$CFG_DIR/config.yaml"
CFG_LEGACY="$CFG_DIR/config.json"

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31mERROR\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] && die "do not run with sudo"
command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust first: https://rustup.rs"
[ -f "$REPO_DIR/Cargo.toml" ] || die "run this from the geco checkout"

say "Building geco (release)…"
( cd "$REPO_DIR" && cargo build --release )
NEW_BIN="$REPO_DIR/target/release/geco"

OLD_BIN="$(command -v geco 2>/dev/null || true)"
if [ -n "$OLD_BIN" ] && [ "$OLD_BIN" != "$NEW_BIN" ]; then
    DEST="$OLD_BIN"
elif [ -d /opt/homebrew/bin ] && [ -w /opt/homebrew/bin ]; then
    DEST="/opt/homebrew/bin/geco"
else
    DEST="$HOME/.local/bin/geco"
    mkdir -p "$HOME/.local/bin"
    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) warn "~/.local/bin is not on PATH — add to ~/.zshrc:  export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
    esac
fi

say "Installing to $DEST"
rm -f "$DEST" 2>/dev/null || die "cannot remove existing $DEST — run: sudo rm -f '$DEST' and retry"
cp "$NEW_BIN" "$DEST"
if ! "$DEST" --version >/dev/null 2>&1; then
    if [ "$(uname)" = "Darwin" ] && command -v codesign >/dev/null 2>&1; then
        warn "binary failed to run — re-signing ad-hoc"
        codesign -s - -f "$DEST" 2>/dev/null || true
    fi
fi
"$DEST" --version >/dev/null 2>&1 || die "installed binary does not run"
say "Installed: $("$DEST" --version)"

if [ -f "$CFG_LEGACY" ] && [ ! -f "$CFG" ]; then
    CFG="$CFG_LEGACY"
    say "Using existing JSON config: $CFG (YAML is the standard now; migrate when convenient)"
elif [ ! -f "$CFG" ]; then
    mkdir -p "$CFG_DIR"
    cp "$REPO_DIR/geco.example.yaml" "$CFG"
    "$NEW_BIN" schema > "$CFG_DIR/geco.schema.json" 2>/dev/null || true
    say "Scaffolded config: $CFG (+ editor schema)  ← EDIT THIS: move your occasional MCP servers in"
else
    say "Config exists: $CFG"
    "$NEW_BIN" schema > "$CFG_DIR/geco.schema.json" 2>/dev/null || true
fi

say "Building tool snapshot…"
"$DEST" --config "$CFG" refresh || warn "refresh reported issues (fix config, then: geco refresh)"

say "Verifying meta-tool surface…"
TOOLS=$(printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"install","version":"0"}}}\n{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}\n' \
  | "$DEST" --config "$CFG" serve --no-refresh 2>/dev/null \
  | python3 -c 'import sys,json
for l in sys.stdin:
    m=json.loads(l)
    if m.get("id")==2: print(len(m["result"]["tools"]))' || echo 0)
# 8 meta-tools: 4 gateway + 3 skills + health
[ "$TOOLS" = "8" ] && say "Proxy OK: 8 meta-tools (4 gateway + 3 skills + health)" || warn "expected 8 meta-tools, got: $TOOLS"

# Companion opencode TUI plugin: toasts gateway health problems in-app.
OC_PLUGIN_DIR="$HOME/.config/opencode/plugin"
if [ -d "$HOME/.config/opencode" ]; then
    mkdir -p "$OC_PLUGIN_DIR"
    cp "$REPO_DIR/opencode-plugin/geco-tui.ts" "$OC_PLUGIN_DIR/geco-tui.ts"
    say "Installed opencode health plugin: $OC_PLUGIN_DIR/geco-tui.ts (restart opencode)"
fi

cat <<EOT

Done. Wire it into your agent (and REMOVE the servers you moved behind it):

  OpenCode (~/.config/opencode/opencode.json):
    "mcp": { "geco": { "type": "local", "command": ["$DEST", "serve"], "enabled": true } }

  Claude Desktop (claude_desktop_config.json):
    "mcpServers": { "geco": { "command": "$DEST", "args": ["serve"] } }

Uninstall:  bash $REPO_DIR/uninstall.sh [--purge]
EOT
