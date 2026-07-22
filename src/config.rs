//! Configuration: a single JSON file, deliberately compatible with the
//! `mcpServers` shape used by Claude Desktop ("command" + "args") AND the
//! OpenCode shape ("command" as an array). Unknown fields are ignored so an
//! entry can be copy-pasted from either tool's config unchanged.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Skill libraries served on demand (dirs containing <skill>/SKILL.md).
    /// Skills here cost ZERO session context until read_skill fetches them.
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(rename = "mcpServers", alias = "servers", default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProxyConfig {
    /// Seconds allowed for a proxied tool call.
    pub call_timeout_secs: u64,
    /// Seconds allowed for a backend to start + initialize.
    pub startup_timeout_secs: u64,
    /// Refresh the tool snapshot at startup when the config changed.
    pub refresh_on_start: bool,
    /// How often to check if warm backends are still alive (seconds).
    /// 0 = disabled. When a dead connection is detected, it is reset
    /// so the next execute reconnects fresh.
    pub health_check_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            call_timeout_secs: 120,
            startup_timeout_secs: 30,
            refresh_on_start: true,
            health_check_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SkillsConfig {
    /// Directories scanned for skills (tilde-expanded).
    pub dirs: Vec<String>,
    /// Directories SWEPT into `dirs[0]` at startup and on refresh: any
    /// skill (subdir with SKILL.md) found here is MOVED into the library.
    /// Point this at your agent's native auto-loaded skills dir to enforce
    /// "nothing is ever native" — wherever a model drops a skill, the
    /// gateway absorbs it. Name collisions are skipped and reported.
    pub absorb: Vec<String>,
}

/// "command": "npx" + "args": [...]  (Claude style)  — or —
/// "command": ["npx", "-y", "server"] (OpenCode style)
/// Both forms deserialize to Vec<String>: a bare string becomes ["string"].
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct CommandField(Vec<String>);

impl<'de> Deserialize<'de> for CommandField {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Line(Vec<String>),
            Bin(String),
        }
        match Raw::deserialize(d)? {
            Raw::Line(l) => Ok(CommandField(l)),
            Raw::Bin(b) => Ok(CommandField(vec![b])),
        }
    }
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct ServerConfig {
    pub command: CommandField,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// One-line description shown in `list_mcp_servers`. Optional but worth
    /// writing: it is the only thing the agent sees before drilling in.
    #[serde(default)]
    pub description: Option<String>,
    /// Tool allowlist (glob, `*` wildcard). Empty = every tool.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tool denylist (glob). Deny wins over allow.
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    /// Resolved (binary, argv) with `${VAR}` env expansion applied.
    pub fn command_line(&self) -> Result<(String, Vec<String>)> {
        let (bin, mut argv) = {
            let cmd = &self.command.0;
            let Some((first, rest)) = cmd.split_first() else {
                bail!("empty command array");
            };
            (first.clone(), rest.to_vec())
        };
        argv.extend(self.args.iter().cloned());
        let bin = expand_value(&bin)?;
        let argv = argv
            .iter()
            .map(|a| expand_value(a))
            .collect::<Result<Vec<_>>>()?;
        Ok((bin, argv))
    }

    /// Env map with `${VAR}` / `${keychain:...}` expansion, resolved at
    /// spawn time.
    pub fn env_expanded(&self) -> Result<BTreeMap<String, String>> {
        self.env
            .iter()
            .map(|(k, v)| Ok((k.clone(), expand_value(v)?)))
            .collect()
    }

    /// Allow/deny filter for a backend tool name. Deny wins.
    pub fn tool_allowed(&self, name: &str) -> bool {
        if self.deny.iter().any(|p| glob_match(p, name)) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.iter().any(|p| glob_match(p, name))
    }
}

/// Minimal `*` glob: segments between stars must appear in order.
/// Covers `foo`, `foo*`, `*foo`, `*foo*`, `foo*bar`.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(found) => {
                // First segment must anchor at the start unless pattern starts with '*'.
                if i == 0 && found != 0 {
                    return false;
                }
                pos += found + part.len();
            }
            None => return false,
        }
    }
    // Last segment must anchor at the end unless pattern ends with '*'.
    if let Some(last) = parts.last() {
        if !last.is_empty() && !text.ends_with(last) {
            return false;
        }
    }
    true
}

/// Expand `${VAR}` (process environment, missing vars expand to "") and
/// `${keychain:SERVICE}` (OS keychain, missing items are a hard error —
/// secrets must fail loudly, not silently expand to nothing).
pub fn expand_value(input: &str) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find('}') {
            Some(end) => {
                let var = &rest[start + 2..start + 2 + end];
                out.push_str(&resolve_var(var)?);
                rest = &rest[start + 2 + end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve_var(var: &str) -> Result<String> {
    if let Some(service) = var.strip_prefix("keychain:") {
        return keychain_lookup(service.trim());
    }
    Ok(std::env::var(var).unwrap_or_default())
}

/// Resolve a secret from the OS keychain at spawn time. The secret never
/// appears in a config file, shell rc, or the proxy's own environment —
/// only in the spawned backend's env.
///
/// Deliberately NOT a generic `${cmd:...}`: arbitrary command execution
/// from a config file is an injection surface. Secrets come from the OS
/// keychain, nothing else.
fn keychain_lookup(service: &str) -> Result<String> {
    if service.is_empty() {
        bail!("empty keychain service name in ${{keychain:}}");
    }
    let (bin, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("security", vec!["find-generic-password", "-s", service, "-w"])
    } else {
        // Linux desktop: libsecret. Headless hosts should use plain env vars.
        ("secret-tool", vec!["lookup", "service", service])
    };
    let out = std::process::Command::new(bin)
        .args(&args)
        .output()
        .with_context(|| format!("cannot run '{bin}' to resolve keychain item '{service}'"))?;
    if !out.status.success() {
        bail!("keychain item '{service}' not found ('{bin}' exited {})", out.status);
    }
    let secret = String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    if secret.is_empty() {
        bail!("keychain item '{service}' resolved to an empty value");
    }
    Ok(secret)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yaml") | Some("yml")
        );
        let cfg: Config = if is_yaml {
            serde_norway::from_str(&raw)
                .with_context(|| format!("cannot parse YAML config {}", path.display()))?
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("cannot parse JSON config {}", path.display()))?
        };
        if cfg.servers.is_empty() && cfg.skills.dirs.is_empty() {
            bail!(
                "config has no mcpServers entries and no skills.dirs — \
                 nothing for the proxy to serve"
            );
        }
        if cfg.servers.is_empty() {
            tracing::info!(
                "config has no mcpServers — serving skills only \
                 (list_skills / search / read_skill)"
            );
        }
        Ok(cfg)
    }

    /// JSON Schema generated from these structs — valid for both the YAML
    /// and JSON config forms.
    pub fn schema_json() -> String {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema).unwrap_or_default()
    }

    /// Stable hash over everything that affects the tool snapshot. Uses
    /// the raw (unexpanded) command/args/env strings — expanding `${...}`
    /// here would trigger keychain lookups as a side effect of hashing,
    /// which is wrong for `geco inspect` and other read-only subcommands.
    pub fn snapshot_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for (name, s) in &self.servers {
            // Length-prefix each field to prevent hash collisions from
            // adjacent string boundary shifts (e.g. "foo"+"bar" vs "foob"+"ar").
            let field = |h: &mut Sha256, bytes: &[u8]| {
                h.update((bytes.len() as u64).to_le_bytes());
                h.update(bytes);
            };
            field(&mut hasher, name.as_bytes());
            for part in &s.command.0 {
                field(&mut hasher, part.as_bytes());
            }
            for a in &s.args {
                field(&mut hasher, a.as_bytes());
            }
            for (k, v) in &s.env {
                field(&mut hasher, k.as_bytes());
                field(&mut hasher, v.as_bytes());
            }
            for p in s.allow.iter().chain(s.deny.iter()) {
                field(&mut hasher, p.as_bytes());
            }
            hasher.update([s.enabled as u8]);
            if let Some(d) = &s.description {
                field(&mut hasher, d.as_bytes());
            }
        }
        hex(&hasher.finalize())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_style() {
        let cfg: Config = serde_json::from_str(
            r#"{"mcpServers":{"icm":{"command":"/opt/homebrew/bin/icm","args":["serve"],"env":{}}}}"#,
        )
        .unwrap();
        let (bin, argv) = cfg.servers["icm"].command_line().unwrap();
        assert_eq!(bin, "/opt/homebrew/bin/icm");
        assert_eq!(argv, vec!["serve"]);
    }

    #[test]
    fn parses_opencode_style() {
        let cfg: Config = serde_json::from_str(
            r#"{"mcpServers":{"icm":{"type":"local","command":["icm","serve"],"enabled":true}}}"#,
        )
        .unwrap();
        let (bin, argv) = cfg.servers["icm"].command_line().unwrap();
        assert_eq!(bin, "icm");
        assert_eq!(argv, vec!["serve"]);
    }

    #[test]
    fn allow_deny() {
        let s: ServerConfig = serde_json::from_str(
            r#"{"command":"x","allow":["icm_memory_*","icm_wake_up"],"deny":["*_forget_topic"]}"#,
        )
        .unwrap();
        assert!(s.tool_allowed("icm_memory_store"));
        assert!(s.tool_allowed("icm_wake_up"));
        assert!(!s.tool_allowed("icm_memory_forget_topic")); // deny wins
        assert!(!s.tool_allowed("icm_transcript_show")); // not in allow
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("foo*", "foobar"));
        assert!(!glob_match("foo*", "xfoobar"));
        assert!(glob_match("*bar", "foobar"));
        assert!(!glob_match("*bar", "barfoo"));
        assert!(glob_match("f*r", "foobar"));
        assert!(!glob_match("f*r", "foobaz"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exactly"));
    }

    #[test]
    fn env_expansion() {
        std::env::set_var("OM_TEST_VAR", "hello");
        assert_eq!(expand_value("${OM_TEST_VAR}/x").unwrap(), "hello/x");
        assert_eq!(expand_value("no vars").unwrap(), "no vars");
        assert_eq!(expand_value("${OM_MISSING_VAR}").unwrap(), "");
    }

    #[test]
    fn keychain_missing_item_is_hard_error() {
        // Regardless of platform, a nonexistent item (or missing keychain
        // binary) must produce an error naming the service — never an
        // empty expansion.
        let err = expand_value("${keychain:om-test-nonexistent-item-xyz}").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("om-test-nonexistent-item-xyz"), "got: {msg}");
    }

    #[test]
    fn keychain_empty_service_rejected() {
        assert!(expand_value("${keychain:}").is_err());
    }

    #[test]
    fn yaml_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.yaml");
        std::fs::write(&p, r#"
proxy:
  startup_timeout_secs: 60
mcpServers:
  websearch:
    command: [npx, -y, mcp-searxng]
    env:
      SEARXNG_URL: http://spark.local:8888
    description: private web search
    deny: ["*_dangerous"]
"#).unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.proxy.startup_timeout_secs, 60);
        let (bin, argv) = cfg.servers["websearch"].command_line().unwrap();
        assert_eq!(bin, "npx");
        assert_eq!(argv, vec!["-y", "mcp-searxng"]);
        assert!(!cfg.servers["websearch"].tool_allowed("x_dangerous"));
    }

    #[test]
    fn schema_covers_the_surface() {
        let s = Config::schema_json();
        for key in ["mcpServers", "allow", "deny", "call_timeout_secs"] {
            assert!(s.contains(key), "schema missing {key}");
        }
    }

    #[test]
    fn snapshot_hash_includes_description() {
        // Regression: editing only a server's description must change the
        // hash so the snapshot refreshes. Previously description was excluded.
        let mk = |desc: Option<&str>| -> String {
            let d = match desc {
                Some(s) => format!(r#","description":"{s}""#),
                None => String::new(),
            };
            let raw = format!(r#"{{"mcpServers":{{"s":{{"command":["npx"]{d}}}}}}}"#);
            let cfg: Config = serde_json::from_str(&raw).unwrap();
            cfg.snapshot_hash()
        };

        let h_none = mk(None);
        let h_a = mk(Some("first description"));
        let h_b = mk(Some("second description"));

        assert_ne!(h_none, h_a, "hash must change when description is added");
        assert_ne!(h_a, h_b, "hash must change when description is edited");
        assert_ne!(h_none, h_b, "hash must differ between no-desc and a desc");
    }
}