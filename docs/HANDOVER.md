# Handover — geco, 2026-07-25

Start-here for a fresh session. What is done, what is measured, what is next,
and the traps that already cost time once.

---

## 1. State of the repo

`main` is clean and green. 137 tests, ~85% coverage, CI runs 10 checks on
Linux/macOS/Windows. Version is **0.8.0 in `Cargo.toml` but deliberately NOT
tagged** — no public release, because there are no users yet and shipping
binaries nobody downloads is waste. The changelog for 0.8.0 is written.

One PR may still be open: **#29 `feat/llm-picker`** (the LLM picker). Check
`gh pr list`. Everything else is merged and the stale branches are deleted.

**Untracked junk:** `ts.py` in the repo root is not mine and not referenced —
left alone deliberately.

---

## 2. The one thing to understand first

geco hides backend tool/skill schemas from the model's context until searched
for. Fixed cost ~4.3 KB regardless of how much hides behind it — measured by a
test, not asserted.

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

## 4. What shipped for the picker

`src/picker.rs` + `proxy.picker` config. **Off by default.**

- **Transports:** `sampling` (MCP-native, zero config) and `command` (any CLI —
  `opencode run …`, `claude -p`, or a wrapper script). `mode: auto` prefers
  sampling, then command.
- **No new dependencies, deliberately.** A built-in HTTP client would drag in a
  TLS stack, which is the exact objection that sank the embeddings branch. The
  user brings the transport.
- **Every failure degrades to lexical** — unsupported, timeout, prose reply,
  invented tool. Enabling a picker can cost latency; it cannot break search.
- **Serves tools *and* skills.**
- Retrieves `candidates` (default 50) rather than the caller's `limit`, because
  a picker can only choose from what it is handed.

---

## 5. Next steps, in order

### 5.1 Add `/no_think` to the picker prompt — one line, already measured

Tool-picking is **classification, not reasoning**. Measured on the Spark's
`qwen4b`, same weights, only serving config differing:

| | rank-1 | precision |
|---|---|---|
| thinking on | 8% | 20% |
| thinking off (`/no_think`) | **28%** | **70%** |

Reasoning mode spent **1,125 completion tokens** answering a one-line
multiple-choice question, exhausting the budget before emitting JSON. Put a
"answer immediately, do not deliberate" instruction (and `/no_think` for Qwen)
into `Picker::prompt()` in `src/picker.rs`.

### 5.2 Wire `Transport::Sampling` — the last real gap

Currently **detected but not implemented**: `Picker::pick()` returns `None` for
`Transport::Sampling`, so it silently falls back to lexical. This is the
zero-config path that works on any sampling-capable client without a CLI.

Design is settled, mechanism confirmed:
- `RequestContext<RoleServer>` has a `.peer` field.
- `Peer<RoleServer>::create_message(CreateMessageRequestParams)` sends the
  sampling request (rmcp 2.2, `service/server.rs:437`).
- Client support is already detected: `params.capabilities.sampling.is_some()`
  at `initialize`, stored in `Proxy::client_supports_sampling`.
- **Approach:** stash the peer at `initialize` (like `client` is stashed), then
  call `create_message` from the search handler. Avoids touching the `#[tool]`
  macro.
- Reuse `Picker::prompt()` and `Picker::parse_choice()` so the shipped
  behaviour matches what was measured.

### 5.3 Scale `candidates` to the model

50 candidates ≈ a 4,000-token prompt. Fine for a hosted model; heavy for a
local 4B. Either document the relationship or auto-shrink. Evidence: 42s/query
on a Legion Go 2 handheld, ~2s with qwen-coder on the Spark.

### 5.4 Result compaction (new idea, unbuilt)

Splits cleanly:
- **geco can** compact a tool *result* before it ever enters context — same
  mission as hiding schemas, one layer up. Cheapest start: `execute` already
  has typed error codes, so return a compact error instead of a backend's raw
  dump.
- **Only a client plugin can** rewrite history retroactively. geco ships
  `opencode-plugin/geco-tui.ts` which uses an `event`-based API
  (`session.idle`, `session.created`). **Unknown:** whether opencode plugins
  can edit transcripts — check the docs before promising it.
- **Trap:** stripping failed tool calls entirely makes the model forget it
  failed, so it retries the same broken call forever. Replace, don't delete —
  swap a 2,000-token stack trace for `tool X failed: program not found`.

---

## 6. Environment traps that already cost time

- **Rust toolchain is in WSL**, not on Windows. `~/.cargo/bin`. There is no
  cargo on the Windows PATH.
- **`$HOME` differs by shell.** Git Bash → `/c/Users/bernd`; WSL →
  `/home/bernd`. `/mnt/c/...` paths only work in WSL. This has silently written
  files to the wrong place more than once.
- **`sed -i` fails on `/mnt/c`** from WSL ("Operation not permitted"). Edit
  from the Windows side or use a different tool.
- **The pre-push hook** now finds the WSL toolchain (it silently checked
  *nothing* for weeks before that fix). It runs fmt/clippy/tests, and typos if
  installed.
- **`scripts/lint.sh`** mirrors CI's lint job locally (shellcheck, actionlint,
  typos, gitleaks). Run it before pushing prose-heavy commits — a typo has
  failed CI before. It reports what it *skipped*, so a clean result is honest.
- **PowerShell 5.1 sends request bodies as ISO-8859-1.** The harvest contains
  CJK tool descriptions; without explicit UTF-8 bytes, ~64% of LLM calls fail.
  Cost an hour of misdiagnosis.
- **Workflow approval rejects control characters / non-ASCII** in scripts.
  Filter to printable ASCII when inlining harvested data.
- **The DGX Spark** is at `http://10.0.4.108:8081/v1` (models: qwen-coder,
  qwen4b, nemotron, qwen, qwen-oc, qwen36). The user authorised qwen-coder and
  qwen4b; **ask before using others** so the box isn't crashed blindly.
- **LM Studio** at `localhost:1234` is reachable from Windows only.
- **Local hardware is a Lenovo Legion Go 2 handheld** — slow local inference is
  the hardware, not the code.

---

## 7. Working agreements from this session

- **Measure, don't argue.** Every ranking change gets the held-out corpus. If
  it doesn't beat the number, it doesn't ship, however good it looks.
- **Never optimise against the set you measure on.** A symmetric create/open
  synonym raised the training group and lowered held-out; that is how it was
  caught.
- **Run it, don't reason about it.** Three bugs this session were invisible to
  inspection and obvious on first execution — `geco health` didn't exist,
  `geco refresh` was a no-op for a live proxy, and the picker silently
  discarded every choice because models echo `server::tool`.
- **Quality over "I changed something."** A knob that measurably does nothing
  gets reverted, not shipped.
- **Write the dead ends down.** The rejection table is worth more than the
  feature list.
- Small thing the user cares about: **don't caption honesty** — no "honestly",
  "to be honest", "the honest number". Let the work carry it.
