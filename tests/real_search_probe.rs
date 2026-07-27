//! Probe geco's search against REAL harvested MCP tools.
//!
//! Not a gate — an observatory. Every other search test runs on 8 fictional
//! servers whose descriptions I wrote. This one loads the manifests harvested
//! from ~150 real, popular MCP servers (5000+ tools) and prints where geco
//! ranks the tool a realistic query is aiming at, so "how does this actually
//! do in the wild?" can be answered by looking rather than guessing.
//!
//! Set GECO_HARVEST to the harvested result JSON to run it:
//!   GECO_HARVEST=~/geco-harvest/main149-result.json \
//!     cargo test --test real_search_probe -- --nocapture
//! Without the env var it no-ops, so CI (which has no corpus) stays green.

use geco::snapshot::{ServerSnapshot, Snapshot, ToolSnapshot};

// The task-output file wraps the workflow return under a `result` key
// ({summary, agentCount, logs, result: {...}}). The raw workflow return has
// `manifests` at top level. Accept either by making the wrapper optional.
#[derive(serde::Deserialize)]
struct Wrapper {
    #[serde(default)]
    result: Option<Inner>,
    #[serde(default)]
    manifests: Option<Vec<Manifest>>,
}

#[derive(serde::Deserialize)]
struct Inner {
    manifests: Vec<Manifest>,
}

struct HarvestResult {
    manifests: Vec<Manifest>,
}

impl HarvestResult {
    fn from_str(raw: &str) -> Self {
        let w: Wrapper = serde_json::from_str(raw).expect("harvest JSON must parse");
        let manifests = w
            .result
            .map(|r| r.manifests)
            .or(w.manifests)
            .expect("no manifests found (neither top-level nor under `result`)");
        HarvestResult { manifests }
    }
}

#[derive(serde::Deserialize)]
struct Manifest {
    repo: String,
    #[serde(default)]
    tools: Vec<HTool>,
}

#[derive(serde::Deserialize)]
struct HTool {
    name: String,
    #[serde(default)]
    description: String,
}

fn load() -> Option<Snapshot> {
    let path = std::env::var("GECO_HARVEST").ok()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let parsed = HarvestResult::from_str(&raw);

    let mut snap = Snapshot::default();
    let mut total = 0usize;
    for m in parsed.manifests {
        if m.tools.is_empty() {
            continue;
        }
        // Strip the "github.com/" the agents prefixed, keep "owner/name".
        let server = m.repo.trim_start_matches("github.com/").to_string();
        let tools: Vec<ToolSnapshot> = m
            .tools
            .into_iter()
            .map(|t| ToolSnapshot {
                name: t.name,
                description: t.description,
                input_schema: serde_json::json!({}),
            })
            .collect();
        total += tools.len();
        snap.servers.insert(
            server,
            ServerSnapshot {
                tools,
                refresh_status: "ok".into(),
                ..Default::default()
            },
        );
    }
    eprintln!(
        "\nloaded {} real servers, {} tools total\n",
        snap.servers.len(),
        total
    );
    Some(snap)
}

/// Emit a diverse sample of real tools as JSON, for a swarm to build labeled
/// queries from. Spread across servers (a few per server) so the corpus
/// covers many domains, and skip trivial/empty descriptions since a query
/// can't be grounded in a tool that doesn't describe itself.
///
///   GECO_HARVEST=~/geco-harvest/main149-result.json \
///   GECO_SAMPLE_OUT=/mnt/c/.../sample_tools.json \
///     cargo test --test real_search_probe emit_tool_sample -- --nocapture
#[test]
fn emit_tool_sample() {
    let Some(out) = std::env::var("GECO_SAMPLE_OUT").ok() else {
        eprintln!("GECO_SAMPLE_OUT not set — skipping sampler");
        return;
    };
    let path = std::env::var("GECO_HARVEST").expect("GECO_HARVEST required to sample");
    let raw = std::fs::read_to_string(&path).expect("read harvest");
    let parsed = HarvestResult::from_str(&raw);

    let per_server = std::env::var("GECO_SAMPLE_PER_SERVER")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3usize);

    let mut sample: Vec<serde_json::Value> = Vec::new();
    for m in &parsed.manifests {
        let server = m.repo.trim_start_matches("github.com/").to_string();
        let n = m.tools.len();
        if n == 0 {
            continue;
        }
        // All sibling names, so the query-generator can be told to target one
        // tool distinguishably from the rest of its server.
        let siblings: Vec<&str> = m.tools.iter().map(|t| t.name.as_str()).collect();
        let mut taken = 0;
        // Deterministic spread: stride through the tool list rather than
        // taking the first few, so we don't only sample a server's A-tools.
        let stride = (n / per_server).max(1);
        let mut i = 0;
        while i < n && taken < per_server {
            let t = &m.tools[i];
            let d = t.description.trim();
            // Printable-ASCII descriptions only: geco's tokenizer is English,
            // and non-ASCII/control chars break the workflow approval dialog.
            let clean = d.chars().all(|c| c.is_ascii_graphic() || c == ' ')
                && t.name.chars().all(|c| c.is_ascii_graphic());
            if d.len() >= 25 && clean {
                let sibs: Vec<&&str> = siblings
                    .iter()
                    .filter(|s| **s != t.name.as_str())
                    .take(12)
                    .collect();
                sample.push(serde_json::json!({
                    "server": server,
                    "name": t.name,
                    "description": d.chars().take(200).collect::<String>(),
                    "siblings": sibs,
                }));
                taken += 1;
            }
            i += stride;
        }
    }
    std::fs::write(&out, serde_json::to_string(&sample).unwrap()).expect("write sample");
    eprintln!("wrote {} sampled tools to {out}", sample.len());
}

// ---- Labeled real-corpus eval ---------------------------------------------
// The scoreboard the whole harvest was for: real semantic queries (generated
// by a multi-model swarm, vocab-banned, cross-model verified) against the full
// 5000+ real-tool corpus. Measures where geco ranks the ONE tool each query
// was written to find. Split by generator model to see if geco favours one
// phrasing style. Set GECO_LABELED to the eval JSON to run.

#[derive(serde::Deserialize)]
struct LabeledWrapper {
    #[serde(default)]
    result: Option<LabeledInner>,
    #[serde(default)]
    queries: Option<Vec<LQ>>,
}
#[derive(serde::Deserialize)]
struct LabeledInner {
    queries: Vec<LQ>,
}
#[derive(serde::Deserialize, Clone)]
struct LQ {
    query: String,
    server: String,
    tool: String,
    #[serde(default)]
    gen_model: String,
}

#[derive(Default)]
struct EvalStat {
    n: usize,
    hit1: usize,
    hit3: usize,
    hit5: usize,
    rr: f64,
}
impl EvalStat {
    fn record(&mut self, rank: Option<usize>) {
        self.n += 1;
        if let Some(r) = rank {
            if r == 0 {
                self.hit1 += 1;
            }
            if r < 3 {
                self.hit3 += 1;
            }
            if r < 5 {
                self.hit5 += 1;
            }
            self.rr += 1.0 / (r as f64 + 1.0);
        }
    }
    fn line(&self, label: &str) -> String {
        let p = |k: usize| {
            if self.n == 0 {
                0.0
            } else {
                k as f64 * 100.0 / self.n as f64
            }
        };
        format!(
            "{label:10} n={:<4} recall@1={:5.1}%  recall@3={:5.1}%  recall@5={:5.1}%  MRR={:.3}",
            self.n,
            p(self.hit1),
            p(self.hit3),
            p(self.hit5),
            if self.n == 0 {
                0.0
            } else {
                self.rr / self.n as f64
            }
        )
    }
}

/// How wide must the BM25F net be before the right tool is even IN it?
///
/// This is the ceiling for any rerank strategy — an LLM (or embedding) picker
/// can only choose from the candidates it is handed, so recall@K bounds it
/// absolutely. recall@5 is 22%, which is why embedding rerank could not move
/// rank-1. The question this answers: does a wider net (K=50, 100) contain the
/// answer often enough to make an LLM picker worth the round-trip?
#[test]
fn recall_at_k_ceiling() {
    let Some(snap) = load() else {
        eprintln!("GECO_HARVEST not set — skipping");
        return;
    };
    let Some(lpath) = std::env::var("GECO_LABELED").ok() else {
        eprintln!("GECO_LABELED not set — skipping");
        return;
    };
    let raw = std::fs::read_to_string(&lpath).expect("read labeled");
    let w: LabeledWrapper = serde_json::from_str(&raw).expect("labeled JSON parses");
    let queries: Vec<LQ> = w
        .result
        .map(|r| r.queries)
        .or(w.queries)
        .expect("no queries");

    let ks = [1usize, 5, 10, 20, 50, 100, 200];
    let max_k = *ks.iter().max().unwrap();
    let mut hits = vec![0usize; ks.len()];

    for q in &queries {
        let res = geco::search::search(&snap, &q.query, max_k);
        if let Some(pos) = res
            .iter()
            .position(|h| h.server == q.server && h.tool == q.tool)
        {
            for (i, k) in ks.iter().enumerate() {
                if pos < *k {
                    hits[i] += 1;
                }
            }
        }
    }

    eprintln!(
        "\n=== BM25F recall@K ceiling ({} queries, {} tools) ===",
        queries.len(),
        snap.servers.values().map(|s| s.tools.len()).sum::<usize>()
    );
    for (i, k) in ks.iter().enumerate() {
        eprintln!(
            "  recall@{:<4} {:5.1}%   ({} of {})",
            k,
            hits[i] as f64 * 100.0 / queries.len() as f64,
            hits[i],
            queries.len()
        );
    }
    eprintln!("\nA reranker (LLM or embedding) fed top-K can never beat recall@K.\n");
}

/// Build the catalogue one query is asked against: the target's own server
/// plus `slice_servers - 1` others, drawn deterministically so the same query
/// always meets the same neighbours and a change in ranking is the only thing
/// that can move a number. `slice_servers >= servers.len()` returns everything.
fn slice_for(
    full: &Snapshot,
    servers: &[String],
    q: &LQ,
    qi: usize,
    slice_servers: usize,
) -> Snapshot {
    if slice_servers >= servers.len() {
        return full.clone();
    }
    let mut state = (qi as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let mut chosen: Vec<&str> = Vec::new();
    if full.servers.contains_key(&q.server) {
        chosen.push(q.server.as_str());
    }
    let mut guard = 0;
    while chosen.len() < slice_servers && guard < slice_servers * 64 {
        guard += 1;
        let cand = servers[next() % servers.len()].as_str();
        if !chosen.contains(&cand) {
            chosen.push(cand);
        }
    }
    let mut slice = Snapshot::default();
    for s in chosen {
        if let Some(v) = full.servers.get(s) {
            slice.servers.insert(s.to_string(), v.clone());
        }
    }
    slice
}

/// Emit `{query, expected, candidates[]}` JSONL so an external LLM picker can
/// be scored against the same labeled corpus. K and sample size from env.
///
/// `GECO_PICKER_SERVERS=8` scores against a REALISTIC catalogue instead of all
/// 131 servers — the condition an actual user is in, and the one the published
/// 40% figure does not describe.
#[test]
fn emit_picker_candidates() {
    let Some(out) = std::env::var("GECO_PICKER_OUT").ok() else {
        eprintln!("GECO_PICKER_OUT not set — skipping");
        return;
    };
    let snap = load().expect("GECO_HARVEST required");
    let lpath = std::env::var("GECO_LABELED").expect("GECO_LABELED required");
    let raw = std::fs::read_to_string(&lpath).expect("read labeled");
    let w: LabeledWrapper = serde_json::from_str(&raw).expect("labeled JSON parses");
    let queries: Vec<LQ> = w
        .result
        .map(|r| r.queries)
        .or(w.queries)
        .expect("no queries");

    let k: usize = std::env::var("GECO_PICKER_K")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let take: usize = std::env::var("GECO_PICKER_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    let slice_servers: usize = std::env::var("GECO_PICKER_SERVERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let all_servers: Vec<String> = snap.servers.keys().cloned().collect();

    // Stride across the corpus so the sample spans models and domains.
    let stride = (queries.len() / take).max(1);
    let mut lines = Vec::new();
    let mut reachable = 0usize;
    let mut cand_total = 0usize;
    // `GECO_PICKER_ALL=1` skips BM25F and offers the picker the ENTIRE
    // catalogue. Not a tuning knob — a different architecture. Lexical
    // retrieval drops any tool scoring zero, which caps recall at 90.5%
    // regardless of K; handing over everything is the only way to reach 100%,
    // and the question is what the extra distractors cost in precision.
    let send_all = std::env::var("GECO_PICKER_ALL").is_ok();

    for (qi, q) in queries.iter().enumerate().step_by(stride).take(take) {
        let catalogue = slice_for(&snap, &all_servers, q, qi, slice_servers);
        let hits: Vec<geco::search::Hit> = if send_all {
            // Catalogue order: stable, and arbitrary with respect to the query,
            // so no ranking signal leaks in through position.
            catalogue
                .servers
                .iter()
                .flat_map(|(server, s)| {
                    s.tools.iter().map(move |t| geco::search::Hit {
                        server: server.clone(),
                        tool: t.name.clone(),
                        description: geco::snapshot::short_desc(&t.description, 140),
                        score: 0.0,
                        coverage: 0.0,
                    })
                })
                .collect()
        } else {
            geco::search::search(&catalogue, &q.query, k)
        };
        // Whether the picker even CAN be right on this case. Scoring without
        // this conflates "the model chose badly" with "the answer was not on
        // the menu", which are different problems with different fixes.
        if hits
            .iter()
            .any(|h| h.server == q.server && h.tool == q.tool)
        {
            reachable += 1;
        }
        cand_total += hits.len();
        let cands: Vec<serde_json::Value> = hits
            .into_iter()
            .map(|h| {
                serde_json::json!({
                    "server": h.server,
                    "tool": h.tool,
                    "description": h.description,
                })
            })
            .collect();
        lines.push(
            serde_json::json!({
                "query": q.query,
                "expected_server": q.server,
                "expected_tool": q.tool,
                "candidates": cands,
            })
            .to_string(),
        );
    }
    let n = lines.len();
    std::fs::write(&out, lines.join("\n")).expect("write picker candidates");
    eprintln!(
        "wrote {n} picker cases (K={k}, servers={}) to {out}\n  \
         mean candidates {:.0}, answer reachable in {:.1}% — that is the ceiling \
         for any picker scored on this file",
        if slice_servers == usize::MAX {
            "all".to_string()
        } else {
            slice_servers.to_string()
        },
        cand_total as f64 / n as f64,
        reachable as f64 * 100.0 / n as f64,
    );
}

/// What does search do on a catalogue somebody actually runs?
///
/// Every number this project has published about ranking is measured against
/// 5,216 tools from 131 servers, which is nobody's config. A real one is a
/// handful of servers and a few hundred tools — a corpus 20x smaller, with 20x
/// fewer distractors. Optimising against the 131-server number without knowing
/// the realistic one risks solving a problem no user has.
///
/// Method: for each labeled query, build a catalogue of S servers consisting of
/// the query's target server plus S-1 others drawn deterministically from the
/// harvest, then rank within it. Every query stays scorable, so n is the full
/// 210 at every size rather than a thinning subset.
///
/// Two caveats worth keeping, in the direction of "this is optimistic":
/// the co-installed servers are drawn at random across 131 diverse repos,
/// while a real config is topically clustered (a developer installs github
/// AND gitlab AND a git server), which packs in more near-duplicates than
/// random sampling does. And the queries are vocabulary-banned by
/// construction, which is the hard end of real traffic.
///
///   GECO_HARVEST=… GECO_LABELED=… \
///     cargo test --test real_search_probe realistic_catalogue -- --nocapture
#[test]
fn realistic_catalogue_slices() {
    let Some(full) = load() else {
        eprintln!("GECO_HARVEST not set — skipping");
        return;
    };
    let Some(lpath) = std::env::var("GECO_LABELED").ok() else {
        eprintln!("GECO_LABELED not set — skipping");
        return;
    };
    let raw = std::fs::read_to_string(&lpath).expect("read labeled");
    let w: LabeledWrapper = serde_json::from_str(&raw).expect("labeled JSON parses");
    let queries: Vec<LQ> = w
        .result
        .map(|r| r.queries)
        .or(w.queries)
        .expect("no queries");

    let servers: Vec<String> = full.servers.keys().cloned().collect();
    // The last K is effectively unbounded, and it measures the thing K cannot
    // fix: `rank_with` drops any document scoring <= 0, so a tool sharing no
    // term with the query is not a badly-ranked candidate — it is NOT A
    // CANDIDATE. The gap between `r@inf` and `in-cat` is the share of targets
    // lexical retrieval cannot hand a picker at ANY width, and therefore the
    // hard ceiling on everything built downstream of it.
    let ks = [1usize, 5, 10, 20, 50, usize::MAX];

    eprintln!(
        "\n=== Realistic-catalogue slices ({} queries) ===",
        queries.len()
    );
    eprintln!(
        "{:>8}  {:>7}  {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}  {:>7}",
        "servers", "tools", "r@1", "r@5", "r@10", "r@20", "r@50", "r@inf", "in-cat"
    );

    for slice_servers in [4usize, 8, 12, 20, 40, servers.len()] {
        let mut hits = vec![0usize; ks.len()];
        let mut in_catalogue = 0usize;
        let mut tool_total = 0usize;

        for (qi, q) in queries.iter().enumerate() {
            let slice = slice_for(&full, &servers, q, qi, slice_servers);
            tool_total += slice.servers.values().map(|s| s.tools.len()).sum::<usize>();

            // Is the target even present? It is, by construction, unless the
            // labeled set names a server the harvest does not have.
            if slice
                .servers
                .get(&q.server)
                .is_some_and(|s| s.tools.iter().any(|t| t.name == q.tool))
            {
                in_catalogue += 1;
            }

            let res = geco::search::search(&slice, &q.query, *ks.iter().max().unwrap());
            if let Some(pos) = res
                .iter()
                .position(|h| h.server == q.server && h.tool == q.tool)
            {
                for (i, k) in ks.iter().enumerate() {
                    if pos < *k {
                        hits[i] += 1;
                    }
                }
            }
        }

        let n = queries.len() as f64;
        eprintln!(
            "{:>8}  {:>7.0}  {:>6.1}% {:>6.1}% {:>6.1}% {:>6.1}% {:>6.1}% {:>6.1}%  {:>6.1}%",
            chosen_label(slice_servers, servers.len()),
            tool_total as f64 / n,
            hits[0] as f64 * 100.0 / n,
            hits[1] as f64 * 100.0 / n,
            hits[2] as f64 * 100.0 / n,
            hits[3] as f64 * 100.0 / n,
            hits[4] as f64 * 100.0 / n,
            hits[5] as f64 * 100.0 / n,
            in_catalogue as f64 * 100.0 / n,
        );
    }
    eprintln!(
        "\n`tools` is the mean catalogue size. `in-cat` is the share of queries whose\n\
         target is present at all — the ceiling for handing the picker EVERYTHING.\n\
         Predicted picker accuracy = recall@K x precision(K); measured precision is\n\
         87% at K=50 and 73% at K=200 distractors (docs/search-ranking.md).\n"
    );
}

/// The 9.5% that no value of K reaches: queries whose target tool scores
/// exactly zero, so `rank_with` never emits it as a candidate at all.
///
/// This is the hard floor under every downstream idea — a picker, a reranker
/// and an embedding cross-encoder alike can only choose from what retrieval
/// hands over. Printing the actual pairs is the point: they are the spec for
/// whatever is meant to fix them, and reading twenty of them says more about
/// whether description rewriting would work than any amount of argument.
#[test]
fn unreachable_queries() {
    let Some(full) = load() else {
        eprintln!("GECO_HARVEST not set — skipping");
        return;
    };
    let Some(lpath) = std::env::var("GECO_LABELED").ok() else {
        eprintln!("GECO_LABELED not set — skipping");
        return;
    };
    let raw = std::fs::read_to_string(&lpath).expect("read labeled");
    let w: LabeledWrapper = serde_json::from_str(&raw).expect("labeled JSON parses");
    let queries: Vec<LQ> = w
        .result
        .map(|r| r.queries)
        .or(w.queries)
        .expect("no queries");

    let mut unreachable = Vec::new();
    for q in &queries {
        // One server only: the smallest possible catalogue. If the target does
        // not score even when it is competing with nothing but its own
        // siblings, the query and the tool share no term at all — a corpus-size
        // effect is ruled out by construction.
        let mut solo = Snapshot::default();
        if let Some(s) = full.servers.get(&q.server) {
            solo.servers.insert(q.server.clone(), s.clone());
        }
        let res = geco::search::search(&solo, &q.query, usize::MAX);
        if !res.iter().any(|h| h.tool == q.tool) {
            let desc = full
                .servers
                .get(&q.server)
                .and_then(|s| s.tools.iter().find(|t| t.name == q.tool))
                .map(|t| t.description.clone())
                .unwrap_or_default();
            unreachable.push((q.query.clone(), q.tool.clone(), desc));
        }
    }

    eprintln!(
        "\n=== Zero-score targets: {} of {} ({:.1}%) ===",
        unreachable.len(),
        queries.len(),
        unreachable.len() as f64 * 100.0 / queries.len() as f64
    );
    eprintln!("Alone with its own siblings, the right tool still does not score.\n");
    for (q, tool, desc) in unreachable.iter().take(25) {
        eprintln!("  query: {q}");
        eprintln!("  tool:  {tool}");
        eprintln!(
            "  desc:  {}\n",
            desc.chars()
                .take(120)
                .collect::<String>()
                .replace('\n', " ")
        );
    }
}

fn chosen_label(n: usize, total: usize) -> String {
    if n >= total {
        format!("{n} (all)")
    } else {
        n.to_string()
    }
}

#[test]
fn labeled_eval() {
    let Some(snap) = load() else {
        eprintln!("GECO_HARVEST not set — skipping labeled eval");
        return;
    };
    let Some(lpath) = std::env::var("GECO_LABELED").ok() else {
        eprintln!("GECO_LABELED not set — skipping labeled eval");
        return;
    };
    let raw = std::fs::read_to_string(&lpath).expect("read labeled");
    let w: LabeledWrapper = serde_json::from_str(&raw).expect("labeled JSON parses");
    let queries: Vec<LQ> = w
        .result
        .map(|r| r.queries)
        .or(w.queries)
        .expect("no queries");

    let mut overall = EvalStat::default();
    let mut by_model: std::collections::BTreeMap<String, EvalStat> = Default::default();
    let mut misses: Vec<String> = Vec::new();

    for q in &queries {
        let hits = geco::search::search(&snap, &q.query, 5);
        let rank = hits
            .iter()
            .position(|h| h.server == q.server && h.tool == q.tool);
        overall.record(rank);
        by_model
            .entry(q.gen_model.clone())
            .or_default()
            .record(rank);
        if rank.is_none() {
            let got: Vec<String> = hits.iter().take(3).map(|h| h.tool.clone()).collect();
            misses.push(format!(
                "  [{}] {:52} want {}::{}  got {:?}",
                q.gen_model, q.query, q.server, q.tool, got
            ));
        }
    }

    eprintln!(
        "\n=== LABELED real-corpus eval ({} queries) ===",
        queries.len()
    );
    eprintln!("{}", overall.line("overall"));
    for (m, s) in &by_model {
        eprintln!("{}", s.line(m));
    }
    eprintln!("\nrank-1 misses ({}):", misses.len());
    for m in misses.iter().take(40) {
        eprintln!("{m}");
    }
    eprintln!();
}

/// Show the top-5 hits for a realistic query across the WHOLE corpus (every
/// harvested server at once — the real condition geco runs in). The comment
/// on each says what we're watching for.
#[test]
fn probe_real_tools() {
    let Some(snap) = load() else {
        eprintln!("GECO_HARVEST not set — skipping real-tool probe");
        return;
    };

    // (query, what we're testing). No hardcoded "expected" — the point is to
    // LOOK at what ranks, because I might guess the real tool name wrong.
    let probes: &[(&str, &str)] = &[
        (
            "create a new github issue",
            "verb→noun migration: issue_write has no 'create' in its name",
        ),
        (
            "search for code on github",
            "one of 7 search_* siblings must win on the noun",
        ),
        ("merge a pull request", "clean case — should be easy"),
        (
            "send an email",
            "sim=1.0 collision: send-mail vs send-shared-mailbox-mail — only the NAME differs",
        ),
        (
            "create a spark batch",
            "sim=1.0: create_spark_batch vs create_pyspark_batch",
        ),
        (
            "take a screenshot of the page",
            "cross-server: many servers have a screenshot tool",
        ),
        (
            "run a database aggregation",
            "mongodb aggregate vs aggregate-db",
        ),
        (
            "get amazon product reviews",
            "brightdata: web_data_amazon_product vs _product_reviews (0.96)",
        ),
        (
            "scrape a web page",
            "synonym candidate scrape/crawl — does firecrawl_scrape rank?",
        ),
        (
            "list kubernetes pods",
            "kubectl/k8s servers — generic verb+noun",
        ),
        (
            "what stocks am I holding",
            "pure semantic — no shared vocabulary with any tool name",
        ),
        ("delete a file", "common verb across many servers"),
        // UPSIDE probes: the query uses the user's word, the target tool uses
        // the OTHER word — this is the only case a synonym can actually help.
        (
            "crawl a website",
            "crawl->scrape: should surface firecrawl_scrape / scrape_url_elements",
        ),
        (
            "verify a workflow",
            "verify->validate: should surface validate_workflow",
        ),
        (
            "compile the project",
            "compile->build: should surface build_project",
        ),
        (
            "duplicate a component",
            "duplicate->copy: should surface a copy/duplicate tool",
        ),
        (
            "run the test suite",
            "run->execute: should surface run_tests / execute tools",
        ),
    ];

    eprintln!("=== geco search over the real harvested corpus (top 5) ===\n");
    for (q, note) in probes {
        let hits = geco::search::search(&snap, q, 5);
        eprintln!("Q: {q:38}  // {note}");
        if hits.is_empty() {
            eprintln!("    (nothing matched)");
        }
        for (i, h) in hits.iter().enumerate() {
            eprintln!("    {}. {}::{}", i + 1, h.server, h.tool);
        }
        eprintln!();
    }
}
