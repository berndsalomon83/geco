#!/usr/bin/env bash
# geco uninstaller. --purge also deletes config + snapshot cache.
set -euo pipefail
say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*"; }

PURGE=0; [ "${1:-}" = "--purge" ] && PURGE=1

# Remove geco entries from agent configs (backup first).
#
# Guarded, because `set -e` made this line abort the whole script — before the
# binary was removed, before anything was purged, and without printing a word.
# The user was left with geco fully installed and an uninstaller that reported
# nothing.
#
# Two guards, not one, because "is python3 there?" is the wrong question on the
# machine this is most likely to happen on. macOS ships `/usr/bin/python3` as a
# Command Line Tools STUB: `command -v` finds it, running it prints "no
# developer tools were found" and exits non-zero. A presence check passes and
# `set -e` kills the script anyway. The `|| warn` puts the heredoc in an AND-OR
# list, which `set -e` exempts, so an unwritable config or a raising script is
# survived too.
#
# Editing the agent config is the one step that needs python3 at all;
# everything below is plain shell and runs regardless.
if command -v python3 >/dev/null 2>&1; then
python3 <<'PYEOF' || warn "agent-config cleanup failed — remove the \"geco\" entry by hand"
import json, os, shutil
home = os.path.expanduser("~")
for p in [os.path.join(home, ".config/opencode/opencode.json"),
          os.path.join(home, "Library/Application Support/Claude/claude_desktop_config.json")]:
    if not os.path.exists(p): continue
    try:
        cfg = json.load(open(p))
    except Exception:
        print(f"    skip (cannot parse): {p}"); continue
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
else
    warn "python3 not found — leaving agent configs alone."
    warn "Remove the \"geco\" entry from your agent's mcpServers block by hand."
fi

BIN="$(command -v geco 2>/dev/null || true)"
if [ -n "$BIN" ]; then rm -f "$BIN" && say "Removed binary: $BIN"; fi

if [ "$PURGE" = 1 ]; then
    # geco resolves these through the `directories` crate, which uses a
    # DIFFERENT shape per platform: XDG on Linux keys off the bare project name
    # (`~/.cache/geco`), while macOS uses the reverse-DNS bundle id
    # (`~/Library/Caches/dev.geco.geco`). This list only ever had the
    # dev.geco.geco spellings, so on Linux --purge printed its success line and
    # left both the snapshot — a full dump of every backend tool name,
    # description and input schema — and health.json, which carries per-backend
    # call counts and the recent-no-match query log. Reinstalling later then
    # resurrected the stale snapshot, since it is keyed on the config path.
    # Both shapes are listed; the ones that do not exist are no-ops.
    # Through the same variables the `directories` crate reads, not just their
    # defaults: home-manager, Nix and several distros set XDG_CACHE_HOME, and
    # hardcoding ~/.cache would print the success line while leaving the
    # snapshot exactly where a reinstall will find it again.
    rm -rf "$HOME/.config/geco" \
           "${XDG_CONFIG_HOME:-$HOME/.config}/geco" \
           "${XDG_CACHE_HOME:-$HOME/.cache}/geco" \
           "${XDG_DATA_HOME:-$HOME/.local/share}/geco" \
           "$HOME/.cache/geco" \
           "$HOME/.local/share/geco" \
           "$HOME/.cache/dev.geco.geco" \
           "$HOME/.local/share/dev.geco.geco" \
           "$HOME/Library/Caches/dev.geco.geco" \
           "$HOME/Library/Application Support/dev.geco.geco" 2>/dev/null || true
    say "Purged config + snapshot cache + health data"
else
    say "Config kept at ~/.config/geco (re-run with --purge to delete)"
fi

# Remove the opencode health plugin (installed by install.sh).
for d in "plugins" "plugin"; do
    f="$HOME/.config/opencode/$d/geco-tui.ts"
    if [ -f "$f" ]; then
        rm -f "$f"
        say "Removed opencode health plugin ($d/)"
    fi
done

# The picker agent, also dropped by install.sh into opencode's own tree. It
# lives outside ~/.config/geco, so --purge would never have reached it — an
# uninstalled geco left an agent named "picker" in the user's opencode.
for d in "agents" "agent"; do
    f="$HOME/.config/opencode/$d/picker.md"
    if [ -f "$f" ]; then
        rm -f "$f"
        say "Removed picker agent ($d/)"
    fi
done
say "Done. Restart your agents; re-add any servers you still want direct."
