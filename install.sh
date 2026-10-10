#!/usr/bin/env bash
# geco installer
#
#   curl -fsSL https://raw.githubusercontent.com/berndsalomon83/geco/main/install.sh | bash
#
# or `bash install.sh [--from-source]` from a clone or an extracted release
# archive.
#
#   1. take the binary from the release archive this script sits in, else
#      download a prebuilt (or --from-source / fall back to cargo build)
#   2. install the binary (existing install > /opt/homebrew/bin > ~/.local/bin)
#      — remove-then-copy, never overwrite in place (macOS signature cache)
#   3. scaffold ~/.config/geco/config.yaml if missing
#   4. build the first tool snapshot + verify the meta-tool surface
set -euo pipefail

# Where this script lives — empty when it is piped from curl, which has no
# file and no directory beside it. `:-` because bash leaves BASH_SOURCE empty
# for a script read from stdin, and `set -u` would abort on the bare
# expansion before the first line of output.
SCRIPT_DIR=""
if [ -n "${BASH_SOURCE[0]:-}" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi
GH_REPO="berndsalomon83/geco"
RAW_URL="https://raw.githubusercontent.com/$GH_REPO/main"
CFG_DIR="$HOME/.config/geco"
CFG="$CFG_DIR/config.yaml"
CFG_JSON="$CFG_DIR/config.json"
# Downloads land here and are removed on exit, whatever the outcome.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31mERROR\033[0m %s\n' "$*" >&2; exit 1; }

# Parse arguments before any environment checks, so --help always works.
FROM_SOURCE=0
for arg in "$@"; do
    case "$arg" in
        --from-source) FROM_SOURCE=1 ;;
        -h|--help)
            printf 'usage: %s [--from-source]\n\n' "$0"
            printf '  --from-source   build with cargo instead of downloading a release\n'
            exit 0 ;;
        *) die "unknown option: $arg (try --help)" ;;
    esac
done

[ "$(id -u)" -eq 0 ] && die "do not run with sudo"

# Map this machine to a release target triple. Empty = no prebuilt for it.
detect_target() {
    case "$(uname -s)" in
        Darwin) case "$(uname -m)" in
                    arm64)  echo "aarch64-apple-darwin" ;;
                    x86_64) echo "x86_64-apple-darwin" ;;
                esac ;;
        Linux)  case "$(uname -m)" in
                    x86_64)          echo "x86_64-unknown-linux-musl" ;;
                    aarch64|arm64)   echo "aarch64-unknown-linux-gnu" ;;
                esac ;;
    esac
}

# SHA-256 of a file. macOS ships `shasum`, not `sha256sum`; checking only for
# the latter skipped verification on every Mac without a word.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

# Print the path to an extracted prebuilt binary, or nothing. Returns 1 when
# there is no usable prebuilt (the caller falls back to a source build) and 2
# on a checksum mismatch, which must stop the install rather than fall back.
# It runs inside $(…), so it reports by return code and the caller decides
# what is fatal.
fetch_prebuilt() {
    target="$(detect_target)"
    [ -n "$target" ] || return 1
    tmp="$WORK"

    # `gh` first when it is logged in: authenticated, so it is not subject to
    # GitHub's anonymous API rate limit. Plain curl works for everyone else.
    if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
        gh release download --repo "$GH_REPO" --pattern "*${target}*" \
            --pattern "SHA256SUMS" --dir "$tmp" >/dev/null 2>&1 || return 1
    else
        api="https://api.github.com/repos/$GH_REPO/releases/latest"
        url="$(curl -fsSL "$api" 2>/dev/null \
               | grep -o "https://[^\"]*${target}[^\"]*\.tar\.gz" | head -1)" || return 1
        [ -n "$url" ] || return 1
        curl -fsSL -o "$tmp/$(basename "$url")" "$url" || return 1
        curl -fsSL -o "$tmp/SHA256SUMS" "${url%/*}/SHA256SUMS" >/dev/null 2>&1 || true
    fi

    # Glob rather than parsing `ls`, which mangles unusual filenames.
    archive=""
    for f in "$tmp"/*.tar.gz; do
        if [ -f "$f" ]; then archive="$f"; break; fi
    done
    [ -n "$archive" ] || return 1

    # Verify if we got sums; a missing SHA256SUMS is not fatal, a mismatch is.
    if [ -s "$tmp/SHA256SUMS" ]; then
        want="$(grep "$(basename "$archive")" "$tmp/SHA256SUMS" | awk '{print $1}' | head -1)"
        got="$(sha256_of "$archive")"
        if [ -n "$want" ] && [ -n "$got" ] && [ "$want" != "$got" ]; then
            printf '\033[1;31mERROR\033[0m checksum mismatch on %s\n' "$(basename "$archive")" >&2
            return 2
        fi
        [ -n "$got" ] || warn "no sha256sum or shasum — skipping checksum verification" >&2
    fi

    tar xzf "$archive" -C "$tmp" || return 1
    found="$(find "$tmp" -type f -name geco -perm -u+x | head -1)"
    [ -n "$found" ] || return 1
    echo "$found"
}

NEW_BIN=""
if [ "$FROM_SOURCE" -eq 0 ] && [ -n "$SCRIPT_DIR" ] && [ -x "$SCRIPT_DIR/geco" ]; then
    # Run from an extracted release archive: the binary is right here.
    NEW_BIN="$SCRIPT_DIR/geco"
    say "Using the geco binary from this release archive"
elif [ "$FROM_SOURCE" -eq 0 ]; then
    say "Looking for a prebuilt geco…"
    rc=0
    NEW_BIN="$(fetch_prebuilt)" || rc=$?
    [ "$rc" -eq 2 ] && die "refusing to install a download that does not match its published checksum"
    [ -n "$NEW_BIN" ] && say "Using prebuilt binary (no Rust toolchain needed)"
fi

if [ -z "$NEW_BIN" ]; then
    if [ -z "$SCRIPT_DIR" ] || [ ! -f "$SCRIPT_DIR/Cargo.toml" ]; then
        die "no prebuilt binary for this machine — build from source instead:
  git clone https://github.com/$GH_REPO.git && cd geco && bash install.sh --from-source"
    fi
    [ "$FROM_SOURCE" -eq 1 ] || warn "no prebuilt binary available — falling back to a source build"
    command -v cargo >/dev/null 2>&1 || die "cargo not found — install Rust first: https://rustup.rs"
    say "Building geco (release)…"
    ( cd "$SCRIPT_DIR" && cargo build --release )
    NEW_BIN="$SCRIPT_DIR/target/release/geco"
fi

# The example config, picker wrapper and opencode files come from the clone
# or archive this script sits in, or — when piped from curl — from the
# release archive just downloaded, which ships them beside the binary.
if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/geco.example.yaml" ]; then
    ASSET_DIR="$SCRIPT_DIR"
else
    ASSET_DIR="$(dirname "$NEW_BIN")"
fi

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
        # $HOME rather than a literal ~, so the message shows the real path.
        *) warn "$HOME/.local/bin is not on PATH — add to your shell rc:  export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
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

if [ -f "$CFG_JSON" ] && [ ! -f "$CFG" ]; then
    CFG="$CFG_JSON"
    say "Using existing JSON config: $CFG"
elif [ ! -f "$CFG" ]; then
    [ -f "$ASSET_DIR/geco.example.yaml" ] ||
        die "no geco.example.yaml to scaffold from — copy it from $RAW_URL/geco.example.yaml to $CFG"
    mkdir -p "$CFG_DIR"
    cp "$ASSET_DIR/geco.example.yaml" "$CFG"
    "$NEW_BIN" schema > "$CFG_DIR/geco.schema.json" 2>/dev/null || true
    say "Scaffolded config: $CFG (+ editor schema)  ← EDIT THIS: move your occasional MCP servers in"
else
    say "Config exists: $CFG"
    "$NEW_BIN" schema > "$CFG_DIR/geco.schema.json" 2>/dev/null || true
fi

# Scaffold the picker assets (wrapper script + harness configs) so the LLM
# picker works out of the box for opencode. The script is generic — it
# branches on GECO_CLIENT — and users can edit it or add harnesses.
# Existing files are never overwritten: the user may have customised them.
#
# Guarded on the FILE, not the directory: a first run that created the dir and
# then failed would otherwise mean the assets are never scaffolded again. The
# copies are non-fatal because a missing picker must not abort an otherwise
# good install.
PICKER_DIR="$CFG_DIR/picker"
if [ ! -f "$PICKER_DIR/pick.sh" ] && [ -f "$ASSET_DIR/picker/pick.sh" ]; then
    mkdir -p "$PICKER_DIR"
    if cp "$ASSET_DIR/picker/pick.sh" "$PICKER_DIR/pick.sh" &&
       cp "$ASSET_DIR/picker/opencode.json" "$PICKER_DIR/opencode.json"; then
        chmod +x "$PICKER_DIR/pick.sh"
        say "Scaffolded picker: $PICKER_DIR/pick.sh (+ opencode.json)"
    else
        warn "picker assets not scaffolded — the LLM picker stays off until you add them"
    fi
elif [ -f "$PICKER_DIR/pick.sh" ]; then
    say "Picker assets exist: $PICKER_DIR (not overwritten)"
fi
# The picker agent for opencode — a minimal named agent with no tools. Goes
# where opencode discovers agents. Only for people who actually have opencode:
# creating the opencode config dir here would also make the health-plugin
# block below think opencode is installed, and drop a plugin on a machine
# that has never seen it.
#
# ${XDG_CONFIG_HOME:-…}, not a hardcoded ~/.config: opencode resolves its
# global config through XDG_CONFIG_HOME, so on a machine that sets it, the
# hardcoded path checked (and wrote) a tree opencode never reads — pick.sh
# then ran `--agent picker` against a missing agent file and every pick
# errored.
OC_CFG_ROOT="${XDG_CONFIG_HOME:-$HOME/.config}/opencode"
OC_AGENTS_DIR="$OC_CFG_ROOT/agents"
# Decided ONCE, here, before `geco refresh` runs. The example config's skill
# library lives under ~/.config/opencode, and refresh's absorb step creates
# it — so a check made after the refresh found the dir geco itself had just
# made, and dropped the health plugin on a machine that never had opencode.
HAVE_OPENCODE=0
[ -d "$OC_CFG_ROOT" ] && HAVE_OPENCODE=1
if [ "$HAVE_OPENCODE" -eq 1 ] && [ -f "$ASSET_DIR/picker/picker.md" ]; then
    if [ ! -f "$OC_AGENTS_DIR/picker.md" ]; then
        mkdir -p "$OC_AGENTS_DIR"
        cp "$ASSET_DIR/picker/picker.md" "$OC_AGENTS_DIR/picker.md" &&
            say "Scaffolded picker agent: $OC_AGENTS_DIR/picker.md"
    else
        say "Picker agent exists: $OC_AGENTS_DIR/picker.md (not overwritten)"
    fi
fi

say "Building tool snapshot…"
"$DEST" --config "$CFG" refresh || warn "refresh reported issues (fix config, then: geco refresh)"

say "Verifying meta-tool surface…"
# The check needs python3 only to parse the reply. Saying so is the difference
# between an honest skip and a false alarm: without this guard, a machine with
# no python3 (macOS has not shipped one in the base system since 12.3) gets
# "proxy did not serve: …" on a perfectly good install — which is the same
# cry-wolf this check was just rewritten to stop.
if ! command -v python3 >/dev/null 2>&1; then
    warn "python3 not found — skipping the meta-tool check. The install itself is fine;"
    warn "verify by hand with: geco health"
else
    TOOLS=$(printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"install","version":"0"}}}\n{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}\n' \
      | "$DEST" --config "$CFG" serve --no-refresh 2>/dev/null \
      | python3 -c 'import sys,json
for l in sys.stdin:
    m=json.loads(l)
    if m.get("id")==2: print(" ".join(sorted(t["name"] for t in m["result"]["tools"])))' || echo "")
    # Checked BY NAME, not by count. This used to assert exactly 8, went stale
    # the release the surface dropped to 6, and then warned on every single
    # install — which trains a user to ignore the one line whose whole job is
    # to tell them the install is broken. A count is also the wrong invariant:
    # `read_skill` is served only when a skill library is configured, and the
    # browse tools (list_mcp_servers, list_tools, list_skills) only under
    # `proxy.surface: full` — requiring them warned on every default install
    # once `lean` became the default. The two below are served on every
    # surface and config; the rest are reported, not required.
    # if/else rather than `A && B || C`, which runs C when B fails, not only
    # when A does.
    MISSING=""
    for t in search execute; do
        case " $TOOLS " in
            *" $t "*) ;;
            *) MISSING="$MISSING $t" ;;
        esac
    done
    if [ -z "$MISSING" ]; then
        say "Proxy OK — meta-tools: $TOOLS"
    else
        warn "proxy did not serve:$MISSING (got: ${TOOLS:-nothing})"
    fi
fi

# Companion opencode TUI plugin: toasts gateway health problems in-app.
# Guarded on the source file: release archives (and so a curl-piped install)
# ship no opencode-plugin/ directory — the plugin is parked, see
# opencode-plugin/TODO.md — and under `set -e` an unguarded cp would end the
# install before the wire-it-into-your-agent instructions print.
OC_PLUGIN_DIR="$OC_CFG_ROOT/plugins"
if [ "$HAVE_OPENCODE" -eq 1 ] && [ -f "$ASSET_DIR/opencode-plugin/geco-tui.ts" ]; then
    mkdir -p "$OC_PLUGIN_DIR"
    if cp "$ASSET_DIR/opencode-plugin/geco-tui.ts" "$OC_PLUGIN_DIR/geco-tui.ts"; then
        say "Installed opencode health plugin: $OC_PLUGIN_DIR/geco-tui.ts (restart opencode)"
    else
        warn "health plugin not installed — copy opencode-plugin/geco-tui.ts to $OC_PLUGIN_DIR yourself"
    fi
fi

cat <<EOT

Done. Wire it into your agent (and REMOVE the servers you moved behind it):

  OpenCode (~/.config/opencode/opencode.json):
    "mcp": { "geco": { "type": "local", "command": ["$DEST", "serve"], "enabled": true } }

  Claude Desktop (claude_desktop_config.json):
    "mcpServers": { "geco": { "command": "$DEST", "args": ["serve"] } }

Uninstall:  curl -fsSL $RAW_URL/uninstall.sh | bash
            (add  -s -- --purge  after bash to delete config and caches too)
EOT
