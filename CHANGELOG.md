# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning is [semver](https://semver.org/), with the pre-1.0 caveat that a
minor bump may break.

## [Unreleased]

### Fixed

Found by a systematic production-readiness audit rather than by use, then each
one adversarially re-checked before it was believed. Roughly a third of what
the audit proposed survived that second pass; the rest is not recorded here
because it was not real.

- **A screenshot came back as `result: ""`.** `execute` serialized every
  non-text content block into the same JSON string it returns as its one text
  block, which is the worst available outcome in three directions at once: a
  200 KB PNG became ~270 KB of base64 inside a *text* block, so no client could
  render it; the model paid roughly 70k tokens to not see an image; and
  `result` — the field the tool description actually documents — read `""`, so
  a model following the contract concluded the call returned nothing. Non-text
  blocks are now attached as real content blocks after the JSON summary, and
  the summary lists them under `attachments` as `{type, mime_type, bytes}`.
  Same path fired for audio and embedded resources from any backend.
- **One batched `execute` against a cold backend started one child process per
  call.** Every call missed the empty connection cache at the same moment and
  ran its own connect; measured at **8 children for an 8-call batch**, seven of
  them discarded seconds later having already paid for an `npx` resolve and a
  node runtime. Worse than the spike: for a backend that owns an exclusive
  resource — a browser profile lock, a fixed port, a single-writer database —
  the losers contend with the winner, so the winner can fail *because* of them,
  and the user sees an intermittent `backend_error` that goes away on retry.
  Connects are now single-flight, and a failed attempt is shared with everyone
  queued behind it rather than repeated by each in turn.
- **A slow pick made the whole gateway look hung.** `search` held the snapshot
  read guard across the picker's model round-trip — up to 20 seconds by
  default, 300 if the timeout is raised. The next writer (a
  `tools/list_changed` rescan, a config save) queued behind it, and every
  reader then queued behind the writer. The one a user actually feels is
  `execute`, whose trailing health publish reads the snapshot: a tool call that
  had already succeeded sat there until the picker returned. Pinned by a test
  that asserts an *order* rather than a duration, so it does not go flaky on a
  loaded machine.
- **The picker's command transport leaked a process on every timeout.**
  `tokio::time::timeout` only drops the future; tokio does not reap a child on
  drop unless told to. The command is typically an agent CLI, so the abandoned
  process ran its inference to completion, billed the user's account for an
  answer nobody would read, and held a rate-limit slot — while geco logged
  nothing worse than a clean fallback. A session of searches each slightly over
  the bound accumulated one orphan apiece. Also: a wrapper script that exits
  non-zero is now reported rather than having its stdout parsed as an answer.
- **Every install ended with a false warning.** Both installers verified
  "expected 8 meta-tools" against a surface that has served 6 since 0.8.0 (or
  4 without a skill library), so the last line before the "Done" banner was
  always `WARN expected 8 meta-tools, got: 6` — which teaches a user to ignore
  the one line whose job is to tell them the install is broken. A count was
  the wrong invariant anyway; both now check the four always-on gateway tools
  by name and report whatever else is served.
- **`geco refresh` always exited 0**, which made both installers'
  "refresh reported issues" branches unreachable: a fresh install whose every
  scaffolded backend failed to spawn printed the status table and then declared
  success. It now exits 1 and names the servers that failed, matching `health`
  and `validate`. The snapshot is still written and a running proxy still
  nudged — a broken config is exactly when a live proxy most needs to reload.
- **`install.ps1` wrote `geco.schema.json` through `Set-Content`**, which in
  PowerShell 5.1 emits the system ANSI codepage. The schema carries the config
  doc comments verbatim, em-dashes included, so the file was not valid UTF-8:
  the YAML language server failed to load it and config autocomplete silently
  did not work, on Windows only — while `geco schema` from a shell looked fine,
  making it read as an editor bug. Now written as UTF-8 with no BOM. Same fix
  applied to `uninstall.ps1`, which rewrites the user's agent config.
- **`uninstall.sh` did nothing at all without a working python3.** `set -e`
  plus an unguarded heredoc meant the interpreter aborting took the whole
  script with it — before the binary was removed, before anything was purged,
  and without printing a word. Only the agent-config edit needs python3; that
  step now warns and is skipped. Guarded twice, because presence is the wrong
  question on the machine this is likeliest to happen on: macOS ships
  `/usr/bin/python3` as a Command Line Tools **stub** that `command -v` finds
  and that exits non-zero when run, so a presence check passes and `set -e`
  kills the script anyway. The heredoc is now in an AND-OR list, which `set -e`
  exempts, so an unwritable config or a raising script is survived too.
- **`uninstall.sh --purge` left the data behind on Linux.** It listed only the
  macOS-shaped `dev.geco.geco` paths, so on Linux it printed its success line
  while leaving `~/.cache/geco` (a full dump of every backend tool name,
  description and schema) and `~/.local/share/geco/health.json` (per-backend
  call counts and the recent-no-match query log) in place — and a later
  reinstall resurrected the stale snapshot.
- **`list_skills` blamed the config in the one state where the config is
  right.** The tool is only advertised when `skills.dirs` is set, so reaching
  its empty branch means the library *is* configured and simply empty — yet it
  answered `is_error` with "no skill libraries configured (configure
  `skills.dirs`)". Every fresh install is in that state, because the installers
  scaffold a `dirs` path most users have never had. It now returns a successful
  empty answer naming the directories it searched, matching `search`, which
  already declines to call a valid empty result a failed call.
- **INSTALL.md** told users to ask their model about gateway health via a
  `health` meta-tool that 0.8.0 retired, and quoted the pre-0.8.0 4.7 KB
  context cost against README's 4.3 KB — two numbers for the headline claim of
  the project, in the document people read while installing.

### Changed

- **Dogfooded end-to-end against real MCP servers for the first time.** Every
  prior test ran against a Python stub; this stood geco up in front of three
  real, official servers (`filesystem`, `everything`, `context7`) via `npx`
  and drove the full agent loop through `geco serve`: spawn → snapshot →
  `list_mcp_servers` → `search` (found the right real tool at rank 1) →
  `execute` proxied to the real backend and returned the real result. It works.
- **A backend whose command isn't installed now says so, and how to fix it.**
  The message was `cannot spawn backend 'x' (npx)` with the real cause (OS
  "not found") buried — a first-run stumble that sent even the author hunting
  the wrong thing. It now reads: *command 'npx' not found — is it installed and
  on the PATH geco was started with?*, with a note that a tool installed via
  nvm/fnm/volta or only on a shell's PATH is invisible to a GUI-launched agent.
  Decided from the error type (`NotFound`), not its text, so it survives locale
  and platform. README troubleshooting updated to match.

### Added

- **`proxy.surface: lean`** — serve only `search`, `execute` and `read_skill`.
  The three browse tools are **~1 KB of the ~4.1 KB always-on surface, a
  quarter of it**, and nothing they do is unreachable:

  | dropped | reachable via |
  |---|---|
  | `list_mcp_servers` | `search("*")` — one entry per server, tool count, five-name preview |
  | `list_skills` | `search("*", kind="skills")` |
  | `list_tools(server, tool)` | `search(include_schema=true)` |
  | the `issues` health key | now rides on every `search` response that needs it |

  The cost moves from always-on to on-demand, which is the argument geco makes
  about backend schemas, applied to its own surface. `search("*")` is a
  documented sentinel VALUE rather than a second tool or an argument whose
  absence changes the answer — a model choosing between named things is doing
  selection, which is what it is good at; inferring behaviour from a missing
  argument is not. The response keeps the same envelope with `kind: "server"`
  added, because a response whose shape depends on its input is a trap for
  anything parsing it.

  **A miss now volunteers the catalogue by itself**: *nothing matched 'deploy a
  container' — try fewer or more general keywords. Available: github (42
  tools), filesystem (11). Search "\*" for the full catalogue.* Zero always-on
  bytes, arrives exactly when the question is raised, and requires no
  cleverness from the model — which is what makes the browse path safe to hide
  rather than merely cheaper to drop.

  `search("*")` returns **servers**, not every skill: a broad question should
  not be answered with a wall of text nothing in it asked for. Skills are
  counted in the hint instead — a few tokens against hundreds — which still
  tells a model they exist, and that matters because skills are a push
  resource nothing in a user's request ever names.

  **Not the default.** The case for it is a prediction about model behaviour,
  and roughly 30% of those survive measurement here. What genuinely goes is
  enumerating one server's whole tool list; `search("*")` previews five names
  each and `geco inspect` has the rest.
- **geco's own meta-tool surface is now dynamic — no restart, ever.** It used
  to be decided in the constructor, so configuring a first skill library, or
  flipping `proxy.surface`, left the client holding a list that would not
  change until the harness was bounced. That is exactly the hiccup geco exists
  to remove from everything else, so it should not have introduced one of its
  own.

  Every route is now always registered and `tools/list` is computed per request
  from the config as it currently stands; geco advertises
  `tools/list_changed` and sends it when the set actually moves — not on every
  config save, or a client would learn to ignore it. Verified in one session,
  never reconnected: a first skill library appearing, `surface: lean` switching
  on, and back to full, with three notifications delivered.

  Two consequences worth knowing. A tool that is not advertised stays
  **callable**, so a client holding a stale list gets a real answer instead of
  an error — the friendlier half of the same trade. And the instruction text is
  now ONE string rather than a per-surface variant: instructions are delivered
  once at `initialize` and cannot be re-sent, so a surface that moves at
  runtime would otherwise leave whichever text the client holds describing a
  flow that no longer exists. Naming the flow instead of inventorying the tools
  is correct on either surface, and shorter than the text it replaced.
- **`geco stats` counts calls per meta-tool.** The always-on surface is the one
  cost geco publishes a number for, and there was no way to tell which of those
  tools a model ever reaches for — so the question above could only be argued.
  A real session now answers it.
- **The picker shows the model your whole catalogue when it fits**
  (`proxy.picker.whole_catalogue_tokens`, default ~24k tokens ≈ 500 tools).
  This is the largest measured win in the search story, and it is both more
  accurate *and* faster than the lexical shortlist it replaces. On a
  realistic eight-server catalogue (324 tools), same model, same prompt,
  same 210 vocabulary-banned queries:

  | shown to the model | recall ceiling | rank-1 | latency |
  |---|---|---|---|
  | nothing (lexical only) | — | 24.8% | µs |
  | BM25F's top 50 | 82% | 74.6% | 2.2s |
  | **the whole catalogue** | **100%** | **94.8%** | **0.9s** |

  Precision did not fall under seven times the distractors — 90.5% against the
  shortlist's 90.7%, measured like for like — which was the opposite of the
  prediction. **BM25F does not select
  random distractors — it selects the most confusable ones.** Every
  shortlisted candidate was chosen for sharing vocabulary with the query, so
  a shortlist is precisely the set of plausible wrong answers, while the
  tools it discards are mostly obvious non-answers a model rejects at a
  glance. Pre-filtering lexically concentrates the hard negatives.

  It is *faster* for a related reason: an unchanging catalogue is a
  **cacheable prompt prefix**, and a per-query shortlist can never be one.
  Measured at 15,243 prompt tokens: **96.6% served from cache, 13x faster**
  than the same prompt reordered per call. `Picker::prompt` is now split into
  a stable `catalogue_block` and a per-query `question_block`, in that order —
  it previously put the query first, which made every byte of the catalogue
  uncacheable. A test pins that no query text leaks into the prefix, because
  that failure is silent: answers stay correct and only the bill changes.

  The reorder also turned out **not to be accuracy-neutral: 90.5% → 94.8%**,
  same model, same candidates, same wording, only the block order different.
  It was done for caching; the accuracy was re-measured only because changing
  a measured prompt invalidates its number, and the gain was the surprise.
  Worth remembering as a class: **prompt layout moved this corpus further than
  any scoring change ever did.**

  Also newly measured and recorded: **the whole published search story was
  measured on a corpus nobody runs.** All the 13.3%/40% figures come from
  5,216 tools across 131 servers; a real config is a handful of servers and a
  few hundred tools. Lexical rank-1 barely moves across that range
  (13.3% → 24.8%) but recall nearly doubles, so every number downstream of
  retrieval was understated. `tests/real_search_probe.rs` grew
  `realistic_catalogue_slices` and `unreachable_queries` to keep measuring it
  (both env-gated, no-ops in CI).
- **Skills go through the picker too**, in the same call as tools, from one
  combined catalogue. Deliberately one pick rather than one per surface: the
  prompt has no "none of these" escape, so a model asked separately "which
  skill fits?" about a pure tool request will name one anyway, and geco would
  promote that noise to rank 1 of the skill results. Asking once lets the
  model answer the question actually being asked — the best capability here,
  of either kind — and the surface it did not choose keeps its lexical order.

  A skill is listed as `skill: name` rather than `server::name`, and a reply
  echoing that label back resolves — models echoing the prompt's own shape is
  not hypothetical, it cost an entire release of silently discarded picks with
  `server::tool`. With no skill library configured the prompt is byte-identical
  to the one the tool measurements were taken with, so adding this cannot
  silently re-scope a published number.

  **Unmeasured, and flagged as such.** There is no labelled skill corpus, so
  the tool figures do not transfer; the mechanism is identical and the wiring
  is tested end to end, but that is a different claim from "measured".
- **An optional LLM tool picker** (`proxy.picker`) — the first thing that
  crosses the lexical-vs-semantic wall instead of measuring it. Lexical
  ranking puts the right tool first ~13% of the time when a request shares no
  vocabulary with it; handing BM25F's top 50 to a model and asking which one
  the user meant measures **40% rank-1 and 87% precision when the answer was
  in the candidate list**, on the same 210 vocabulary-banned queries over the
  same 5,216 real tools. The right-hand number is the interesting one: the
  semantic judgement is close to solved and what is left is retrieval, which
  is why the picker retrieves 50 rather than the caller's `limit` (recall@50
  is 44% against recall@5's 22%, and no picker beats the recall of its own
  candidate list — the same ceiling that doomed the earlier embedding-rerank
  attempt, which was fed the top 5).

  **Off by default**, and no new dependencies. geco ships no HTTP client: one
  would drag a TLS stack into a binary whose pitch is no runtime
  dependencies, to do what `curl` in a three-line script already does. You
  bring the transport — MCP `sampling` (your client's own model, zero config)
  or `command` (`opencode run …`, `claude -p`, a wrapper script). Every
  failure degrades to the lexical order: unsupported, unreachable, timed out,
  answered in prose, or naming a tool that does not exist. Enabling a picker
  can cost latency; it cannot break search.
- **The picker's MCP sampling transport actually works.** It was detected at
  `initialize` and then quietly unimplemented — `Picker::pick` returned
  `None` for it, which from the outside is indistinguishable from a picker
  whose model agrees with lexical ranking every time. Now the peer is stashed
  at `initialize` and `sampling/createMessage` really crosses the wire.
  Proven by an end-to-end test (`tests/picker_e2e.rs`) that stands up a
  real client over an in-memory duplex, with a control run that pins the
  lexical order first so a picker doing nothing cannot pass.

  A client that *advertises* sampling and then refuses to serve it — the
  common case, since the capability is compiled in and the user declines — is
  asked exactly **once per session**. Retrying on every search would cost a
  doomed round-trip and, on clients that prompt, a dialog, for a lexical
  answer geco was always going to return. Under `mode: auto` it then falls
  through to a configured command. A command that fails is deliberately *not*
  treated the same way: a failed spawn costs microseconds and no interaction,
  and hot-reload can fix the path under a running proxy.
- **The picker prompt now tells the model not to think**, which is worth more
  than every ranking change ever measured on this corpus. Picking a tool is
  classification, not reasoning. Same weights, same prompt, same candidates,
  only the serving config differing: reasoning mode scored **8% rank-1 / 20%
  precision**, reasoning off scored **28% / 70%**. It spent 1,125 completion
  tokens deliberating over a one-line multiple-choice question and hit the
  ceiling before emitting any JSON. Note the direction — 8% is *below* the
  13.3% lexical baseline the feature exists to beat, so a deliberating model
  is worse than no picker at all. The reply budget is sized for the answer
  and not for the thinking, so a model that ignores the instruction truncates
  into the lexical fallback rather than being rewarded for it.
- **The picker is documented** (README "Crossing the wall", plus a commented
  block in `geco.example.yaml`). It shipped with four config knobs reachable
  only by reading `src/config.rs`, which is a feature that does not exist for
  anyone who did not write it. `proxy.search_synonyms` was missing from the
  `proxy` table for the same reason and is now listed.
- **`docs/search-ranking.md`** — the permanent research log for search: the
  scorer, every change that was kept (with the measured evidence), every change
  that was **rejected and why** (stopword list, confidence annotation, symmetric
  create/open, four mined synonyms, an external branch's pm/am hack and
  param-search-tuned constants), the honest ~13% rank-1 ceiling on pure-intent
  queries measured against 5,216 real tools, the parameter sweep that proved
  tuning cannot move it, and the open accept-the-ceiling-vs-embeddings fork.
  Written so the dead ends are not re-walked. Roughly 30% of confident-looking
  ranking changes survive measurement; the log exists so the other 70% stay
  dead.
- **User-configurable search synonyms** (`proxy.search_synonyms`). The escape
  hatch for the one thing geco's lexical ranking cannot do on its own: bridge
  a query and a tool that share no vocabulary. Each entry is a group of
  interchangeable words (`[["k8s", "kubernetes"], ["repo", "repository"]]`),
  applied on top of the built-in table and picked up on config hot-reload.
  Empty by default and byte-identical to plain search when unset — a test
  pins that, so it can't regress anyone not using it. Reaches skill search
  too.

  This was the one genuinely general idea harvested from an external
  `fix/search-quality` branch, which was measured against a new real-corpus
  eval (below) and found to be **overfit**: its headline "95% semantic, zero
  misses" held only on the 8-server toy corpus it was tuned against; on 5,216
  real tools it scored 13.8% rank-1 versus main's 13.3% — statistically
  identical. Its other changes (a `pm`/`am`→`time` synonym, parameter-search
  tuned constants, typo tolerance) were rejected as corpus-tuned or
  no-measurable-gain. User synonyms survived because they are general and
  put the escape hatch in the user's hands, not the maintainer's guesses.
- **A real-corpus eval harness** (`tests/real_search_probe.rs`, gated on
  `GECO_HARVEST` / `GECO_LABELED`, no-ops in CI). Measures geco's rank-1/MRR
  over 5,216 tools harvested from ~150 real MCP servers, against 210
  vocabulary-banned semantic queries generated by a multi-model swarm and
  cross-model verified. It exposed the honest ceiling the toy corpus hid:
  on pure-intent queries that share no words with the target tool, lexical
  search reaches ~13% rank-1. No synonym or typo trick closes that gap — it
  is the lexical-vs-semantic wall, and naming it is worth more than pretending
  a parameter sweep crossed it.
- **Two synonyms mined from real MCP servers**: `validate`↔`verify` and
  `build`↔`compile`. Harvested the tool manifests of ~150 popular MCP servers
  (5,216 tools) and A/B-tested candidate synonyms against real-tool search
  (`tests/real_search_probe.rs`, gated on `GECO_HARVEST`, no-ops without it).
  These two were the only candidates that moved the *right* tool from
  off-the-list to rank 1 on a reverse-vocabulary query — "verify a workflow"
  now finds `n8n_validate_workflow` instead of playwright's UI-assert tools;
  "compile the project" finds `build_project` instead of dbt's `compile_sql`.
  Four other confident-looking candidates (mail/email, scrape/crawl,
  copy/duplicate/clone, execute/run) were measured and **rejected** as
  neutral-to-noise. The harvest's larger finding: geco's name-weighting
  already resolves the worst real-world collisions — including tools with
  byte-identical descriptions (`create_spark_batch` vs `create_pyspark_batch`)
  that no embedding could tell apart — so synonyms help only a narrow seam.
- **The config and skill watchers are finally tested.** `spawn_hot_reload`
  and `spawn_skill_reload` were called by exactly one place — `main.rs` — and
  by no test at all; the existing hot-reload test called `reload_from_config`
  directly and said so in a comment. That gap already cost one silent
  feature: the skill watcher resolved paths with a blocking lock read inside
  an async task, which panics instantly, so hot-reload never ran for anyone
  and nothing reported it. A test that skips the wiring cannot catch a bug in
  the wiring. These drive the real thing — write a file, poll, assert the
  agent sees the change through `list_mcp_servers` / `list_skills` rather
  than through a private field. Verified by disabling both watchers, which
  fails both tests. `server.rs` coverage 70% → 81%.

- **`scripts/lint.sh`** — runs CI's `lint` job locally (shellcheck,
  actionlint, typos, gitleaks, PSScriptAnalyzer). Missing tools are skipped
  rather than fatal, but it always prints which ones it did **not** run: a
  linter that silently checks nothing is worse than none, because you believe
  it. Written after a prose typo reached CI, since the local loop was
  `fmt && clippy && test` and none of those read spelling.
- **A coverage table in every CI run's summary** (`cargo-llvm-cov`,
  informational, gates nothing). A percentage target drives tests toward
  whatever is cheapest to cover rather than whatever is riskiest, so there
  isn't one; the per-file table sorted worst-first is the useful part. It
  exists because `backend.rs` sat at 461 lines with no direct tests until a
  human noticed, and its first test found a panic.

### Fixed

- **The periodic health check could not detect a backend that died while
  idle** — the exact case it exists for. It relied on `is_closed()`, which an
  rmcp client only flips when it next sends on the connection; a child killed
  between calls left the connection reporting healthy indefinitely (measured:
  still "Up" ten seconds after a confirmed kill, while a real call failed at
  once). So the README's promise that the check resets a silently-dead
  backend "so the agent never sees a stale failure" was false for idle death.
  The check now **actively probes** with a `list_tools` round-trip (not the
  spec's `ping`, which a live server may not implement and which would then
  cause false resets) and resets non-responders. Found by killing a warm
  backend's child from outside and watching the loop miss it. The reactive
  path — detect-on-next-call — always worked and is unchanged. A probe
  *timeout* is deliberately NOT treated as death: a dead child fails the
  probe promptly with a transport error, whereas a timeout usually means the
  backend is busy (many MCP servers process requests serially, so a probe
  waits behind an in-flight tool call). Resetting on timeout would tear down
  a live backend mid-call and spawn a duplicate child on every sweep that
  overlaps a slow call — so only a prompt error resets.
- **The pre-push hook never ran on Windows.** It looked for `cargo` on `PATH`
  only, so a toolchain installed inside WSL — invisible to Git Bash, which is
  what git runs hooks with — made it print "skipping" on every push and check
  nothing. It now falls back to `~/.cargo/bin`, then through WSL, and runs
  `typos` too when installed.
- `gitleaks` in `scripts/lint.sh` scans the working tree rather than history,
  because git refuses to trust a checkout on a Windows drive from WSL
  ("dubious ownership") — at which point gitleaks scans ~0 bytes and reports
  *"no leaks found"*. A clean result from a scan that read nothing is the
  worst possible output. Narrower than CI, which still scans all history.

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

- **Unit tests for `src/backend.rs`** — 461 lines that had none. They pin
  behaviour nothing asserted: `enabled: false` servers never enter the
  registry at all (rather than being filtered later, one stray lookup away
  from spawning), `deny` is enforced at *execution* and before connecting
  (hiding a tool is cosmetic if calling it by name still works), a backend
  that cannot spawn fails only itself and still appears in the snapshot with
  its reason, and a filtered refresh contains exactly the filtered server.
  The first run found a real panic — see Fixed.
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

- **`proxy.refresh_concurrency: 0` panicked the refresh.** Zero is documented
  as "unlimited" and was implemented as `Semaphore::new(usize::MAX)` — but
  tokio panics above `MAX_PERMITS` (`usize::MAX >> 3`), so the documented way
  to disable the cap crashed the task instead. Found by the first unit test
  ever written against `backend.rs`, which had 461 lines and no direct
  coverage.
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
