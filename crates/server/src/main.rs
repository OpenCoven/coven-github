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
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logging.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("coven_github=info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            config: config_path,
        } => {
            let config_str = std::fs::read_to_string(&config_path).map_err(|e| {
                anyhow::anyhow!("failed to read config at {}: {e}", config_path.display())
            })?;
            let config: Config = toml::from_str(&config_str)?;
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
