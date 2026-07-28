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
#
# Caveat: this is the most recently CREATED session, which is normally the one
# calling geco but is not guaranteed to be — with two windows open, or once
# this script's own `opencode run` has written a row, the newest session may be
# a different one. The consequence is a picker on a neighbouring session's
# model, not a broken pick, so it is not worth a heavier correlation scheme.
discover_opencode() {
    local db="${XDG_DATA_HOME:-$HOME/.local/share}/opencode/opencode.db"
    if ! command -v sqlite3 >/dev/null 2>&1; then
        echo "pick.sh: sqlite3 not found — cannot read the opencode session model," \
             "falling back to opencode's default" >&2
        return 1
    fi
    if [[ ! -f "$db" ]]; then
        echo "pick.sh: no opencode database at $db — falling back to its default model" >&2
        return 1
    fi
    # Say nothing on success; only the failures above are worth a line. Without
    # them a missing sqlite3 silently swapped the session's model for whatever
    # opencode defaults to, and the picks just quietly got worse.
    sqlite3 "$db" \
        "SELECT json_extract(model, '\$.providerID') || '/' || json_extract(model, '\$.id') \
         FROM session ORDER BY time_created DESC LIMIT 1;"
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
        # Unknown harness: try opencode, the most common one. If that is not
        # installed either, say so and exit non-zero. Printing an empty line
        # and exiting 0 looked to geco like a model that answered with nothing,
        # so it advised the user to check that their model is not in reasoning
        # mode — for a harness that had no runner at all. A non-zero exit
        # routes through the branch that logs this script's own stderr.
        if ! command -v opencode >/dev/null 2>&1; then
            echo "pick.sh: no runner for GECO_CLIENT='${CLIENT:-<unset>}' and opencode is" \
                 "not installed — add a discover_/run_ branch for your harness" >&2
            exit 1
        fi
        MODEL=$(discover_opencode || true)
        run_opencode "$MODEL" "$PROMPT"
        ;;
esac