# Handover — geco, 2026-07-28

Start-here for a fresh session. What is done, what is measured, what is next,
and the traps that already cost time once.

---

## 1. State of the repo

`main` is at `7043364` with **177 tests** and CI runs 10 checks on
Linux/macOS/Windows.

Two commits arrived from outside this session (`92a7628`, `945a800`) and **both
were red on CI** — rustfmt, and the Windows test leg. They are reviewed and
repaired in the three commits above; §5 has what the review found.

**The last release is v0.7.0.** `Cargo.toml` says 0.8.0, which never shipped,
and everything since — the picker, the lean surface, two audits' worth of fixes
— is unreleased. So the prebuilt binaries the installers fetch are older than
the docs describe; `main` needs a source build. Whatever is tagged next carries
**three breaking changes**: the default meta-tool surface (six → three),
`execute`'s response shape (`content` → `attachments` + real blocks), and
`geco refresh` now exiting 1 when a backend fails. A 0.9.0 covers it under the
pre-1.0 caveat the changelog already states.

**Untracked junk:** `ts.py` in the repo root is not mine and not referenced —
left alone deliberately. There is also a zero-byte file whose name is the
literal string `| head -14`, left by some shell accident; same treatment.

---

## 2. The one thing to understand first

geco hides backend tool/skill schemas from the model's context until searched
for. Fixed cost **2,562 B** by default (three meta-tools; `proxy.surface: full` is 4,063 B for six) regardless of how much hides behind it — measured
by a test, not asserted.

The hard problem is **search quality**, and the whole story is in
[`docs/search-ranking.md`](search-ranking.md). **Read that before touching
ranking.** Short version:

| approach | rank-1 on the real corpus |
|---|---|
| BM25F lexical (shipped default) | **13.3%** |
| + parameter tuning | 13.8% (noise — one query of 210) |
| + embedding rerank (all-MiniLM) | 12.9% (*worse*) |
| **+ LLM picker, 4B model** | **28%** |
| **+ LLM picker, 80B model** | **40%** (87% precision when reachable) |

The wall is semantic: when a request shares no vocabulary with a tool, no
lexical trick reaches it. Roughly **30% of confident-looking ranking changes
survive measurement** — the rejection table in the doc exists so the other 70%
stay dead. Do not re-propose stopword lists, symmetric create/open synonyms,
`pm`/`am` mappings, BM25 parameter sweeps, or embedding rerank. All measured,
all rejected, all recorded.

---

## 3. Measurement rig (this is the valuable part)

Nothing about ranking should be argued; it should be measured. The rig exists.

**Corpus lives in WSL at `~/geco-harvest/`** (not in git — large, third-party):

| file | what it is |
|---|---|
| `main149-result.json` | 5,216 tools harvested from 131 real MCP servers |
| `labeled_eval.json` | 210 vocabulary-banned semantic queries, multi-model generated, cross-model verified |
| `picker_cases.jsonl` | emitted candidate sets for scoring an external picker |

**Harness:** `tests/real_search_probe.rs`, all env-gated so CI stays green.

```bash
export GECO_HARVEST=~/geco-harvest/main149-result.json
export GECO_LABELED=~/geco-harvest/labeled_eval.json
cargo test --test real_search_probe labeled_eval -- --nocapture       # rank-1/MRR
cargo test --test real_search_probe recall_at_k_ceiling -- --nocapture # rerank ceiling
```

`recall_at_k_ceiling` is the one that explains everything:

```
recall@1 13.3%   @5 22.4%   @10 27.6%   @20 32.9%
recall@50 44.3%  @100 55.2% @200 64.3%
```

**A reranker can never beat recall@K.** Embedding rerank was fed top-5 — a
22.4% ceiling — so it never had a chance. This is why the picker retrieves 50.

**Scoring an external picker** (`scratchpad/picker.ps1`, run from Windows —
LM Studio binds 127.0.0.1 and WSL cannot reach it):

```powershell
.\picker.ps1 -Model qwen4b -Url "http://10.0.4.108:8081/v1/chat/completions" -Max 25
```

---

## 4. The picker is finished

`src/picker.rs` + `proxy.picker` config. **Off by default.** Documented in
README ("Crossing the wall"), `geco.example.yaml` and the CHANGELOG — it
shipped in #29 with none of that, reachable only by reading `src/config.rs`.

**The headline result, on a realistic 8-server catalogue (324 tools), same 210
queries:** lexical 24.8% → BM25F top-50 + picker 74.6% → **whole catalogue +
picker 94.8%**. Sending everything is more accurate *and* faster (0.9s vs
2.2s), for two linked reasons:

- **BM25F does not select random distractors, it selects the most confusable
  ones.** A shortlist is precisely the set of plausible wrong answers; the
  tools it discards are obvious non-answers. Precision did not fall under 7x
  the distractors.
- **An unchanging catalogue is a cacheable prompt prefix** — 96.6% of a 15k
  prompt served from cache, 13x faster. A per-query shortlist can never cache.

And the surprise: **putting the catalogue BEFORE the question was worth 4.3
points on its own** (90.5% → 94.8%), same model, same candidates, same wording.
It was done for caching. Prompt *layout* moved this corpus further than any
scoring change ever has — worth remembering as a class.

- **Both transports work and are proven end to end** in
  `tests/picker_e2e.rs`: `sampling` over a real MCP session on an in-memory
  duplex, `command` with a real child process. Each test pins the lexical
  order with a control run first, so a picker that does nothing cannot pass by
  agreeing with BM25F.
- **Thinking off is worth 8% → 28% rank-1** on the same weights. `/no_think`
  is in the prompt, but it is a request, not a control: measured against a real
  endpoint neither it nor `enable_thinking: false` changed anything, because
  the server's chat template decides. The reply budget (512 tokens) is sized
  for the answer and not the thinking, so a model that deliberates anyway
  truncates into the lexical fallback rather than being rewarded — 8% is
  *below* the 13.3% baseline, so accommodating it would be worse than not
  asking. geco detects the symptom and logs the remedy.
- **A client that advertises sampling and then refuses is asked once per
  session.** Under `mode: auto` it then falls through to a configured command.
  A failed *command* is deliberately not treated the same way: a failed spawn
  costs microseconds and hot-reload can fix the path under a running proxy.
- **No new dependencies**, deliberately. A built-in HTTP client would drag in
  a TLS stack, which is the exact objection that sank the embeddings branch.

Two things the picker itself broke, now fixed and tested: it held the snapshot
read lock across the model round-trip (stalling the whole gateway for the
timeout), and it leaked the model subprocess on every timeout.

**Skills go through the same picker**, in the same call, from one combined
catalogue. Deliberately not two picks: the prompt has no "none of these"
escape, so a model asked separately "which skill fits?" about a pure tool
request will name one anyway, and geco would promote that noise to rank 1.
Asking once lets the model answer what was actually asked — the best
capability here, of either kind — and the surface it did not choose keeps its
lexical order.

**Caveat worth keeping:** the tool numbers are measured; the skill path is
not. There is no labelled skill corpus, and building one (same shape as
`labeled_eval.json`) is the only way to know what it is worth. The mechanism
is identical and the wiring is tested end to end, but "measured" and "same
code path as something measured" are not the same claim.

---

## 5. What the audit found, and what is left

A six-dimension production-readiness audit ran on 2026-07-25 (lifecycle,
concurrency, security, model-facing contract, docs-vs-code, release
readiness), each finding then adversarially re-checked. **16 of ~31 survived**
— the rejection rate is the useful part, and it matches the ~30% the ranking
work sees.

Fixed on this branch, all with tests that were confirmed to go red without the
fix: `execute` dropping non-text content blocks, no single-flight on backend
connect (measured: 8 children for an 8-call batch), the installers' false
"expected 8 meta-tools" warning, `geco refresh` always exiting 0, PowerShell
writing non-UTF-8, `uninstall.sh` dying silently without python3 and missing
the Linux purge paths, `list_skills` blaming a correct config, and two stale
INSTALL.md claims. Details in the CHANGELOG.

A second, pre-merge audit ran on 2026-07-27 over the whole diff before it
landed: six dimensions again, adversarially verified again, **16 survived**,
zero blockers. The one that mattered: whole-catalogue mode let `search`
fabricate a rank-1 hit for a request the gateway could not serve, because the
prompt has no "none of these" escape. A query that scores nothing on either
surface now gets no pick at all.

A third review ran on 2026-07-28 over the two externally-authored commits:
five dimensions, adversarially verified, **21 of 26 survived** plus 4 from a
completeness critic. The pattern worth carrying forward is *where* the defects
were. The Rust logic was sound; what broke was everything around it —

- the shipped example config named a path geco could not spawn (`~`),
- the new test could not pass on Windows (`\U` in a YAML double-quoted scalar),
- the installer wrote into another tool's config tree unconditionally,
- release archives did not contain the assets the shipped config points at,
- the one new coupling (`GECO_CLIENT`) had no test at all.

Every one of those is invisible to "it worked on my machine", because the
machine in question was a Mac with opencode already installed. **The surfaces
that break are the ones the author does not stand on.**

### Still open, in rough order

1. **Nobody has used it.** Every number here is measured against corpora,
   stubs and a driven binary. The picker's 94.8% is on *vocabulary-banned*
   queries, deliberately the hard end; real traffic is unmeasured. "Does an
   agent work better with this, all day, for weeks" is the question that
   decides production readiness and cannot be answered from this side. First
   real use is on a Mac with opencode.
2. **The lean default is a judgement, not a measurement.** `proxy.surface`
   defaults to `lean` (search/execute/read_skill, 2,562 B). Everything the
   browse tools did is reachable — `search("*")`, the automatic
   catalogue-in-the-hint on a miss, `issues` riding on search — but whether
   models take those routes in daily use is unverified. `geco stats` counts
   calls per meta-tool; that is the evidence to look at.
3. **Skills are unmeasured.** They go through the same picker in the same
   call, and the wiring is tested end to end, but there is no labelled skill
   corpus, so the tool numbers do not transfer. Building one (same shape as
   `labeled_eval.json`) is the only way to know.
4. **The whole-catalogue budget is a guess above 324 tools.** 805 and 1,589
   are unmeasured. That is where a two-stage server-then-tool pick would earn
   its keep, if it does.
5. **Untested: the upgrade path.** A 0.7.0 snapshot and config meeting the new
   binary. v0.7.0 shipped, so those users exist. `Snapshot::load` survives
   corrupt JSON but nothing covers schema evolution.
6. **Untested: failure drills.** Disk full mid-snapshot, a backend that hangs
   rather than dies, killed mid-refresh.
7. **The picker has never met a real client.** Every measurement went through
   the scoring harness or a stub command. `mode: sampling` has not faced a
   client that implements it; `mode: command` has not run a real agent CLI in
   a real session.
8. **Picker prompt injection is unmitigated, and may be unmitigable.** Tool
   descriptions come from third-party backends and become the prompt of a
   model — under `command` mode, a model with shell access running as the
   user. You cannot pick a tool without showing the model its description, so
   there is no clean fix; the questions worth answering are whether the
   `{prompt}` argv contract should be replaced by stdin (so no user is tempted
   to write `bash -lc "... '{prompt}'"`), and what the README should say.
   Currently it says nothing.
9. **Result compaction** (unbuilt, from the previous handover). geco can
   compact a tool *result* before it enters context — same mission as hiding
   schemas, one layer up. Cheapest start: `execute` already has typed error
   codes, so return a compact error instead of a backend's raw dump. Only a
   client plugin can rewrite history retroactively; **unknown** whether
   opencode plugins can edit transcripts — check the docs before promising it.
   **Trap:** stripping failed tool calls entirely makes the model forget it
   failed, so it retries the same broken call forever. Replace, don't delete.
10. **Scaling `candidates` to the model was considered and rejected** — see
   `docs/search-ranking.md`. geco has no signal for which model is behind an
   opaque argv or which one a client routes sampling to, so any auto-shrink is
   guessing at hardware it cannot see, and guessing downward costs recall. The
   relationship is documented instead.

---

## 6. Environment traps that already cost time

- **Rust toolchain is in WSL**, not on Windows. `~/.cargo/bin`. There is no
  cargo on the Windows PATH. Run it as
  `wsl -e bash -lc 'cd /mnt/c/Users/bernd/github/geco && ~/.cargo/bin/cargo …'`.
- **`$HOME` differs by shell.** Git Bash → `/c/Users/bernd`; WSL →
  `/home/bernd`. `/mnt/c/...` paths only work in WSL, and `/tmp` is a
  different directory in each. Cost time again this session: a `cp x /tmp/bak`
  from Git Bash is not readable from WSL, and vice versa.
- **`git checkout <file>` discards uncommitted work.** Use `git stash push --
  <file>` when you want it back. Cost one redo of a finished change.
- **`sed -i` fails on `/mnt/c`** from WSL ("Operation not permitted"). Edit
  from the Windows side or use a different tool.
- **WSL's `/tmp` does not survive between `wsl -e` invocations.** A probe that
  writes a workspace in one call and reads it in the next finds nothing, and
  the symptom is an empty result rather than an error — which reads exactly
  like "the feature did nothing". Put multi-step probes in one shell, or under
  `$HOME`.
- **`git` inside WSL sees every CRLF file as modified.** `git diff --stat` from
  WSL on this checkout reports ~11k changed lines across files nothing touched.
  Use Windows git to read the working-tree state.
- **Piping to `tail`/`grep` hides the exit code.** `cargo fmt --check | tail;
  echo $?` reports *tail's* status, which is always 0. That is how a red
  rustfmt was read as green this session. Redirect to a file and echo `$?`
  straight after the command.
- **The pre-push hook** now finds the WSL toolchain (it silently checked
  *nothing* for weeks before that fix). It runs fmt/clippy/tests, and typos if
  installed.
- **`scripts/lint.sh`** mirrors CI's lint job locally (shellcheck, actionlint,
  typos, gitleaks). Run it before pushing prose-heavy commits — a typo has
  failed CI before. It reports what it *skipped*, so a clean result is honest.
  **PSScriptAnalyzer runs too**, via `powershell.exe` — it was installed all
  along, and the script probed only for `pwsh` (PowerShell 7), so it reported
  "NOT CHECKED" while claiming everything that ran was clean, and `.ps1`
  findings reached CI. Fixed; a run that reports no skips really did check all
  five.
- **PowerShell 5.1 writes the system ANSI codepage** from `Set-Content`,
  `>` and `Out-File` unless told otherwise. It also sends request bodies as
  ISO-8859-1: the harvest contains CJK tool descriptions, and without explicit
  UTF-8 bytes ~64% of LLM calls fail. Both cost an hour of misdiagnosis. Use
  `[System.IO.File]::WriteAllText($p, $s, (New-Object System.Text.UTF8Encoding $false))`.
- **Workflow approval rejects control characters / non-ASCII** in scripts.
  Filter to printable ASCII when inlining harvested data.
- **The DGX Spark** is at `http://10.0.4.108:8081/v1` (models: qwen-coder,
  qwen4b, nemotron, qwen, qwen-oc, qwen36). The user authorised qwen-coder and
  qwen4b; **ask before using others** so the box isn't crashed blindly.
- **LM Studio** at `localhost:1234` is reachable from Windows only.
- **Local hardware is a Lenovo Legion Go 2 handheld** — slow local inference is
  the hardware, not the code.

---

## 7. Working agreements

- **Measure, don't argue.** Every ranking change gets the held-out corpus. If
  it doesn't beat the number, it doesn't ship, however good it looks.
- **Never optimise against the set you measure on.** A symmetric create/open
  synonym raised the training group and lowered held-out; that is how it was
  caught.
- **Run it, don't reason about it.** Every bug that mattered this session and
  last was invisible to inspection and obvious on first execution — `geco
  health` didn't exist, `geco refresh` was a no-op for a live proxy, the
  picker discarded every choice because models echo `server::tool`, and
  `execute` turned a screenshot into `result: ""`.
- **A new test proves nothing until you watch it fail.** Break the thing it
  guards, see red, put it back. One test this session passed against the
  *unfixed* code because it measured which child answered rather than how many
  were spawned — it looked like a guard and was decoration.
- **Quality over "I changed something."** A knob that measurably does nothing
  gets reverted, not shipped.
- **Write the dead ends down.** The rejection table is worth more than the
  feature list.
- Small thing the user cares about: **don't caption honesty** — no "honestly",
  "to be honest", "the honest number". Let the work carry it.
