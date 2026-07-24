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
