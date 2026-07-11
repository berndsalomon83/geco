//! The tool snapshot: what each backend offers, captured once and cached on
//! disk. This replaces lazy-mcp's separate `structure_generator` binary —
//! the proxy refreshes the snapshot itself whenever the config hash changes
//! (or on demand via the `refresh_tools` meta-tool).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub config_hash: String,
    pub generated_at: String,
    pub servers: BTreeMap<String, ServerSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerSnapshot {
    #[serde(default)]
    pub description: String,
    /// "ok" or "error: ..." from the last refresh attempt.
    #[serde(default)]
    pub refresh_status: String,
    #[serde(default)]
    pub tools: Vec<ToolSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSnapshot {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl Snapshot {
    pub fn cache_path(config_path: &Path) -> PathBuf {
        let dir = directories::ProjectDirs::from("dev", "geco", "geco")
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".ondemand-mcp-cache"));
        // One snapshot per config file location.
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(config_path.to_string_lossy().as_bytes());
        let tag: String = h
            .finalize()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        dir.join(format!("snapshot-{tag}.json"))
    }

    pub fn load(path: &Path) -> Option<Snapshot> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn tool_count(&self) -> usize {
        self.servers.values().map(|s| s.tools.len()).sum()
    }

    pub fn find_tool(&self, server: &str, tool: &str) -> Option<&ToolSnapshot> {
        self.servers
            .get(server)?
            .tools
            .iter()
            .find(|t| t.name == tool)
    }
}

/// Trim a description for list output: first sentence-ish, capped.
pub fn short_desc(desc: &str, max: usize) -> String {
    let flat = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = flat
        .char_indices()
        .nth(max)
        .map(|(i, _)| i)
        .unwrap_or(flat.len());
    if cut < flat.len() {
        format!("{}…", &flat[..cut].trim_end())
    } else {
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("snap.json");
        let mut s = Snapshot {
            config_hash: "abc".into(),
            generated_at: "now".into(),
            ..Default::default()
        };
        s.servers.insert(
            "icm".into(),
            ServerSnapshot {
                description: "memory".into(),
                refresh_status: "ok".into(),
                tools: vec![ToolSnapshot {
                    name: "icm_memory_store".into(),
                    description: "Store".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                }],
            },
        );
        s.save(&p).unwrap();
        let loaded = Snapshot::load(&p).unwrap();
        assert_eq!(loaded.config_hash, "abc");
        assert_eq!(loaded.tool_count(), 1);
        assert!(loaded.find_tool("icm", "icm_memory_store").is_some());
    }

    #[test]
    fn short_desc_caps() {
        assert_eq!(short_desc("hello world", 50), "hello world");
        let long = "x".repeat(300);
        assert!(short_desc(&long, 50).chars().count() <= 51);
    }
}
