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
#[derive(Debug, Clone, Serialize)]
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

impl schemars::JsonSchema for CommandField {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CommandField".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut map = serde_json::Map::new();
        map.insert("oneOf".into(), serde_json::json!([
            {"type": "string"},
            {"type": "array", "items": {"type": "string"}}
        ]));
        map.into()
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
    /// Environment variables to forward from the proxy's own environment
    /// to the backend at spawn time. Shortcut for `env: { KEY: "${KEY}" }`
    /// — useful when multiple backends share the same secret that's already
    /// in the proxy's env (e.g. `pass_env: ["GITHUB_TOKEN"]`).
    #[serde(default)]
    pub pass_env: Vec<String>,
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
    /// spawn time. Also forwards `pass_env` variables from the proxy's own
    /// environment — missing vars are silently skipped (the backend will
    /// see them as unset, which is the correct behavior for optional vars).
    pub fn env_expanded(&self) -> Result<BTreeMap<String, String>> {
        let mut envs: BTreeMap<String, String> = self
            .env
            .iter()
            .map(|(k, v)| Ok((k.clone(), expand_value(v)?)))
            .collect::<Result<_>>()?;
        for key in &self.pass_env {
            if envs.contains_key(key) {
                continue; // explicit env wins over pass_env
            }
            if let Ok(val) = std::env::var(key) {
                envs.insert(key.clone(), val);
            }
        }
        Ok(envs)
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
            None => bail!("unterminated ${{...}} in value: {input}")
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
    let (bin, args): (String, Vec<String>) = if cfg!(target_os = "macos") {
        ("security".into(), vec![
            "find-generic-password".into(), "-s".into(), service.into(), "-w".into()
        ])
    } else if cfg!(target_os = "windows") {
        // Windows Credential Manager via PowerShell + Win32 CredReadW.
        // The service name is passed via the GECO_KEYCHAIN_SERVICE env var
        // to avoid PowerShell injection through string interpolation.
        let ps_script = "\
         $sig='[DllImport(\"advapi32.dll\", CharSet=CharSet.Unicode, SetLastError=true)]\
         public static extern bool CredReadW(string t, uint ty, uint f, out System.IntPtr c);\
         [DllImport(\"advapi32.dll\")] public static extern void CredFree(System.IntPtr c);';\
         $t=Add-Type -MemberDefinition $sig -Name Cred -PassThru;\
         $p=[IntPtr]::Zero;\
         $svc=$env:GECO_KEYCHAIN_SERVICE;\
         if($t::CredReadW($svc,1,0,[ref]$p)){\
         $base=$p.ToInt64();\
         $cb=([Runtime.InteropServices.Marshal]::ReadInt32([IntPtr]$base+32));\
         $blob=[IntPtr]($base+40);\
         $b=New-Object byte[] $cb;\
         [Marshal]::Copy($blob,$b,0,$cb);\
         $pw=[Text.Encoding]::Unicode.GetString($b);\
         $t::CredFree($p);\
         Write-Output $pw}else{exit 1}";
        let mut cmd = std::process::Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", ps_script]);
        cmd.env("GECO_KEYCHAIN_SERVICE", service);
        let out = cmd
            .output()
            .with_context(|| format!("cannot run 'powershell' to resolve keychain item '{service}'"))?;
        if !out.status.success() {
            bail!("keychain item '{service}' not found ('powershell' exited {})", out.status);
        }
        let raw = String::from_utf8_lossy(&out.stdout);
        let secret = raw.strip_suffix("\r\n").or_else(|| raw.strip_suffix('\n')).unwrap_or(&raw);
        let secret = secret.to_string();
        if secret.is_empty() {
            bail!("keychain item '{service}' resolved to an empty value");
        }
        return Ok(secret);
    } else {
        // Linux desktop: libsecret. Headless hosts should use plain env vars.
        ("secret-tool".into(), vec![
            "lookup".into(), "service".into(), service.into()
        ])
    };
    let out = std::process::Command::new(&bin)
        .args(&args)
        .output()
        .with_context(|| format!("cannot run '{bin}' to resolve keychain item '{service}'"))?;
    if !out.status.success() {
        bail!("keychain item '{service}' not found ('{bin}' exited {})", out.status);
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    // Strip the single trailing newline that CLI tools add — don't use
    // trim_end_matches because it would strip legitimate trailing newlines
    // in the secret itself.
    let secret = raw.strip_suffix("\r\n").or_else(|| raw.strip_suffix('\n')).unwrap_or(&raw);
    let secret = secret.to_string();
    if secret.is_empty() {
        bail!("keychain item '{service}' resolved to an empty value");
    }
    Ok(secret)
}

/// Check whether a binary is resolvable: either a path that exists, or a
/// name found in any directory on PATH. Used by `validate` to catch typo'd
/// commands before they fail at first `execute`.
fn binary_exists(bin: &str) -> bool {
    let p = std::path::Path::new(bin);
    // If it contains a path separator, check the path directly.
    if p.components().count() > 1 {
        return p.exists();
    }
    // Otherwise scan PATH dirs.
    let Some(path) = std::env::var_os("PATH") else { return true; };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.exists() {
            return true;
        }
        // On Windows, try with .exe suffix.
        if cfg!(target_os = "windows") {
            let with_ext = dir.join(format!("{bin}.exe"));
            if with_ext.exists() {
                return true;
            }
        }
    }
    false
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

    /// Validate the config for common mistakes. Returns a list of
    /// (severity, message) tuples — "error" means the proxy will fail
    /// at runtime, "warning" means something might not work as expected.
    pub fn validate(&self) -> Vec<(String, String)> {
        let mut findings = Vec::new();

        // Proxy-level checks.
        if self.proxy.startup_timeout_secs == 0 {
            findings.push(("error".into(),
                "proxy.startup_timeout_secs is 0 — backends will fail to initialize instantly".into()));
        }
        if self.proxy.call_timeout_secs == 0 {
            findings.push(("error".into(),
                "proxy.call_timeout_secs is 0 — every tool call will time out instantly".into()));
        }

        // Per-server checks.
        for (name, s) in &self.servers {
            if s.command.0.is_empty() {
                findings.push(("error".into(),
                    format!("server '{name}': empty command array")));
            } else if let Ok((bin, _)) = s.command_line() {
                // Warn if the binary doesn't exist on PATH or as a path.
                // Skipped for known package managers that download the real
                // server at runtime — a missing npx is the user's env problem,
                // not a config mistake.
                const DOWNLOADERS: &[&str] = &[
                    "npx", "uvx", "uv", "node", "nodejs",
                    "python3", "python", "pip", "pipx",
                    "docker", "docker-compose",
                    "bun", "deno", "cargo",
                ];
                let bin_name = std::path::Path::new(&bin)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(&bin);
                if !DOWNLOADERS.contains(&bin_name) && !binary_exists(&bin) {
                    findings.push(("warning".into(),
                        format!("server '{name}': command '{bin}' not found on PATH (will fail at spawn)")));
                }
            }
            // Try keychain lookups — verifies the item exists at validate time.
            // Loop through all ${keychain:...} occurrences in each value.
            for (k, v) in &s.env {
                let mut search = 0;
                while let Some(start) = v[search..].find("${keychain:") {
                    let abs = search + start;
                    if let Some(end) = v[abs..].find('}') {
                        let service = &v[abs + 11..abs + end];
                        if service.is_empty() {
                            findings.push(("error".into(),
                                format!("server '{name}': env '{k}' has empty keychain service name")));
                        } else if keychain_lookup(service).is_err() {
                            findings.push(("error".into(),
                                format!("server '{name}': keychain item '{service}' not found (will fail at spawn)")));
                        }
                        search = abs + end + 1;
                    } else {
                        break; // unterminated ${keychain: — expand_value will error at runtime
                    }
                }
            }
            // Warn on unset ${VAR} references in command, args, and env.
            // A typo like ${GITHUB_TOKN} silently expands to "" — the backend
            // gets an empty token and fails with an opaque auth error. Keychain
            // refs are already checked above (hard error); these are plain env.
            let scan_var = |value: &str, field: &str, findings: &mut Vec<(String, String)>| {
                let mut search = 0;
                while let Some(start) = value[search..].find("${") {
                    let abs = search + start;
                    if let Some(end) = value[abs + 2..].find('}') {
                        let var = &value[abs + 2..abs + 2 + end];
                        // Skip keychain refs — handled above.
                        if !var.starts_with("keychain:") && std::env::var(var).is_err() {
                            findings.push(("warning".into(),
                                format!("server '{name}': {field} references '${{{var}}}' which is not set (will expand to empty)")));
                        }
                        search = abs + 2 + end + 1;
                    } else {
                        break;
                    }
                }
            };
            for c in &s.command.0 {
                scan_var(c, "command", &mut findings);
            }
            for a in &s.args {
                scan_var(a, "args", &mut findings);
            }
            for (k, v) in &s.env {
                scan_var(v, &format!("env[{k}]"), &mut findings);
            }
            // pass_env vars not in the proxy's env (warning).
            for var in &s.pass_env {
                if std::env::var(var).is_err() {
                    findings.push(("warning".into(),
                        format!("server '{name}': pass_env '{var}' not set in proxy environment")));
                }
            }
            // Empty glob patterns.
            for p in s.allow.iter().chain(s.deny.iter()) {
                if p.is_empty() {
                    findings.push(("warning".into(),
                        format!("server '{name}': empty glob pattern in allow/deny")));
                }
            }
        }

        // Skills dir checks.
        for dir in &self.skills.dirs {
            let expanded = crate::skills::expand_tilde(dir);
            if !expanded.exists() {
                findings.push(("warning".into(),
                    format!("skills.dirs: '{}' does not exist", dir)));
            }
        }
        for dir in &self.skills.absorb {
            let expanded = crate::skills::expand_tilde(dir);
            if !expanded.exists() {
                findings.push(("warning".into(),
                    format!("skills.absorb: '{}' does not exist", dir)));
            }
        }

        findings
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
            for p in &s.pass_env {
                field(&mut hasher, p.as_bytes());
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
        for key in ["mcpServers", "allow", "deny", "call_timeout_secs", "pass_env", "health_check_secs"] {
            assert!(s.contains(key), "schema missing {key}");
        }
    }

    /// The checked-in geco.schema.json must match the runtime-generated schema.
    /// Catches drift whenever a config field is added/removed without
    /// regenerating the file (`geco schema > geco.schema.json`).
    #[test]
    fn schema_json_matches_committed_file() {
        let committed = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("geco.schema.json")
        ).expect("geco.schema.json must exist at crate root");
        let generated = Config::schema_json();
        // Compare after parsing so formatting/whitespace differences don't
        // cause false failures — only structural drift is reported.
        let committed_val: serde_json::Value = serde_json::from_str(&committed)
            .expect("geco.schema.json must be valid JSON");
        let generated_val: serde_json::Value = serde_json::from_str(&generated)
            .expect("runtime schema must be valid JSON");
        assert_eq!(committed_val, generated_val,
            "geco.schema.json is stale — run: geco schema > geco.schema.json");
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

    #[test]
    fn pass_env_forwards_from_process_env() {
        std::env::set_var("GECO_TEST_PASS_ENV", "secret123");
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"pass_env":["GECO_TEST_PASS_ENV"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let envs = cfg.servers["s"].env_expanded().unwrap();
        assert_eq!(envs["GECO_TEST_PASS_ENV"], "secret123");
        std::env::remove_var("GECO_TEST_PASS_ENV");
    }

    #[test]
    fn pass_env_missing_var_is_silently_skipped() {
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"pass_env":["GECO_NONEXISTENT_VAR_XYZ"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let envs = cfg.servers["s"].env_expanded().unwrap();
        assert!(!envs.contains_key("GECO_NONEXISTENT_VAR_XYZ"));
    }

    #[test]
    fn pass_env_does_not_override_explicit_env() {
        std::env::set_var("GECO_TEST_OVERRIDE", "from_process");
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"env":{"GECO_TEST_OVERRIDE":"from_config"},"pass_env":["GECO_TEST_OVERRIDE"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let envs = cfg.servers["s"].env_expanded().unwrap();
        // Explicit env wins over pass_env.
        assert_eq!(envs["GECO_TEST_OVERRIDE"], "from_config");
        std::env::remove_var("GECO_TEST_OVERRIDE");
    }

    #[test]
    fn validate_catches_missing_keychain() {
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"env":{"TOKEN":"${keychain:nonexistent-xyz-123}"}}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "error" && m.contains("nonexistent-xyz-123")),
                "expected error about missing keychain item: {findings:?}");
    }

    #[test]
    fn validate_warns_missing_pass_env() {
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"pass_env":["GECO_NONEXISTENT_XYZ"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "warning" && m.contains("GECO_NONEXISTENT_XYZ")),
                "expected warning about missing pass_env: {findings:?}");
    }

    #[test]
    fn validate_warns_missing_skills_dir() {
        let raw = r#"{"skills":{"dirs":["/nonexistent/path/xyz"]},"mcpServers":{"s":{"command":["npx"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "warning" && m.contains("does not exist")),
                "expected warning about missing skills dir: {findings:?}");
    }

    #[test]
    fn validate_passes_clean_config() {
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"env":{"FOO":"bar"}}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        let errors: Vec<_> = findings.iter().filter(|(s, _)| s == "error").collect();
        assert!(errors.is_empty(), "expected no errors for clean config: {findings:?}");
    }

    #[test]
    fn validate_catches_zero_startup_timeout() {
        let raw = r#"{"proxy":{"startup_timeout_secs":0},"mcpServers":{"s":{"command":["npx"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "error" && m.contains("startup_timeout_secs")),
                "expected error about zero startup_timeout_secs: {findings:?}");
    }

    #[test]
    fn validate_catches_zero_call_timeout() {
        let raw = r#"{"proxy":{"call_timeout_secs":0},"mcpServers":{"s":{"command":["npx"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "error" && m.contains("call_timeout_secs")),
                "expected error about zero call_timeout_secs: {findings:?}");
    }

    #[test]
    fn validate_warns_missing_binary() {
        let raw = r#"{"mcpServers":{"s":{"command":["geco-nonexistent-binary-xyz"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "warning" && m.contains("not found on PATH")),
                "expected warning about missing binary: {findings:?}");
    }

    #[test]
    fn validate_does_not_warn_for_npx() {
        // npx is a known package manager — even if missing, it's not a config typo.
        let raw = r#"{"mcpServers":{"s":{"command":["npx","-y","some-server"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(!findings.iter().any(|(_, m)| m.contains("not found on PATH")),
                "npx should not trigger binary-not-found warning: {findings:?}");
    }

    #[test]
    fn validate_warns_unset_env_var() {
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"env":{"TOKEN":"${GECO_TYPO_VAR_XYZ}"}}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "warning" && m.contains("GECO_TYPO_VAR_XYZ")),
                "expected warning about unset env var: {findings:?}");
    }

    #[test]
    fn validate_warns_unset_env_var_in_args() {
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"args":["${GECO_TYPO_ARG_XYZ}"]}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        assert!(findings.iter().any(|(s, m)| s == "warning" && m.contains("GECO_TYPO_ARG_XYZ")),
                "expected warning about unset env var in args: {findings:?}");
    }

    #[test]
    fn validate_does_not_warn_set_env_var() {
        std::env::set_var("GECO_SET_VAR_TEST", "value");
        let raw = r#"{"mcpServers":{"s":{"command":["npx"],"env":{"X":"${GECO_SET_VAR_TEST}"}}}}"#;
        let cfg: Config = serde_json::from_str(raw).unwrap();
        let findings = cfg.validate();
        let env_warnings: Vec<_> = findings.iter()
            .filter(|(_, m)| m.contains("GECO_SET_VAR_TEST"))
            .collect();
        assert!(env_warnings.is_empty(), "set var should not warn: {findings:?}");
        std::env::remove_var("GECO_SET_VAR_TEST");
    }
}