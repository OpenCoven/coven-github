//! coven-github server — main entry point.

use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use coven_github_config::Config;
use coven_github_webhook::routes::{
    audit, handle_webhook, healthz, list_memory, list_tasks, revoke_memory, routing, usage,
    AppState,
};
use coven_github_worker as worker;

#[derive(Parser)]
#[command(name = "coven-github", about = "Coven-native GitHub App coding agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start the webhook server and worker pool.
    Serve {
        #[arg(long, default_value = "config/local.toml")]
        config: PathBuf,
    },
    /// Validate a config file and print actionable next steps.
    ///
    /// Exits non-zero if any error-severity problem is found, so it doubles as
    /// a pre-flight check in CI or a container entrypoint.
    Doctor {
        #[arg(long, default_value = "config/local.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logging.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("coven_github=info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Doctor {
            config: config_path,
        } => {
            // Doctor reports problems on stderr and via exit code; it must not
            // start the server even if the config is clean.
            let exit = run_doctor(&config_path);
            std::process::exit(exit);
        }
        Command::Serve {
            config: config_path,
        } => {
            let config = Config::load(&config_path)?;

            // Fail fast on a broken config instead of crashing later with an
            // opaque error mid-request. `doctor` gives the full report.
            let diagnostics = config.check();
            let error_count = diagnostics.iter().filter(|d| d.is_error()).count();
            for d in &diagnostics {
                match d.severity {
                    coven_github_config::Severity::Error => {
                        tracing::error!(field = %d.field, "config error: {}", d.message)
                    }
                    coven_github_config::Severity::Warning => {
                        tracing::warn!(field = %d.field, "config warning: {}", d.message)
                    }
                }
            }
            if error_count > 0 {
                anyhow::bail!(
                    "config has {error_count} error(s) — run `coven-github doctor --config {}` for details",
                    config_path.display()
                );
            }

            let config = Arc::new(config);

            // Durable deliveries + task queue (issue #2): open before serving
            // so a broken storage path fails the boot, not the first delivery.
            // Any `running` rows belong to a previous process — requeue them
            // (or fail them once their attempts are spent).
            let store = coven_github_store::Store::open(&config.storage.path)?;
            let (requeued, failed) = store
                .recover_interrupted(config.worker.max_retries + 1)
                .await?;
            tracing::info!(
                requeued,
                failed,
                "durable store ready at {}",
                config.storage.path.display()
            );

            let notify = Arc::new(tokio::sync::Notify::new());

            // Spawn the worker claim loop.
            let worker_config = config.clone();
            let worker_store = store.clone();
            let worker_notify = notify.clone();
            tokio::spawn(async move {
                worker::run(worker_config, worker_store, worker_notify).await;
            });

            // Branch Gardener scheduler (issue #14): enqueue one adapter-side
            // garden task per configured installation/repo schedule. The durable
            // store delivery id makes each schedule slot idempotent across
            // process restarts.
            let gardener_config = config.clone();
            let gardener_store = store.clone();
            let gardener_notify = notify.clone();
            tokio::spawn(async move {
                worker::gardener_schedule::run(gardener_config, gardener_store, gardener_notify)
                    .await;
            });

            // Memory retention sweep (issue #6): when a retention horizon is
            // configured, periodically expire audit rows older than it. The
            // first tick fires immediately, so a stale audit is trimmed at boot.
            if let Some(retention_days) = config.memory.retention_days {
                let sweep_store = store.clone();
                tokio::spawn(async move {
                    let mut ticker =
                        tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
                    loop {
                        ticker.tick().await;
                        match sweep_store.expire_memory_activity(retention_days).await {
                            Ok(0) => {}
                            Ok(expired) => {
                                tracing::info!(expired, "expired memory activity past retention")
                            }
                            Err(e) => tracing::error!("memory retention sweep failed: {e:#}"),
                        }
                    }
                });
            }

            // Task-history retention sweep (issue #12): expire terminal tasks
            // older than the configured horizon; in-flight work is never touched.
            if let Some(retention_days) = config.storage.task_retention_days {
                let sweep_store = store.clone();
                tokio::spawn(async move {
                    let mut ticker =
                        tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
                    loop {
                        ticker.tick().await;
                        match sweep_store.expire_terminal_tasks(retention_days).await {
                            Ok(0) => {}
                            Ok(expired) => {
                                tracing::info!(expired, "expired terminal task rows past retention")
                            }
                            Err(e) => tracing::error!("task retention sweep failed: {e:#}"),
                        }
                    }
                });
            }

            // Build router.
            let state = AppState {
                config: config.clone(),
                store,
                notify,
            };

            let app = Router::new()
                .route("/healthz", get(healthz))
                .route("/webhook", post(handle_webhook))
                .route("/api/github/tasks", get(list_tasks))
                .route("/api/github/memory", get(list_memory))
                .route("/api/github/memory/revoke", post(revoke_memory))
                .route("/api/github/usage", get(usage))
                .route("/api/github/audit", get(audit))
                .route("/api/github/routing", get(routing))
                .with_state(state)
                .layer(TraceLayer::new_for_http());

            let bind = &config.server.bind;
            tracing::info!("coven-github listening on {bind}");
            let listener = tokio::net::TcpListener::bind(bind).await?;
            axum::serve(listener, app).await?;
        }
    }

    Ok(())
}

/// Load + validate a config and print a human-readable report.
///
/// Returns the process exit code: `0` if there are no errors (warnings are
/// allowed), `1` if validation found errors, `2` if the file could not be read
/// or parsed at all.
fn run_doctor(config_path: &std::path::Path) -> i32 {
    use coven_github_config::Severity;

    let config = match Config::load(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("✗ {e}");
            return 2;
        }
    };

    let diagnostics = config.check();
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = diagnostics.len() - errors;

    for d in &diagnostics {
        let mark = match d.severity {
            Severity::Error => "✗ error",
            Severity::Warning => "! warn ",
        };
        eprintln!("{mark}  {:<28}  {}", d.field, d.message);
        eprintln!("         {:<28}  next: {}", "", d.next_step);
    }

    if diagnostics.is_empty() {
        println!(
            "✓ config at {} looks good — {} familiar(s) configured.",
            config_path.display(),
            config.familiars.len()
        );
        println!(
            "next: coven-github serve --config {}",
            config_path.display()
        );
    } else {
        eprintln!(
            "\n{errors} error(s), {warnings} warning(s) in {}",
            config_path.display()
        );
    }

    if errors > 0 {
        1
    } else {
        0
    }
}
