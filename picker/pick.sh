#!/bin/bash
# geco picker wrapper — harness-agnostic model discovery for the LLM picker.
#
# geco sets GECO_CLIENT to the MCP client name from the initialize handshake
# (e.g. "opencode", "claude-ai", "cursor"). This script branches on it to
# discover the current model the harness-specific way, then calls a harness
# runner with that model so the picker always uses the same model as the
# calling session.
#
# Shipped with geco and scaffolded into ~/.config/geco/picker/ by install.sh.
# Edit it freely — geco only cares that the command it runs prints the
# model's reply to stdout. The {prompt} placeholder is substituted by geco.
#
# Adding a new harness: add a function `discover_<client>` that echoes
# "provider/model", and a `run_<client>` that takes the model and prompt
# and prints the model's reply to stdout. Then add them to the case below.

set -euo pipefail

PROMPT=""
PICKER_DIR="$(cd "$(dirname "$0")" && pwd)"
CLIENT="${GECO_CLIENT:-}"

# Extract the prompt argument (geco substitutes {prompt} or appends it)
for arg in "$@"; do
    PROMPT="$arg"
done

# ---------------------------------------------------------------------------
# Model discovery — each harness stores its session model differently.
# ---------------------------------------------------------------------------

# opencode: SQLite DB at ~/.local/share/opencode/opencode.db, session.model
# column (JSON: {"id":"...","providerID":"..."}). Latest session = current.
discover_opencode() {
    local db="$HOME/.local/share/opencode/opencode.db"
    [[ -f "$db" ]] || return 1
    sqlite3 "$db" \
        "SELECT json_extract(model, '\$.providerID') || '/' || json_extract(model, '\$.id') \
         FROM session ORDER BY time_created DESC LIMIT 1;" 2>/dev/null
}

# Claude Desktop / claude-ai: add discovery here when it stores a model.
# discover_claude() { ... }

# Generic fallback: no discovery, let the runner use its default model.
discover_unknown() { return 1; }

# ---------------------------------------------------------------------------
# Runners — each harness has a different CLI to invoke its model.
# ---------------------------------------------------------------------------

# opencode: `opencode run --dir <picker-config> --agent picker -m <model>`.
# The picker opencode.json (alongside this script) disables all tools,
# stripping the system prompt from ~10k tokens to ~737 so a per-search
# call is cheap. The picker agent is scaffolded into
# ~/.config/opencode/agents/picker.md by install.sh.
run_opencode() {
    local model="$1" prompt="$2"
    if [[ -n "$model" ]]; then
        opencode run --dir "$PICKER_DIR" --agent picker -m "$model" "$prompt"
    else
        opencode run --dir "$PICKER_DIR" --agent picker "$prompt"
    fi
}

# Claude / other harnesses: add runners here.
# run_claude() { ... }

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------

case "$CLIENT" in
    opencode)
        MODEL=$(discover_opencode || true)
        run_opencode "$MODEL" "$PROMPT"
        ;;
    # claude-ai|claude)
    #     MODEL=$(discover_claude || true)
    #     run_claude "$MODEL" "$PROMPT"
    #     ;;
    *)
        # Unknown harness: try opencode as the most common, then fail
        # gracefully — geco falls back to lexical ranking on any error.
        MODEL=$(discover_opencode || true)
        if [[ -n "$MODEL" ]]; then
            run_opencode "$MODEL" "$PROMPT"
        else
            echo ""
        fi
        ;;
esac