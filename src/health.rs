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
pub struct HealthReport {
    pub generation: u64,
    pub updated_at: String,
    /// The MCP client we're serving, from the initialize handshake
    /// ("opencode", "claude-ai", …) — so the plugin only toasts for itself.
    pub client: Option<String>,
    pub healthy: bool,
    pub issues: Vec<Issue>,
}

pub fn health_path() -> PathBuf {
    let dir = directories::ProjectDirs::from("dev", "geco", "geco")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".ondemand-mcp"));
    dir.join("health.json")
}

/// Write the current problem set. Empty issues == healthy (and that itself is
/// worth writing, so the plugin can clear a stale warning).
pub fn write(client: Option<&str>, issues: Vec<Issue>) {
    let path = health_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let report = HealthReport {
        generation: GENERATION.fetch_add(1, Ordering::SeqCst) + 1,
        updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        client: client.map(str::to_string),
        healthy: issues.is_empty(),
        issues,
    };
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        // best-effort atomic-ish write
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn read(path: &Path) -> Option<HealthReport> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_generation_increments() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path()); // ProjectDirs data dir under HOME on linux
        let issues = vec![Issue {
            kind: "backend".into(),
            subject: "github".into(),
            detail: "endpoint down".into(),
            severity: "error".into(),
        }];
        write(Some("opencode"), issues.clone());
        let r1 = read(&health_path()).unwrap();
        assert_eq!(r1.client.as_deref(), Some("opencode"));
        assert!(!r1.healthy);
        assert_eq!(r1.issues.len(), 1);
        write(Some("opencode"), vec![]);
        let r2 = read(&health_path()).unwrap();
        assert!(r2.healthy);
        assert!(r2.generation > r1.generation);
    }
}
