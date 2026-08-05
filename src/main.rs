use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::Proxy;
use geco::snapshot::Snapshot;

#[derive(Parser)]
#[command(
    name = "geco",
    version,
    about = "geco — the generic connector: tools, skills & more on demand, out of context until summoned"
)]
struct Cli {
    /// Path to the config file (default: ./geco.yaml or
    /// ~/.config/geco/config.yaml; JSON also accepted)
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the stdio MCP proxy (default)
    Serve {
        /// Force a snapshot refresh at startup
        #[arg(long)]
        refresh: bool,
        /// Never refresh at startup, even if the config changed
        #[arg(long, conflicts_with = "refresh")]
        no_refresh: bool,
    },
    /// Rebuild the tool snapshot and exit
    Refresh,
    /// Print the cached snapshot summary and exit
    Inspect,
    /// Print the config JSON Schema (valid for YAML and JSON configs)
    Schema,
    /// Print runtime metrics from the last health.json write
    Stats,
    /// Report what is currently broken: failing backends, config and skill
    /// problems, and why. Exits non-zero when anything is wrong.
    Health,
    /// Validate the config for common mistakes (missing keychain items, missing dirs, etc.)
    Validate,
}

/// `touch`, without the content round-trip: sets the modified time to now and
/// leaves every byte alone.
///
/// A running proxy watches its config file and gates on mtime, so this is how
/// a CLI process asks a live instance to rebuild. Using `File::set_times`
/// rather than rewriting the file means a crash mid-call cannot damage
/// someone's config — there is nothing to half-write.
fn bump_mtime(path: &std::path::Path) -> std::io::Result<()> {
    // Write access is what Windows requires to set timestamps.
    let f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))
}

fn find_config(cli: &Cli) -> Result<PathBuf> {
    if let Some(p) = &cli.config {
        return Ok(p.clone());
    }
    for name in ["geco.yaml", "geco.yml", "geco.json"] {
        let local = PathBuf::from(name);
        if local.exists() {
            return Ok(local);
        }
    }
    // ~/.config/geco/config.{yaml,yml,json} on every platform (predictable).
    // On Windows, HOME is typically unset — fall back to USERPROFILE.
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if let Some(home) = home {
        let dir = PathBuf::from(&home).join(".config/geco");
        for name in ["config.yaml", "config.yml", "config.json"] {
            let p = dir.join(name);
            if p.exists() {
                return Ok(p);
            }
        }
    }
    if let Some(dirs) = directories::ProjectDirs::from("dev", "geco", "geco") {
        for name in ["config.yaml", "config.yml", "config.json"] {
            let p = dirs.config_dir().join(name);
            if p.exists() {
                return Ok(p);
            }
        }
    }
    bail!(
        "no config found — pass --config, or create ./geco.yaml or \
         ~/.config/geco/config.yaml (JSON also accepted)"
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout belongs to the MCP protocol — all logging goes to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    if matches!(cli.command, Some(Cmd::Schema)) {
        println!("{}", Config::schema_json());
        return Ok(());
    }

    // Health needs only health.json — it has to work when the config is the
    // very thing that's broken, so it must not require parsing one.
    //
    // This is the command that replaced the `health` meta-tool. That tool sat
    // in every session's context to answer a question that only comes up once
    // something has already failed; the agent gets the same facts in-band
    // from `execute`'s error code and `list_mcp_servers`, and a human gets
    // the full picture here.
    if matches!(cli.command, Some(Cmd::Health)) {
        let health_path = geco::health::health_path();
        let report = match geco::health::read(&health_path) {
            geco::health::HealthFile::Missing => {
                // Not an error: no file means no proxy has run, which is a
                // different situation from a proxy reporting trouble.
                eprintln!(
                    "no health file at {} — the proxy has not run yet",
                    health_path.display()
                );
                return Ok(());
            }
            geco::health::HealthFile::Unreadable(e) => {
                // A file that EXISTS but cannot be parsed is not "never
                // ran" — saying so sent people hunting for a missing file
                // sitting right there, possibly full of recorded errors.
                // Exit 1: the state is unknown, and unknown must not pass
                // a health gate.
                eprintln!(
                    "health file {} exists but cannot be read: {e}\n\
                     remove it, or let a running proxy rewrite it",
                    health_path.display()
                );
                std::process::exit(1);
            }
            geco::health::HealthFile::Report(r) => *r,
        };
        let client = report.client.as_deref().unwrap_or("unknown client");
        // A clean shutdown marker means nothing is serving: yesterday's
        // issues are not "currently broken", and must not fail a script
        // gating on this exit code today.
        if let Some(stopped) = &report.stopped_at {
            let state = if report.issues.is_empty() {
                "was healthy".to_string()
            } else {
                format!("last state: {} issue(s)", report.issues.len())
            };
            eprintln!("proxy stopped at {stopped} ({client}) — {state}");
            return Ok(());
        }
        if report.issues.is_empty() {
            eprintln!("healthy — no issues as of {} ({client})", report.updated_at);
            return Ok(());
        }
        let errors = report
            .issues
            .iter()
            .filter(|i| i.severity == "error")
            .count();
        eprintln!(
            "{} issue(s), {errors} error(s) as of {} ({client})\n",
            report.issues.len(),
            report.updated_at
        );
        for i in &report.issues {
            eprintln!("{:<8} {:<8} {}", i.severity, i.kind, i.subject);
            eprintln!("         {}", i.detail);
        }
        eprintln!("\nnext: `geco validate` for config mistakes, `geco inspect` for the snapshot");
        // Non-zero on errors only, so a warning doesn't fail someone's script
        // while a genuinely dead backend still does.
        if errors > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Stats needs only health.json — work even with no config file at all.
    if matches!(cli.command, Some(Cmd::Stats)) {
        let health_path = geco::health::health_path();
        match geco::health::read(&health_path) {
            geco::health::HealthFile::Report(report) => {
                if let Some(metrics) = &report.metrics {
                    eprintln!("geco metrics (since {})", metrics.started_at);
                    eprintln!("  total calls:    {}", metrics.total_calls);
                    eprintln!("  total errors:   {}", metrics.total_errors);
                    if metrics.searches > 0 {
                        let miss_pct =
                            metrics.searches_no_match as f64 * 100.0 / metrics.searches as f64;
                        eprintln!(
                            "  searches:       {} ({} found nothing, {:.0}%)",
                            metrics.searches, metrics.searches_no_match, miss_pct
                        );
                        eprintln!(
                            "  search → exec:  {} (calls to a tool search had surfaced)",
                            metrics.search_to_execute
                        );
                        // Skills are a "push" resource: nothing in a request
                        // names them, so surfaces-without-reads means the
                        // descriptions don't match how work gets described,
                        // while near-zero surfaces means nobody is looking.
                        eprintln!(
                            "  skills:         surfaced in {} search(es), read {} time(s)",
                            metrics.skill_surfaces, metrics.skill_reads
                        );
                    }
                    // Which meta-tools the model actually reaches for. The
                    // always-on surface is the one cost geco promises a number
                    // for, so a tool at zero calls after real use is a tool
                    // whose bytes are being paid for nothing — and this is the
                    // only way to know rather than guess. `proxy.surface: lean`
                    // drops the three browse tools; these counts are the
                    // evidence for or against making that the default.
                    if !metrics.per_meta_tool.is_empty() {
                        eprintln!();
                        eprintln!("meta-tool calls (the always-on surface earning its bytes):");
                        let mut rows: Vec<(&String, &u64)> = metrics.per_meta_tool.iter().collect();
                        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
                        for (name, n) in rows {
                            eprintln!("  {name:18} {n:>6}");
                        }
                    }
                    eprintln!();
                    if !metrics.recent_no_match.is_empty() {
                        eprintln!("queries that matched nothing (most recent last):");
                        for q in &metrics.recent_no_match {
                            eprintln!("  {q}");
                        }
                        eprintln!();
                    }
                    eprintln!(
                        "{:24} {:>6} {:>6} {:>5} {:>8}  last_call",
                        "server", "calls", "errors", "cold", "avg_ms"
                    );
                    for (name, m) in &metrics.per_backend {
                        let avg = if m.calls > 0 {
                            m.total_latency_ms.checked_div(m.calls).unwrap_or(0)
                        } else {
                            0
                        };
                        eprintln!(
                            "{name:24} {calls:>6} {errors:>6} {cold:>5} {avg:>8}  {last}",
                            calls = m.calls,
                            errors = m.errors,
                            cold = m.cold_starts,
                            avg = avg,
                            last = m.last_call_at.as_deref().unwrap_or("-")
                        );
                    }
                } else {
                    eprintln!(
                        "no metrics in {} (proxy not running or old version)",
                        health_path.display()
                    );
                }
            }
            geco::health::HealthFile::Missing => eprintln!(
                "no health file at {} — is the proxy running?",
                health_path.display()
            ),
            geco::health::HealthFile::Unreadable(e) => eprintln!(
                "health file {} exists but cannot be read: {e}",
                health_path.display()
            ),
        }
        return Ok(());
    }

    // Inspect needs only the cached snapshot — work even with no config file.
    // The "current/stale" label requires the config hash, but we degrade
    // gracefully to "?" when the config can't be found or parsed.
    if matches!(cli.command, Some(Cmd::Inspect)) {
        let snapshot_path = match find_config(&cli).ok() {
            Some(cp) => Snapshot::cache_path(&cp),
            None => {
                eprintln!("no snapshot — no config found (pass --config)");
                return Ok(());
            }
        };
        match Snapshot::load(&snapshot_path) {
            Some(snap) => {
                let config_hash_label =
                    match find_config(&cli).ok().and_then(|cp| Config::load(&cp).ok()) {
                        Some(cfg) => {
                            if snap.config_hash == cfg.snapshot_hash() {
                                "current"
                            } else {
                                "STALE"
                            }
                        }
                        None => "? (config unreadable)",
                    };
                eprintln!(
                    "snapshot {} (config {}) generated {}",
                    snapshot_path.display(),
                    config_hash_label,
                    snap.generated_at
                );
                for (name, s) in &snap.servers {
                    eprintln!("{name:24} {:4} tools  {}", s.tools.len(), s.refresh_status);
                }
            }
            None => eprintln!("no snapshot yet — run: geco refresh"),
        }
        return Ok(());
    }

    // All remaining subcommands (Refresh, Validate, Serve) need a config file.
    let config_path = find_config(&cli)?;
    let snapshot_path = Snapshot::cache_path(&config_path);
    let cfg = Config::load(&config_path)?;
    let hash = cfg.snapshot_hash();
    let registry = std::sync::Arc::new(Registry::from_config(&cfg));

    match cli.command.unwrap_or(Cmd::Serve {
        refresh: false,
        no_refresh: false,
    }) {
        Cmd::Refresh => {
            // Absorb stray skills before rebuilding the snapshot, so a skill
            // dropped in an absorb dir is swept into the library and shows up
            // in the very same refresh — same contract the running proxy
            // offers at startup and on its watcher. Without this, `geco
            // refresh` only probes backends; absorb runs solely in the live
            // proxy (Server::new + watcher), so a refresh with no proxy
            // running left the native dir full and the library stale.
            if !cfg.skills.absorb.is_empty() {
                if let Some(library) = cfg.skills.dirs.first() {
                    let (moved, errors) = geco::skills::absorb_into(library, &cfg.skills.absorb);
                    if !moved.is_empty() {
                        eprintln!(
                            "absorbed {} stray skill(s) into the library: {}",
                            moved.len(),
                            moved.join(", ")
                        );
                    }
                    for e in &errors {
                        eprintln!("skill absorb: {e}");
                    }
                } else {
                    eprintln!(
                        "note: skills.absorb configured but skills.dirs is \
                         empty — nothing to absorb into"
                    );
                }
            }

            let snap = registry.build_snapshot(hash, None).await;
            snap.save(&snapshot_path)?;
            for (name, s) in &snap.servers {
                eprintln!("{name:24} {:4} tools  {}", s.tools.len(), s.refresh_status);
            }
            eprintln!("snapshot: {}", snapshot_path.display());

            // Rewriting the snapshot file is not enough on its own: a running
            // proxy holds its snapshot in memory and only ever WRITES this
            // file, so it would keep serving the old catalogue and overwrite
            // what we just saved. Since the config watcher gates on mtime, a
            // touch is all it takes to make a live instance rebuild — which
            // is what someone running `geco refresh` actually wants.
            //
            // Content is untouched; only the timestamp moves. If no proxy is
            // running this is a no-op.
            match bump_mtime(&config_path) {
                Ok(()) => eprintln!("nudged any running proxy to reload"),
                Err(e) => eprintln!(
                    "note: could not touch {} ({e}) — a running proxy keeps its \
                     old catalogue until restarted",
                    config_path.display()
                ),
            }

            // A backend that would not answer is a failed refresh, and the
            // exit code has to say so. It used to always return 0, which made
            // both installers' "refresh reported issues" branches unreachable:
            // a fresh install whose every example backend failed to spawn
            // printed the status table and then declared success. `health` and
            // `validate` both exit 1 on errors; this now matches them.
            let failed: Vec<&str> = snap
                .servers
                .iter()
                .filter(|(_, s)| s.refresh_status != "ok")
                .map(|(name, _)| name.as_str())
                .collect();
            if !failed.is_empty() {
                eprintln!(
                    "{} of {} servers failed to refresh: {}",
                    failed.len(),
                    snap.servers.len(),
                    failed.join(", ")
                );
                std::process::exit(1);
            }
        }
        Cmd::Validate => {
            let findings = cfg.validate();
            if findings.is_empty() {
                eprintln!("config OK — no issues found");
            } else {
                let errors = findings.iter().filter(|(s, _)| s == "error").count();
                let warnings = findings.iter().filter(|(s, _)| s == "warning").count();
                for (severity, msg) in &findings {
                    let icon = if severity == "error" {
                        "ERROR"
                    } else {
                        "WARN "
                    };
                    eprintln!("{icon}: {msg}");
                }
                eprintln!("\n{} error(s), {} warning(s)", errors, warnings);
                if errors > 0 {
                    std::process::exit(1);
                }
            }
        }
        Cmd::Schema => {
            // Handled above before config loading — unreachable here, but
            // kept for match exhaustiveness.
            unreachable!()
        }
        // Handled above, before the config is loaded, because each must work
        // when the config itself is what's broken.
        Cmd::Inspect | Cmd::Stats | Cmd::Health => unreachable!(),
        Cmd::Serve {
            refresh,
            no_refresh,
        } => {
            let cached = Snapshot::load(&snapshot_path);
            let stale = cached
                .as_ref()
                .map(|s| s.config_hash != hash)
                .unwrap_or(true);
            let snapshot = if refresh || (stale && cfg.proxy.refresh_on_start && !no_refresh) {
                tracing::info!(
                    "refreshing tool snapshot ({} servers){}",
                    registry.backends.len(),
                    if stale { " — config changed" } else { "" }
                );
                let snap = registry.build_snapshot(hash, None).await;
                if let Err(e) = snap.save(&snapshot_path) {
                    tracing::warn!("cannot persist snapshot: {e:#}");
                }
                snap
            } else {
                if stale && cached.is_some() {
                    tracing::warn!(
                        "serving a STALE snapshot — the config changed since it was built; \
                         run `geco refresh`"
                    );
                }
                cached.unwrap_or_else(|| {
                    tracing::warn!("serving with EMPTY snapshot — run `geco refresh`");
                    Snapshot {
                        config_hash: hash.clone(),
                        ..Default::default()
                    }
                })
            };
            // Not a constant: the surface is `lean` by default (three tools,
            // two without a skill library) and `full` is six. The line said 6
            // unconditionally, which was never true for the default config.
            tracing::info!(
                "geco up: {} servers, {} tools behind the {} meta-tool surface",
                snapshot.servers.len(),
                snapshot.tool_count(),
                if cfg.proxy.lean_surface() {
                    "lean"
                } else {
                    "full"
                }
            );
            let proxy = Proxy::new(
                (*registry).clone(),
                snapshot,
                snapshot_path,
                config_path.clone(),
                cfg.skills.dirs.clone(),
                cfg.skills.absorb.clone(),
            );
            proxy.publish_health().await;
            proxy.clone().spawn_hot_reload();
            proxy.clone().spawn_health_check();
            proxy.clone().spawn_skill_reload();
            proxy.clone().spawn_tool_change_listener();
            let service = proxy
                .clone()
                .serve(rmcp::transport::stdio())
                .await
                .context("failed to start stdio server")?;

            // Graceful shutdown on SIGINT/SIGTERM — without this, killing the
            // proxy orphans every backend child process. The signal task sets
            // the shutdown flag (so hot-reload/health/skill watchers stop)
            // and cancels the stdio service so `waiting()` returns promptly.
            let proxy_for_signal = proxy.clone();
            let cancel_token = service.cancellation_token();
            tokio::spawn(async move {
                use tokio::signal;
                #[cfg(unix)]
                let mut sigterm = {
                    use signal::unix::{signal, SignalKind};
                    match signal(SignalKind::terminate()) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("cannot install SIGTERM handler: {e}");
                            return;
                        }
                    }
                };
                #[cfg(unix)]
                {
                    tokio::select! {
                        _ = signal::ctrl_c() => {}
                        _ = sigterm.recv() => {}
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = signal::ctrl_c().await;
                }
                tracing::info!("signal received — shutting down");
                proxy_for_signal.set_shutdown();
                cancel_token.cancel();
            });

            let _ = service.waiting().await;
            proxy.shutdown_backends().await;
            // Health.json outlives this process; without the marker, a
            // script running `geco health` tomorrow reads yesterday's
            // issues as "currently broken" (or a last healthy write as
            // proof something is serving when nothing is).
            geco::health::mark_stopped();
        }
    }
    Ok(())
}
