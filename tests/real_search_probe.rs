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

/// Emit `{query, expected, candidates[]}` JSONL so an external LLM picker can
/// be scored against the same labeled corpus. K and sample size from env.
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

    // Stride across the corpus so the sample spans models and domains.
    let stride = (queries.len() / take).max(1);
    let mut lines = Vec::new();
    for q in queries.iter().step_by(stride).take(take) {
        let cands: Vec<serde_json::Value> = geco::search::search(&snap, &q.query, k)
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
    std::fs::write(&out, lines.join("\n")).expect("write picker candidates");
    eprintln!("wrote {} picker cases (K={k}) to {out}", lines.len());
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
