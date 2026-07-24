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

## The fork (open decision)

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

The cheap middle paths were investigated and ruled out (above). This is a
genuine architecture decision, and it now rests on measured evidence rather than
hope.

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
