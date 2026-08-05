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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// (body hash, as-written file hash) of the last write to each path. Keyed
/// by path so parallel tests with different temp dirs don't clobber each
/// other. The file hash is what lets `should_skip_write` verify the DISK.
static LAST_CONTENT_HASHES: Mutex<Option<HashMap<PathBuf, (u64, u64)>>> = Mutex::new(None);

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Check whether a write for this body can be skipped. The in-process match
/// alone is NOT enough: health.json is one global file shared by every geco
/// instance (two agent sessions is normal MCP life), so another proxy — or
/// the user, deleting the file as a manual reset — can change the disk
/// behind this process's memory. Skipping on memory alone made a proxy's
/// standing problem set silently vanish from disk forever: its every
/// publish rebuilt an identical body, the map said "already written", and
/// the file kept claiming whatever the other instance last wrote.
///
/// But symmetric re-assertion is its own bug: two live instances that both
/// always rewrote a foreign file ping-ponged it every sweep, and `geco
/// health`'s exit code oscillated with whichever sweep ran last (measured:
/// 63 healthy-flag flips in 200 samples). So re-assertion is one-sided —
/// a body WITH issues always reclaims the file (errors must not stay
/// buried), while a healthy body reclaims it only from nobody: when the
/// file is missing, unreadable, or carries a clean-exit marker. A healthy
/// instance seeing another live instance's problem report leaves it alone;
/// the rare case of two instances with DIFFERENT standing problem sets
/// still alternates, which is accepted — both claims are true, and
/// arbitrating them would need per-instance files.
/// Does NOT update the hash — call `commit_hash` after the write succeeds.
fn should_skip_write(path: &Path, body_hash: u64, has_issues: bool) -> bool {
    let stored_file_hash = {
        let guard = LAST_CONTENT_HASHES
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match guard.as_ref().and_then(|m| m.get(path)) {
            Some((b, f)) if *b == body_hash => *f,
            _ => return false,
        }
    };
    let Ok(bytes) = std::fs::read(path) else {
        return false; // missing or unreadable — (re)create it
    };
    if fnv1a(&bytes) == stored_file_hash {
        return true; // disk already says exactly what we would write
    }
    if has_issues {
        return false; // problems re-assert over whatever is there
    }
    // Healthy, and the file is someone else's: only claim it back when its
    // writer said goodbye (or the content is not a report at all).
    match serde_json::from_slice::<HealthReport>(&bytes) {
        Ok(other) => other.stopped_at.is_none(), // live foreign report — defer
        Err(_) => false,
    }
}

/// Record both hashes after a successful write to disk.
fn commit_hash(path: &Path, body_hash: u64, file_hash: u64) {
    let mut guard = LAST_CONTENT_HASHES
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(path.to_path_buf(), (body_hash, file_hash));
}

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

/// `#[serde(default)]` at the struct level so a health.json written by an
/// older geco still loads when fields are added.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Metrics {
    pub started_at: String,
    pub total_calls: u64,
    pub total_errors: u64,
    pub per_backend: std::collections::BTreeMap<String, BackendMetrics>,
    /// Search effectiveness. A tool the agent cannot find may as well not be
    /// installed, so these answer the question the tool-call counters can't:
    /// is discovery actually working?
    pub searches: u64,
    /// Searches that returned nothing at all.
    pub searches_no_match: u64,
    /// Executions of a tool that a recent search had surfaced — i.e. search
    /// led somewhere, rather than the agent finding the tool another way.
    pub search_to_execute: u64,
    /// The most recent queries that matched nothing, capped. This is the
    /// actionable list: each one is either a missing tool or a description
    /// that doesn't say what the tool is for.
    pub recent_no_match: Vec<String>,
    /// Searches that surfaced at least one SKILL. Skills are a "push"
    /// resource — nothing in a user's request names them, so the model has to
    /// reach for one unprompted. These two counters answer the only question
    /// that matters about that: are skills being found, and then actually
    /// read? A library with many surfaces and no reads means the descriptions
    /// don't match how work is described; near-zero surfaces means the model
    /// isn't looking at all, which is a different problem entirely.
    pub skill_surfaces: u64,
    /// Successful `read_skill` calls — a skill that actually got used.
    pub skill_reads: u64,
    /// Calls per meta-tool. The always-on surface is the one cost geco makes a
    /// promise about, so which of those tools a model ACTUALLY reaches for is
    /// the number that should decide whether any of them keeps its bytes.
    /// Without this the question can only be argued, and in this project
    /// roughly 30% of confident-looking predictions survive measurement.
    #[serde(default)]
    pub per_meta_tool: std::collections::BTreeMap<String, u64>,
}

/// `#[serde(default)]` at the struct level for the same reason as `Metrics`:
/// a health.json whose writer and reader disagree about the field set must
/// still load, not surface as "the proxy has not run yet".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
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
    /// Set by `mark_stopped` when a proxy exits cleanly. The file always
    /// outlives its writer, so without this a proxy that hit an outage
    /// yesterday failed `geco health` scripts today — the exit code carried
    /// last week's state as "currently broken". A stale None still means
    /// "running or crashed"; a Some means the last writer said goodbye.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<String>,
}

/// Where the health file lives. `GECO_HEALTH` overrides the default —
/// the companion opencode plugin already honours that variable when
/// locating the file, so geco not honouring it meant pointing the plugin
/// somewhere else silently broke the pairing. It also lets the test suite
/// redirect writes into a temp dir instead of scribbling on the developer's
/// real health.json.
pub fn health_path() -> PathBuf {
    if let Some(p) = std::env::var_os("GECO_HEALTH") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let dir = directories::ProjectDirs::from("dev", "geco", "geco")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".geco"));
    dir.join("health.json")
}

/// Write the current problem set to a specific path (test-friendly; the
/// production path is `health_path()`). Empty issues == healthy.
///
/// Skips the disk write entirely if the content (issues + metrics) hasn't
/// changed since the last write. Note this only elides genuinely redundant
/// writes: once any tool call has run, `metrics.last_call_at` and the counters
/// differ every time, so the skip mostly covers the idle case (repeated
/// publish_health with no calls in between) rather than the busy one.
pub fn write_to(path: &Path, client: Option<&str>, issues: Vec<Issue>, metrics: Option<Metrics>) {
    // Build the report body (without generation/updated_at, which always
    // change and would defeat the content-hash check).
    let body = HealthReportBody {
        client: client.map(str::to_string),
        healthy: issues.is_empty(),
        issues,
        metrics,
    };
    let body_json = serde_json::to_string(&body).unwrap_or_default();
    let hash = fnv1a(body_json.as_bytes());

    // Skip the write if nothing changed since last time (per-path).
    if should_skip_write(path, hash, !body.issues.is_empty()) {
        return;
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let report = HealthReport {
        generation: GENERATION.fetch_add(1, Ordering::SeqCst) + 1,
        updated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        client: body.client,
        healthy: body.healthy,
        issues: body.issues,
        metrics: body.metrics,
        // A live publish always clears the clean-exit marker: whatever
        // wrote it, something is serving again now.
        stopped_at: None,
    };
    let json = match serde_json::to_string_pretty(&report) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("health: cannot serialize report: {e}");
            return;
        }
    };
    {
        static HEALTH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            HEALTH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        if std::fs::write(&tmp, &json).is_ok() {
            if std::fs::rename(&tmp, path).is_ok() {
                commit_hash(path, hash, fnv1a(json.as_bytes()));
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}

/// Stamp the health file with a clean-exit marker. Called once, after the
/// backends are down. Rewrites the CURRENT file (whatever instance wrote it
/// last) rather than this process's view, so a newer report from a second
/// session is annotated, not overwritten — and that session's next publish
/// clears the marker again via the disk verification in
/// `should_skip_write`. Failures are ignored: exit is not the moment to
/// fight the filesystem.
pub fn mark_stopped() {
    let path = health_path();
    let HealthFile::Report(mut report) = read(&path) else {
        return;
    };
    report.stopped_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    // Bump the FILE's generation, not this process's counter: the file may
    // carry a higher generation from another instance, and the plugin's
    // fresh-write detection needs monotonicity per file.
    report.generation = report.generation.saturating_add(1);
    if let Ok(json) = serde_json::to_string_pretty(&report) {
        let tmp = path.with_extension(format!("json.tmp.stop.{}", std::process::id()));
        if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// The mutable parts of a health report — used for content hashing
/// (generation and updated_at always change, so they're excluded).
#[derive(Serialize)]
struct HealthReportBody {
    client: Option<String>,
    healthy: bool,
    issues: Vec<Issue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<Metrics>,
}

/// Write the current problem set. Empty issues == healthy (and that itself is
/// worth writing, so the plugin can clear a stale warning).
pub fn write(client: Option<&str>, issues: Vec<Issue>, metrics: Option<Metrics>) {
    write_to(&health_path(), client, issues, metrics);
}

/// What's at the health path. `Missing` and `Unreadable` used to collapse
/// into one `None` → "the proxy has not run yet" — reporting a present file
/// with recorded errors as a proxy that never existed. The exact
/// absent-vs-corrupt conflation snapshot.rs already fixed for snapshots;
/// health.json gets the same treatment.
pub enum HealthFile {
    Missing,
    Unreadable(String),
    /// Boxed for the variant-size lint: a report is ~264 bytes against
    /// Missing's zero, and every caller immediately unpacks it anyway.
    Report(Box<HealthReport>),
}

pub fn read(path: &Path) -> HealthFile {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HealthFile::Missing,
        Err(e) => return HealthFile::Unreadable(e.to_string()),
    };
    match serde_json::from_str(&raw) {
        Ok(rep) => HealthFile::Report(Box::new(rep)),
        Err(e) => {
            tracing::warn!("health file {} is unreadable: {e}", path.display());
            HealthFile::Unreadable(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(path: &Path) -> HealthReport {
        match read(path) {
            HealthFile::Report(r) => *r,
            HealthFile::Missing => panic!("health file missing at {}", path.display()),
            HealthFile::Unreadable(e) => panic!("health file unreadable: {e}"),
        }
    }

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
        let r1 = report(&path);
        assert_eq!(r1.client.as_deref(), Some("opencode"));
        assert!(!r1.healthy);
        assert_eq!(r1.issues.len(), 1);
        write_to(&path, Some("opencode"), vec![], None);
        let r2 = report(&path);
        assert!(r2.healthy);
        assert!(r2.generation > r1.generation);
    }

    #[test]
    fn identical_content_skips_disk_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let issues = vec![Issue {
            kind: "backend".into(),
            subject: "test".into(),
            detail: "flapping".into(),
            severity: "error".into(),
        }];
        write_to(&path, Some("opencode"), issues.clone(), None);
        let gen1 = report(&path).generation;
        write_to(&path, Some("opencode"), issues, None);
        let gen2 = report(&path).generation;
        assert_eq!(gen1, gen2, "identical content should skip the disk write");
    }

    /// The skip must trust memory only as far as the disk agrees. health.json
    /// is one global file shared by every instance: another proxy overwriting
    /// it (or the user deleting it as a manual reset) used to be invisible —
    /// this process's map said "already written" and its standing problem set
    /// stayed vanished from disk forever.
    #[test]
    fn skip_write_notices_the_file_changed_behind_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let issues = vec![Issue {
            kind: "backend".into(),
            subject: "github".into(),
            detail: "endpoint down".into(),
            severity: "error".into(),
        }];
        write_to(&path, Some("opencode"), issues.clone(), None);

        // Deleted behind our back — the same body must be written again.
        std::fs::remove_file(&path).unwrap();
        write_to(&path, Some("opencode"), issues.clone(), None);
        assert!(
            !report(&path).healthy,
            "a deleted health file must be recreated, not skipped"
        );

        // Overwritten by another instance — ditto.
        std::fs::write(
            &path,
            r#"{"generation":9,"updated_at":"t","healthy":true,"issues":[]}"#,
        )
        .unwrap();
        write_to(&path, Some("opencode"), issues, None);
        let r = report(&path);
        assert!(
            !r.healthy,
            "an externally replaced health file must be restored: {r:?}"
        );
    }

    /// One-sided re-assertion: a HEALTHY instance must not rewrite another
    /// live instance's problem report — symmetric re-assertion made two
    /// instances ping-pong the file every sweep and `geco health`'s exit
    /// code oscillate. It reclaims the file only from a stopped writer.
    #[test]
    fn healthy_writer_defers_to_a_live_problem_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        write_to(&path, Some("opencode"), vec![], None);
        assert!(report(&path).healthy);

        // Another live instance records a problem set.
        let foreign = r#"{"generation":9,"updated_at":"t","healthy":false,"issues":[{"kind":"backend","subject":"github","detail":"down","severity":"error"}]}"#;
        std::fs::write(&path, foreign).unwrap();
        write_to(&path, Some("opencode"), vec![], None);
        assert!(
            !report(&path).healthy,
            "a healthy publish must not bury another live instance's errors"
        );

        // Same foreign content but with a clean-exit marker: nobody claims
        // to be serving, so the healthy instance takes the file back.
        let stopped = foreign.replace(r#""issues":"#, r#""stopped_at":"t2","issues":"#);
        std::fs::write(&path, stopped).unwrap();
        write_to(&path, Some("opencode"), vec![], None);
        let r = report(&path);
        assert!(
            r.healthy && r.stopped_at.is_none(),
            "a stopped writer's file must be reclaimed: {r:?}"
        );
    }

    /// Corrupt and absent are different situations and must read differently:
    /// "the proxy has not run yet" about a present file full of recorded
    /// errors sends people hunting for a missing file that is sitting there.
    #[test]
    fn read_distinguishes_missing_from_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        assert!(matches!(read(&path), HealthFile::Missing));
        std::fs::write(&path, "{ not json").unwrap();
        assert!(matches!(read(&path), HealthFile::Unreadable(_)));
        // Forward-compat: unknown/missing fields load via serde(default)
        // rather than surfacing as corrupt.
        std::fs::write(&path, r#"{"healthy":false,"issues":[]}"#).unwrap();
        assert!(matches!(read(&path), HealthFile::Report(_)));
    }

    /// mark_stopped annotates the CURRENT file; the next live publish clears
    /// the marker again (via the disk verification, even when the body is
    /// otherwise unchanged).
    #[test]
    fn stopped_marker_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        std::env::set_var("GECO_HEALTH", &path);
        write_to(&path, Some("opencode"), vec![], None);
        assert!(report(&path).stopped_at.is_none());
        mark_stopped();
        std::env::remove_var("GECO_HEALTH");
        assert!(
            report(&path).stopped_at.is_some(),
            "a clean exit must leave the marker"
        );
        write_to(&path, Some("opencode"), vec![], None);
        assert!(
            report(&path).stopped_at.is_none(),
            "a live publish must clear the marker"
        );
    }
}
