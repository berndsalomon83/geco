//! Both picker transports, driven for real: a live MCP session for `sampling`
//! and a live child process for `command`.
//!
//! This is a whole file of scaffolding for a handful of assertions, and the
//! scaffolding is the point. The picker's failure mode is not a wrong answer,
//! it is silence — code that reads correctly, compiles, runs, and quietly does
//! nothing, which from the outside is indistinguishable from a picker whose
//! model agrees with lexical ranking every time. This repo has shipped that
//! bug twice already: `Picker::pick` returned `None` for `Transport::Sampling`
//! for an entire release, and before that every choice a real model made was
//! discarded because models answer `server::tool` where `parse_choice`
//! expected `tool`.
//!
//! Neither is reachable from a unit test, because both live in the wiring
//! rather than in a function. So each test here drives the real mechanism —
//! a real handshake over an in-memory duplex, a real `sampling/createMessage`
//! crossing the transport, a real process spawned and its stdout read — and
//! each pins the lexical order first with a control run, so a picker that does
//! nothing cannot pass by agreeing with BM25F.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::{Proxy, SearchParams};
use geco::snapshot::{ServerSnapshot, Snapshot, ToolSnapshot};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ClientCapabilities, ClientInfo, ContentBlock, Implementation};
// SEP-2577 deprecates sampling and rmcp marks the whole type family; see the
// note on `Picker::run_sampling` for why geco still speaks it. Scoped to this
// import and the two methods below rather than the file, so an unrelated
// deprecation still shows up here.
#[allow(deprecated)]
use rmcp::model::{CreateMessageRequestParams, CreateMessageResult, SamplingMessage};
use rmcp::service::RequestContext;
use rmcp::{ClientHandler, ErrorData as McpError, RoleClient, ServiceExt};
use serde_json::json;

/// A client that advertises sampling and answers with a canned reply — or
/// refuses, which is the more interesting case.
#[derive(Clone)]
struct FakeClient {
    /// What the "model" replies. Ignored when `refuse` is set.
    reply: String,
    /// Advertise the capability at initialize, then error on every request.
    /// Real clients do this: the capability is compiled in, the user declines.
    refuse: bool,
    /// How many times the proxy actually asked. The number that proves geco
    /// stops asking a client that will not answer.
    calls: Arc<AtomicUsize>,
}

impl ClientHandler for FakeClient {
    #[allow(deprecated)]
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            ClientCapabilities::builder().enable_sampling().build(),
            Implementation::from_build_env(),
        )
    }

    #[allow(deprecated)]
    async fn create_message(
        &self,
        _params: CreateMessageRequestParams,
        _ctx: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, McpError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.refuse {
            return Err(McpError::internal_error("user declined sampling", None));
        }
        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text(self.reply.clone()),
            "fake-model".to_string(),
        ))
    }
}

/// Two tools that a request for "read" matches, whose lexical order is
/// predictable and whose right answer is the other one — the shape of the
/// semantic wall the picker exists to climb.
fn two_tool_snapshot() -> Snapshot {
    let mut snap = Snapshot::default();
    snap.servers.insert(
        "files".into(),
        ServerSnapshot {
            description: "local filesystem".into(),
            refresh_status: "ok".into(),
            tools: vec![ToolSnapshot {
                name: "read_text_file".into(),
                description: "Read a text file from disk and return its contents".into(),
                input_schema: json!({"type": "object"}),
            }],
        },
    );
    snap.servers.insert(
        "docs".into(),
        ServerSnapshot {
            description: "the web".into(),
            refresh_status: "ok".into(),
            tools: vec![ToolSnapshot {
                name: "web_url_read".into(),
                description: "Read a page over HTTP and return it as text".into(),
                input_schema: json!({"type": "object"}),
            }],
        },
    );
    snap
}

/// Point `publish_health` at a temp file. `initialize` publishes health
/// unconditionally, and these tests run a real handshake — without this they
/// overwrite the health.json a developer's own running geco is reporting
/// through. Mirrors the guard in `proxy_e2e.rs`, for the same reason.
fn isolate_health() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("geco-picker-health-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("GECO_HEALTH", dir.join("health.json"));
    });
}

/// An interpreter to stand in for "a model behind a command". Skipped rather
/// than failed when absent, unless `GECO_REQUIRE_E2E` says otherwise — a run
/// that quietly covered nothing is worse than a red one.
fn python_bin() -> Option<String> {
    for bin in ["python3", "python"] {
        // Exit status, not just spawnability: Windows' App Execution Alias
        // stub for `python` runs fine and exits 9009.
        let runs = std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if runs {
            return Some(bin.to_string());
        }
    }
    assert!(
        std::env::var_os("GECO_REQUIRE_E2E").is_none(),
        "GECO_REQUIRE_E2E is set but neither `python3` nor `python` runs — \
         the picker command transport would have skipped itself silently"
    );
    eprintln!("no python3/python available — skipping the command transport test");
    None
}

/// A two-skill library on disk. The names deliberately share no vocabulary
/// with the query used against them, which is the case the picker exists for.
fn skill_library() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, desc) in [
        (
            "test-planner",
            "Plan tests systematically before writing them",
        ),
        ("code-reviewer", "Review code for bugs and improvements"),
    ] {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n\n# {name}\n"),
        )
        .unwrap();
    }
    dir
}

fn proxy_with(picker: serde_json::Value) -> Proxy {
    proxy_with_skills(picker, vec![])
}

fn proxy_with_skills(picker: serde_json::Value, skill_dirs: Vec<String>) -> Proxy {
    isolate_health();
    let cfg: Config = serde_json::from_value(json!({
        "proxy": { "picker": picker },
        "mcpServers": {
            "files": { "command": ["true"] },
            "docs":  { "command": ["true"] },
        },
    }))
    .expect("test config");
    // The backends are never started: the snapshot is handed over ready-made
    // and nothing in this file executes a tool.
    let dir = tempfile::tempdir().unwrap();
    let proxy = Proxy::new(
        Registry::from_config(&cfg),
        two_tool_snapshot(),
        dir.path().join("snapshot.json"),
        dir.path().join("config.yaml"),
        skill_dirs,
        vec![],
    );
    std::mem::forget(dir); // outlive the test rather than race its cleanup
    proxy
}

/// Run one search and return every result as `(kind, name)` in order, so a
/// test can assert across both surfaces at once.
async fn search_all(proxy: &Proxy, query: &str) -> Vec<(String, String)> {
    let res = proxy
        .search(Parameters(SearchParams {
            query: query.into(),
            limit: Some(5),
            include_schema: None,
            kind: None,
        }))
        .await
        .expect("search must not error");
    let text = res
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("text block");
    let body: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    body["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap_or_default().to_string(),
                r["name"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// The whole parsed search response, for the tests that care about the
/// envelope — hints, counts, the `issues` channel — and not just the order.
async fn search_body(proxy: &Proxy, query: &str, kind: Option<&str>) -> serde_json::Value {
    let res = proxy
        .search(Parameters(SearchParams {
            query: query.into(),
            limit: Some(10),
            include_schema: None,
            kind: kind.map(str::to_string),
        }))
        .await
        .expect("search must not error");
    let text = res
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("text block");
    serde_json::from_str(&text).expect("search returns JSON")
}

/// Run one search and return the (server, tool) pairs in ranked order.
async fn search_order(proxy: &Proxy, query: &str) -> Vec<(String, String)> {
    let res = proxy
        .search(Parameters(SearchParams {
            query: query.into(),
            limit: Some(5),
            include_schema: None,
            kind: Some("tools".into()),
        }))
        .await
        .expect("search must not error");
    let text = res
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("search returns a text block");
    let body: serde_json::Value = serde_json::from_str(&text).expect("search returns JSON");
    body["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| {
            (
                r["server"].as_str().unwrap_or_default().to_string(),
                r["name"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Connect a client to a clone of `proxy` and leave the session running for as
/// long as the returned guard is held. Proxy is `Clone` over shared `Arc`
/// state, so the peer stashed by the served clone is visible to the original —
/// which lets the test call `search` directly instead of routing a `tools/call`
/// back through the same socket.
async fn connect(proxy: &Proxy, client: FakeClient) -> impl Sized {
    let (server_io, client_io) = tokio::io::duplex(1 << 20);
    let served = proxy.clone();
    let server = tokio::spawn(async move {
        if let Ok(running) = served.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });
    let running_client = client.serve(client_io).await.expect("handshake");
    (server, running_client)
}

/// The whole feature in one assertion: the model names a tool that lexical
/// ranking put second, and that tool comes back first.
#[tokio::test]
async fn sampling_hoists_the_model_choice_to_rank_one() {
    let calls = Arc::new(AtomicUsize::new(0));

    // Control: with no picker, the lexical order is whatever it is. Capture it
    // so the real assertion below cannot pass by accidentally agreeing with
    // BM25F — a picker that does nothing would otherwise look identical.
    let plain = proxy_with(json!({ "mode": "off" }));
    let lexical = search_order(&plain, "read").await;
    assert!(
        lexical.len() >= 2,
        "need at least two hits for a pick to be observable: {lexical:?}"
    );

    // The model is told to choose whichever tool lexical ranking ranked LAST,
    // so a hoist is unmistakable in either direction.
    let (want_server, want_tool) = lexical.last().unwrap().clone();
    let proxy = proxy_with(json!({ "mode": "sampling", "candidates": 10, "timeout_secs": 10 }));
    let _session = connect(
        &proxy,
        FakeClient {
            reply: format!(r#"{{"server":"{want_server}","tool":"{want_tool}"}}"#),
            refuse: false,
            calls: calls.clone(),
        },
    )
    .await;

    let picked = search_order(&proxy, "read").await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one search should cost exactly one sampling round-trip"
    );
    assert_eq!(
        picked.first(),
        Some(&(want_server, want_tool)),
        "the model's choice must be rank 1, was: {picked:?} (lexical: {lexical:?})"
    );
    assert_eq!(
        picked.len(),
        lexical.len(),
        "a pick reorders results, it does not add or drop them"
    );
}

/// A client that advertises sampling and then refuses it is the common case,
/// not an exotic one — the capability is compiled into the client, the user
/// declines the prompt. Asking again on every search would cost a round-trip
/// and, on clients that prompt, a dialog, for a lexical answer geco was always
/// going to return.
#[tokio::test]
async fn a_client_that_refuses_sampling_is_asked_once_and_never_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let proxy = proxy_with(json!({ "mode": "sampling", "candidates": 10, "timeout_secs": 10 }));
    let _session = connect(
        &proxy,
        FakeClient {
            reply: String::new(),
            refuse: true,
            calls: calls.clone(),
        },
    )
    .await;

    let first = search_order(&proxy, "read").await;
    let second = search_order(&proxy, "read").await;
    let third = search_order(&proxy, "read").await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a refusal must disable the transport, not be retried on every search"
    );
    // Failure degrades to lexical ranking; it does not degrade to no results.
    assert!(!first.is_empty(), "a refused pick must still return hits");
    assert_eq!(first, second, "results must be stable after a refusal");
    assert_eq!(second, third);
}

/// The transport most people will actually configure, since almost no client
/// implements sampling yet: geco spawns a process, hands it the prompt, and
/// reads a choice off its stdout. Driven with a real child rather than a stub,
/// because the parts that break here — argument substitution, the prompt
/// arriving at all, stdout capture — are exactly the parts a stub replaces.
#[tokio::test]
async fn a_command_transport_hoists_the_choice_it_prints() {
    let Some(python) = python_bin() else { return };

    let plain = proxy_with(json!({ "mode": "off" }));
    let lexical = search_order(&plain, "read").await;
    assert!(lexical.len() >= 2, "need two hits: {lexical:?}");
    let (want_server, want_tool) = lexical.last().unwrap().clone();

    // Stands in for `opencode run` or a curl wrapper. It also asserts, from
    // inside the child, that the prompt really arrived and really carries the
    // instruction not to deliberate: a non-zero exit here surfaces as a failed
    // pick, and the surrounding assertions then fail loudly rather than
    // passing on a fallback that looks the same as success.
    let script = format!(
        "import sys; p = sys.argv[1]; \
         assert 'read' in p and '/no_think' in p, p[:200]; \
         print('{{\"server\": \"{want_server}\", \"tool\": \"{want_tool}\"}}')"
    );
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", script],
        "candidates": 10,
        "timeout_secs": 30,
    }));

    let picked = search_order(&proxy, "read").await;
    assert_eq!(
        picked.first(),
        Some(&(want_server, want_tool)),
        "the command's choice must be rank 1, was: {picked:?} (lexical: {lexical:?})"
    );
}

/// The whole point of whole-catalogue mode: the model reaches a tool that
/// lexical retrieval never offered.
///
/// `search.rs` drops any tool scoring zero, so a request sharing no term with
/// its tool makes that tool *not a candidate* — measured at 9.5% of queries,
/// at every catalogue size, unfixable by any value of K. Handing the model the
/// whole catalogue is the only thing that reaches them, and it measured 94.8%
/// rank-1 against the shortlist's 74.6%.
///
/// The tool picked here is deliberately one that scores ZERO for the query, so
/// a shortlist could not contain it however wide.
#[tokio::test]
async fn whole_catalogue_mode_reaches_a_tool_lexical_search_cannot_offer() {
    let Some(python) = python_bin() else { return };

    // "disk" appears only in `read_text_file`'s description, so the query has
    // real lexical footing and yet `web_url_read` scores zero and cannot be
    // shortlisted at any width. Both halves are preconditions: without the
    // first, this would be the no-lexical-signal case, where geco declines to
    // pick at all on purpose (see
    // `a_query_that_matches_nothing_is_not_given_a_fabricated_answer`).
    let unreachable = ("docs", "web_url_read");
    let query = "somewhere on disk";
    let plain = proxy_with(json!({ "mode": "off" }));
    let lexical = search_order(&plain, query).await;
    assert!(
        !lexical.is_empty(),
        "precondition: the query must score SOMETHING, or this tests the \
         no-signal guard instead: {lexical:?}"
    );
    assert!(
        !lexical
            .iter()
            .any(|(s, t)| (s.as_str(), t.as_str()) == unreachable),
        "precondition: lexical search must NOT surface the target: {lexical:?}"
    );

    let script = format!(
        "import sys; print('{{\"server\": \"{}\", \"tool\": \"{}\"}}')",
        unreachable.0, unreachable.1
    );
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", script],
        // Generous enough that the two-tool catalogue always fits.
        "whole_catalogue_tokens": 24000,
        "timeout_secs": 30,
    }));

    let got = search_order(&proxy, query).await;
    assert_eq!(
        got.first().map(|(s, t)| (s.as_str(), t.as_str())),
        Some(unreachable),
        "the model's choice must be rank 1 even though lexical never ranked \
         it at all: {got:?}"
    );
}

/// Over the budget, the shortlist is used — and the budget is really consulted.
///
/// Asserting only that a pick still lands would pass whether or not
/// `whole_catalogue_tokens` is honoured, because both pools contain the tool in
/// that case. So this names a tool the shortlist CANNOT contain: if the budget
/// were ignored — a renamed key, a saturating multiply, an inverted comparison
/// in `whole_catalogue` — the model would see the whole catalogue, the pick
/// would land, and this fails. It is the same query and the same reply as
/// `whole_catalogue_mode_reaches_a_tool_lexical_search_cannot_offer`; only the
/// budget differs, and it must flip the outcome.
#[tokio::test]
async fn a_catalogue_over_budget_really_falls_back_to_the_shortlist() {
    let Some(python) = python_bin() else { return };

    let unreachable = ("docs", "web_url_read");
    let query = "somewhere on disk";
    let plain = proxy_with(json!({ "mode": "off" }));
    let lexical = search_order(&plain, query).await;
    assert!(!lexical.is_empty(), "precondition: needs lexical signal");
    assert!(
        !lexical
            .iter()
            .any(|(s, t)| (s.as_str(), t.as_str()) == unreachable),
        "precondition: the shortlist must not contain the target: {lexical:?}"
    );

    let script = format!(
        "import sys; print('{{\"server\": \"{}\", \"tool\": \"{}\"}}')",
        unreachable.0, unreachable.1
    );
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", script],
        "whole_catalogue_tokens": 1, // nothing fits
        "candidates": 10,
        "timeout_secs": 30,
    }));

    let got = search_order(&proxy, query).await;
    assert_eq!(
        got, lexical,
        "over budget the model never sees the tool it named, so the pick must \
         be discarded and the lexical order returned untouched: {got:?}"
    );
}

/// `search("*")` is the browse path, and it has to answer three things at
/// once: what servers exist, that skills exist at all, and how to see them.
///
/// It returns SERVERS, not every skill — a broad question should not be
/// answered with a wall of text nothing in it asked for. Skills are counted
/// rather than listed, which is a few tokens instead of hundreds and still
/// tells the model they are there. That matters because skills are a push
/// resource: nothing in a user's request names one, so a model never told they
/// exist will not go looking.
#[tokio::test]
async fn the_overview_query_lists_servers_and_points_at_skills() {
    let lib = skill_library();
    let dirs = vec![lib.path().to_str().unwrap().to_string()];
    let proxy = proxy_with_skills(json!({ "mode": "off" }), dirs);

    let body = search_body(&proxy, "*", None).await;
    let kinds: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap_or(""))
        .collect();
    assert!(
        kinds.iter().all(|k| *k == "server"),
        "the broad overview is servers only: {kinds:?}"
    );
    let names: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap_or(""))
        .collect();
    assert!(
        names.contains(&"files") && names.contains(&"docs"),
        "{names:?}"
    );
    // The envelope is the same one every other search returns — a response
    // whose shape depends on its input is a trap for anything parsing it.
    assert_eq!(body["count"], 2);
    assert!(body["results"][0]["tools"].is_number(), "{body}");

    let hint = body["hint"].as_str().unwrap_or_default();
    assert!(
        hint.contains("2 skill"),
        "the count is the whole nudge — say how many: {hint}"
    );
    assert!(
        hint.contains("kind=\"skills\""),
        "and name the call that lists them: {hint}"
    );

    // Asked for by name, skills ARE listed in full — this is the list_skills
    // replacement.
    let skills = search_body(&proxy, "*", Some("skills")).await;
    let listed: Vec<&str> = skills["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed.contains(&"code-reviewer"), "{listed:?}");
}

/// A miss has to say what IS here, without the model knowing any trick.
///
/// It arrives automatically on the one response that was otherwise nearly
/// empty, and it answers the only question a miss actually raises: was that my
/// wording, or does this gateway simply not have it?
#[tokio::test]
async fn a_search_that_matches_nothing_volunteers_what_is_available() {
    let proxy = proxy_with(json!({ "mode": "off" }));
    let body = search_body(&proxy, "xylophone quantum tectonics", Some("tools")).await;

    assert_eq!(body["count"], 0);
    let hint = body["hint"].as_str().unwrap_or_default();
    assert!(hint.contains("files") && hint.contains("docs"), "{hint}");
    assert!(
        hint.contains("tools"),
        "say how much is behind each: {hint}"
    );
    assert!(
        hint.contains('*'),
        "and name the call that shows everything: {hint}"
    );
}

/// Skills go through the same picker as tools, in the same call.
///
/// The target is whichever skill lexical ranked LAST, so the hoist is
/// unmistakable and the test cannot quietly start passing because the ranking
/// happened to agree — the same control the tool tests use.
#[tokio::test]
async fn a_skill_is_pickable_and_hoisted_like_a_tool() {
    let Some(python) = python_bin() else { return };
    let lib = skill_library();
    let dirs = vec![lib.path().to_str().unwrap().to_string()];
    let query = "plan a code review";

    let plain = proxy_with_skills(json!({ "mode": "off" }), dirs.clone());
    let lexical = search_all(&plain, query).await;
    let skills_before: Vec<_> = lexical
        .iter()
        .filter(|(k, _)| k == "skill")
        .cloned()
        .collect();
    assert!(
        skills_before.len() >= 2,
        "precondition: need two skills ranked for a hoist to be observable: {lexical:?}"
    );
    let want = skills_before.last().unwrap().1.clone();

    // The model names the skill using the `skill:` label the prompt lists it
    // under — the shape a real model most often echoes back.
    let script = format!("import sys; print('{{\"server\": \"\", \"tool\": \"skill: {want}\"}}')");
    let proxy = proxy_with_skills(
        json!({
            "mode": "command",
            "command": [python, "-c", script],
            "whole_catalogue_tokens": 24000,
            "timeout_secs": 30,
        }),
        dirs,
    );

    let got = search_all(&proxy, query).await;
    let skills: Vec<_> = got.iter().filter(|(k, _)| k == "skill").collect();
    assert_eq!(
        skills.first().map(|(_, n)| n.as_str()),
        Some(want.as_str()),
        "the picked skill must lead the skill results: {got:?} (was {skills_before:?})"
    );
    // Picking a skill must not disturb the tool list — the model said the best
    // answer was a skill, so the tools stay in lexical order.
    let tools_plain: Vec<_> = lexical.iter().filter(|(k, _)| k == "tool").collect();
    let tools_after: Vec<_> = got.iter().filter(|(k, _)| k == "tool").collect();
    assert_eq!(
        tools_plain, tools_after,
        "a skill pick must leave the tool list alone"
    );
}

/// "Nothing matched" has to stay reachable.
///
/// Whole-catalogue mode hands the model every tool, and the prompt has no way
/// to decline — so without a guard a request the gateway cannot serve comes
/// back as `count: 1` with a confidently wrong best match, and `count: 0`
/// becomes unreachable forever. That also silences the no-match metric, which
/// is the documented signal for a tool whose description does not say what it
/// is for.
#[tokio::test]
async fn a_query_that_matches_nothing_is_not_given_a_fabricated_answer() {
    let Some(python) = python_bin() else { return };

    // Scores zero against both tools in the fixture.
    let query = "xylophone quantum tectonics";
    let plain = proxy_with(json!({ "mode": "off" }));
    assert!(
        search_order(&plain, query).await.is_empty(),
        "precondition: this query must match nothing lexically"
    );

    // A picker that would happily name a real tool if it were asked.
    let script = "import sys; print('{\"server\": \"files\", \"tool\": \"read_text_file\"}')";
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", script],
        "whole_catalogue_tokens": 24000,
        "timeout_secs": 30,
    }));

    assert!(
        search_order(&proxy, query).await.is_empty(),
        "an unanswerable request must still answer nothing, not the model's \
         best guess at what a filesystem could do about it"
    );
}

/// The failure mode that actually happens in the field, and the one hardest to
/// see: a model that answers with nothing at all.
///
/// Measured against a real endpoint serving Qwen with reasoning enabled, 7 of 8
/// searches came back with entirely empty content — the whole output budget
/// spent in a hidden reasoning channel before any JSON was emitted. Neither
/// `/no_think` in the prompt nor `enable_thinking: false` on the request
/// changed it. From the outside this is indistinguishable from a picker whose
/// model agrees with lexical ranking every time, so what must be guaranteed is
/// that results still come back, unreordered and complete.
#[tokio::test]
async fn a_model_that_answers_with_nothing_still_returns_the_lexical_order() {
    let Some(python) = python_bin() else { return };

    let plain = proxy_with(json!({ "mode": "off" }));
    let lexical = search_order(&plain, "read").await;
    assert!(lexical.len() >= 2, "need two hits: {lexical:?}");

    // Exits 0, prints nothing: a wrapper whose model burned its budget.
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", "pass"],
        "candidates": 10,
        "timeout_secs": 30,
    }));

    let got = search_order(&proxy, "read").await;
    assert_eq!(
        got, lexical,
        "an empty pick must leave the lexical order exactly as it was"
    );
    // Repeated, because this is the case that trips the streak warning — it
    // must stay a warning and never become a failure.
    assert_eq!(search_order(&proxy, "read").await, lexical);
    assert_eq!(search_order(&proxy, "read").await, lexical);
    assert_eq!(search_order(&proxy, "read").await, lexical);
}

/// A pick takes seconds; the snapshot lock must not.
///
/// The failure this guards is not a wrong answer, it is a gateway that looks
/// hung on an unrelated request. If `search` holds the snapshot read guard
/// across the picker round-trip, then the next writer — a `tools/list_changed`
/// rescan, a config save — queues behind it, and every reader after that
/// queues behind the writer. The one a user feels is `execute`, whose trailing
/// health publish reads the snapshot: a tool call that already succeeded sits
/// there for the whole picker timeout.
///
/// Asserted as an ORDER rather than a duration, so it does not go flaky on a
/// loaded machine: with a writer interleaved, an unrelated reader must finish
/// while the picker is still thinking.
#[tokio::test]
async fn a_slow_pick_does_not_block_the_rest_of_the_gateway() {
    use std::time::{Duration, Instant};

    let Some(python) = python_bin() else { return };

    let cfg: Config = serde_json::from_value(json!({
        "proxy": { "startup_timeout_secs": 1 },
        "mcpServers": { "files": { "command": ["true"] }, "docs": { "command": ["true"] } },
    }))
    .unwrap();

    // Four seconds of "model", which is not unusual for an agent CLI.
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", "import time; time.sleep(4); print('{}')"],
        "candidates": 10,
        "timeout_secs": 30,
    }));

    let searching = proxy.clone();
    let search = tokio::spawn(async move { search_order(&searching, "read").await });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // A writer, which is what turns a held read guard into a queue.
    let writer = proxy.clone();
    let reload = tokio::spawn(async move { writer.reload_from_config(cfg).await });
    tokio::time::sleep(Duration::from_millis(400)).await;

    // An ordinary reader. It must not be stuck behind the picker.
    let started = Instant::now();
    let _ = proxy.list_mcp_servers().await.expect("list must not error");
    let waited = started.elapsed();

    assert!(
        !search.is_finished(),
        "the picker finished early — this test proves nothing unless the read \
         happens while a pick is still in flight"
    );
    assert!(
        waited < Duration::from_secs(2),
        "an unrelated read waited {waited:?} behind a pick that is still running"
    );

    let _ = reload.await;
    let _ = search.await;
}

/// The prompt template's `{prompt}` placeholder, and the guard against a
/// nested geco recursing into another pick.
#[tokio::test]
async fn a_command_gets_the_prompt_substituted_and_a_no_recursion_marker() {
    let Some(python) = python_bin() else { return };

    let plain = proxy_with(json!({ "mode": "off" }));
    let lexical = search_order(&plain, "read").await;
    let (want_server, want_tool) = lexical.last().unwrap().clone();

    // `--task={prompt}` proves substitution happens inside an argument rather
    // than only by appending, and os.environ proves the recursion marker is
    // set on the child that a user's wrapper script would see.
    let script = format!(
        "import sys, os; \
         t = [a for a in sys.argv if a.startswith('--task=')][0]; \
         assert 'read' in t, t[:200]; \
         assert os.environ.get('GECO_NO_PICKER') == '1', 'no recursion marker'; \
         print('{{\"tool\": \"{want_server}::{want_tool}\"}}')"
    );
    let proxy = proxy_with(json!({
        "mode": "command",
        "command": [python, "-c", script, "--task={prompt}"],
        "candidates": 10,
        "timeout_secs": 30,
    }));

    let picked = search_order(&proxy, "read").await;
    assert_eq!(
        picked.first(),
        Some(&(want_server, want_tool)),
        "substituted prompt + `server::tool` reply must resolve: {picked:?}"
    );
}
