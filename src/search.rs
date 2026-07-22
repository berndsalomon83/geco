//! One-call tool discovery across every backend — the feature lazy-mcp
//! lacks. Scoring is deliberately simple and dependency-free: exact name,
//! substring, then token hits in name/description/server.
//!
//! `score_match` is the shared scoring helper used by both tool search here
//! and skill search in `skills.rs` — same ranking, no duplication.

use crate::snapshot::Snapshot;

#[derive(Debug, serde::Serialize)]
pub struct Hit {
    pub server: String,
    pub tool: String,
    pub description: String,
    #[serde(skip)]
    pub score: i64,
}

/// Score a candidate against a query. `server` is an optional extra surface
/// (tool search counts server-name hits; skills have none).
///
/// Ranking: exact name (100) > name substring (60) > per-token hits in
/// name (20) / server (10) / description (8). Tokens shorter than 2 chars
/// are dropped — they'd match everything.
pub fn score_match(query_lower: &str, name: &str, description: &str, server: Option<&str>) -> i64 {
    let tokens: Vec<&str> = query_lower
        .split_whitespace()
        .filter(|t| t.len() > 1)
        .collect();
    let name_l = name.to_lowercase();
    let desc_l = description.to_lowercase();
    let server_l = server.map(|s| s.to_lowercase());
    let mut score = 0i64;

    if name_l == query_lower {
        score += 100;
    } else if name_l.contains(query_lower) {
        score += 60;
    }
    for t in &tokens {
        if name_l.contains(t) {
            score += 20;
        }
        if desc_l.contains(t) {
            score += 8;
        }
        if let Some(sl) = &server_l {
            if sl.contains(t) {
                score += 10;
            }
        }
    }
    score
}

pub fn search(snapshot: &Snapshot, query: &str, limit: usize) -> Vec<Hit> {
    let q = query.to_lowercase();
    let mut hits: Vec<Hit> = Vec::new();

    for (server, snap) in &snapshot.servers {
        for tool in &snap.tools {
            let score = score_match(&q, &tool.name, &tool.description, Some(server));
            if score > 0 {
                hits.push(Hit {
                    server: server.clone(),
                    tool: tool.name.clone(),
                    description: crate::snapshot::short_desc(&tool.description, 140),
                    score,
                });
            }
        }
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.tool.cmp(&b.tool)));
    hits.truncate(limit);
    hits
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
                    mk("icm_memory_store", "Store important information in long-term memory"),
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
}
