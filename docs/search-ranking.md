# Search ranking: the road, and the parts of it not to walk twice

geco's whole value is that a tool it can't surface may as well not be installed.
So search quality is the product, and every change to it has been **measured**,
not argued. This is the lab notebook: what was tried, what the numbers said,
what was kept, and — more importantly — what was **rejected and why**, so nobody
re-runs a hunt that already came back empty.

The one rule behind all of it: **a held-out set the tuning never sees is the
only judge that isn't rigged.** Optimising against the set you measure on always looks
like a win. Several changes below looked obviously right and measured worse;
they are recorded so their appeal doesn't lure a second attempt.

---

## The scorer

BM25F over tokenised names, server names, and descriptions.

- **Tokenisation:** split on separators and camelCase (`browser_takeScreenshot`
  → `browser`, `take`, `screenshot`), lowercase, conservative plural stemming,
  drop single characters.
- **Fields & weights:** a hit in the name counts 3× one in the description
  (`W_NAME=3`, `W_SERVER=1`, `W_DESC=1`). See the sweep below for why 3 is not
  a number worth second-guessing.
- **IDF** per query over the current corpus, so a term appearing all over your
  tools counts for almost nothing.
- **Dependency-free and stateless.** Ranking a few hundred tools costs
  microseconds; there is no index to build, persist, or invalidate.

It replaced a weighted-substring scheme that matched query tokens as raw
substrings — "what time is it" ranked `list_commits` above `get_current_time`
because "is" matched l-**is**-t and "it" matched comm-**it**-s. Token boundaries
stop the accident; IDF makes it not matter anyway. Both are needed.

---

## Kept — each earned its place by measurement

| Change | Evidence |
|---|---|
| **Synonym table** (bug↔issue, save↔write, folder↔directory, …) | Semantic recall@5 36.4% → 90.9% on the eval corpus, no loss on queries that already worked. |
| **Leading-verb expansion** (verb in the *first* token expands: "open a bug report" → create) | Semantic recall@1 54.5% → 72.7%. Position is what makes it safe — "any open defects?" (adjective, not leading) is untouched. |
| **`know` → `recall`** | Added only after 12 held-out queries existed to judge it, 3 using "know" for something else. Semantic recall@5 90.9% → 100%, held-out unchanged. |
| **`validate`↔`verify`, `build`↔`compile`** (mined from 150 real servers) | The only 2 of 6 mined candidates that moved the right tool to rank 1 on a reverse-vocabulary probe. |
| **User-configurable synonyms** (`proxy.search_synonyms`) | The escape hatch for the semantic wall (below). General, byte-identical no-op when unset — a test pins that. |

---

## Rejected — measured worse or no better. Do not re-try without new evidence.

| Change | Why it looked right | What the numbers said |
|---|---|---|
| **Stopword list** ("what", "is", "this"…) | Removing noise words should sharpen matches. | Lexical MRR 0.938 → 0.931, recall@3 100% → 97.5%. IDF already makes common words weightless; removing them threw away the little signal they carried. |
| **Confidence annotation** (label low-coverage hits "uncertain") | Help the model distrust weak matches. | Local models (gemma, qwen3) refused to act on correctly-labelled results 4–5 times out of 5. Correct answers labelled low made the model give up. |
| **Symmetric `create`↔`open`** | The verb that separates create_issue from get_issue. | Lifted the training group (54.5% → 63.6%) while the **held-out** group *fell* (87.5% → 75.0%). Textbook overfitting. The real mapping lives in `leading_verb_synonyms` instead, where position disambiguates verb from adjective. |
| **`mail`↔`email`, `scrape`↔`crawl`, `copy`↔`duplicate`↔`clone`, `execute`↔`run`** | Mined from real servers; sound-looking. | Neutral-to-noise on a real-tool probe. The literal word already matched; the synonym only pulled in near-misses (scrape/crawl surfaced crawler-*analytics* for "scrape a web page"). |
| **`pm`/`am` → `time`** (from an external branch) | Made "3pm" match time tools. | Absurd in general — "pm" means many things. Pure overfit to one corpus query. |
| **Parameter-search-tuned constants** (external branch) | Grid-searched to "95% semantic, zero misses". | Tuned against the 8-server toy corpus. On 5,216 real tools: **13.8% vs main's 13.3%** — the gains were memorised, not real. |
| **Typo tolerance, parameter indexing, adaptive filter** (external branch) | Plausibly general. | +0.5% on real data. Not worth the complexity. |

The recurring lesson: **~30% of confident-looking changes survive measurement.**
The other 70% look identical to the winners until you run them against data the
change didn't get to see.

---

## The real ceiling — the number the toy corpus hid

Every number above ≤ 2026-07 came from **8 fictional servers whose descriptions
were hand-written**, and they flattered geco because the eval queries shared
vocabulary with the tools.

To measure it properly, a multi-agent workflow harvested the tool manifests of
**~150 real, popular MCP servers (5,216 tools)**, then a second multi-model swarm
(opus/sonnet/haiku, cross-model verified) generated **210 vocabulary-banned
semantic queries** — queries that describe a goal in words the tool's name and
description never use. Measured with `tests/real_search_probe.rs` (`labeled_eval`):

```
overall   n=210   recall@1 = 13.3%   recall@3 = 19.0%   recall@5 = 22.4%
```

That is the floor: on pure-intent queries, lexical search finds the right tool
first about **1 time in 8**. The gap between this and the toy corpus's 65% is
the difference between "queries that share the tool's words" and "queries that
don't." Real usage is somewhere between; this is the hard end.

Note the shape: recall@5 (22%) is barely above recall@1 (13%). The right tool is
usually not *buried* — it is *absent* from the top 5 entirely, because the query
and the tool share no words to match on.

---

## What does NOT cross the wall (measured)

- **Parameter tuning.** Swept `W_NAME ∈ {1,2,3} × W_DESC ∈ {1,2,3} × B ∈ {0.5,0.75}`
  against the 210-query real corpus. The best configuration (`W_NAME=2,
  W_DESC=1, B=0.75`) reached **13.8%** versus the shipped `W_NAME=3`'s **13.3%**
  — a difference of *one query out of 210*, i.e. noise. Everything else was
  equal or worse; favouring the description *hurt* (down to 11.4%). That
  single-query gain is not worth detuning `W_NAME=3`, which was chosen for the
  lexical queries the 3× name weighting exists to serve — chasing it would be
  the same mistake as the external branch, optimising one scoreboard at the
  cost of another. Reweighting only reshuffles which tools sit in the top 5; it
  cannot summon a tool that never matched, and recall@5 stayed ~22% throughout.
- **More synonyms.** By definition a vocabulary-banned query shares no word with
  the tool, so a synonym of the tool's word still doesn't match it.
- **Typo tolerance, parameter indexing** (external branch): +0.5%.

**Nothing lexical crosses it.** The 13% is the lexical-vs-semantic boundary
itself, not a knob left unturned.

---

## The third path: let an LLM pick (MEASURED — this one works)

Both forks below assume geco must *rank* by itself. It doesn't. The client
already has a capable model, and geco can ask it — in a **separate, throwaway
session** that never touches the agent's context — "here are N candidate tools,
which one does this request want?"

Measured 2026-07 against the same 210-query corpus (50-query sample, local
`qwen-coder`, BM25F supplying the candidates):

| candidate net | recall ceiling | rank-1 | picked right *when reachable* | latency |
|---|---|---|---|---|
| BM25F alone | — | **13.3%** | — | µs |
| LLM picker, K=50 | 46% | **40.0%** | **87.0%** | ~2s |
| LLM picker, K=200 | 60% | **44.0%** | 73.3% | ~7s |

**Three times better than lexical, and the decomposition says why.** The model
picks correctly **87% of the time when the answer is in the list** — it is
near-perfect at exactly the semantic judgment that synonyms, tuning, and
embedding rerank all failed at. Accuracy ≈ `recall@K × precision`, so the only
remaining lever is how wide the net is.

Widening past K=50 has diminishing returns: the ceiling rises but *precision
falls* (87% → 73%) as distractors crowd the prompt, and latency triples. K=50
delivers 91% of the K=200 accuracy at a third of the cost.

Note the real ceiling for a **realistic** deployment: this corpus is 5,216 tools
from 131 servers, which nobody runs. With ~10 servers and ~200 tools, geco can
skip BM25F entirely and hand the picker the *whole* catalogue — recall becomes
100%, and expected rank-1 is the precision figure, **73–87%**. That is the
no-geco baseline (an LLM choosing from everything it can see) delivered at
geco's fixed ~4 KB context cost.

Caveats worth keeping: 50-query sample; a small local model (a frontier model
should do better on precision); several scored "misses" are label ambiguity
(`search` vs `search_engine`, `create_workbook` vs `file`, and one case of the
same tool name on a different server), so true quality is above the headline.

Open design questions, none of them blockers: which client contract (MCP
`sampling/createMessage` is protocol-native but rarely implemented; shelling
out to an installed agent CLI — `opencode run --agent … --format json` — works
today), how to stop a nested geco recursing when the sub-session loads it, and
whether the per-search latency is acceptable as a default or an opt-in.

### Answered, and shipped as `proxy.picker`

All three of the above, resolved by building it:

- **Contract:** both. `sampling` for clients that implement it (zero config,
  the client's own model) and `command` for everything else. `auto` prefers
  sampling and falls back. No HTTP client was added — that would drag in a TLS
  stack, which is the exact objection that sank the embeddings branch.
- **Recursion:** the child is spawned with `GECO_NO_PICKER=1`, so a nested
  geco can refuse to pick.
- **Latency:** opt-in. Off by default, and every failure path degrades to the
  lexical order.

### The serving config beat every ranking change ever measured here

The single most valuable line in this whole document is not a ranking change.
It is an instruction in the prompt telling the model **not to reason**. Same
weights, same prompt, same candidates, only the serving config differing
(measured on `qwen4b`):

| | rank-1 | precision |
|---|---|---|
| thinking on | 8% | 20% |
| thinking off (`/no_think`) | **28%** | **70%** |

It spent **1,125 completion tokens** deliberating over a one-line
multiple-choice question and hit the token ceiling before emitting any JSON.
Picking a tool is classification, not reasoning, and a reasoning-mode model
answers it as though it were reasoning.

Note the direction: **8% is below the 13.3% lexical baseline.** A deliberating
model is worse than no picker at all. That is why the reply budget is sized
for the answer (512 tokens) rather than for the thinking — a model that
ignores the instruction truncates into the lexical fallback instead of being
rewarded with enough room to ramble.

The generalisable lesson, and the reason this sits in the research log rather
than in a code comment: when an LLM underperforms a dumb baseline by 40%, look
at how it is being *served* before you look at the prompt or the model. Two
runs of identical weights differed by 3.5x.

### Not done: auto-scaling `candidates` to the model

50 candidates is roughly a 4,000-token prompt: ~2s against a small model on a
GPU box, ~42s on a handheld. Tempting to shrink it automatically for slow
models. **Rejected for lack of a signal**: under `command` the model is behind
an opaque argv, and under `sampling` the client chooses the model and never
says which. Any auto-shrink would be geco guessing at hardware it cannot see,
and guessing *downward* costs recall — the one thing the measurements say is
the binding constraint. The relationship is documented instead and the knob
left with the user.

## The realistic catalogue — the number that describes a user (2026-07-25)

Everything above is measured against **5,216 tools from 131 servers, which
nobody runs.** A real config is a handful of servers and a few hundred tools.
Optimising against the 131-server number risks solving a problem no user has,
so: for each labeled query, build a catalogue of S servers — the query's own
target server plus S-1 others drawn deterministically — and rank inside it.
Every query stays scorable, so n is the full 210 at every size.

| servers | tools | r@1 | r@5 | r@20 | r@50 | **r@inf** | in-catalogue |
|---|---|---|---|---|---|---|---|
| 4 | 154 | 29.0% | 55.7% | 78.1% | 86.7% | **90.5%** | 100% |
| 8 | 324 | 24.8% | 45.7% | 70.5% | 81.9% | **90.5%** | 100% |
| 12 | 487 | 22.9% | 40.5% | 63.8% | 77.6% | **90.5%** | 100% |
| 20 | 805 | 21.9% | 38.6% | 55.7% | 69.5% | **90.5%** | 100% |
| 40 | 1589 | 18.6% | 31.0% | 48.6% | 59.0% | **90.5%** | 100% |
| 131 (all) | 5216 | 13.3% | 22.4% | 32.9% | 44.3% | **90.5%** | 100% |

The bottom row reproduces the published 13.3% / 44.3% exactly — that is the
control, and it says the slicing harness is sound.

**Lexical rank-1 barely moves** (13.3% → 29.0% across a 34x corpus shrink):
the semantic wall is a property of the query-tool pair, not of the corpus size.
**Recall nearly doubles** (44.3% → 81.9% at eight servers), because there are
simply fewer distractors above the right answer. Everything downstream of
retrieval gets easier on a real config; ranking itself does not.

### The 9.5% floor: `r@inf` is the same at every size

`rank_with` drops any document scoring `<= 0` (`search.rs`), so a tool sharing
no term with the query is not a badly-ranked candidate — **it is not a
candidate**. `r@inf` measures how often the target scores at all, and it is
**90.5% at every catalogue size**, because zero-overlap is a property of the
pair and nothing else.

That is the hard floor under every downstream idea. A picker, a reranker and a
cross-encoder alike can only choose from what retrieval hands over, so **no
amount of K, and no better picker, gets past 90.5%** while BM25F is the
retrieval step. Only two things do: rewriting the documents so they *do* share
terms, or skipping lexical retrieval and offering the whole catalogue.

Read the 20 unreachable pairs (`cargo test … unreachable_queries`) before
designing anything to fix them; they are the spec. The pattern is one thing,
almost without exception — **the user names a goal, the tool names its
mechanism**:

| the request | the tool it needed | what it says about itself |
|---|---|---|
| convert one webpage into markdown | `firecrawl_scrape` | "Scrape content from a single URL" |
| put a sum calculation into a spreadsheet | `apply_formula` | "Apply Excel formula to cell." |
| look up someone's professional background | `get_person_profile` | "Get a specific person's LinkedIn profile." |
| pull records from the orders table | `mysql_query` | "Run SQL queries against MySQL database" |
| begin designing a PCB from scratch | `create_project` | "Create a new KiCAD project" |
| which ports are open on a machine | `nmap_scan` | "Execute an Nmap scan against a target." |
| put a box onto my flowchart | `add-rectangle` | "add a new Rectangle vertex cell … Draw.io" |

No synonym table reaches these, and that is not a gap in the table: several
need *domain knowledge* rather than a word mapping (KiCAD is for PCBs, nmap
scans ports). But every one of them is obvious to a model that reads the
description — which is precisely why the picker works on the 90.5% that do
score, and precisely why the fix for the remaining 9.5% is to expand the
documents rather than the query.

### The picker on a realistic catalogue: 74.6%

Same shipped prompt, same `parse_choice`, `qwen-coder` on the Spark, K=50,
eight-server slices, n=209:

| | full corpus (131 servers) | **realistic (8 servers)** |
|---|---|---|
| lexical rank-1 | 13.3% | 24.8% |
| candidates reachable | 44.3% | **82.3%** |
| **picker rank-1** | ~32% | **74.6%** |
| precision when reachable | 87% | **90.7%** |
| latency | ~2s | 2.2s |

**The number a real user experiences is ~75%, not 40%**, and precision *rises*
on a smaller catalogue (90.7% vs 87%) because the distractors are fewer and
less confusable. Treat 74.6% as a **floor**: `qwen-coder` is a small local
model, and the frontier-model figure on the hard corpus was already better.

### The width ladder: BM25F is the problem, not the solution

Same slices, same model, same prompt, same parser. Only how many candidates the
picker is handed changes. n=209/210.

*(Beware a coincidence in this document: `r@inf` above is 90.5%, and the
whole-catalogue rank-1 below was also 90.5% before the prompt reorder. They
are unrelated quantities that happened to land on the same number.)*

| candidates | mean | reachable | **rank-1** | precision | s/query |
|---|---|---|---|---|---|
| K=50 (the old default) | 47 | 82.3% | 74.6% | 90.7% | 2.2 |
| K=∞ — every tool that scores | 141 | 90.9% | 78.5% | 86.3% | 6.5 |
| **the whole catalogue, no BM25F** | 324 | 100% | **90.5%** | **90.5%** | 10.3 |
| **…and with the prompt reordered** | 324 | 100% | **94.8%** | **94.8%** | 10.1 |

**Handing over everything is 16 points better than the shipped default**, and
the reason is not the one predicted. Precision was expected to collapse under
7x the distractors — the full-corpus measurement had it falling 87% → 73%
between K=50 and K=200. It did not fall at all: **90.5% at 324 candidates
versus 90.7% at 47.** Unparsable replies went *down*, to zero.

The explanation is worth keeping, because it inverts the usual intuition about
retrieval. **BM25F does not select random distractors — it selects the most
confusable ones.** Every one of its 50 candidates was chosen for sharing
vocabulary with the query, so the shortlist is precisely the set of
plausible-looking wrong answers. The other 274 tools in the catalogue are
mostly obvious non-answers that a model discards at a glance. Pre-filtering
lexically concentrates the hard negatives and throws away the easy ones,
which is the worst possible trade to hand a model that is good at semantics
and bad at nothing else.

Note also that the old 87% → 73% precision curve was measured drawing 200
candidates from **5,216 diverse tools**. Those 200 were confusable in a way a
user's own 324 are not. That number does not transfer, and this is why the
realistic corpus had to be measured rather than extrapolated to.

So the ranking of options, for a catalogue that fits:

1. **Send everything** — 94.8%. No retrieval step, no ranking, no tuning.
2. K=∞ — 78.5%, and 90.9% of the ceiling is wasted on lexical filtering.
3. K=50 — 74.6%, fastest.
4. Lexical alone — 24.8%.

### And it is also the cheapest, once the prompt is in the right order

The obvious objection to sending everything is cost: ~324 tools is ~15k tokens
per search, and the first measurement above took 10.3s a query. Both evaporate
under prompt caching, and the reason is the same property that makes the mode
work at all — **in whole-catalogue mode the candidate list is identical on
every search**, so it is a reusable prefix. A BM25F shortlist never is: it is
recomputed per query by construction.

Measured against a real endpoint, 245 tools / 15,243 prompt tokens, six
different queries:

| | first call | subsequent | prompt tokens cached |
|---|---|---|---|
| **stable prefix** (catalogue first) | 53s (cold model load) | **0.8–1.2s** | **14,727 of 15,243 (96.6%)** |
| prefix reordered per call | 12.9s | 12.8–13.0s | 0 |

**13x faster, and 96.6% of the prompt free.** So whole-catalogue mode is not a
quality-for-cost trade at all — against the K=50 shortlist it is *both* 16
points more accurate *and* faster (0.9s against 2.2s), because the shortlist
cannot cache.

This only works if the catalogue leads. `Picker::prompt` originally put the
query first, which puts every byte of the catalogue after variable text and
makes the whole thing uncacheable. The prompt is now split into
`catalogue_block` (stable, cacheable) and `question_block` (per-query), with a
test pinning that no query text leaks into the first — the failure mode is
silent, since the answers stay correct and only the bill changes.

**And the reorder was not accuracy-neutral: it gained 4.3 points**, 90.5% →
**94.8%**, on the same model, the same candidates and the same wording. Only
the order of the two blocks changed. Not predicted, and not why it was done —
the reorder was for caching, and the accuracy was re-measured only because
changing a measured prompt invalidates its number. Plausibly recency: with the
question last it is the freshest thing in context when the model answers,
where question-first lets it start forming an answer before it has seen the
options. Worth remembering as a class of change — **prompt *layout* moved this
corpus further than any scoring change ever did.**

One caveat on the two latency figures, which are not comparable: the 10.1s
above is the scoring harness, where every one of the 210 cases has a different
8-server slice, so no prefix is ever reused. The 0.9s is one fixed catalogue
across queries, which is what a real deployment looks like. Both are correct
for what they measure.

**Still open: where it stops fitting.** 324 tools works. The 20-server slice is
805 and the 40-server slice 1,589; both need measuring before the default
budget is treated as anything but a guess, and that is where a two-stage
server-then-tool pick would earn its keep if it does.

### `/no_think` is not a control — the serving config is

Measured while setting the above up, and it invalidates an assumption in the
row above about how the 28%/8% split was obtained. Against the Spark's
`qwen4b`, **7 of 8 searches returned entirely empty content**: the whole
512-token budget went to a hidden `reasoning_content` channel before any JSON
was emitted. Neither `/no_think` in the prompt nor
`chat_template_kwargs.enable_thinking = false` on the request changed it.

**Whether a model deliberates is decided by the server's chat template, and a
client cannot reliably override it.** The 28% figure came from a serving config
where thinking was already off; that was never a property of the prompt. The
prompt instruction is kept because it is free and helps where it is honoured,
but it is not a guarantee, and the difference matters because the symptom is
*silence* — identical results, seconds slower, no error, indistinguishable
from a picker that agrees with lexical ranking. geco now detects it
(`maxTokens` with empty content, or three consecutive empty picks) and says so.

The budget is deliberately not raised to accommodate a thinking model: at 8%
rank-1 it is worse than not asking. The fix belongs in the serving config.

## The fork — closed

Crossing the wall means giving up something geco is built on. The choice is
therefore explicit, not incremental:

1. **Accept the ceiling.** geco is a *lexical* gateway: microseconds, zero
   dependencies, nothing to invalidate. Mitigate with what already works — the
   "always search first" instruction, `proxy.search_synonyms`, and pushing
   backends to write descriptions in the words users actually use. No pretence about
   what it is.
2. **Cross it with embeddings.** An opt-in semantic backend behind a feature
   flag, lexical staying the default. Real work and a real optional dependency —
   but now there is a scoreboard (`labeled_eval`) to prove whether it earns its
   cost *before* committing.

   **Measured, 2026-07 (branch `feat/embeddings-done-right`, well-packaged:
   opt-in, rustls, model downloaded not committed):** BM25F → embedding rerank
   with all-MiniLM-L6-v2 (quantized) does **not** cross the wall. On the
   210-query real corpus, BM25+embeddings scored recall@1 **12.9%** vs
   BM25-only's **13.3%** — *worse* — and recall@5 was **identical at 22.4%**.
   That last number is the point: a reranker only reorders the top-K BM25F
   already retrieved, so it cannot lift recall@5, and the wall is precisely the
   tools BM25F never surfaces. This does not close the embeddings fork forever —
   it rules out *rerank* with a *small general model*. Actually crossing the
   wall would need embedding-based **retrieval** over all tools (not rerank) and
   probably a stronger model — a much larger bet, now taken against evidence
   that the cheap version buys nothing (and costs an onnxruntime dependency in
   every build).

The cheap middle paths were investigated and ruled out (above). This is a
genuine architecture decision, and it now rests on measured evidence rather than
hope.

**Attempted, and rejected as packaged (branch `fix/search-generalization`,
2026-07):** a prototype wired in `fastembed` / all-MiniLM-L6-v2 as a BM25F →
embedding rerank. The *architecture* was right — the exact option 2 above,
with graceful fallback to lexical. But it overshot everything that makes geco
geco: it committed the 22 MB model into git (the download cache), pulled
onnxruntime *and* an OpenSSL system dependency (it would not compile without
`libssl-dev`), defaulted the heavy path ON, and deleted this very document and
the eval harness. The one number that decides the fork — does rerank beat 13%
on `labeled_eval`? — was never obtained, because it would not build. Kept as
proof the wiring is possible; the branch itself was deleted. If the fork is
taken, build it opt-in (off by default, `rustls` not OpenSSL, model downloaded
not committed, harness kept) and **measure before committing to onnxruntime.**

---

## Reproducing the measurements

The toy-corpus eval runs in CI:

```bash
cargo test --test search_eval -- --nocapture
```

The real-corpus measurements need the harvested data (large, third-party, not in
the repo — regenerate with the harvest workflow, or ask the maintainer). They
no-op without the env vars, so CI stays green:

```bash
GECO_HARVEST=~/geco-harvest/main149-result.json \
GECO_LABELED=~/geco-harvest/labeled_eval.json \
  cargo test --test real_search_probe -- --nocapture
```

`probe_real_tools` prints top-5 for hand-picked queries; `labeled_eval` prints
rank-1/MRR over the full corpus, split by generator model.
