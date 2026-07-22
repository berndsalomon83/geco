use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rmcp::ServiceExt;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::Proxy;
use geco::snapshot::Snapshot;

#[derive(Parser)]
#[command(name = "geco", version, about = "geco — the generic connector: tools, skills & more on demand, out of context until summoned")]
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
    // ~/.config/geco/config.{yaml,yml,json} on every platform (predictable),
    // then the platform-native config dir as a fallback.
    if let Some(home) = std::env::var_os("HOME") {
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    if matches!(cli.command, Some(Cmd::Schema)) {
        println!("{}", Config::schema_json());
        return Ok(());
    }
    let config_path = find_config(&cli)?;
    let cfg = Config::load(&config_path)?;
    let hash = cfg.snapshot_hash();
    let snapshot_path = Snapshot::cache_path(&config_path);
    let registry = std::sync::Arc::new(Registry::from_config(&cfg));

    match cli.command.unwrap_or(Cmd::Serve { refresh: false, no_refresh: false }) {
        Cmd::Refresh => {
            let snap = registry.build_snapshot(hash, None).await;
            snap.save(&snapshot_path)?;
            for (name, s) in &snap.servers {
                eprintln!("{name:24} {:4} tools  {}", s.tools.len(), s.refresh_status);
            }
            eprintln!("snapshot: {}", snapshot_path.display());
        }
        Cmd::Schema => {
            // Handled above before config loading — unreachable here, but
            // kept for match exhaustiveness.
            unreachable!()
        }
        Cmd::Inspect => {
            match Snapshot::load(&snapshot_path) {
                Some(snap) => {
                    eprintln!(
                        "snapshot {} (config {}) generated {}",
                        snapshot_path.display(),
                        if snap.config_hash == hash { "current" } else { "STALE" },
                        snap.generated_at
                    );
                    for (name, s) in &snap.servers {
                        eprintln!("{name:24} {:4} tools  {}", s.tools.len(), s.refresh_status);
                    }
                }
                None => eprintln!("no snapshot yet — run: geco refresh"),
            }
        }
        Cmd::Serve { refresh, no_refresh } => {
            let cached = Snapshot::load(&snapshot_path);
            let stale = cached.as_ref().map(|s| s.config_hash != hash).unwrap_or(true);
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
                cached.unwrap_or_else(|| {
                    tracing::warn!("serving with EMPTY snapshot — run refresh_tools");
                    Snapshot { config_hash: hash.clone(), ..Default::default() }
                })
            };
            tracing::info!(
                "geco up: {} servers, {} tools behind 9 meta-tools",
                snapshot.servers.len(),
                snapshot.tool_count()
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
            let service = proxy
                .clone()
                .serve(rmcp::transport::stdio())
                .await
                .context("failed to start stdio server")?;
            service.waiting().await?;
            proxy.shutdown_backends().await;
        }
    }
    Ok(())
}
