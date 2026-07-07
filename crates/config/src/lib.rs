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
    /// Automatic review trigger policy. Absent section = all lanes off.
    #[serde(default)]
    pub review: ReviewConfig,
    /// Durable adapter state (issue #2). Absent section = default path.
    #[serde(default)]
    pub storage: StorageConfig,
    /// Hosted memory governance policy (issue #6). Absent section = memory off.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Task API authentication (issue #3). Absent section = open mode, which
    /// is only safe for local development.
    #[serde(default)]
    pub api: ApiConfig,
    /// Installation-scoped routing policy (issue #7). Absent = open routing:
    /// every installation sees every familiar with all triggers enabled (the
    /// self-hosted default). Once any [[installations]] block exists, routing
    /// fails closed for installations not listed.
    #[serde(default)]
    pub installations: Vec<InstallationConfig>,
}

/// Routing and trigger policy for one GitHub App installation (issue #7).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InstallationConfig {
    /// GitHub App installation id.
    pub id: u64,
    /// Account login this installation belongs to (informational).
    pub account: Option<String>,
    /// Familiar ids this installation may route to. Empty = all familiars.
    #[serde(default)]
    pub familiars: Vec<String>,
    /// Installation-wide trigger switches; repos may override.
    #[serde(default)]
    pub triggers: TriggerPolicy,
    /// Usage limits for this installation (issue #15). Absent = unlimited.
    #[serde(default)]
    pub limits: InstallationLimits,
    /// Per-repo overrides keyed "owner/name".
    #[serde(default)]
    pub repos: std::collections::HashMap<String, RepoRoutingOverride>,
}

/// Tier limits for one installation (issue #15). `None` = unlimited.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct InstallationLimits {
    /// Max tasks running at once for this installation.
    pub max_concurrent: Option<u32>,
    /// Max tasks accepted per rolling 24 hours.
    pub max_tasks_per_day: Option<u32>,
}

/// Which trigger lanes may create work. All on by default.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct TriggerPolicy {
    /// Issue assigned to a familiar's bot user.
    #[serde(default = "default_true")]
    pub assignment: bool,
    /// Trigger labels on issues and the per-PR review-label opt-in.
    #[serde(default = "default_true")]
    pub labels: bool,
    /// Maintainer `@familiar <verb>` commands.
    #[serde(default = "default_true")]
    pub commands: bool,
    /// Automatic PR review lane.
    #[serde(default = "default_true")]
    pub reviews: bool,
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        Self {
            assignment: true,
            labels: true,
            commands: true,
            reviews: true,
        }
    }
}

/// Per-repo routing override; unset fields inherit from the installation.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoRoutingOverride {
    /// `false` disables every trigger for the repository.
    pub enabled: Option<bool>,
    pub assignment: Option<bool>,
    pub labels: Option<bool>,
    pub commands: Option<bool>,
    pub reviews: Option<bool>,
}

/// A serializable installation routing policy for the Cave dashboard (#18).
#[derive(Debug, Clone, Serialize)]
pub struct RoutingView {
    pub installation_id: u64,
    pub account: Option<String>,
    /// Familiar ids this installation may route to (resolved allow-list, or all
    /// configured familiars).
    pub familiars: Vec<String>,
    /// Installation-level trigger lanes.
    pub triggers: TriggerPolicy,
    /// Per-repo trigger overrides.
    pub repos: std::collections::HashMap<String, RepoRoutingOverride>,
    /// `true` when the installation is explicitly configured; `false` means
    /// open routing (nothing configured) or fail-closed (unlisted) — read
    /// `familiars` to tell which.
    pub listed: bool,
}

/// The effective routing view for one delivery: which familiars are visible
/// and which trigger lanes are open (issue #7).
pub struct RoutingScope<'a> {
    familiars: Vec<&'a FamiliarConfig>,
    triggers: TriggerPolicy,
}

impl<'a> RoutingScope<'a> {
    /// A scope that routes nothing (unknown installation, fail closed).
    fn closed() -> Self {
        Self {
            familiars: Vec::new(),
            triggers: TriggerPolicy {
                assignment: false,
                labels: false,
                commands: false,
                reviews: false,
            },
        }
    }

    pub fn familiars(&self) -> impl Iterator<Item = &'a FamiliarConfig> + '_ {
        self.familiars.iter().copied()
    }

    pub fn familiar_by_bot(&self, login: &str) -> Option<&'a FamiliarConfig> {
        self.familiars
            .iter()
            .copied()
            .find(|f| f.bot_username == login)
    }

    pub fn familiar_by_label(&self, label: &str) -> Option<&'a FamiliarConfig> {
        self.familiars
            .iter()
            .copied()
            .find(|f| f.trigger_labels.iter().any(|l| l == label))
    }

    pub fn familiar_by_id(&self, id: &str) -> Option<&'a FamiliarConfig> {
        self.familiars.iter().copied().find(|f| f.id == id)
    }

    pub fn assignment_enabled(&self) -> bool {
        self.triggers.assignment
    }
    pub fn labels_enabled(&self) -> bool {
        self.triggers.labels
    }
    pub fn commands_enabled(&self) -> bool {
        self.triggers.commands
    }
    pub fn reviews_enabled(&self) -> bool {
        self.triggers.reviews
    }
}

/// Hosted memory governance policy (issue #6). Off by default; opting in is a
/// hosted decision coordinated with the coven-code side of the contract (see
/// `docs/memory-contract.md`).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct MemoryConfig {
    /// Master opt-in. `false` (or section absent) → the adapter emits no memory
    /// policy and the runtime does no memory work.
    #[serde(default)]
    pub enabled: bool,
    /// Written memory stays `pending` until a maintainer approves it.
    #[serde(default = "default_true")]
    pub approval_required: bool,
    /// Optional retention horizon for durable memory.
    pub retention_days: Option<u32>,
    /// Per-repo overrides keyed "owner/name".
    #[serde(default)]
    pub repos: std::collections::HashMap<String, RepoMemoryOverride>,
}

/// Per-repo override of the global [`MemoryConfig`]; unset fields inherit.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoMemoryOverride {
    pub enabled: Option<bool>,
    pub approval_required: Option<bool>,
}

impl MemoryConfig {
    fn overrides(&self, repo: &str) -> Option<&RepoMemoryOverride> {
        self.repos.get(repo)
    }

    /// Whether memory is opted in for `repo` ("owner/name").
    pub fn enabled_for(&self, repo: &str) -> bool {
        self.overrides(repo)
            .and_then(|o| o.enabled)
            .unwrap_or(self.enabled)
    }

    /// Whether writes for `repo` require maintainer approval.
    pub fn approval_required_for(&self, repo: &str) -> bool {
        self.overrides(repo)
            .and_then(|o| o.approval_required)
            .unwrap_or(self.approval_required)
    }
}

fn default_true() -> bool {
    true
}

/// Task API access control. See `docs/security.md`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ApiConfig {
    /// `open` (unauthenticated; local development only) or `token`
    /// (bearer tokens required; fail closed — the hosted posture).
    #[serde(default)]
    pub mode: ApiMode,
    /// Operator-wide token with visibility across every installation
    /// (self-hosted Cave polling).
    pub service_token: Option<String>,
    /// Tenant tokens scoped to a single installation (and optionally to a
    /// subset of its repositories).
    #[serde(default)]
    pub tenants: Vec<TenantToken>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiMode {
    #[default]
    Open,
    Token,
}

/// A bearer token granting read access to one installation's tasks.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TenantToken {
    pub token: String,
    pub installation_id: u64,
    /// Optional narrower scope: only these `owner/name` repositories.
    #[serde(default)]
    pub repos: Vec<String>,
}

/// Durable store location. See `docs/durable-task-store.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// SQLite database path; parent directories are created at startup.
    #[serde(default = "default_storage_path")]
    pub path: PathBuf,
    /// Days to retain terminal task history before a periodic sweep deletes it
    /// (issue #12). Absent = keep indefinitely. In-flight tasks are never
    /// expired.
    pub task_retention_days: Option<u32>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: default_storage_path(),
            task_retention_days: None,
        }
    }
}

fn default_storage_path() -> PathBuf {
    PathBuf::from("data/coven-github.db")
}

/// Automatic review trigger policy (issue #10). Lanes default to off.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReviewConfig {
    /// Familiar id that runs auto-triggered reviews.
    pub familiar: Option<String>,
    /// Review PRs on opened / synchronize / reopened / ready_for_review.
    #[serde(default)]
    pub pull_request: bool,
    /// Also auto-review draft PRs. The adapter's own PRs are drafts, so this
    /// defaults to off; an explicit review label still works on drafts.
    #[serde(default)]
    pub include_drafts: bool,
    /// Adapter-authored instruction forwarded as the brief's audit_instruction.
    pub audit_instruction: Option<String>,
    /// Minimum finding severity that publishes (`info`, `low`, `medium`,
    /// `high`, `critical`). Absent = every severity publishes (issue #11).
    pub min_severity: Option<String>,
    /// Where gated findings publish (issue #11): `check_run` (default),
    /// `advisory_comment` (also on the status comment), or `request_changes`
    /// (submit a PR review that requests changes when findings exist).
    pub publish: Option<String>,
    /// Per-repo overrides keyed "owner/name".
    #[serde(default)]
    pub repos: std::collections::HashMap<String, RepoReviewOverride>,
}

/// Per-repo override of the global [`ReviewConfig`]; unset fields inherit.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoReviewOverride {
    pub pull_request: Option<bool>,
    pub include_drafts: Option<bool>,
    pub familiar: Option<String>,
    pub audit_instruction: Option<String>,
    pub min_severity: Option<String>,
    pub publish: Option<String>,
}

impl ReviewConfig {
    fn overrides(&self, repo: &str) -> Option<&RepoReviewOverride> {
        self.repos.get(repo)
    }

    pub fn pull_request_enabled(&self, repo: &str) -> bool {
        self.overrides(repo)
            .and_then(|o| o.pull_request)
            .unwrap_or(self.pull_request)
    }

    pub fn drafts_included(&self, repo: &str) -> bool {
        self.overrides(repo)
            .and_then(|o| o.include_drafts)
            .unwrap_or(self.include_drafts)
    }

    pub fn reviewer(&self, repo: &str) -> Option<&str> {
        self.overrides(repo)
            .and_then(|o| o.familiar.as_deref())
            .or(self.familiar.as_deref())
    }

    pub fn audit_instruction_for(&self, repo: &str) -> Option<String> {
        self.overrides(repo)
            .and_then(|o| o.audit_instruction.clone())
            .or_else(|| self.audit_instruction.clone())
    }

    /// Minimum publishable severity for `repo` (raw string; issue #11).
    pub fn min_severity_for(&self, repo: &str) -> Option<String> {
        self.overrides(repo)
            .and_then(|o| o.min_severity.clone())
            .or_else(|| self.min_severity.clone())
    }

    /// Findings publication mode for `repo` (raw string; issue #11).
    pub fn publish_for(&self, repo: &str) -> Option<String> {
        self.overrides(repo)
            .and_then(|o| o.publish.clone())
            .or_else(|| self.publish.clone())
    }
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
    /// Execution backend (issue #5): `host` runs coven-code directly (dev /
    /// self-hosted); `container` runs each task attempt in a fresh container
    /// with resource limits — the hosted posture.
    #[serde(default)]
    pub backend: WorkerBackendKind,
    /// Container backend settings; ignored for the host backend.
    #[serde(default)]
    pub container: ContainerConfig,
    /// Hosted mode (any [[installations]] configured) refuses the host
    /// backend unless the operator explicitly opts in here.
    #[serde(default)]
    pub allow_host_backend: bool,
}

/// Which sandbox executes coven-code sessions (issue #5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerBackendKind {
    #[default]
    Host,
    Container,
}

/// Container backend settings (issue #5). Defaults are a conservative
/// hardened profile; see `docs/container-isolation.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ContainerConfig {
    /// Image containing the coven-code runtime.
    #[serde(default = "default_container_image")]
    pub image: String,
    /// Docker-compatible CLI (docker, podman, nerdctl).
    #[serde(default = "default_docker_bin")]
    pub docker_bin: PathBuf,
    /// coven-code invocation inside the image.
    #[serde(default = "default_container_coven_code")]
    pub coven_code_bin: String,
    /// CPU limit (docker `--cpus`).
    #[serde(default = "default_container_cpus")]
    pub cpus: f64,
    /// Memory limit (docker `--memory`), e.g. "2g".
    #[serde(default = "default_container_memory")]
    pub memory: String,
    /// Process count limit (docker `--pids-limit`).
    #[serde(default = "default_container_pids")]
    pub pids: u32,
    /// Size of the writable /tmp tmpfs, e.g. "256m".
    #[serde(default = "default_container_tmpfs")]
    pub tmpfs_size: String,
    /// Network mode (docker `--network`): "bridge", "none", or a custom
    /// egress-restricted network.
    #[serde(default = "default_container_network")]
    pub network: String,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            image: default_container_image(),
            docker_bin: default_docker_bin(),
            coven_code_bin: default_container_coven_code(),
            cpus: default_container_cpus(),
            memory: default_container_memory(),
            pids: default_container_pids(),
            tmpfs_size: default_container_tmpfs(),
            network: default_container_network(),
        }
    }
}

fn default_container_image() -> String {
    "ghcr.io/opencoven/coven-code:latest".to_string()
}
fn default_docker_bin() -> PathBuf {
    PathBuf::from("docker")
}
fn default_container_coven_code() -> String {
    "coven-code".to_string()
}
fn default_container_cpus() -> f64 {
    1.0
}
fn default_container_memory() -> String {
    "2g".to_string()
}
fn default_container_pids() -> u32 {
    512
}
fn default_container_tmpfs() -> String {
    "256m".to_string()
}
fn default_container_network() -> String {
    "bridge".to_string()
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

    /// Resolves the effective routing scope for one delivery (issue #7).
    ///
    /// With no `[[installations]]` configured, routing is open: every
    /// familiar, all triggers — the self-hosted default. Once installations
    /// are configured, an unlisted installation id routes nothing (fail
    /// closed), and a listed one sees only its allow-listed familiars with
    /// its trigger policy (repo overrides applied).
    pub fn scope_for(&self, installation_id: u64, repo: &str) -> RoutingScope<'_> {
        if self.installations.is_empty() {
            return RoutingScope {
                familiars: self.familiars.iter().collect(),
                triggers: TriggerPolicy::default(),
            };
        }
        let Some(installation) = self.installations.iter().find(|i| i.id == installation_id)
        else {
            return RoutingScope::closed();
        };
        let familiars: Vec<&FamiliarConfig> = if installation.familiars.is_empty() {
            self.familiars.iter().collect()
        } else {
            self.familiars
                .iter()
                .filter(|f| installation.familiars.contains(&f.id))
                .collect()
        };
        let mut triggers = installation.triggers;
        if let Some(o) = installation.repos.get(repo) {
            if o.enabled == Some(false) {
                return RoutingScope {
                    familiars,
                    triggers: TriggerPolicy {
                        assignment: false,
                        labels: false,
                        commands: false,
                        reviews: false,
                    },
                };
            }
            triggers = TriggerPolicy {
                assignment: o.assignment.unwrap_or(triggers.assignment),
                labels: o.labels.unwrap_or(triggers.labels),
                commands: o.commands.unwrap_or(triggers.commands),
                reviews: o.reviews.unwrap_or(triggers.reviews),
            };
        }
        RoutingScope { familiars, triggers }
    }

    /// The routing policy view for one installation, for the Cave dashboard
    /// (issue #18). Resolves familiars and triggers the same way [`scope_for`]
    /// does, but at the installation level (not one delivery), and includes the
    /// per-repo overrides so the dashboard can render them.
    ///
    /// [`scope_for`]: Config::scope_for
    pub fn routing_view(&self, installation_id: u64) -> RoutingView {
        let all_familiars = || self.familiars.iter().map(|f| f.id.clone()).collect::<Vec<_>>();
        // Open routing (no installations configured): every familiar, all lanes.
        if self.installations.is_empty() {
            return RoutingView {
                installation_id,
                account: None,
                familiars: all_familiars(),
                triggers: TriggerPolicy::default(),
                repos: std::collections::HashMap::new(),
                listed: false,
            };
        }
        // Configured but unlisted: fail closed — routes nothing.
        let Some(installation) = self.installations.iter().find(|i| i.id == installation_id) else {
            return RoutingView {
                installation_id,
                account: None,
                familiars: Vec::new(),
                triggers: TriggerPolicy {
                    assignment: false,
                    labels: false,
                    commands: false,
                    reviews: false,
                },
                repos: std::collections::HashMap::new(),
                listed: false,
            };
        };
        let familiars = if installation.familiars.is_empty() {
            all_familiars()
        } else {
            self.familiars
                .iter()
                .map(|f| f.id.clone())
                .filter(|id| installation.familiars.contains(id))
                .collect()
        };
        RoutingView {
            installation_id,
            account: installation.account.clone(),
            familiars,
            triggers: installation.triggers,
            repos: installation.repos.clone(),
            listed: true,
        }
    }

    /// Usage limits for one installation (issue #15). Unlimited when the
    /// installation is not configured.
    pub fn limits_for(&self, installation_id: u64) -> InstallationLimits {
        self.installations
            .iter()
            .find(|i| i.id == installation_id)
            .map(|i| i.limits)
            .unwrap_or_default()
    }

    /// `installation id → max_concurrent` for every configured cap — the
    /// worker's claim filter (issue #15).
    pub fn concurrency_caps(&self) -> std::collections::HashMap<u64, u32> {
        self.installations
            .iter()
            .filter_map(|i| i.limits.max_concurrent.map(|cap| (i.id, cap)))
            .collect()
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

        match self.worker.backend {
            WorkerBackendKind::Host => {
                if !binary_resolvable(&self.worker.coven_code_bin) {
                    out.push(Diagnostic::error(
                        "worker.coven_code_bin",
                        format!(
                            "coven-code binary not found at '{}' (and not on PATH) — build/install coven-code or fix the path.",
                            self.worker.coven_code_bin.display()
                        ),
                    ));
                }
                // Hosted posture gate (issue #5): multi-tenant installations
                // must not run arbitrary repository workloads on the host
                // unless the operator explicitly accepts that risk.
                if !self.installations.is_empty() && !self.worker.allow_host_backend {
                    out.push(Diagnostic::error(
                        "worker.backend",
                        "installations are configured (hosted posture) but worker.backend is 'host' — set worker.backend = \"container\", or set worker.allow_host_backend = true to accept host execution explicitly.",
                    ));
                }
            }
            WorkerBackendKind::Container => {
                if !binary_resolvable(&self.worker.container.docker_bin) {
                    out.push(Diagnostic::error(
                        "worker.container.docker_bin",
                        format!(
                            "container CLI not found at '{}' (and not on PATH) — install docker/podman or fix worker.container.docker_bin.",
                            self.worker.container.docker_bin.display()
                        ),
                    ));
                }
                if self.worker.container.image.trim().is_empty() {
                    out.push(Diagnostic::error(
                        "worker.container.image",
                        "container image is empty — set worker.container.image to the coven-code runtime image.",
                    ));
                }
            }
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

        // ── Storage ─────────────────────────────────────────────────────
        if self.storage.path.is_dir() {
            out.push(Diagnostic::error(
                "storage.path",
                format!(
                    "'{}' is a directory — storage.path must be the SQLite database file itself.",
                    self.storage.path.display()
                ),
            ));
        } else if !self.storage.path.exists() {
            // Startup creates the file and any missing parents; surface where
            // it will land so operators mount/persist the right volume.
            let parent = self
                .storage
                .path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| ".".to_string());
            out.push(Diagnostic::warning(
                "storage.path",
                format!(
                    "database does not exist yet — it will be created under '{parent}' at startup; make sure that path is on a persistent volume."
                ),
            ));
        }

        // ── Task API auth (issue #3) ────────────────────────────────────
        match self.api.mode {
            ApiMode::Open => {
                if self.api.service_token.is_some() || !self.api.tenants.is_empty() {
                    out.push(Diagnostic::warning(
                        "api.mode",
                        "tokens are configured but api.mode is 'open' — they are ignored; set api.mode = \"token\" to enforce them.",
                    ));
                } else {
                    out.push(Diagnostic::warning(
                        "api.mode",
                        "the task API is unauthenticated (api.mode = \"open\") — fine for local development, never expose it publicly; hosted deployments must use \"token\".",
                    ));
                }
            }
            ApiMode::Token => {
                if self.api.service_token.is_none() && self.api.tenants.is_empty() {
                    out.push(Diagnostic::error(
                        "api.mode",
                        "api.mode is 'token' but no api.service_token or [[api.tenants]] tokens are configured — every task API call would fail.",
                    ));
                }
            }
        }
        let mut seen_api_tokens = std::collections::HashSet::new();
        for candidate in self
            .api
            .service_token
            .iter()
            .chain(self.api.tenants.iter().map(|t| &t.token))
        {
            let trimmed = candidate.trim();
            if trimmed.len() < 16 {
                out.push(Diagnostic::warning(
                    "api.tenants[].token",
                    "an API token is shorter than 16 characters — use a long random string.",
                ));
            }
            if !trimmed.is_empty() && !seen_api_tokens.insert(trimmed) {
                out.push(Diagnostic::error(
                    "api.tenants[].token",
                    "duplicate API token — two scopes would be indistinguishable at the boundary.",
                ));
            }
        }

        // ── Installations (issue #7) ────────────────────────────────────
        let known_familiar_ids: std::collections::HashSet<&str> =
            self.familiars.iter().map(|f| f.id.as_str()).collect();
        let mut seen_installations = std::collections::HashSet::new();
        for installation in &self.installations {
            if installation.id == 0 {
                out.push(Diagnostic::error(
                    "installations[].id",
                    "an installation has id 0 — set it to the numeric GitHub App installation id.",
                ));
            } else if !seen_installations.insert(installation.id) {
                out.push(Diagnostic::error(
                    "installations[].id",
                    format!(
                        "duplicate installation id {} — merge the blocks; the first would silently win.",
                        installation.id
                    ),
                ));
            }
            for id in &installation.familiars {
                if !known_familiar_ids.contains(id.as_str()) {
                    out.push(Diagnostic::error(
                        "installations[].familiars",
                        format!(
                            "installation {} allows familiar '{id}', which matches no configured [[familiars]] block.",
                            installation.id
                        ),
                    ));
                }
            }
            if installation.limits.max_concurrent == Some(0)
                || installation.limits.max_tasks_per_day == Some(0)
            {
                out.push(Diagnostic::error(
                    "installations[].limits",
                    format!(
                        "installation {} has a limit of 0 — no task would ever run; omit the limit for unlimited or set it to 1+.",
                        installation.id
                    ),
                ));
            }
        }

        // ── Review policy ───────────────────────────────────────────────
        let known_ids: std::collections::HashSet<&str> =
            self.familiars.iter().map(|f| f.id.as_str()).collect();
        let mut check_reviewer = |scope: &str, reviewer: Option<&str>| match reviewer {
            Some(id) if known_ids.contains(id) => {}
            Some(id) => out.push(Diagnostic::error(
                "review.familiar",
                format!("{scope} resolves to '{id}', which matches no configured familiar id."),
            )),
            None => out.push(Diagnostic::error(
                "review.familiar",
                format!("{scope} is enabled but no reviewer familiar is set."),
            )),
        };
        if self.review.pull_request {
            check_reviewer("the pull_request review lane", self.review.familiar.as_deref());
        }
        for (repo, o) in &self.review.repos {
            if o.pull_request == Some(true) {
                check_reviewer(
                    &format!("the pull_request review override for '{repo}'"),
                    o.familiar.as_deref().or(self.review.familiar.as_deref()),
                );
            }
        }
        // Publication policy values are closed enums (issue #11): a typo would
        // silently change what publishes, so doctor rejects unknown values.
        let severities = ["info", "low", "medium", "high", "critical"];
        let publish_modes = ["check_run", "advisory_comment", "request_changes"];
        let mut check_policy = |scope: &str, min_severity: Option<&str>, publish: Option<&str>| {
            if let Some(value) = min_severity {
                if !severities.contains(&value.to_ascii_lowercase().as_str()) {
                    out.push(Diagnostic::error(
                        "review.min_severity",
                        format!("{scope} has unknown min_severity '{value}' — use one of: {}.", severities.join(", ")),
                    ));
                }
            }
            if let Some(value) = publish {
                if !publish_modes.contains(&value.to_ascii_lowercase().as_str()) {
                    out.push(Diagnostic::error(
                        "review.publish",
                        format!("{scope} has unknown publish mode '{value}' — use one of: {}.", publish_modes.join(", ")),
                    ));
                }
            }
        };
        check_policy(
            "the [review] section",
            self.review.min_severity.as_deref(),
            self.review.publish.as_deref(),
        );
        for (repo, o) in &self.review.repos {
            check_policy(
                &format!("the review override for '{repo}'"),
                o.min_severity.as_deref(),
                o.publish.as_deref(),
            );
        }

        // ── Memory governance (issue #6) ────────────────────────────────
        // Memory is off by default; when an operator enables it anywhere,
        // warn if writes are not approval-gated — that is the posture that
        // lets untrusted content shape future reviews.
        let memory_on = self.memory.enabled || self.memory.repos.values().any(|o| o.enabled == Some(true));
        if memory_on {
            let gated = self.memory.approval_required
                && self.memory.repos.values().all(|o| o.approval_required != Some(false));
            if !gated {
                out.push(Diagnostic::warning(
                    "memory.approval_required",
                    "memory is enabled with approval_required = false — learned facts write without maintainer review.",
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
        "worker.backend" => {
            "Set worker.backend = \"container\" for hosted isolation, or worker.allow_host_backend = true to explicitly accept host execution."
        }
        "worker.container.docker_bin" => {
            "Install a docker-compatible CLI or point worker.container.docker_bin at it."
        }
        "worker.container.image" => {
            "Set worker.container.image to the image that carries the coven-code runtime."
        }
        "familiars" => "Add at least one [[familiars]] block for the bot account that should receive work.",
        "familiars[].id" => "Give each familiar a stable, unique id.",
        "familiars[].bot_username" => {
            "Set familiars[].bot_username to the GitHub App bot login, usually ending in [bot]."
        }
        "familiars[].trigger_labels" => {
            "Add labels such as coven:fix if this familiar should run from labels, or rely on assignment/mentions only."
        }
        "review.familiar" => {
            "Set review.familiar (or the repo override's familiar) to the id of a configured [[familiars]] block."
        }
        "storage.path" => {
            "Point storage.path at a writable SQLite file location on a persistent volume."
        }
        "memory.approval_required" => {
            "Keep memory.approval_required = true so learned facts need maintainer review, or accept the risk deliberately."
        }
        "api.mode" => {
            "Set api.mode = \"token\" and configure api.service_token and/or [[api.tenants]] tokens for hosted use."
        }
        "api.tenants[].token" => {
            "Generate long random tokens (e.g. openssl rand -hex 32) and keep each scope's token unique."
        }
        "review.min_severity" => {
            "Set review.min_severity to one of: info, low, medium, high, critical."
        }
        "review.publish" => {
            "Set review.publish to one of: check_run, advisory_comment, request_changes."
        }
        "installations[].id" => {
            "Copy the numeric installation id from the GitHub App installations page into installations[].id."
        }
        "installations[].familiars" => {
            "Reference only ids that appear in a [[familiars]] block, or leave the list empty to allow all."
        }
        "installations[].limits" => {
            "Remove the zero limit (unlimited) or set max_concurrent / max_tasks_per_day to 1 or more."
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
            review: ReviewConfig::default(),
            storage: StorageConfig::default(),
            memory: MemoryConfig::default(),
            api: ApiConfig::default(),
            installations: vec![],
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

    fn routing_config(familiars: Vec<FamiliarConfig>) -> Config {
        let dir = tmpdir();
        config_with(
            GitHubAppConfig {
                app_id: 1,
                private_key_path: write_pem(&dir),
                webhook_secret: "a-long-random-webhook-secret".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 1,
                coven_code_bin: write_bin(&dir),
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
                backend: WorkerBackendKind::Host,
                container: ContainerConfig::default(),
                allow_host_backend: true,
            },
            familiars,
        )
    }

    fn fam(id: &str) -> FamiliarConfig {
        FamiliarConfig {
            id: id.into(),
            display_name: id.into(),
            bot_username: format!("coven-{id}[bot]"),
            model: None,
            skills: vec![],
            trigger_labels: vec![],
        }
    }

    #[test]
    fn routing_view_reflects_open_listed_and_fail_closed() {
        let mut cfg = routing_config(vec![fam("cody"), fam("nova")]);

        // Open routing (no installations): every familiar, all lanes, unlisted.
        let open = cfg.routing_view(123);
        assert_eq!(open.familiars, vec!["cody", "nova"]);
        assert!(open.triggers.reviews);
        assert!(!open.listed);

        // Listed with an allow-list and a disabled lane.
        cfg.installations = vec![InstallationConfig {
            id: 123,
            account: Some("acme".into()),
            familiars: vec!["cody".into()],
            triggers: TriggerPolicy {
                reviews: false,
                ..Default::default()
            },
            limits: InstallationLimits::default(),
            repos: std::collections::HashMap::new(),
        }];
        let listed = cfg.routing_view(123);
        assert_eq!(listed.familiars, vec!["cody"]);
        assert_eq!(listed.account.as_deref(), Some("acme"));
        assert!(!listed.triggers.reviews);
        assert!(listed.listed);

        // Configured but unlisted → fail closed.
        let closed = cfg.routing_view(999);
        assert!(closed.familiars.is_empty());
        assert!(!closed.triggers.assignment);
        assert!(!closed.listed);
    }

    #[test]
    fn review_policy_resolves_repo_overrides() {
        let policy = ReviewConfig {
            familiar: Some("cody".to_string()),
            pull_request: true,
            include_drafts: false,
            audit_instruction: Some("global".to_string()),
            min_severity: Some("medium".to_string()),
            publish: None,
            repos: std::collections::HashMap::from([(
                "OpenCoven/quiet".to_string(),
                RepoReviewOverride {
                    pull_request: Some(false),
                    include_drafts: Some(true),
                    familiar: Some("nova".to_string()),
                    audit_instruction: None,
                    min_severity: None,
                    publish: Some("advisory_comment".to_string()),
                },
            )]),
        };

        // Publication policy inherits globally and overrides per repo (#11).
        assert_eq!(
            policy.min_severity_for("OpenCoven/loud").as_deref(),
            Some("medium")
        );
        assert_eq!(
            policy.min_severity_for("OpenCoven/quiet").as_deref(),
            Some("medium")
        );
        assert_eq!(policy.publish_for("OpenCoven/loud"), None);
        assert_eq!(
            policy.publish_for("OpenCoven/quiet").as_deref(),
            Some("advisory_comment")
        );

        assert!(policy.pull_request_enabled("OpenCoven/loud"));
        assert!(!policy.pull_request_enabled("OpenCoven/quiet"));
        assert!(!policy.drafts_included("OpenCoven/loud"));
        assert!(policy.drafts_included("OpenCoven/quiet"));
        assert_eq!(policy.reviewer("OpenCoven/loud"), Some("cody"));
        assert_eq!(policy.reviewer("OpenCoven/quiet"), Some("nova"));
        assert_eq!(
            policy.audit_instruction_for("OpenCoven/quiet").as_deref(),
            Some("global")
        );
    }

    #[test]
    fn memory_policy_defaults_off_and_resolves_repo_overrides() {
        let mut memory = MemoryConfig::default();
        assert!(!memory.enabled_for("acme/any"), "memory is off by default");

        memory.enabled = true;
        assert!(memory.enabled_for("acme/any"));
        // approval_required serde-defaults to true, but Default::default() is
        // false; set it explicitly to model the deserialized default.
        memory.approval_required = true;
        assert!(memory.approval_required_for("acme/any"));

        memory.repos.insert(
            "acme/quiet".to_string(),
            RepoMemoryOverride {
                enabled: Some(false),
                approval_required: None,
            },
        );
        assert!(!memory.enabled_for("acme/quiet"), "override disables the repo");
        assert!(memory.enabled_for("acme/loud"));
    }

    #[test]
    fn doctor_warns_when_memory_enabled_without_approval() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let mut cfg = config_with(
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
            },
            vec![good_familiar()],
        );
        cfg.memory.enabled = true;
        cfg.memory.approval_required = false;

        let warned = cfg
            .check()
            .iter()
            .any(|d| d.field == "memory.approval_required");
        assert!(warned, "diags: {:?}", cfg.check());
        // It is a warning, not an error — the operator may accept the risk.
        assert!(errors(&cfg.check()).is_empty());
    }

    #[test]
    fn review_policy_defaults_to_disabled() {
        let policy = ReviewConfig::default();
        assert!(!policy.pull_request_enabled("OpenCoven/any"));
        assert!(policy.reviewer("OpenCoven/any").is_none());
        assert!(!policy.drafts_included("OpenCoven/any"));
    }

    #[test]
    fn doctor_flags_review_enabled_without_known_familiar() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let mut cfg = config_with(
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
            },
            vec![good_familiar()],
        );
        cfg.review.pull_request = true;
        cfg.review.familiar = Some("ghost".to_string());

        let diags = cfg.check();
        assert!(
            errors(&diags).contains(&"review.familiar"),
            "diags: {diags:?}"
        );

        // A known familiar id resolves cleanly.
        cfg.review.familiar = Some("cody".to_string());
        assert!(errors(&cfg.check()).is_empty());

        // The lane enabled with no reviewer at all is also an error.
        cfg.review.familiar = None;
        assert!(errors(&cfg.check()).contains(&"review.familiar"));
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
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
    fn token_mode_without_tokens_is_an_error_and_open_mode_warns() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let mut cfg = config_with(
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
            },
            vec![good_familiar()],
        );

        // Default open mode: a warning, not an error.
        let diags = cfg.check();
        assert!(errors(&diags).is_empty(), "diags: {diags:?}");
        assert!(
            diags
                .iter()
                .any(|d| d.field == "api.mode" && d.message.contains("unauthenticated")),
            "open mode must warn: {diags:?}"
        );

        // Token mode with nothing to match against would deny every call.
        cfg.api.mode = ApiMode::Token;
        let diags = cfg.check();
        assert!(
            errors(&diags).contains(&"api.mode"),
            "token mode without tokens must error: {diags:?}"
        );

        // A configured tenant token clears the error.
        cfg.api.tenants = vec![TenantToken {
            token: "a-long-random-api-token".into(),
            installation_id: 1,
            repos: vec![],
        }];
        assert!(errors(&cfg.check()).is_empty());
    }

    #[test]
    fn installation_policy_is_validated() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let mut cfg = config_with(
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
            },
            vec![good_familiar()],
        );
        cfg.installations = vec![
            InstallationConfig {
                id: 7,
                account: None,
                familiars: vec!["ghost".to_string()],
                triggers: TriggerPolicy::default(),
                limits: InstallationLimits::default(),
                repos: std::collections::HashMap::new(),
            },
            InstallationConfig {
                id: 7,
                account: None,
                familiars: vec![],
                triggers: TriggerPolicy::default(),
                limits: InstallationLimits::default(),
                repos: std::collections::HashMap::new(),
            },
        ];
        let diags = cfg.check();
        let errs = errors(&diags);
        assert!(errs.contains(&"installations[].familiars"), "{errs:?}");
        assert!(errs.contains(&"installations[].id"), "{errs:?}");
    }

    #[test]
    fn hosted_posture_refuses_the_host_backend_without_explicit_opt_in() {
        let dir = tmpdir();
        let pem = write_pem(&dir);
        let bin = write_bin(&dir);
        let mut cfg = config_with(
            GitHubAppConfig {
                app_id: 123,
                private_key_path: pem,
                webhook_secret: "a-long-random-webhook-secret".into(),
                api_base_url: None,
            },
            WorkerConfig {
                concurrency: 4,
                coven_code_bin: bin.clone(),
                workspace_root: dir.clone(),
                timeout_secs: 600,
                max_retries: 2,
                backend: WorkerBackendKind::Host,
                container: ContainerConfig::default(),
                allow_host_backend: false,
            },
            vec![good_familiar()],
        );
        cfg.installations = vec![InstallationConfig {
            id: 7,
            account: None,
            familiars: vec![],
            triggers: TriggerPolicy::default(),
            limits: InstallationLimits::default(),
            repos: std::collections::HashMap::new(),
        }];

        // Hosted posture + host backend: refused.
        let diags = cfg.check();
        assert!(errors(&diags).contains(&"worker.backend"), "{diags:?}");

        // Explicit operator opt-in clears it.
        cfg.worker.allow_host_backend = true;
        let diags = cfg.check();
        assert!(!errors(&diags).contains(&"worker.backend"), "{diags:?}");

        // Container backend also clears it — but validates the runtime CLI.
        cfg.worker.allow_host_backend = false;
        cfg.worker.backend = WorkerBackendKind::Container;
        cfg.worker.container.docker_bin = bin; // any executable file
        let diags = cfg.check();
        assert!(!errors(&diags).contains(&"worker.backend"), "{diags:?}");
        cfg.worker.container.docker_bin = dir.join("no-such-docker");
        let diags = cfg.check();
        assert!(
            errors(&diags).contains(&"worker.container.docker_bin"),
            "{diags:?}"
        );
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
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
            backend: WorkerBackendKind::Host,
            container: ContainerConfig::default(),
            allow_host_backend: false,
            },
            vec![good_familiar(), good_familiar()],
        );
        let diags = cfg.check();
        let errs = errors(&diags);
        assert!(errs.contains(&"familiars[].id"));
        assert!(errs.contains(&"familiars[].bot_username"));
    }
}
