//! Configuration types for coven-github installations.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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

fn default_timeout() -> u64 {
    600
}
fn default_retries() -> u32 {
    2
}

impl Config {
    /// Load and parse a config file from disk (TOML).
    ///
    /// This only checks that the file is present and structurally valid TOML.
    /// Use [`Config::check`] for the semantic, operator-facing validation that
    /// powers the `doctor` command.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config at {}: {e}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse config at {}: {e}", path.display()))?;
        Ok(config)
    }

    /// Run semantic validation over a parsed config and return every diagnostic
    /// found. An empty `Error`-severity set means the config is ready to serve.
    ///
    /// This never touches the network and never reads secret *contents* — it
    /// only checks for placeholder values, missing files, and mapping mistakes
    /// that would otherwise fail at runtime with an opaque error.
    pub fn check(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();

        // ── GitHub App ──────────────────────────────────────────────────
        if self.github.app_id == 0 {
            out.push(Diagnostic::error(
                "github.app_id",
                "App ID is 0 — set it to the numeric App ID from your GitHub App settings page.",
            ));
        }

        match path_status(&self.github.private_key_path) {
            PathStatus::Missing => out.push(Diagnostic::error(
                "github.private_key_path",
                format!(
                    "private key not found at '{}' — download the App's PEM and point this at it.",
                    self.github.private_key_path.display()
                ),
            )),
            PathStatus::NotAFile => out.push(Diagnostic::error(
                "github.private_key_path",
                format!(
                    "'{}' exists but is not a file.",
                    self.github.private_key_path.display()
                ),
            )),
            PathStatus::Ok => {
                if !pem_looks_valid(&self.github.private_key_path) {
                    out.push(Diagnostic::warning(
                        "github.private_key_path",
                        "file does not start with a PEM header ('-----BEGIN') — confirm it is the downloaded private key.",
                    ));
                }
            }
        }

        let secret = self.github.webhook_secret.trim();
        if secret.is_empty() {
            out.push(Diagnostic::error(
                "github.webhook_secret",
                "webhook secret is empty — set it to the secret configured in the GitHub App.",
            ));
        } else if PLACEHOLDER_SECRETS
            .iter()
            .any(|p| secret.eq_ignore_ascii_case(p))
        {
            out.push(Diagnostic::error(
                "github.webhook_secret",
                format!("webhook secret is still the placeholder '{secret}' — replace it with the real secret."),
            ));
        } else if secret.len() < 16 {
            out.push(Diagnostic::warning(
                "github.webhook_secret",
                "webhook secret is shorter than 16 characters — use a long random string.",
            ));
        }

        // ── Worker ──────────────────────────────────────────────────────
        if self.worker.concurrency == 0 {
            out.push(Diagnostic::error(
                "worker.concurrency",
                "concurrency is 0 — no tasks would ever run; set it to 1 or more.",
            ));
        }

        if !binary_resolvable(&self.worker.coven_code_bin) {
            out.push(Diagnostic::error(
                "worker.coven_code_bin",
                format!(
                    "coven-code binary not found at '{}' (and not on PATH) — build/install coven-code or fix the path.",
                    self.worker.coven_code_bin.display()
                ),
            ));
        }

        // ── Familiars ───────────────────────────────────────────────────
        if self.familiars.is_empty() {
            out.push(Diagnostic::error(
                "familiars",
                "no [[familiars]] configured — add at least one block so webhooks can route to a familiar.",
            ));
        }

        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_bots = std::collections::HashSet::new();
        for fam in &self.familiars {
            let label = if fam.id.is_empty() {
                "<unnamed>"
            } else {
                &fam.id
            };
            if fam.id.is_empty() {
                out.push(Diagnostic::error(
                    "familiars[].id",
                    "a familiar is missing an id.",
                ));
            } else if !seen_ids.insert(fam.id.as_str()) {
                out.push(Diagnostic::error(
                    "familiars[].id",
                    format!("duplicate familiar id '{}' — ids must be unique.", fam.id),
                ));
            }

            if fam.bot_username.trim().is_empty() {
                out.push(Diagnostic::error(
                    "familiars[].bot_username",
                    format!("familiar '{label}' has no bot_username — assignment and mentions cannot match it."),
                ));
            } else {
                if !seen_bots.insert(fam.bot_username.as_str()) {
                    out.push(Diagnostic::error(
                        "familiars[].bot_username",
                        format!("duplicate bot_username '{}' — two familiars would race the same events.", fam.bot_username),
                    ));
                }
                if !fam.bot_username.ends_with("[bot]") {
                    out.push(Diagnostic::warning(
                        "familiars[].bot_username",
                        format!("bot_username '{}' does not end in '[bot]' — GitHub App bot logins normally do.", fam.bot_username),
                    ));
                }
            }

            if fam.trigger_labels.is_empty() {
                out.push(Diagnostic::warning(
                    "familiars[].trigger_labels",
                    format!("familiar '{label}' has no trigger_labels — it will only run on direct bot assignment/mention."),
                ));
            }
        }

        out
    }
}

/// Severity of a [`Diagnostic`] produced by [`Config::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Blocks a usable release; the server should not be started.
    Error,
    /// Worth fixing but not fatal.
    Warning,
}

/// A single config-validation finding, scoped to a config field.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub field: String,
    pub message: String,
    pub next_step: String,
}

impl Diagnostic {
    fn error(field: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            severity: Severity::Error,
            field: field.to_string(),
            next_step: next_step_for(field, &message).to_string(),
            message,
        }
    }
    fn warning(field: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            severity: Severity::Warning,
            field: field.to_string(),
            next_step: next_step_for(field, &message).to_string(),
            message,
        }
    }
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

const PLACEHOLDER_SECRETS: &[&str] = &[
    "CHANGE_ME",
    "CHANGEME",
    "changeme",
    "replace-with-a-random-webhook-secret",
    "replace-me",
    "your-secret",
    "secret",
];

fn next_step_for(field: &str, _message: &str) -> &'static str {
    match field {
        "github.app_id" => "Copy the numeric App ID from the GitHub App settings page into github.app_id.",
        "github.private_key_path" => {
            "Download the GitHub App private key PEM and set github.private_key_path to that file."
        }
        "github.webhook_secret" => {
            "Generate or copy the GitHub App webhook secret and set github.webhook_secret."
        }
        "worker.concurrency" => "Set worker.concurrency to 1 or more.",
        "worker.coven_code_bin" => {
            "Install coven-code with headless support or set worker.coven_code_bin to the binary path."
        }
        "familiars" => "Add at least one [[familiars]] block for the bot account that should receive work.",
        "familiars[].id" => "Give each familiar a stable, unique id.",
        "familiars[].bot_username" => {
            "Set familiars[].bot_username to the GitHub App bot login, usually ending in [bot]."
        }
        "familiars[].trigger_labels" => {
            "Add labels such as coven:fix if this familiar should run from labels, or rely on assignment/mentions only."
        }
        _ => "Update this config field, then rerun coven-github doctor.",
    }
}

enum PathStatus {
    Ok,
    Missing,
    NotAFile,
}

fn path_status(p: &Path) -> PathStatus {
    match std::fs::metadata(p) {
        Ok(m) if m.is_file() => PathStatus::Ok,
        Ok(_) => PathStatus::NotAFile,
        Err(_) => PathStatus::Missing,
    }
}

/// Cheap sniff that a file begins with a PEM header without logging its bytes.
fn pem_looks_valid(p: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else {
        return false;
    };
    let mut buf = [0u8; 16];
    match f.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b"-----BEGIN"),
        Err(_) => false,
    }
}

/// True if `bin` resolves to an executable: either an existing file at the given
/// path, or (for a bare name) a file found on `PATH`.
fn binary_resolvable(bin: &Path) -> bool {
    if bin.is_file() {
        return true;
    }
    // Bare command name (no path separator) → search PATH.
    if bin.components().count() == 1 {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                if dir.join(bin).is_file() {
                    return true;
                }
            }
        }
    }
    false
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn good_familiar() -> FamiliarConfig {
        FamiliarConfig {
            id: "cody".into(),
            display_name: "Cody".into(),
            bot_username: "coven-cody[bot]".into(),
            model: None,
            skills: vec![],
            trigger_labels: vec!["coven:fix".into()],
        }
    }

    /// A config that points every path at something real so `check` only fires
    /// on the field under test. The PEM and binary live next to each other.
    fn config_with(
        github: GitHubAppConfig,
        worker: WorkerConfig,
        familiars: Vec<FamiliarConfig>,
    ) -> Config {
        Config {
            server: ServerConfig {
                bind: "0.0.0.0:3000".into(),
                cave_base_url: None,
            },
            github,
            worker,
            familiars,
        }
    }

    fn write_pem(dir: &Path) -> PathBuf {
        let p = dir.join("key.pem");
        std::fs::write(
            &p,
            b"-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();
        p
    }

    fn write_bin(dir: &Path) -> PathBuf {
        let p = dir.join("coven-code");
        std::fs::write(&p, b"#!/bin/sh\n").unwrap();
        p
    }

    fn tmpdir() -> PathBuf {
        // Unique-enough temp dir without pulling in an extra dependency.
        let base =
            std::env::temp_dir().join(format!("coven-github-cfg-test-{}", std::process::id()));
        let dir = base.join(format!("{:p}", &base));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn errors(diags: &[Diagnostic]) -> Vec<&str> {
        diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.field.as_str())
            .collect()
    }

    #[test]
    fn clean_config_has_no_errors() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let cfg = config_with(
            GitHubAppConfig {
                app_id: 123,
                private_key_path: pem,
                webhook_secret: "a-long-random-webhook-secret".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 4,
                coven_code_bin: bin,
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
            },
            vec![good_familiar()],
        );
        let diags = cfg.check();
        assert!(errors(&diags).is_empty(), "diags: {diags:?}");
    }

    #[test]
    fn flags_placeholder_secret_and_zero_app_id_and_missing_pem() {
        let dir = tmpdir();
        let bin = write_bin(&dir);
        let cfg = config_with(
            GitHubAppConfig {
                app_id: 0,
                private_key_path: dir.join("does-not-exist.pem"),
                webhook_secret: "CHANGE_ME".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 4,
                coven_code_bin: bin,
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
            },
            vec![good_familiar()],
        );
        let diags = cfg.check();
        let errs = errors(&diags);
        assert!(errs.contains(&"github.app_id"));
        assert!(errs.contains(&"github.webhook_secret"));
        assert!(errs.contains(&"github.private_key_path"));
    }

    #[test]
    fn flags_starter_webhook_secret_placeholder() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let cfg = config_with(
            GitHubAppConfig {
                app_id: 1,
                private_key_path: pem,
                webhook_secret: "replace-with-a-random-webhook-secret".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 1,
                coven_code_bin: bin,
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
            },
            vec![good_familiar()],
        );
        let diags = cfg.check();
        let errs = errors(&diags);
        assert!(errs.contains(&"github.webhook_secret"));
    }

    #[test]
    fn flags_missing_binary_and_empty_familiars() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let cfg = config_with(
            GitHubAppConfig {
                app_id: 1,
                private_key_path: pem,
                webhook_secret: "a-long-random-webhook-secret".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 0,
                coven_code_bin: dir.join("nope-not-here"),
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
            },
            vec![],
        );
        let diags = cfg.check();
        let errs = errors(&diags);
        assert!(errs.contains(&"worker.coven_code_bin"));
        assert!(errs.contains(&"worker.concurrency"));
        assert!(errs.contains(&"familiars"));
    }

    #[test]
    fn first_run_errors_include_operator_next_steps() {
        let dir = tmpdir();
        let cfg = config_with(
            GitHubAppConfig {
                app_id: 0,
                private_key_path: dir.join("does-not-exist.pem"),
                webhook_secret: "CHANGE_ME".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 0,
                coven_code_bin: dir.join("nope-not-here"),
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
            },
            vec![],
        );

        let diags = cfg.check();
        let app_id = diags
            .iter()
            .find(|d| d.field == "github.app_id")
            .expect("missing App ID should be diagnosed");
        assert!(
            app_id.next_step.contains("GitHub App settings"),
            "diagnostic should tell the operator where to get the App ID: {app_id:?}"
        );

        let bin = diags
            .iter()
            .find(|d| d.field == "worker.coven_code_bin")
            .expect("missing coven-code should be diagnosed");
        assert!(
            bin.next_step.contains("Install coven-code"),
            "diagnostic should tell the operator how to unblock headless runs: {bin:?}"
        );

        assert!(
            diags.iter().all(|d| !d.next_step.trim().is_empty()),
            "every first-run diagnostic should include a concrete next step: {diags:?}"
        );
    }

    #[test]
    fn flags_duplicate_familiar_ids_and_bots() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let cfg = config_with(
            GitHubAppConfig {
                app_id: 1,
                private_key_path: pem,
                webhook_secret: "a-long-random-webhook-secret".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 1,
                coven_code_bin: bin,
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
            },
            vec![good_familiar(), good_familiar()],
        );
        let diags = cfg.check();
        let errs = errors(&diags);
        assert!(errs.contains(&"familiars[].id"));
        assert!(errs.contains(&"familiars[].bot_username"));
    }
}
