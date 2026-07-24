//! One-call tool discovery across every backend — the feature lazy-mcp
//! lacks. Shared by tool search here and skill search in `skills.rs`, so both
//! surfaces rank the same way.
//!
//! Scoring is BM25F: term frequency with saturation, inverse document
//! frequency over the corpus being searched, and per-field weights (a hit in
//! the name counts for more than one in the description).
//!
//! Why not the simpler weighted-substring scheme this replaced: it matched
//! query tokens as raw substrings, so "what time is it" ranked `list_commits`
//! above `get_current_time` — "is" matched l-IS-t, "it" matched comm-IT-s, and
//! two accidental hits outweighed the real one. Token boundaries stop the
//! accident; IDF makes it not matter anyway, because a term that occurs all
//! over the corpus carries almost no weight. Both are needed: boundaries alone
//! still over-reward common words, IDF alone still matches inside words.
//!
//! Dependency-free and stateless — IDF is computed per query over the current
//! corpus. Scanning a few hundred tools costs microseconds and there is no
//! index to build, persist, or invalidate.

use std::collections::HashMap;

/// BM25 term-frequency saturation. Higher = repeated terms keep mattering.
const K1: f64 = 1.2;
/// BM25 length normalisation. Keeps verbose descriptions from dominating.
const B: f64 = 0.75;

const W_NAME: f64 = 3.0;
const W_SERVER: f64 = 1.0;
const W_DESC: f64 = 1.0;

/// Minimum query-token length for a prefix match ("config" -> "configuration").
/// Below this, prefixes match far too much ("is" -> "issue", "island", ...).
const MIN_PREFIX_LEN: usize = 4;

/// Something searchable: a backend tool, or a skill.
pub struct Doc<'a> {
    pub name: &'a str,
    pub description: &'a str,
    /// Extra surface for tools (the server name); skills have none.
    pub server: Option<&'a str>,
}

#[derive(Debug, serde::Serialize)]
pub struct Hit {
    pub server: String,
    pub tool: String,
    pub description: String,
    #[serde(skip)]
    pub score: f64,
    #[serde(skip)]
    pub coverage: f64,
}

/// One ranked document.
pub struct Ranked {
    pub index: usize,
    pub score: f64,
    /// How much of the query's *information* this document actually matched,
    /// IDF-weighted, in 0..=1. Raw BM25 scores can't be compared across
    /// queries; this can, which is what makes it usable as a confidence
    /// signal rather than just an ordering.
    ///
    /// IDF-weighting is what keeps it honest: matching "an" and "this" should
    /// not look like understanding the question, while matching "screenshot"
    /// should.
    pub coverage: f64,
}

/// Split an identifier or phrase into comparable tokens: lowercased, broken
/// on non-alphanumerics AND camelCase boundaries, then lightly stemmed.
/// `browser_takeScreenshot` and "take a screenshot" both yield `screenshot`.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            // camelCase boundary: a capital right after a lowercase.
            if ch.is_uppercase() && prev_lower && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.extend(ch.to_lowercase());
            prev_lower = ch.is_lowercase();
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter()
        .filter(|t| t.chars().count() > 1)
        .map(|t| stem(&t))
        .collect()
}

/// Deliberately crude plural stripping, so "issues" finds `create_issue`.
/// Over-stemming is harmless here because queries and documents go through
/// the same function — "status" and "status" still agree as "statu". The
/// guards just avoid the cases where it would be obviously silly.
fn stem(t: &str) -> String {
    let n = t.len();
    if n >= 4 && t.ends_with('s') && !t.ends_with("ss") && !t.ends_with("us") && !t.ends_with("is")
    {
        t[..n - 1].to_string()
    } else {
        t.to_string()
    }
}

// NOTE: a stopword list ("what", "is", "this", …) was tried here and measured
// WORSE — lexical MRR 0.938 -> 0.931, recall@3 100% -> 97.5%. IDF already
// makes common words nearly weightless, and removing them threw away the
// little signal they did carry. Left out deliberately; re-measure before
// reintroducing it.

/// Words that mean the same thing for the purpose of finding a tool. Every
/// entry here comes from a MEASURED failure in the eval corpus, not from
/// imagination: the misses were "open a bug report" -> create_issue, "save
/// text to disk" -> write_file, "how big is this file" -> get_file_info.
/// Those are synonym gaps, not deep semantic ones, which is why a table is
/// the right size of tool for them.
///
/// Groups are symmetric: any member matches any other. Entries must be given
/// in stemmed form, since queries and documents are stemmed before lookup.
const SYNONYM_GROUPS: &[&[&str]] = &[
    &["issue", "bug", "ticket", "defect"],
    // NOT here, deliberately: ["create", "open"]. As a symmetric entry it
    // lifted the semantic set (recall@1 54.5% -> 63.6%) — the very queries
    // the table was built from — while the HELD-OUT set fell (87.5% ->
    // 75.0%) and overall MRR stayed flat. "any open defects?" wants
    // list_issues, and open->create promotes create_issue over it.
    // The mapping is real; making it symmetric was the mistake. It lives in
    // leading_verb_synonyms() instead, where position disambiguates verb
    // from adjective. Keep verb mappings out of this table.
    &["write", "save"],
    &["size", "big", "large"],
    &["store", "remember", "memorize"],
    // "know" earns its place: "what do you know about X" is how people ask a
    // memory tool for what it has. It was added only after twelve held-out
    // queries existed to judge it, three of them using "know" for something
    // else entirely ("know which tables exist", "let me know what time it
    // is", "know who is in this channel"). Those three still rank correctly
    // and the held-out score did not move; semantic recall@5 went 90.9% ->
    // 100%. Motivated by training data, validated on data it never saw.
    &["recall", "retrieve", "remember", "know"],
    &["search", "find", "lookup", "query"],
    &["web", "online", "internet"],
    &["delete", "remove", "drop"],
    &["directory", "folder", "dir"],
    &["message", "post"],
    &["screenshot", "capture", "picture", "image"],
    &["rename", "move"],
    &["edit", "modify", "change", "update"],
    &["timezone", "time"],
];

/// A synonym match counts for less than the word the user actually typed.
/// Without the discount, "bug" hitting a tool that says "issue" would rank
/// level with one that says "bug", and literal matches should win.
const SYNONYM_WEIGHT: f64 = 0.6;

/// Verb synonyms applied ONLY to the first token of a query, and only in one
/// direction.
///
/// The failure this targets: "open a bug report" and "file a ticket" put
/// create_issue THIRD, behind get_issue and add_issue_comment, because
/// bug/ticket -> issue matches all three equally. The word that picks one out
/// is the verb, and both phrases mean create.
///
/// A plain symmetric entry for this measured WORSE — see the note in
/// SYNONYM_GROUPS. Position is what makes it safe: in "any open defects?"
/// the same word is an adjective, and it isn't leading, so nothing fires. An
/// imperative request puts its verb first; that is the whole heuristic.
fn leading_verb_synonyms(token: &str) -> &'static [&'static str] {
    match token {
        "open" | "file" | "raise" | "log" => &["create"],
        _ => &[],
    }
}

fn synonym_table() -> &'static HashMap<String, Vec<String>> {
    static TABLE: std::sync::OnceLock<HashMap<String, Vec<String>>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for group in SYNONYM_GROUPS {
            for word in *group {
                let key = stem(word);
                for other in *group {
                    let val = stem(other);
                    if val != key {
                        let e = m.entry(key.clone()).or_default();
                        if !e.contains(&val) {
                            e.push(val);
                        }
                    }
                }
            }
        }
        m
    })
}

/// A query token plus the words it may also be satisfied by.
struct QueryTerm {
    token: String,
    synonyms: Vec<String>,
}

/// How many times `q` occurs in `field`, counting exact token matches and
/// prefix matches for long-enough query tokens.
fn count_in(field: &[String], q: &str) -> f64 {
    field
        .iter()
        .filter(|t| *t == q || (q.len() >= MIN_PREFIX_LEN && t.starts_with(q)))
        .count() as f64
}

struct Fields {
    name: Vec<String>,
    server: Vec<String>,
    desc: Vec<String>,
    len: f64,
}

impl Fields {
    fn of(doc: &Doc) -> Self {
        let name = tokenize(doc.name);
        let server = doc.server.map(tokenize).unwrap_or_default();
        let desc = tokenize(doc.description);
        let len = W_NAME * name.len() as f64
            + W_SERVER * server.len() as f64
            + W_DESC * desc.len() as f64;
        Self {
            name,
            server,
            desc,
            len,
        }
    }

    /// Field-weighted frequency of one query token in this document.
    fn tf_of(&self, q: &str) -> f64 {
        W_NAME * count_in(&self.name, q)
            + W_SERVER * count_in(&self.server, q)
            + W_DESC * count_in(&self.desc, q)
    }

    /// Frequency of a query term, satisfied literally or by a synonym at a
    /// discount. Folding synonyms into tf (rather than adding extra query
    /// terms) keeps the query length — and therefore IDF and coverage —
    /// measured against what the user actually asked.
    fn weighted_tf(&self, t: &QueryTerm) -> f64 {
        let direct = self.tf_of(&t.token);
        if t.synonyms.is_empty() {
            return direct;
        }
        let via_synonym: f64 = t.synonyms.iter().map(|s| self.tf_of(s)).sum();
        direct + SYNONYM_WEIGHT * via_synonym
    }
}

/// Rank `docs` against `query`, best first. Returns `(index, score)` for the
/// documents that matched at all, capped at `limit`.
pub fn rank(docs: &[Doc], query: &str, limit: usize) -> Vec<Ranked> {
    if docs.is_empty() {
        return Vec::new();
    }
    let ordered = tokenize(query);
    // Position is lost by the sort below, so capture the leading token first.
    let leading = ordered.first().cloned();
    let q_tokens = {
        let mut t = ordered;
        t.sort();
        t.dedup();
        t
    };
    let table = synonym_table();
    let q_terms: Vec<QueryTerm> = q_tokens
        .iter()
        .map(|t| {
            let mut synonyms = table.get(t).cloned().unwrap_or_default();
            if leading.as_deref() == Some(t.as_str()) {
                for extra in leading_verb_synonyms(t) {
                    let extra = (*extra).to_string();
                    if !synonyms.contains(&extra) {
                        synonyms.push(extra);
                    }
                }
            }
            QueryTerm {
                token: t.clone(),
                synonyms,
            }
        })
        .collect();
    let fields: Vec<Fields> = docs.iter().map(Fields::of).collect();
    let n = docs.len() as f64;
    let avg_len = fields.iter().map(|f| f.len).sum::<f64>() / n;

    // Inverse document frequency per query term. Probabilistic form, +1
    // inside the log so it never goes negative for a term in most documents.
    let mut idf: HashMap<&str, f64> = HashMap::new();
    for t in &q_terms {
        let d = fields.iter().filter(|f| f.weighted_tf(t) > 0.0).count() as f64;
        idf.insert(t.token.as_str(), ((n - d + 0.5) / (d + 0.5) + 1.0).ln());
    }
    // Total information in the query, for the coverage denominator.
    let total_idf: f64 = q_terms.iter().map(|t| idf[t.token.as_str()]).sum();

    let q_lower = query.trim().to_lowercase();
    let mut scored: Vec<Ranked> = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        let mut score = 0.0;
        let mut matched = 0usize;
        let mut matched_idf = 0.0;
        for t in &q_terms {
            let tf = f.weighted_tf(t);
            if tf <= 0.0 {
                continue;
            }
            matched += 1;
            let qi = idf[t.token.as_str()];
            matched_idf += qi;
            let norm = tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * f.len / avg_len));
            score += qi * norm;
        }
        if score <= 0.0 {
            continue;
        }
        let coverage = if total_idf > 0.0 {
            matched_idf / total_idf
        } else {
            0.0
        };

        // Coverage: a document matching every query term beats one that
        // matched a single rare term, even if that term scored highly.
        score *= 1.0 + 0.5 * (matched as f64 / q_terms.len().max(1) as f64);

        // Intent bonuses, kept from the previous scorer: naming the tool
        // outright, or quoting a run of its name, is a strong signal.
        let name_lower = docs[i].name.to_lowercase();
        if name_lower == q_lower {
            score += 50.0;
        } else if !q_lower.is_empty() && name_lower.contains(&q_lower) {
            score += 5.0;
        }

        scored.push(Ranked {
            index: i,
            score,
            coverage,
        });
    }

    // Deterministic order: score, then name, so equal scores don't shuffle.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| docs[a.index].name.cmp(docs[b.index].name))
    });
    scored.truncate(limit);
    scored
}

pub fn search(snapshot: &crate::snapshot::Snapshot, query: &str, limit: usize) -> Vec<Hit> {
    // Flatten to a doc list, remembering where each came from.
    let mut origin: Vec<(&str, &crate::snapshot::ToolSnapshot)> = Vec::new();
    for (server, snap) in &snapshot.servers {
        for tool in &snap.tools {
            origin.push((server.as_str(), tool));
        }
    }
    let docs: Vec<Doc> = origin
        .iter()
        .map(|(server, tool)| Doc {
            name: &tool.name,
            description: &tool.description,
            server: Some(server),
        })
        .collect();

    rank(&docs, query, limit)
        .into_iter()
        .map(|r| {
            let (server, tool) = origin[r.index];
            Hit {
                server: server.to_string(),
                tool: tool.name.clone(),
                description: crate::snapshot::short_desc(&tool.description, 140),
                score: r.score,
                coverage: r.coverage,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{ServerSnapshot, Snapshot, ToolSnapshot};

    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        let mk = |name: &str, desc: &str| ToolSnapshot {
            name: name.into(),
            description: desc.into(),
            input_schema: serde_json::json!({}),
        };
        s.servers.insert(
            "icm".into(),
            ServerSnapshot {
                tools: vec![
                    mk(
                        "icm_memory_store",
                        "Store important information in long-term memory",
                    ),
                    mk("icm_memory_recall", "Search long-term memory"),
                    mk("icm_wake_up", "Compact critical-facts pack"),
                ],
                ..Default::default()
            },
        );
        s.servers.insert(
            "github".into(),
            ServerSnapshot {
                tools: vec![mk("create_issue", "Create a GitHub issue in a repository")],
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn exact_name_wins() {
        let hits = search(&snap(), "icm_memory_recall", 5);
        assert_eq!(hits[0].tool, "icm_memory_recall");
    }

    #[test]
    fn cross_server_token_search() {
        let hits = search(&snap(), "github issue", 5);
        assert_eq!(hits[0].tool, "create_issue");
        assert_eq!(hits[0].server, "github");
    }

    #[test]
    fn description_terms_match() {
        let hits = search(&snap(), "long-term memory", 5);
        assert!(hits.iter().any(|h| h.tool == "icm_memory_store"));
        assert!(hits.iter().any(|h| h.tool == "icm_memory_recall"));
    }

    #[test]
    fn limit_respected() {
        assert_eq!(search(&snap(), "memory", 1).len(), 1);
    }

    #[test]
    fn tokenizer_splits_identifiers_and_camel_case() {
        assert_eq!(tokenize("icm_memory_store"), vec!["icm", "memory", "store"]);
        assert_eq!(
            tokenize("browserTakeScreenshot"),
            vec!["browser", "take", "screenshot"]
        );
        assert_eq!(tokenize("get-file-info"), vec!["get", "file", "info"]);
        // Single characters carry no signal and match everything.
        assert_eq!(tokenize("a b file"), vec!["file"]);
    }

    #[test]
    fn stemming_is_conservative() {
        assert_eq!(stem("issues"), "issue");
        assert_eq!(stem("files"), "file");
        // Left alone: too short, -ss, -us, -is.
        assert_eq!(stem("is"), "is");
        assert_eq!(stem("class"), "class");
        assert_eq!(stem("status"), "status");
        assert_eq!(stem("analysis"), "analysis");
    }

    #[test]
    fn short_words_do_not_match_inside_longer_ones() {
        // The regression this scorer was rewritten for: "is"/"it" used to
        // match l-IS-t and comm-IT-s as substrings and outrank the real hit.
        let mut s = Snapshot::default();
        s.servers.insert(
            "time".into(),
            ServerSnapshot {
                tools: vec![ToolSnapshot {
                    name: "get_current_time".into(),
                    description: "Return the current time in a given timezone".into(),
                    input_schema: serde_json::json!({}),
                }],
                ..Default::default()
            },
        );
        s.servers.insert(
            "github".into(),
            ServerSnapshot {
                tools: vec![ToolSnapshot {
                    name: "list_commits".into(),
                    description: "List commits on a branch".into(),
                    input_schema: serde_json::json!({}),
                }],
                ..Default::default()
            },
        );
        let hits = search(&s, "what time is it", 5);
        assert_eq!(hits[0].tool, "get_current_time", "got {hits:?}");
    }

    #[test]
    fn plural_query_finds_singular_tool() {
        let hits = search(&snap(), "issues", 5);
        assert_eq!(hits[0].tool, "create_issue");
    }

    #[test]
    fn no_match_returns_nothing() {
        assert!(search(&snap(), "xyzzy_nonexistent", 5).is_empty());
    }

    /// Position is the entire justification for the leading-verb table, so
    /// test position rather than the mapping. A symmetric open -> create
    /// passes the first assertion and fails the second.
    #[test]
    fn leading_verb_expands_but_the_same_word_later_does_not() {
        let mut s = Snapshot::default();
        s.servers.insert(
            "github".into(),
            ServerSnapshot {
                tools: vec![
                    ToolSnapshot {
                        name: "create_issue".into(),
                        description: "Create a new issue in a repository".into(),
                        input_schema: serde_json::json!({}),
                    },
                    ToolSnapshot {
                        name: "list_issues".into(),
                        description: "List issues in a repository".into(),
                        input_schema: serde_json::json!({}),
                    },
                ],
                ..Default::default()
            },
        );

        // Leading: a verb. "open a bug report" means create one.
        let hits = search(&s, "open a bug report", 5);
        assert_eq!(hits[0].tool, "create_issue", "got {hits:?}");

        // Not leading: an adjective describing existing issues, so the same
        // word must not drag create_issue to the top.
        let hits = search(&s, "any open defects", 5);
        assert_eq!(hits[0].tool, "list_issues", "got {hits:?}");
    }
}
