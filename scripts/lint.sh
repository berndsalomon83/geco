#!/usr/bin/env bash
# Run CI's `lint` job locally — shellcheck, PSScriptAnalyzer, actionlint,
# typos, gitleaks.
#
# WHY: these are the checks with no local equivalent, and the gap is not
# theoretical. A prose typo in a test assertion reached CI and failed the
# build, because the local loop was `cargo clippy && cargo fmt && cargo test`
# and none of those look at spelling.
#
# This deliberately does NOT fail when a tool is missing — you should be able
# to run it on any machine — but it always prints what it SKIPPED. A linter
# that silently checks nothing is worse than no linter, because you believe
# it. The exit code covers only what actually ran.
#
#     bash scripts/lint.sh
#
# Installing the missing ones (all optional). Tool names are indented past
# the comment marker on purpose, and this sentence avoids putting that first
# tool's name right after a "#": such a line is read as a linter directive,
# not prose, and this file twice failed its own check by explaining that.
#   - shellcheck ....... apt install shellcheck | brew install shellcheck
#   - actionlint ....... github.com/rhysd/actionlint/releases
#   - typos ............ cargo install typos-cli | brew install typos-cli
#   - gitleaks ......... github.com/gitleaks/gitleaks/releases
#   - PSScriptAnalyzer . pwsh -c 'Install-Module PSScriptAnalyzer -Scope CurrentUser'
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

# Tools installed via `cargo install` land here, which a non-login shell does
# not have on PATH — so `typos` would be reported as missing right after you
# installed it.
export PATH="$HOME/.cargo/bin:$PATH"

cyan=$'\033[1;36m'; red=$'\033[1;31m'; green=$'\033[1;32m'; yellow=$'\033[1;33m'; off=$'\033[0m'

fail=0
skipped=()

have() { command -v "$1" >/dev/null 2>&1; }

# run <label> <command...> — report pass/fail, print output only on failure.
run() {
    label="$1"; shift
    printf '%s%-18s%s' "$cyan" "$label" "$off"
    if out="$("$@" 2>&1)"; then
        printf '%sok%s\n' "$green" "$off"
    else
        printf '%sFAILED%s\n' "$red" "$off"
        printf '%s\n' "$out" | tail -30
        fail=1
    fi
}

if have shellcheck; then
    run "shellcheck" shellcheck install.sh uninstall.sh scripts/lint.sh
else
    skipped+=("shellcheck")
fi

if have actionlint; then
    run "actionlint" actionlint
else
    skipped+=("actionlint")
fi

if have typos; then
    run "typos" typos
else
    skipped+=("typos")
fi

if have gitleaks; then
    # `dir` mode, not `git` mode, deliberately. Scanning history needs git to
    # trust the checkout, and under WSL-on-a-Windows-drive it does not
    # ("dubious ownership") — at which point gitleaks scans ~0 bytes and
    # reports "no leaks found in partial scan". A clean bill of health from a
    # scan that read nothing is the worst possible output, so this scans the
    # working tree instead, which always works.
    #
    # Narrower than CI, which scans all history with fetch-depth: 0. A secret
    # committed and then removed is caught there, not here.
    run "gitleaks (worktree)" gitleaks dir . --no-banner --redact
else
    skipped+=("gitleaks")
fi

# `pwsh` is PowerShell 7 and is NOT what a Windows box has by default — the
# stock shell is Windows PowerShell 5.1, whose executable is `powershell.exe`.
# Looking only for `pwsh` reported PSScriptAnalyzer as skipped on the one
# platform the .ps1 files are actually for, and a change that failed this
# check reached CI while the local run said "everything that ran is clean".
PS_BIN=""
if have pwsh; then
    PS_BIN=pwsh
elif have powershell.exe; then
    PS_BIN=powershell.exe
fi

if [ -n "$PS_BIN" ]; then
    # shellcheck disable=SC2016  # $found is PowerShell's, not the shell's
    run "PSScriptAnalyzer" "$PS_BIN" -NoProfile -Command '
        $found = @()
        foreach ($f in @("install.ps1", "uninstall.ps1")) {
          $found += Invoke-ScriptAnalyzer -Path $f -Settings ./PSScriptAnalyzerSettings.psd1
        }
        if ($found.Count -gt 0) { $found | Format-List; exit 1 }
        exit 0'
else
    skipped+=("PSScriptAnalyzer")
fi

echo
if [ ${#skipped[@]} -gt 0 ]; then
    printf '%sNOT CHECKED:%s %s\n' "$yellow" "$off" "${skipped[*]}"
    printf '  These ran in CI but not here — a clean result below does not cover them.\n'
fi

if [ "$fail" -ne 0 ]; then
    printf '%slint failed%s\n' "$red" "$off"
    exit 1
fi

printf '%severything that ran is clean%s\n' "$green" "$off"
