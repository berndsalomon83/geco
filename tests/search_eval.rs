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
//!   heldout  — semantic queries written AFTER the synonym table and never
//!              consulted while building it. The semantic group scores its
//!              own training set; this group is the generalisation check,
//!              and the one to believe.
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
        if self.n == 0 {
            0.0
        } else {
            k as f64 * 100.0 / self.n as f64
        }
    }
    fn mrr(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.rr_sum / self.n as f64
        }
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
    heldout: Stats,
    misses: Vec<String>,
    /// Coverage of the TOP hit, split by whether that top hit was right.
    /// The question this answers: can coverage tell a confident-and-correct
    /// answer apart from a confident-and-wrong one? If the two distributions
    /// overlap, it is useless as a confidence signal no matter how principled
    /// it looks.
    cov_top_right: Vec<f64>,
    cov_top_wrong: Vec<f64>,
}

fn describe(label: &str, v: &mut [f64]) -> String {
    if v.is_empty() {
        return format!("{label:14} (none)");
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| v[((v.len() as f64 - 1.0) * p).round() as usize];
    format!(
        "{label:14} n={:<3} min={:.2}  p25={:.2}  median={:.2}  p75={:.2}  max={:.2}",
        v.len(),
        v[0],
        pct(0.25),
        pct(0.50),
        pct(0.75),
        v[v.len() - 1]
    )
}

fn evaluate() -> Report {
    let corpus = load_corpus();
    let snap = build_snapshot(&corpus);

    let mut overall = Stats::default();
    let mut lexical = Stats::default();
    let mut semantic = Stats::default();
    let mut heldout = Stats::default();
    let mut misses = Vec::new();
    let mut cov_top_right = Vec::new();
    let mut cov_top_wrong = Vec::new();

    for q in &corpus.queries {
        let hits = geco::search::search(&snap, &q.q, 5);
        let rank = hits.iter().position(|h| q.expect.contains(&h.tool));

        // The signal under test: how much of the query did the TOP hit match?
        if let Some(top) = hits.first() {
            if q.expect.contains(&top.tool) {
                cov_top_right.push(top.coverage);
            } else {
                cov_top_wrong.push(top.coverage);
            }
        }

        overall.record(rank);
        match q.kind.as_str() {
            "lexical" => lexical.record(rank),
            "semantic" => semantic.record(rank),
            // Written AFTER the synonym table, deliberately not consulted
            // while building it. The semantic numbers are measured on the
            // queries the table was derived from, so they flatter it; this
            // group is the honest generalisation check.
            "heldout" => heldout.record(rank),
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

    Report {
        overall,
        lexical,
        semantic,
        heldout,
        misses,
        cov_top_right,
        cov_top_wrong,
    }
}

/// Not a gate — a window. Prints what search actually returns for a handful
/// of representative queries, so "why did it pick that?" can be answered by
/// looking rather than guessing. The eval only prints outright misses, which
/// hides the near-misses and the reasons behind a correct-but-lucky hit.
#[test]
fn show_top_hits_for_sample_queries() {
    let corpus = load_corpus();
    let snap = build_snapshot(&corpus);

    eprintln!("\n=== what search returns (top 3) ===");
    for q in [
        "open a bug report",
        "file a ticket",
        "save text to disk",
        "remember this",
        "what do you know about this project",
        "take a picture of the page",
    ] {
        let hits = geco::search::search(&snap, q, 3);
        let shown: Vec<String> = hits
            .iter()
            .map(|h| format!("{}({:.2})", h.tool, h.coverage))
            .collect();
        eprintln!("  {q:38} -> {}", shown.join(", "));
    }
    eprintln!();
}

/// Standalone experiment, not a gate. Reports whether IDF-weighted coverage
/// of the top hit separates correct answers from confidently-wrong ones, and
/// what a threshold would cost in false alarms on answers that were right.
#[test]
fn coverage_as_a_confidence_signal() {
    let mut r = evaluate();

    eprintln!("\n=== IDF-weighted coverage of the TOP hit ===");
    eprintln!("{}", describe("top hit RIGHT", &mut r.cov_top_right));
    eprintln!("{}", describe("top hit WRONG", &mut r.cov_top_wrong));

    eprintln!("\nthreshold   flags a wrong top hit   false alarm on a right one");
    for t in [0.30, 0.40, 0.50, 0.60, 0.70, 0.80] {
        let caught = r.cov_top_wrong.iter().filter(|c| **c < t).count();
        let false_alarm = r.cov_top_right.iter().filter(|c| **c < t).count();
        eprintln!(
            "  < {t:.2}            {caught:>2} / {:<2}                  {false_alarm:>2} / {:<2}",
            r.cov_top_wrong.len(),
            r.cov_top_right.len()
        );
    }
    eprintln!();
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
    eprintln!("{}", r.heldout.line("heldout"));
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

    // Floors sit just under measured performance (BM25F + synonyms, 2026-07):
    //   lexical  recall@1 87.5%  recall@5 100%   MRR 0.938
    //   semantic recall@1 54.5%  recall@5 90.9%  MRR 0.697
    //   heldout  recall@1 87.5%  recall@5 100%   MRR 0.938
    //   overall  MRR 0.893
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
    // Semantic queries need synonym knowledge. The synonym table lifted this
    // from 36.4% to 90.9%; the floor guards that gain.
    assert!(
        r.semantic.pct(r.semantic.hit5) >= 80.0,
        "semantic recall@5 regressed below 80%{summary}"
    );
    // The honest one. The synonym table was DERIVED from the semantic
    // queries above, so their score flatters it — measuring on your own
    // training set always does. These queries were written afterwards and
    // never consulted while building the table, so this floor is what
    // catches a table that has been tuned into memorising the corpus.
    assert!(
        r.heldout.pct(r.heldout.hit5) >= 85.0,
        "held-out recall@5 regressed below 85% — synonyms may be overfitted{summary}"
    );
    assert!(
        r.overall.mrr() >= 0.85,
        "overall MRR regressed below 0.85{summary}"
    );
}
