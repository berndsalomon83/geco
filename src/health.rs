//! Health sink — so failures are LOUD, not silently logged.
//!
//! The proxy runs headless behind an agent; when a backend endpoint is down,
//! an absorb collides, or a refresh partially fails, a tracing::warn to stderr
//! is a scream in an empty room. This module records the current problem set
//! to a small JSON file that a companion opencode TUI plugin polls and turns
//! into a toast — and the `health` meta-tool returns the same set on demand so
//! the agent (or you) can ask "is the gateway OK?" and get a real answer.
//!
//! Design: level-triggered, not edge. We write the FULL current problem set
//! every refresh (empty = healthy). The plugin diffs against what it last
//! showed, so a persistent problem toasts once, and recovery is observable
//! (the set goes empty). A monotonically increasing `generation` lets the
//! plugin detect a fresh write cheaply.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    /// "backend" | "skills" | "preflight" | "config"
    pub kind: String,
    /// The server/skill/thing the issue is about.
    pub subject: String,
    pub detail: String,
    /// "error" | "warning"
    pub severity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackendMetrics {
    pub calls: u64,
    pub errors: u64,
    pub cold_starts: u64,
    pub total_latency_ms: u64,
    pub last_call_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub started_at: String,
    pub total_calls: u64,
    pub total_errors: u64,
    pub per_backend: std::collections::BTreeMap<String, BackendMetrics>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthReport {
    pub generation: u64,
    pub updated_at: String,
    /// The MCP client we're serving, from the initialize handshake
    /// ("opencode", "claude-ai", …) — so the plugin only toasts for itself.
    pub client: Option<String>,
    pub healthy: bool,
    pub issues: Vec<Issue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Metrics>,
}

pub fn health_path() -> PathBuf {
    let dir = directories::ProjectDirs::from("dev", "geco", "geco")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".geco"));
    dir.join("health.json")
}

/// Write the current problem set to a specific path (test-friendly; the
/// production path is `health_path()`). Empty issues == healthy.
pub fn write_to(path: &Path, client: Option<&str>, issues: Vec<Issue>, metrics: Option<Metrics>) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let report = HealthReport {
        generation: GENERATION.fetch_add(1, Ordering::SeqCst) + 1,
        updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        client: client.map(str::to_string),
        healthy: issues.is_empty(),
        issues,
        metrics,
    };
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        // best-effort atomic-ish write
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

/// Write the current problem set. Empty issues == healthy (and that itself is
/// worth writing, so the plugin can clear a stale warning).
pub fn write(client: Option<&str>, issues: Vec<Issue>, metrics: Option<Metrics>) {
    write_to(&health_path(), client, issues, metrics);
}

pub fn read(path: &Path) -> Option<HealthReport> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_generation_increments() {
        // Write to an explicit temp path — no process-global HOME mutation.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let issues = vec![Issue {
            kind: "backend".into(),
            subject: "github".into(),
            detail: "endpoint down".into(),
            severity: "error".into(),
        }];
        write_to(&path, Some("opencode"), issues.clone(), None);
        let r1 = read(&path).unwrap();
        assert_eq!(r1.client.as_deref(), Some("opencode"));
        assert!(!r1.healthy);
        assert_eq!(r1.issues.len(), 1);
        write_to(&path, Some("opencode"), vec![], None);
        let r2 = read(&path).unwrap();
        assert!(r2.healthy);
        assert!(r2.generation > r1.generation);
    }
}
