//! Configuration types for coven-github installations.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Top-level server configuration (loaded from TOML).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: ServerConfig,
    pub github: GitHubAppConfig,
    pub worker: WorkerConfig,
    pub familiars: Vec<FamiliarConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    /// Bind address, e.g. "0.0.0.0:3000"
    pub bind: String,
    /// Public base URL for Cave deep links, e.g. "https://cave.opencoven.ai"
    pub cave_base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubAppConfig {
    pub app_id: u64,
    pub private_key_path: PathBuf,
    pub webhook_secret: String,
    /// Optional: override the GitHub API base URL (for GHES)
    pub api_base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerConfig {
    /// Max concurrent task workers
    pub concurrency: usize,
    /// Path to the coven-code binary
    pub coven_code_bin: PathBuf,
    /// Workspace root for ephemeral task directories
    pub workspace_root: PathBuf,
    /// Task timeout in seconds (default: 600)
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Max retry attempts for infra errors (exit code 2)
    #[serde(default = "default_retries")]
    pub max_retries: u32,
}

fn default_timeout() -> u64 { 600 }
fn default_retries() -> u32 { 2 }

/// Per-familiar configuration for task routing.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FamiliarConfig {
    pub id: String,
    pub display_name: String,
    /// GitHub bot username that triggers this familiar (e.g. "coven-cody[bot]")
    pub bot_username: String,
    /// Model override, e.g. "anthropic/claude-sonnet-4-6"
    pub model: Option<String>,
    /// Skills to inject at session start
    #[serde(default)]
    pub skills: Vec<String>,
    /// Trigger labels (in addition to direct assignment)
    #[serde(default)]
    pub trigger_labels: Vec<String>,
}
