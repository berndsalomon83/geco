#!/usr/bin/env bash
# geco uninstaller. --purge also deletes config + snapshot cache.
set -euo pipefail
say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*"; }

PURGE=0; [ "${1:-}" = "--purge" ] && PURGE=1

# Remove geco entries from agent configs (backup first).
python3 <<'PYEOF'
import json, os, shutil
home = os.path.expanduser("~")
for p in [os.path.join(home, ".config/opencode/opencode.json"),
          os.path.join(home, "Library/Application Support/Claude/claude_desktop_config.json")]:
    if not os.path.exists(p): continue
    try:
        cfg = json.load(open(p))
    except Exception:
        print(f"    skip (unparseable): {p}"); continue
    changed = False
    for key in ("mcp", "mcpServers"):
        if isinstance(cfg.get(key), dict):
            if "geco" in cfg[key]:
                del cfg[key]["geco"]; changed = True
    if changed:
        shutil.copy(p, p + ".bak")
        with open(p, "w") as f: json.dump(cfg, f, indent=2); f.write("\n")
        print(f"    removed geco entry: {p}")
PYEOF

BIN="$(command -v geco 2>/dev/null || true)"
if [ -n "$BIN" ]; then rm -f "$BIN" && say "Removed binary: $BIN"; fi

if [ "$PURGE" = 1 ]; then
    rm -rf "$HOME/.config/geco" \
           "$HOME/.cache/dev.geco.geco" \
           "$HOME/.local/share/dev.geco.geco" \
           "$HOME/Library/Caches/dev.geco.geco" \
           "$HOME/Library/Application Support/dev.geco.geco" 2>/dev/null || true
    say "Purged config + snapshot cache + health data"
else
    say "Config kept at ~/.config/geco (re-run with --purge to delete)"
fi

# Remove the opencode health plugin (installed by install.sh).
if [ -f "$HOME/.config/opencode/plugin/geco-tui.ts" ]; then
    rm -f "$HOME/.config/opencode/plugin/geco-tui.ts"
    say "Removed opencode health plugin"
fi
say "Done. Restart your agents; re-add any servers you still want direct."
