//! Search quality evaluation.
//!
//! Search is the whole product: a tool the agent cannot find may as well not
//! be installed. Before this harness existed, `score_match`'s weights
//! (100/60/20/10/8) were hand-picked and every change to them was a guess.
//!
//! This measures recall@1/@3/@5 and MRR against a fixed corpus, split by
//! query kind:
//!
//!   lexical  — the query shares real vocabulary with the target. A lexical
//!              scorer should get these; a miss is a genuine defect.
//!   semantic — the query needs synonym or world knowledge ("bug report" ->
//!              create_issue). Reported separately so it is never mistaken
//!              for lexical headroom, and so the value of ever adding
//!              embeddings can be read straight off the numbers.
//!
//! Run `cargo test --test search_eval -- --nocapture` for the full report
//! including every miss.

use std::collections::BTreeMap;

use geco::snapshot::{ServerSnapshot, Snapshot, ToolSnapshot};

#[derive(serde::Deserialize)]
struct Corpus {
    servers: BTreeMap<String, SrvFixture>,
    queries: Vec<Query>,
}

#[derive(serde::Deserialize)]
struct SrvFixture {
    #[serde(default)]
    description: String,
    tools: Vec<ToolFixture>,
}

#[derive(serde::Deserialize)]
struct ToolFixture {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(serde::Deserialize)]
struct Query {
    q: String,
    expect: Vec<String>,
    kind: String,
}

fn load_corpus() -> Corpus {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/search_corpus.json"
    ))
    .expect("search corpus fixture must exist");
    serde_json::from_str(&raw).expect("search corpus must be valid JSON")
}

fn build_snapshot(c: &Corpus) -> Snapshot {
    let mut snap = Snapshot::default();
    for (name, srv) in &c.servers {
        snap.servers.insert(
            name.clone(),
            ServerSnapshot {
                description: srv.description.clone(),
                refresh_status: "ok".into(),
                tools: srv
                    .tools
                    .iter()
                    .map(|t| ToolSnapshot {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        input_schema: serde_json::json!({"type": "object"}),
                    })
                    .collect(),
            },
        );
    }
    snap
}

#[derive(Default, Clone)]
struct Stats {
    n: usize,
    hit1: usize,
    hit3: usize,
    hit5: usize,
    rr_sum: f64,
}

impl Stats {
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
            self.rr_sum += 1.0 / (r as f64 + 1.0);
        }
    }
    fn pct(&self, k: usize) -> f64 {
        if self.n == 0 { 0.0 } else { k as f64 * 100.0 / self.n as f64 }
    }
    fn mrr(&self) -> f64 {
        if self.n == 0 { 0.0 } else { self.rr_sum / self.n as f64 }
    }
    fn line(&self, label: &str) -> String {
        format!(
            "{label:10} n={:<3} recall@1={:5.1}%  recall@3={:5.1}%  recall@5={:5.1}%  MRR={:.3}",
            self.n,
            self.pct(self.hit1),
            self.pct(self.hit3),
            self.pct(self.hit5),
            self.mrr()
        )
    }
}

struct Report {
    overall: Stats,
    lexical: Stats,
    semantic: Stats,
    misses: Vec<String>,
}

fn evaluate() -> Report {
    let corpus = load_corpus();
    let snap = build_snapshot(&corpus);

    let mut overall = Stats::default();
    let mut lexical = Stats::default();
    let mut semantic = Stats::default();
    let mut misses = Vec::new();

    for q in &corpus.queries {
        let hits = geco::search::search(&snap, &q.q, 5);
        let rank = hits.iter().position(|h| q.expect.contains(&h.tool));

        overall.record(rank);
        match q.kind.as_str() {
            "lexical" => lexical.record(rank),
            "semantic" => semantic.record(rank),
            other => panic!("unknown query kind {other:?} in fixture"),
        }

        if rank.is_none() || rank == Some(0) {
            // Record misses (and only misses) for the report.
            if rank.is_none() {
                let got: Vec<&str> = hits.iter().take(3).map(|h| h.tool.as_str()).collect();
                misses.push(format!(
                    "  [{}] {:35} want {:?}  got {:?}",
                    q.kind, q.q, q.expect, got
                ));
            }
        }
    }

    Report { overall, lexical, semantic, misses }
}

/// Regression guard. The floors are set just under measured performance —
/// they exist to catch a scoring change that makes things worse, not to
/// certify that search is good. Raise them when the numbers improve.
#[test]
fn search_quality_meets_floors() {
    let r = evaluate();

    eprintln!("\n=== tool search quality ===");
    eprintln!("{}", r.overall.line("overall"));
    eprintln!("{}", r.lexical.line("lexical"));
    eprintln!("{}", r.semantic.line("semantic"));
    if !r.misses.is_empty() {
        eprintln!("\nnot found in top 5 ({}):", r.misses.len());
        for m in &r.misses {
            eprintln!("{m}");
        }
    }
    eprintln!();

    let summary = format!(
        "\n{}\n{}\n{}\n",
        r.overall.line("overall"),
        r.lexical.line("lexical"),
        r.semantic.line("semantic")
    );

    // Floors sit just under measured performance (BM25F, 2026-07):
    //   lexical  recall@1 87.5%  recall@3 100%  recall@5 100%  MRR 0.938
    //   semantic recall@1 36.4%  recall@3 36.4% recall@5 36.4% MRR 0.364
    //   overall  MRR 0.814
    // They catch a scoring change that makes things worse. They do NOT
    // certify that search is good — read the report for that.

    // Lexical queries are the contract: the agent used words that really are
    // in the tool's name or description, so failing to surface it is a bug.
    assert!(
        r.lexical.pct(r.lexical.hit5) >= 95.0,
        "lexical recall@5 regressed below 95%{summary}"
    );
    assert!(
        r.lexical.pct(r.lexical.hit1) >= 80.0,
        "lexical recall@1 regressed below 80%{summary}"
    );
    // Semantic queries need synonym or world knowledge no lexical scorer has
    // ("bug report" -> issue, "big" -> size). The floor is low on purpose: it
    // tracks the number so a change that helps or hurts is visible, without
    // pretending weight-tuning can fix it. This gap IS the measured case for
    // embeddings, should we ever want them.
    assert!(
        r.semantic.pct(r.semantic.hit5) >= 30.0,
        "semantic recall@5 regressed below 30%{summary}"
    );
    assert!(
        r.overall.mrr() >= 0.75,
        "overall MRR regressed below 0.75{summary}"
    );
}
