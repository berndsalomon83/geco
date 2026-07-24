# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning is [semver](https://semver.org/), with the pre-1.0 caveat that a
minor bump may break.

## [0.7.0] — 2026-07-24

### Breaking

- **Backends no longer inherit geco's environment.** A child now starts from a
  clean environment containing an OS/runtime baseline (`PATH`, `HOME` /
  `USERPROFILE`, temp dirs, locale, TLS trust, proxy settings) plus whatever
  its own `env` and `pass_env` declare. Previously every backend received
  every variable geco had, so a token exported for one server was handed to
  all of them.

  **Migration:** a server that relied on an ambient variable must now name it
  in `pass_env`. The symptom is delayed — not a startup error but an auth or
  config failure at the first `execute` — so check any server that reads a
  token from the environment. `proxy.inherit_env: true` restores the old
  behaviour wholesale; `proxy.env_passthrough` passes non-secret variables to
  every backend by glob.

  This also makes `pass_env` meaningful. Before 0.7 it was a no-op: it copied
  variables into a child that already had them.

### Added

- Prebuilt binaries for macOS (arm64, x86_64), Linux (x86_64 static/musl,
  aarch64) and Windows (x86_64). Installing no longer needs a Rust toolchain;
  the installers download, verify SHA-256, and fall back to a source build.
- `notifications/tools/list_changed` is honoured — a backend that adds or
  removes tools at runtime is picked up without a manual refresh.
- Per-server `call_timeout_secs` / `startup_timeout_secs`, so one slow backend
  no longer forces a loose bound on every other server.
- `proxy.refresh_concurrency` (default 8) caps how many backends spawn at once
  during a refresh, instead of starting all of them simultaneously.
- Search-effectiveness metrics in `geco stats`: searches run, how many matched
  nothing, how often a search hit was executed, and the recent queries that
  found nothing.
- `GECO_HEALTH` overrides the health file location — the companion opencode
  plugin already honoured that variable while geco ignored it.

### Changed

- Tool search rewritten to BM25F over tokenised identifiers: separator and
  camelCase splitting, conservative plural stemming, per-field weights, and
  IDF. Lexical recall@5 went from 97.5% to 100% on a 51-query eval corpus;
  the corpus and its floors are in the test suite.
- `search` no longer returns an error when nothing matches — an empty result
  is a valid answer, not a failed call. The shape is now `{results, count}`
  with a hint.

### Fixed

- A backend that died mid-call stayed cached as `Up`. Dead-connection
  detection matched error *text*, but rmcp reports a dead child as "Transport
  closed", which matched nothing — so every subsequent call reused the dead
  connection until the health check noticed. Now decided from the error type.
- Windows keychain lookup never worked. Three separate faults in the embedded
  PowerShell: `Marshal` used without its full type name, pointer arithmetic on
  the wrong side of an `[IntPtr]` cast, and `CredentialBlob` read from the
  address of the pointer rather than the pointer's target.
- Skill hot-reload never ran: the watcher resolved its paths with a blocking
  lock read inside an async task, which panics on the spot.
- A single-server `refresh_tools` could shut down every other backend.
- `install.sh` was broken on any Windows clone (CRLF line endings), and
  `install.ps1` aborted whenever a backend logged a warning.

## [0.6.0] and earlier

No changelog was kept. See the git history.
