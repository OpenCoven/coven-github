//! coven-github server — main entry point.

use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::mpsc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use coven_github_api::tasks::TaskStore;
use coven_github_config::Config;
use coven_github_webhook::routes::{handle_webhook, list_tasks, AppState};
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

            // Open durable state before serving so a bad storage path fails
            // fast rather than on the first webhook (issue #2).
            let store = coven_github_store::Store::open(&config.storage.path)?;
            tracing::info!(path = %config.storage.path.display(), "durable store ready");

            let config = Arc::new(config);

            let (task_tx, task_rx) = mpsc::channel(256);
            let task_store = TaskStore::default();

            // Spawn worker pool.
            let worker_config = config.clone();
            let worker_task_store = task_store.clone();
            tokio::spawn(async move {
                worker::run(worker_config, worker_task_store, task_rx).await;
            });

            // Build router.
            let state = AppState {
                config: config.clone(),
                task_tx,
                task_store,
                store,
            };

            let app = Router::new()
                .route("/webhook", post(handle_webhook))
                .route("/api/github/tasks", get(list_tasks))
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
