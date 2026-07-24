# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning is [semver](https://semver.org/), with the pre-1.0 caveat that a
minor bump may break.

## [0.8.0] — 2026-07-24

**The always-on surface drops from eight meta-tools to six, ~4.7 KB to
~4.3 KB.** Both retirements follow the same rule: a capability that only
matters after a human action, or whose answer has already been delivered
elsewhere, does not belong in a schema that every request carries.

### Breaking

- **The `health` meta-tool is gone; `geco health` replaces it (new command).**
  It cost 218 bytes of every session's context — 5% of the schema surface —
  to answer a question that only arises once something is already broken, and
  by then the answer had already been delivered: `execute` returns a typed
  `error.code` with the reason at the moment of failure, and
  `list_mcp_servers` carries per-server `status` and `last_refresh`.
  Diagnostics belong in the payload, where they cost nothing until needed.

  **Migration:** run `geco health` (exits non-zero when there are errors,
  zero for warnings alone, so it is safe to gate a script on). Agents get
  gateway-level problems from `list_mcp_servers`, which now includes an
  `issues` array — see below.

- **The `refresh_tools` meta-tool is gone; `geco refresh` covers it.** At 480
  bytes it was the third-largest thing in the always-on surface, and it exists
  for a moment that only ever follows a human action: you installed or
  upgraded a backend. Config edits are already picked up live by the config
  watcher, and backends that change their own tools already send
  `tools/list_changed`, so the only case left is a backend upgraded in place —
  a human act with a human fix.

  `geco refresh` now **bumps the config file's mtime** as well as rewriting
  the snapshot. Without that it did nothing for a live session: a running
  proxy holds its snapshot in memory, only ever *writes* that file, and would
  have overwritten what the CLI just saved. Since the config watcher gates on
  mtime, the touch makes a running instance rebuild — registry, snapshot and
  skills. File content is untouched; `File::set_times` moves the timestamp
  only, so an interrupted call cannot damage a config.

  **Migration:** `geco refresh`, or edit/save the config, or restart the
  agent. Note this drops the added/removed/changed diff the tool returned —
  `geco refresh` prints per-server tool counts instead.

- **`list_mcp_servers` returns `{servers: [...]}` instead of a bare array**,
  and adds `issues` when the gateway has a problem — the config- and
  skill-level faults that were previously only visible through the retired
  `health` tool. The key is omitted when there is nothing wrong, but the
  envelope is unconditional: a response that changes shape between healthy
  and broken is a trap, and broken is when you least want a surprise.

  **Migration:** read `response.servers` where you previously read the array.

  Together these three take the schema **4305 → 3662 B** and the always-on
  total **4675 → 4258 B**. The `health` removal nets only 162 B on its own,
  since describing the new `issues` field spent 56 B back.

### Added

- **CLI tests** (`tests/cli.rs`, 11 of them), run against the real binary.
  `src/main.rs` was 441 lines no test could reach — it is a binary, and none
  of its logic lives in the library. Two bugs shipped through that gap in a
  single day: `geco health` did not exist while docs and a design decision
  both assumed it did, and `geco refresh` was a no-op for a running proxy.
  Neither was subtle; both were invisible because nothing ever executed the
  CLI. Both guards were then mutation-tested — reintroducing bug 1 fails four
  tests, reintroducing bug 2 fails one with the message written for it.
- **Twelve more held-out search queries**, written to *falsify* the next
  proposed synonym rather than to confirm it — three use a word the table
  maps while wanting a tool that has nothing to do with it. Held-out is now
  20 queries. This dropped the reported held-out score from 87.5%/100% to
  65%/80%: nothing regressed, the old sample was simply too small and too
  kind. The floors moved with it, and the reasoning is recorded beside them
  so the drop is not later misread as one.
- `know` as a synonym for `recall` — "what do you know about X" is how people
  ask a memory tool what it has. Semantic recall@5 90.9% → 100%, held-out
  unchanged, and the three planted "know"-means-something-else queries still
  rank correctly. Motivated by training data, validated on data it never saw.
- **Hidden skills are now discoverable.** When a library is configured, geco's
  MCP instructions state how many skills exist, that they're invisible
  otherwise, that they are not callable tools, and to search first. Measured
  against three local models on a task where a playbook applied, the model
  consulted geco in 0–1 runs out of 12 before this; 8–12 after. Listing skill
  names was tested and rejected: it costs context proportional to the library
  AND scored worse, because two of three models tried to call a skill name as
  a tool. Cost is one fixed-size sentence, added only when skills exist.
- **Synonym expansion in search.** Covers requests that use different words
  than the tool does — "bug"/"ticket" for issue, "save" for write, "folder"
  for directory. Semantic recall@5 went 36.4% → 90.9% with no loss on queries
  that already worked. Validated against held-out queries written after the
  table, which score identically to the lexical set.
- **Leading-verb expansion**, which fixes the rank-1 problem the table left
  behind. "open a bug report" reached create_issue only at rank 3, because
  bug → issue matched get_issue and add_issue_comment just as well; the word
  that separates them is the verb. Expanding it symmetrically overfits — "any
  open defects?" wants list_issues — so the mapping fires only on a query's
  *first* token, where an imperative puts its verb. Semantic recall@1 54.5% →
  72.7%, MRR 0.697 → 0.818, held-out and lexical groups unchanged to the
  decimal. Rank 1 is the number that matters: a model acts on the first
  result rather than rephrasing.
- Skill-usage counters in `geco stats`: `skill_surfaces` and `skill_reads`.
  Surfaces without reads means the skill descriptions don't match how work
  gets described; near-zero surfaces means nobody is looking.

### Changed

- **`list_skills` and `read_skill` are no longer advertised when no skill
  library is configured** — 640 bytes, 14% of geco's whole always-on surface,
  returned to anyone using it purely as a tool gateway.
- The context-budget test now counts the MCP `instructions` string, which is
  always-on exactly like the tool schemas and was previously unmeasured. This
  *raised* the honest figure to ~4.7 KB — the cost was always that, only the
  measurement was wrong — before the two tool retirements above brought it
  down to ~4.3 KB. The headline number is unchanged from 0.7.0 by
  coincidence, not by standing still.

### Fixed

- Windows keychain lookup read the *address* of the credential-blob pointer
  rather than the pointer's target, returning NUL-padded rubbish where the
  secret should be. Third and final fault in that script; the OS credential
  store is now covered by a round-trip test on macOS in CI and by hand on
  Windows, where the Actions runner cannot create the fixture.

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
