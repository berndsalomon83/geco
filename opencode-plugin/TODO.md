# geco TUI Toast Plugin — Parked

## Status
The geco-tui.ts plugin loads (confirmed in opencode log) but neither
`setInterval` nor event hooks (`event`, `tool.execute.after`) ever fire.
The toast never appears.

## Root Cause
This version of opencode:
- Scans `~/.config/opencode/plugin/` (singular) for plugins
- TUI plugins (`TuiPluginModule` format) fail with "must default export an object with server()"
- Server plugins (function export) load but hooks don't fire
- `setInterval` (with or without `unref()`) doesn't fire — opencode's event loop doesn't yield to JS timers
- `plugin` config key with `file://` URLs or relative paths is silently ignored
- Only npm-installed plugins (like ICM) load and have working hooks

## What We Tried
1. `TuiPluginModule` export (`{ id, tui }`) — not supported
2. Object with `server()` export (`{ id, server }`) — loads but no hooks fire
3. Function export (`export default async ({client}) => {...}`) — loads, but setInterval and hooks don't fire
4. `plugin` config key with `file://` URL — silently ignored
5. `plugin` config key with relative path — silently ignored
6. `.opencode/plugin/` (project-level) — sometimes scanned, hooks still don't fire
7. `setInterval` with `unref()` — timer never fires
8. `setInterval` without `unref()` — timer still never fires

## Open Question: `plugin/` or `plugins/`?
Unresolved, and the repo has said both. The Root Cause section above records
`plugin/` (singular); commit 810f7fe switched the installers to `plugins/`
(plural), which is what `install.sh` and `install.ps1` write today. Both
uninstallers clean up either. Settle this before un-parking — installing into
the directory opencode does not scan would look exactly like "hooks don't
fire", and may well be part of what is recorded above.

There was also a second, divergent copy of the plugin checked in at
`.opencode/plugin/geco-tui.ts` (the project-local dir), left behind by that
same commit. It has been removed; `opencode-plugin/geco-tui.ts` is the only
source of truth.

## Next Steps
1. Check opencode version — upgrade may fix TUI plugin support
2. Check if `hooks.ts` plugin's hooks actually fire (vs just loading) — if they don't, it's an opencode-wide issue
3. Consider publishing geco-tui as an npm package — that's how ICM loads successfully
4. File an issue at https://github.com/anomalyco/opencode/issues if hooks don't fire for any local plugin
5. Try `fs.watch` (used by hooks.ts) instead of setInterval — that might work

## Working API (from SDK docs)
- `client.tui.showToast({ body: { message, variant } })` — untested (hooks never fire to call it)
- `client.app.log({ body: { service, level, message } })` — works on plugin init

## Workaround
Use `geco stats` and `geco health` CLI commands instead. The health system
(health.json) works perfectly — only the in-app toast is broken.
