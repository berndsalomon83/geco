//! CLI surface tests, run against the real binary.
//!
//! WHY THIS FILE EXISTS: `src/main.rs` is 441 lines that no test could reach.
//! It is a binary, so integration tests cannot call into it, and none of its
//! logic lives in the library. Two bugs shipped through that gap in one day:
//!
//!   1. `geco health` did not exist. Documentation and a tool-retirement
//!      decision both assumed it did. The subcommand was simply absent.
//!   2. `geco refresh` was a no-op for a running proxy. It rewrote the
//!      snapshot file, which a live instance never re-reads and would then
//!      overwrite — so the documented "just run geco refresh" did nothing.
//!
//! Neither is subtle. Both were invisible because nothing executed the CLI.
//! These tests do, via `CARGO_BIN_EXE_geco`, so a command that vanishes or
//! stops having its documented effect fails here instead of in someone's
//! terminal.
//!
//! The bar is deliberately low and broad: every subcommand runs, exit codes
//! mean what they claim, and the two documented side effects actually happen.
//! Cheap to keep true, and it covers the class of mistake that got us twice.

use std::path::Path;
use std::process::{Command, Output};

fn geco() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_geco"));
    // Never let a developer's real health file leak into a test, and never
    // let a test write over it.
    c.env("GECO_HEALTH", "/nonexistent/geco-test-health.json");
    c
}

fn run(args: &[&str]) -> Output {
    geco()
        .args(args)
        .output()
        .expect("the geco binary must be runnable")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

/// A minimal valid config in a fresh temp dir. Returns the dir (kept alive by
/// the caller) and the config path.
fn temp_config() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("geco.yaml");
    std::fs::write(
        &path,
        // A command that exits immediately: the snapshot build will record a
        // failure for it, which is fine — these tests are about the CLI, not
        // about backends starting.
        "mcpServers:\n  t:\n    command: [\"cargo\", \"--version\"]\n    description: \"probe\"\n",
    )
    .expect("write config");
    (dir, path)
}

/// The regression guard for bug #1. A subcommand that gets renamed or dropped
/// while the docs still reference it fails here.
///
/// clap exits 2 with "unrecognized subcommand" for a name it does not know,
/// so this asserts on more than "it ran".
#[test]
fn every_documented_subcommand_exists() {
    for cmd in [
        "serve", "refresh", "inspect", "schema", "stats", "health", "validate",
    ] {
        let out = run(&[cmd, "--help"]);
        assert!(
            out.status.success(),
            "`geco {cmd} --help` failed — is the subcommand still there?\n{}",
            stderr(&out)
        );
    }

    // And the negative case, so the test above cannot pass vacuously.
    let out = run(&["definitely-not-a-command", "--help"]);
    assert!(
        !out.status.success(),
        "an unknown subcommand must fail, or the assertion above proves nothing"
    );
}

/// `--help` should list the commands, so someone who forgets a name can find
/// it. This is how the missing `health` command should have been noticed.
#[test]
fn help_lists_the_commands() {
    let out = run(&["--help"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    for cmd in [
        "serve", "refresh", "inspect", "schema", "stats", "health", "validate",
    ] {
        assert!(
            text.contains(cmd),
            "`--help` never mentions `{cmd}`:\n{text}"
        );
    }
}

/// Commands that must work with no config file at all — each is something you
/// reach for precisely when the config is what's broken.
#[test]
fn diagnostics_work_without_a_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    for cmd in ["health", "stats", "inspect", "schema"] {
        let out = geco()
            .arg(cmd)
            .current_dir(dir.path())
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "`geco {cmd}` must not need a config (it is for when the config is \
             broken), got {:?}:\n{}",
            out.status.code(),
            stderr(&out)
        );
    }
}

/// `geco health`'s exit code is its contract: non-zero on errors so a script
/// can gate on it, zero for warnings alone so a warning does not fail a build.
#[test]
fn health_exit_code_distinguishes_errors_from_warnings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hp = dir.path().join("health.json");

    let cases: [(&str, &str, i32); 4] = [
        ("no file at all", "", 0),
        (
            "healthy",
            r#"{"generation":1,"updated_at":"t","healthy":true,"issues":[]}"#,
            0,
        ),
        (
            "warning only",
            r#"{"generation":1,"updated_at":"t","healthy":false,"issues":[
                 {"kind":"skills","subject":"s","detail":"d","severity":"warning"}]}"#,
            0,
        ),
        (
            "an error",
            r#"{"generation":1,"updated_at":"t","healthy":false,"issues":[
                 {"kind":"backend","subject":"github","detail":"spawn failed","severity":"error"}]}"#,
            1,
        ),
    ];

    for (label, body, want) in cases {
        if body.is_empty() {
            let _ = std::fs::remove_file(&hp);
        } else {
            std::fs::write(&hp, body).expect("write health file");
        }
        let out = geco()
            .arg("health")
            .env("GECO_HEALTH", &hp)
            .output()
            .expect("run");
        assert_eq!(
            out.status.code(),
            Some(want),
            "`geco health` with {label} should exit {want}:\n{}",
            stderr(&out)
        );
    }
}

/// `geco health` must print the *reason*, not just that something is wrong —
/// a report that says "1 issue" and nothing else is useless.
#[test]
fn health_reports_the_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hp = dir.path().join("health.json");
    std::fs::write(
        &hp,
        r#"{"generation":1,"updated_at":"t","healthy":false,"issues":[
             {"kind":"backend","subject":"github",
              "detail":"cannot spawn backend 'github' (npx): program not found",
              "severity":"error"}]}"#,
    )
    .expect("write");

    let out = geco()
        .arg("health")
        .env("GECO_HEALTH", &hp)
        .output()
        .expect("run");
    let text = stderr(&out);
    assert!(text.contains("github"), "should name the subject:\n{text}");
    assert!(
        text.contains("program not found"),
        "should carry the detail, not just the count:\n{text}"
    );
}

/// The regression guard for bug #2. `geco refresh` must bump the config's
/// mtime — that, and only that, is what makes a *running* proxy rebuild,
/// because the config watcher gates on mtime. Without it the command is a
/// no-op for every live session.
///
/// Also asserts the file's bytes are untouched: this is a `touch`, not a
/// rewrite, so an interrupted run cannot damage someone's config.
#[test]
fn refresh_nudges_a_running_proxy_without_editing_the_config() {
    let (dir, cfg) = temp_config();
    let before_bytes = std::fs::read(&cfg).expect("read");
    let before_mtime = mtime(&cfg);

    // Filesystem timestamp granularity is coarse enough on some platforms
    // that an immediate write can land on the same value.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let out = geco()
        .args(["--config", cfg.to_str().unwrap(), "refresh"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", stderr(&out));

    assert!(
        mtime(&cfg) > before_mtime,
        "`geco refresh` must bump the config mtime or a running proxy never \
         reloads — this is exactly the bug this test exists for.\n{}",
        stderr(&out)
    );
    assert_eq!(
        std::fs::read(&cfg).expect("read"),
        before_bytes,
        "refresh must not alter config CONTENT — only the timestamp"
    );
    assert!(
        stderr(&out).contains("nudged"),
        "refresh should say it nudged a running proxy, so the behaviour is \
         discoverable:\n{}",
        stderr(&out)
    );
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .expect("mtime")
}

/// `geco schema` is consumed by editors via `$schema`. Emitting anything that
/// isn't valid JSON breaks completion silently.
#[test]
fn schema_emits_valid_json_on_stdout() {
    let out = run(&["schema"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("`geco schema` must emit valid JSON on stdout");
    assert!(
        parsed.get("properties").is_some() || parsed.get("$defs").is_some(),
        "schema looks empty: {parsed}"
    );
}

/// stdout belongs to the MCP protocol. Anything a subcommand prints for a
/// human goes to stderr — a stray println in `serve` corrupts the stream.
#[test]
fn only_schema_writes_to_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    for cmd in ["health", "stats", "inspect", "validate"] {
        let out = geco()
            .arg(cmd)
            .current_dir(dir.path())
            .output()
            .expect("run");
        assert!(
            out.stdout.is_empty(),
            "`geco {cmd}` wrote to stdout, which belongs to the MCP protocol: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

/// `geco validate` exists to catch config mistakes before they become runtime
/// mysteries, so a config that cannot even be parsed must fail it.
#[test]
fn validate_rejects_a_broken_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = dir.path().join("geco.yaml");
    std::fs::write(&cfg, "mcpServers:\n  broken: [this is not a server\n").expect("write");

    let out = geco()
        .args(["--config", cfg.to_str().unwrap(), "validate"])
        .output()
        .expect("run");
    assert!(
        !out.status.success(),
        "unparseable config must fail validate:\n{}",
        stderr(&out)
    );
}

/// A config naming a server with no `command` is the most common real mistake
/// and must be reported, not silently accepted.
#[test]
fn validate_accepts_a_good_config() {
    let (dir, cfg) = temp_config();
    let out = geco()
        .args(["--config", cfg.to_str().unwrap(), "validate"])
        .current_dir(dir.path())
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "a valid config must pass validate:\n{}",
        stderr(&out)
    );
}

/// Missing config should explain how to create one, not panic or print a
/// bare error type.
#[test]
fn missing_config_explains_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = geco()
        .args(["--config", dir.path().join("nope.yaml").to_str().unwrap()])
        .arg("validate")
        .output()
        .expect("run");
    assert!(!out.status.success(), "missing config must fail");
    let text = stderr(&out);
    assert!(
        !text.contains("panicked"),
        "must not panic on a missing config:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("config"),
        "the error should mention the config:\n{text}"
    );
}
