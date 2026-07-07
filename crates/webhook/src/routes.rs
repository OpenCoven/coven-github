//! Axum route handlers for the webhook endpoint.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use coven_github_api::tasks::TaskListStatus;
use coven_github_api::{GitHubEvent, Task, TaskKind};
use coven_github_config::{ApiConfig, ApiMode, Config};
use coven_github_store::{ApiScope, Delivery, Recorded, Routing, Store};

use crate::{
    commands::{parse_mention, Command, MentionKind, COMMAND_LIST},
    events::{parse_event, WebhookPayload},
    verify_signature,
};

/// Shared application state passed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: std::sync::Arc<Config>,
    /// Durable deliveries + task queue (issue #2). Deliveries are recorded —
    /// and deduplicated — here before GitHub is told the webhook succeeded;
    /// the worker claims queued tasks from the same store.
    pub store: Store,
    /// Wake-up signal to the worker pool after a task row is enqueued.
    pub notify: std::sync::Arc<tokio::sync::Notify>,
}

/// Display names by familiar id — the store holds ids; names are config's.
fn familiar_names(config: &Config) -> std::collections::HashMap<String, String> {
    config
        .familiars
        .iter()
        .map(|f| (f.id.clone(), f.display_name.clone()))
        .collect()
}

/// GET /api/github/tasks — task state for CovenCave polling, behind the
/// tenant boundary (issue #3). `token` mode fails closed; a tenant token sees
/// only its own installation (optionally narrowed to repositories); the
/// service token — and `open` mode, for local development — see everything.
/// Every read lands in the audit trail.
pub async fn list_tasks(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let action = "list_tasks";
    let (caller, scope) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            if let Err(e) = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            // Fail closed with a body that reveals nothing about what exists.
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Open => ("open".to_string(), None),
        ApiCaller::Service => ("service".to_string(), None),
        ApiCaller::Tenant(tenant) => (
            format!("tenant:{}", tenant.installation_id),
            Some(ApiScope {
                installation_id: tenant.installation_id,
                repos: if tenant.repos.is_empty() {
                    None
                } else {
                    Some(tenant.repos.clone())
                },
            }),
        ),
    };
    let scope_label = scope
        .as_ref()
        .map(|s| format!("installation:{}", s.installation_id))
        .unwrap_or_else(|| "all".to_string());

    match state
        .store
        .cave_list(familiar_names(&state.config), scope)
        .await
    {
        Ok(tasks) => {
            if let Err(e) = state
                .store
                .record_api_read(&caller, &scope_label, action, &format!("ok:{}", tasks.len()))
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            Json(json!({ "ok": true, "tasks": tasks })).into_response()
        }
        Err(e) => {
            error!("task list unavailable: {e:#}");
            let _ = state
                .store
                .record_api_read(&caller, &scope_label, action, "error")
                .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": "task list unavailable"})),
            )
                .into_response()
        }
    }
}

/// Resolved identity of a task-API caller.
enum ApiCaller<'a> {
    /// `open` mode: unauthenticated local development.
    Open,
    /// Operator-wide service token: unrestricted visibility.
    Service,
    /// Tenant token: one installation's scope.
    Tenant(&'a coven_github_config::TenantToken),
    /// No valid credential in `token` mode. Fail closed.
    Denied,
}

fn authorize_api<'a>(api: &'a ApiConfig, headers: &HeaderMap) -> ApiCaller<'a> {
    if api.mode == ApiMode::Open {
        return ApiCaller::Open;
    }
    let candidate = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let Some(candidate) = candidate else {
        return ApiCaller::Denied;
    };
    if let Some(service) = &api.service_token {
        if token_matches(candidate, service) {
            return ApiCaller::Service;
        }
    }
    for tenant in &api.tenants {
        if token_matches(candidate, &tenant.token) {
            return ApiCaller::Tenant(tenant);
        }
    }
    ApiCaller::Denied
}

/// Constant-time token comparison: comparing fixed-length digests leaks
/// nothing about how many characters of the token matched.
fn token_matches(candidate: &str, expected: &str) -> bool {
    Sha256::digest(candidate.as_bytes()) == Sha256::digest(expected.as_bytes())
}

/// POST /webhook — GitHub webhook receiver.
pub async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Validate HMAC signature.
    let sig = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            warn!("webhook missing x-hub-signature-256");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing signature"})),
            )
                .into_response();
        }
    };

    if let Err(e) = verify_signature(&state.config.github.webhook_secret, &body, &sig) {
        warn!("webhook signature invalid: {e}");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid signature"})),
        )
            .into_response();
    }

    // 2. Parse event type and delivery id headers. GitHub sends
    //    X-GitHub-Delivery on every delivery; it is the idempotency key
    //    (issue #2), so a request without one is not a GitHub delivery.
    let event_type = match headers.get("x-github-event").and_then(|v| v.to_str().ok()) {
        Some(e) => e.to_string(),
        None => {
            warn!("webhook missing x-github-event");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing event type"})),
            )
                .into_response();
        }
    };
    let delivery_id = match headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            warn!("webhook missing x-github-delivery");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing delivery id"})),
            )
                .into_response();
        }
    };

    // 3. Parse payload.
    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!("webhook payload parse error: {e}");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "parse error"})),
            )
                .into_response();
        }
    };

    let event = parse_event(&event_type, &payload);
    info!(?event_type, "received webhook event");

    // Delivery coordinates for the durable record (issue #2): enough to
    // answer "was this accepted, and what did it become?" without retaining
    // the payload body itself.
    let delivery = Delivery {
        delivery_id: delivery_id.clone(),
        event: event_type.clone(),
        action: payload.action.clone(),
        installation_id: payload.installation.as_ref().map(|i| i.id),
        repo: payload
            .repository
            .as_ref()
            .map(|r| format!("{}/{}", r.owner.login, r.name)),
        payload_hash: hex::encode(Sha256::digest(&body)),
    };

    // `ping` is GitHub's webhook-configuration handshake — acknowledge it
    // explicitly so operators get a clear signal the endpoint is wired up.
    if matches!(event, GitHubEvent::Ping) {
        info!("webhook ping received — endpoint configured");
        return match state
            .store
            .record_delivery(delivery, Routing::Ignored("ping"))
            .await
        {
            Ok(_) => (StatusCode::OK, Json(json!({"ok": true, "pong": true}))).into_response(),
            Err(e) => storage_unavailable(e),
        };
    }

    // 4. Route event → task, then persist BEFORE acknowledging: GitHub must
    //    only hear success once durable state exists, and a redelivered
    //    delivery id must never dispatch twice (issue #2).
    match event_to_task(&state, event).await {
        Some(task) => {
            match state
                .store
                .record_delivery(delivery, Routing::Task(&task))
                .await
            {
                Ok(Recorded::New) => {}
                Ok(Recorded::Duplicate) => {
                    info!(delivery_id, "duplicate delivery — already routed, nothing dispatched");
                    return (
                        StatusCode::OK,
                        Json(json!({"ok": true, "duplicate": true})),
                    )
                        .into_response();
                }
                Err(e) => return storage_unavailable(e),
            }
            // The tasks table IS the queue: enqueueing is the durable insert
            // above (with supersession tombstones applied in-transaction);
            // this only wakes the claim loop. No channel, no drop path.
            info!(task_id = %task.id, "task enqueued");
            state.notify.notify_one();
        }
        None => {
            if let Err(e) = state
                .store
                .record_delivery(delivery, Routing::Ignored("unroutable"))
                .await
            {
                return storage_unavailable(e);
            }
        }
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

/// The durable store could not record the delivery: answer 5xx so GitHub
/// retries later instead of treating unheld work as delivered (issue #2).
fn storage_unavailable(e: anyhow::Error) -> axum::response::Response {
    error!("durable store unavailable: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "storage unavailable"})),
    )
        .into_response()
}

/// Maps a parsed event to a worker task, or returns None if not actionable.
async fn event_to_task(state: &AppState, event: GitHubEvent) -> Option<Task> {
    match event {
        GitHubEvent::IssueAssigned(e) => {
            // Find a familiar whose bot_username matches the assignee.
            let familiar = state
                .config
                .familiars
                .iter()
                .find(|f| f.bot_username == e.assignee_login)?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                commander: None,
                kind: TaskKind::FixIssue {
                    issue_number: e.issue_number,
                    issue_title: e.issue_title,
                    issue_body: e.issue_body,
                },
            })
        }

        GitHubEvent::IssueLabeled(e) => {
            let familiar = state
                .config
                .familiars
                .iter()
                .find(|f| f.trigger_labels.iter().any(|label| label == &e.label_name))?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                commander: None,
                kind: TaskKind::FixIssue {
                    issue_number: e.issue_number,
                    issue_title: e.issue_title,
                    issue_body: e.issue_body,
                },
            })
        }

        // Comment surfaces speak the maintainer command protocol (issue #13):
        // only command-position mentions act; casual mentions are ignored.
        GitHubEvent::IssueComment(e) => {
            let (familiar, command) = parse_command(state, &e.comment_body, &e.commenter_login)?;
            let surface = CommandSurface {
                installation_id: e.installation_id,
                repo_owner: &e.repo_owner,
                repo_name: &e.repo_name,
                number: e.issue_number,
                title: &e.issue_title,
                issue_body: &e.issue_body,
                comment_body: &e.comment_body,
                on_pull_request: e.on_pull_request,
                commander: &e.commenter_login,
            };
            command_task(state, familiar, command, surface).await
        }

        GitHubEvent::PullRequestReview(e) => {
            let (familiar, command) = parse_command(state, &e.review_body, &e.reviewer_login)?;
            let surface = CommandSurface {
                installation_id: e.installation_id,
                repo_owner: &e.repo_owner,
                repo_name: &e.repo_name,
                number: e.pr_number,
                title: &e.pr_title,
                issue_body: "",
                comment_body: &e.review_body,
                on_pull_request: true,
                commander: &e.reviewer_login,
            };
            command_task(state, familiar, command, surface).await
        }

        GitHubEvent::PullRequestReviewComment(e) => {
            let (familiar, command) = parse_command(state, &e.comment_body, &e.commenter_login)?;
            let surface = CommandSurface {
                installation_id: e.installation_id,
                repo_owner: &e.repo_owner,
                repo_name: &e.repo_name,
                number: e.pr_number,
                title: &e.pr_title,
                issue_body: "",
                comment_body: &e.comment_body,
                on_pull_request: true,
                commander: &e.commenter_login,
            };
            command_task(state, familiar, command, surface).await
        }

        GitHubEvent::PullRequestChanged(e) => {
            // Loop prevention: never auto-review PRs authored by our own
            // familiars — the adapter's draft PRs would otherwise re-trigger.
            if state
                .config
                .familiars
                .iter()
                .any(|f| f.bot_username == e.author_login)
            {
                return None;
            }
            let repo = format!("{}/{}", e.repo_owner, e.repo_name);
            let policy = &state.config.review;
            let familiar = if e.action == "labeled" {
                // A review label is an explicit per-PR opt-in: it works even
                // when the automatic lane is off, and on drafts — the same
                // contract as issue trigger labels.
                let label = e.label_name.as_deref()?;
                state
                    .config
                    .familiars
                    .iter()
                    .find(|f| f.trigger_labels.iter().any(|t| t == label))?
            } else {
                if !policy.pull_request_enabled(&repo) {
                    return None;
                }
                if e.draft && !policy.drafts_included(&repo) {
                    return None;
                }
                let reviewer = policy.reviewer(&repo)?;
                state.config.familiars.iter().find(|f| f.id == reviewer)?
            };

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                commander: None,
                kind: TaskKind::ReviewPullRequest {
                    pr_number: e.pr_number,
                    pr_title: e.pr_title,
                    reason: e.action,
                },
            })
        }

        // Push review needs a PR-less task kind — a contract v3 lane. The
        // event is parsed and typed so fixtures and policy land now; routing
        // activates with the v3 brief (issue #10).
        GitHubEvent::Push(_) => None,

        // `ping` is acknowledged at the handler; it never produces a task.
        GitHubEvent::Ping | GitHubEvent::Unsupported { .. } => None,
    }
}

/// Finds the first familiar addressed in command position, skipping the bots'
/// own comments (self-trigger loop guard).
fn parse_command<'a>(
    state: &'a AppState,
    body: &str,
    author: &str,
) -> Option<(&'a coven_github_config::FamiliarConfig, Command)> {
    state.config.familiars.iter().find_map(|f| {
        if author == f.bot_username {
            return None;
        }
        match parse_mention(body, &f.bot_username) {
            MentionKind::Command(command) => Some((f, command)),
            MentionKind::Casual | MentionKind::None => None,
        }
    })
}

/// The issue/PR conversation a command arrived on.
struct CommandSurface<'a> {
    installation_id: u64,
    repo_owner: &'a str,
    repo_name: &'a str,
    number: u64,
    title: &'a str,
    issue_body: &'a str,
    comment_body: &'a str,
    on_pull_request: bool,
    commander: &'a str,
}

/// Maps a typed maintainer command to a task. Work commands and gated
/// acknowledgements (cancel, remember/forget) carry the commander for the
/// worker's permission gate; clarifications and status replies carry none.
async fn command_task(
    state: &AppState,
    familiar: &coven_github_config::FamiliarConfig,
    command: Command,
    s: CommandSurface<'_>,
) -> Option<Task> {
    let repo = format!("{}/{}", s.repo_owner, s.repo_name);
    let make = |kind: TaskKind, commander: Option<String>| Task {
        id: uuid::Uuid::new_v4().to_string(),
        installation_id: s.installation_id,
        repo_owner: s.repo_owner.to_string(),
        repo_name: s.repo_name.to_string(),
        familiar_id: familiar.id.clone(),
        commander,
        kind,
    };
    let commander = Some(s.commander.to_string());
    let reply = |body: String| {
        make(
            TaskKind::CommandReply {
                issue_number: s.number,
                body,
            },
            None,
        )
    };

    Some(match command {
        Command::Review | Command::Deepen | Command::Retry if s.on_pull_request => make(
            TaskKind::ReviewPullRequest {
                pr_number: s.number,
                pr_title: s.title.to_string(),
                reason: format!("command:{}", verb(&command)),
            },
            commander,
        ),
        // `retry` on an issue re-runs the fix lane.
        Command::Retry => make(
            TaskKind::FixIssue {
                issue_number: s.number,
                issue_title: s.title.to_string(),
                issue_body: s.issue_body.to_string(),
            },
            commander,
        ),
        Command::Review | Command::Deepen => reply(format!(
            "`{}` needs a pull request; this is an issue. Commands here: {COMMAND_LIST}.",
            verb(&command)
        )),
        Command::Fix { .. } if s.on_pull_request => make(
            TaskKind::AddressReviewComment {
                pr_number: s.number,
                comment_body: s.comment_body.to_string(),
                diff_hunk: None,
            },
            commander,
        ),
        Command::Fix { .. } => make(
            TaskKind::FixIssue {
                issue_number: s.number,
                issue_title: s.title.to_string(),
                issue_body: s.issue_body.to_string(),
            },
            commander,
        ),
        // Cancellation mutates queued work, so it rides a gated adapter task:
        // the worker verifies the commander's write access before tombstoning
        // anything (issue #13). In-flight work is not interrupted (documented
        // limitation); the next review command or PR event re-arms the lane.
        Command::Cancel if s.on_pull_request => make(
            TaskKind::CancelReviews {
                pr_number: s.number,
            },
            commander,
        ),
        Command::Cancel => reply("`cancel` currently applies to queued pull-request reviews only.".to_string()),
        // Memory acknowledgements are gated too: only maintainers should hear
        // how the familiar handles memory intents.
        Command::Remember { .. } | Command::Forget { .. } => make(
            TaskKind::CommandReply {
                issue_number: s.number,
                body: "Noted, but memory persistence is not wired up yet — it lands with the \
                       hosted memory governance contract (#6). Nothing was stored or deleted."
                    .to_string(),
            },
            commander,
        ),
        Command::Status => {
            let items = state
                .store
                .cave_list(familiar_names(&state.config), None)
                .await
                .unwrap_or_else(|e| {
                    warn!("status could not reach the store: {e:#}");
                    Vec::new()
                });
            let mut lines: Vec<String> = items
                .iter()
                .filter(|t| t.repo == repo && t.issue_number == s.number)
                .map(|t| format!("- {} — {}", t.issue_title, status_label(&t.status)))
                .collect();
            let body = if lines.is_empty() {
                format!("No tracked tasks for {repo}#{}.", s.number)
            } else {
                lines.sort();
                format!("Tasks for {repo}#{}:\n{}", s.number, lines.join("\n"))
            };
            reply(body)
        }
        Command::Unknown { verb } => reply(format!(
            "I don't recognize `{verb}`. Supported commands: {COMMAND_LIST}."
        )),
    })
}

fn verb(command: &Command) -> &'static str {
    match command {
        Command::Review => "review",
        Command::Fix { .. } => "fix",
        Command::Deepen => "deepen",
        Command::Retry => "retry",
        Command::Cancel => "cancel",
        Command::Remember { .. } => "remember",
        Command::Forget { .. } => "forget",
        Command::Status => "status",
        Command::Unknown { .. } => "unknown",
    }
}

fn status_label(status: &TaskListStatus) -> &'static str {
    match status {
        TaskListStatus::Queued => "queued",
        TaskListStatus::Running => "running",
        TaskListStatus::Review => "awaiting review",
        TaskListStatus::Done => "done",
        TaskListStatus::Failed => "failed",
        TaskListStatus::Superseded => "superseded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{IssueCommentEvent, IssueLabeledEvent, TaskKind};
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::{path::PathBuf, sync::Arc};

    pub(crate) fn app_state() -> AppState {
        app_state_with_review(coven_github_config::ReviewConfig::default())
    }

    pub(crate) fn app_state_with_review(review: coven_github_config::ReviewConfig) -> AppState {
        AppState {
            config: Arc::new(Config {
                server: ServerConfig {
                    bind: "127.0.0.1:0".to_string(),
                    cave_base_url: None,
                },
                github: GitHubAppConfig {
                    app_id: 1,
                    private_key_path: PathBuf::from("private.pem"),
                    webhook_secret: "secret".to_string(),
                    api_base_url: None,
                },
                worker: WorkerConfig {
                    concurrency: 1,
                    coven_code_bin: PathBuf::from("coven-code"),
                    workspace_root: PathBuf::from("/tmp/coven-github-test"),
                    timeout_secs: 60,
                    max_retries: 0,
                },
                familiars: vec![FamiliarConfig {
                    id: "cody".to_string(),
                    display_name: "Cody".to_string(),
                    bot_username: "coven-cody[bot]".to_string(),
                    model: None,
                    skills: vec![],
                    trigger_labels: vec!["coven:fix".to_string(), "coven:review".to_string()],
                }],
                review,
                storage: coven_github_config::StorageConfig::default(),
                memory: coven_github_config::MemoryConfig::default(),
                api: coven_github_config::ApiConfig::default(),
            }),
            store: Store::open_in_memory().expect("in-memory store"),
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Enqueues a durable task row the way the webhook path would.
    pub(crate) async fn seed_task(state: &AppState, delivery_id: &str, task: &Task) {
        state
            .store
            .record_delivery(
                coven_github_store::Delivery {
                    delivery_id: delivery_id.to_string(),
                    event: "test".to_string(),
                    action: None,
                    installation_id: Some(task.installation_id),
                    repo: Some(format!("{}/{}", task.repo_owner, task.repo_name)),
                    payload_hash: "h".to_string(),
                },
                coven_github_store::Routing::Task(task),
            )
            .await
            .expect("seed task");
    }

    #[tokio::test]
    async fn labeled_issue_routes_to_familiar_trigger_label() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueLabeled(IssueLabeledEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Token refresh is broken.".to_string(),
                label_name: "coven:fix".to_string(),
            }),
        )
        .await
        .expect("matching trigger label should create a task");

        assert_eq!(task.installation_id, 123);
        assert_eq!(task.repo_owner, "OpenCoven");
        assert_eq!(task.repo_name, "coven-code");
        assert_eq!(task.familiar_id, "cody");
        match task.kind {
            TaskKind::FixIssue {
                issue_number,
                issue_title,
                issue_body,
            } => {
                assert_eq!(issue_number, 42);
                assert_eq!(issue_title, "Fix auth");
                assert_eq!(issue_body, "Token refresh is broken.");
            }
            other => panic!("expected FixIssue task, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn labeled_issue_ignores_unknown_labels() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueLabeled(IssueLabeledEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Token refresh is broken.".to_string(),
                label_name: "help wanted".to_string(),
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn issue_comment_ignores_bot_self_mentions() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody thanks, I opened a PR.".to_string(),
                commenter_login: "coven-cody[bot]".to_string(),
                on_pull_request: false,
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn pr_review_comment_ignores_bot_self_mentions() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReviewComment(coven_github_api::PrReviewCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 7,
                pr_title: "Harden sigil parser".to_string(),
                comment_body: "@coven-cody fix: please".to_string(),
                commenter_login: "coven-cody[bot]".to_string(),
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn issue_comment_on_pr_routes_to_pr_iteration() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 73,
                issue_title: "Add spell compiler cache".to_string(),
                issue_body: "".to_string(),
                comment_body: "@coven-cody fix: the lint is still failing".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: true,
            }),
        )
        .await
        .expect("a mention on a PR comment should create a task");

        match task.kind {
            TaskKind::AddressReviewComment {
                pr_number,
                comment_body,
                diff_hunk,
            } => {
                assert_eq!(pr_number, 73);
                assert!(comment_body.contains("lint is still failing"));
                assert!(diff_hunk.is_none());
            }
            other => panic!("expected AddressReviewComment for a PR comment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fix_command_on_issue_routes_to_fix_issue_with_commander() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Token refresh is broken.".to_string(),
                comment_body: "@coven-cody fix".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("a fix command on an issue should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::FixIssue {
                issue_number,
                issue_title,
                ..
            } => {
                assert_eq!(issue_number, 42);
                assert_eq!(issue_title, "Fix auth");
            }
            other => panic!("expected FixIssue for a fix command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn casual_mention_creates_no_task() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "thanks @coven-cody, great work".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await;

        assert!(task.is_none(), "casual mentions must not trigger work");
    }

    #[tokio::test]
    async fn unknown_command_earns_a_clarification_reply() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody can you take a look?".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("unknown command should earn a clarification");

        assert!(task.commander.is_none(), "replies need no permission gate");
        match task.kind {
            TaskKind::CommandReply { issue_number, body } => {
                assert_eq!(issue_number, 42);
                assert!(body.contains("`can`"));
                assert!(body.contains("`review`"), "reply should list commands: {body}");
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submitted_review_mention_routes_to_pr_iteration() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReview(coven_github_api::PrReviewEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 73,
                pr_title: "Harden sigil parser".to_string(),
                review_body: "@coven-cody fix: add test coverage before merge.".to_string(),
                review_state: "changes_requested".to_string(),
                reviewer_login: "octocat".to_string(),
            }),
        )
        .await
        .expect("a mention in a review body should create a task");

        match task.kind {
            TaskKind::AddressReviewComment { pr_number, .. } => assert_eq!(pr_number, 73),
            other => panic!("expected AddressReviewComment for a review, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submitted_review_without_mention_is_ignored() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReview(coven_github_api::PrReviewEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 73,
                pr_title: "Harden sigil parser".to_string(),
                review_body: "LGTM, nice work!".to_string(),
                review_state: "approved".to_string(),
                reviewer_login: "octocat".to_string(),
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn ping_event_produces_no_task() {
        let state = app_state();
        assert!(event_to_task(&state, GitHubEvent::Ping).await.is_none());
    }

}

#[cfg(test)]
mod review_lane_tests {
    use super::tests::{app_state, app_state_with_review};
    use super::*;
    use coven_github_api::PrChangedEvent;
    use coven_github_config::ReviewConfig;

    fn review_on() -> ReviewConfig {
        ReviewConfig {
            familiar: Some("cody".to_string()),
            pull_request: true,
            include_drafts: false,
            audit_instruction: None,
            min_severity: None,
            publish: None,
            repos: std::collections::HashMap::new(),
        }
    }

    fn pr_event(action: &str) -> PrChangedEvent {
        PrChangedEvent {
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            pr_number: 88,
            pr_title: "Add spell compiler cache".to_string(),
            action: action.to_string(),
            label_name: None,
            head_ref: "feat/spell-cache".to_string(),
            head_sha: "abc123".to_string(),
            base_ref: "main".to_string(),
            author_login: "octocat".to_string(),
            draft: false,
        }
    }

    #[tokio::test]
    async fn pr_opened_routes_to_review_when_lane_enabled() {
        let state = app_state_with_review(review_on());
        let task = event_to_task(&state, GitHubEvent::PullRequestChanged(pr_event("opened")))
            .await
            .expect("enabled lane should create a review task");

        assert_eq!(task.familiar_id, "cody");
        match task.kind {
            TaskKind::ReviewPullRequest {
                pr_number, reason, ..
            } => {
                assert_eq!(pr_number, 88);
                assert_eq!(reason, "opened");
            }
            other => panic!("expected ReviewPullRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_synchronize_carries_event_time_head() {
        let state = app_state_with_review(review_on());
        let mut event = pr_event("synchronize");
        event.head_sha = "f00dface".to_string();
        let task = event_to_task(&state, GitHubEvent::PullRequestChanged(event))
            .await
            .expect("synchronize should create a review task");

        match task.kind {
            TaskKind::ReviewPullRequest { reason, .. } => {
                assert_eq!(reason, "synchronize");
            }
            other => panic!("expected ReviewPullRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_opened_is_ignored_when_lane_disabled() {
        let state = app_state();
        assert!(
            event_to_task(&state, GitHubEvent::PullRequestChanged(pr_event("opened"))).await.is_none()
        );
    }

    #[tokio::test]
    async fn familiar_authored_prs_are_never_auto_reviewed() {
        // The adapter's own draft PRs must not re-trigger reviews (loop guard).
        let state = app_state_with_review(review_on());
        let mut event = pr_event("opened");
        event.author_login = "coven-cody[bot]".to_string();
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event)).await.is_none());
    }

    #[tokio::test]
    async fn draft_prs_are_skipped_unless_policy_includes_them() {
        let state = app_state_with_review(review_on());
        let mut event = pr_event("opened");
        event.draft = true;
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event.clone())).await.is_none());

        let mut inclusive = review_on();
        inclusive.include_drafts = true;
        let state = app_state_with_review(inclusive);
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event)).await.is_some());
    }

    #[tokio::test]
    async fn per_repo_override_disables_the_lane() {
        let mut review = review_on();
        review.repos.insert(
            "OpenCoven/coven-code".to_string(),
            coven_github_config::RepoReviewOverride {
                pull_request: Some(false),
                include_drafts: None,
                familiar: None,
                audit_instruction: None,
                min_severity: None,
                publish: None,
            },
        );
        let state = app_state_with_review(review);
        assert!(
            event_to_task(&state, GitHubEvent::PullRequestChanged(pr_event("opened"))).await.is_none()
        );
    }

    #[tokio::test]
    async fn review_label_is_an_explicit_opt_in_even_with_lane_off_and_draft() {
        // No [review] policy at all — the label alone routes, like issue labels.
        let state = app_state();
        let mut event = pr_event("labeled");
        event.label_name = Some("coven:review".to_string());
        event.draft = true;
        let task = event_to_task(&state, GitHubEvent::PullRequestChanged(event))
            .await
            .expect("review label should opt the PR in");
        assert_eq!(task.familiar_id, "cody");
        assert!(matches!(task.kind, TaskKind::ReviewPullRequest { .. }));
    }

    #[tokio::test]
    async fn unknown_labels_do_not_trigger_review() {
        let state = app_state_with_review(review_on());
        let mut event = pr_event("labeled");
        event.label_name = Some("help wanted".to_string());
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event)).await.is_none());
    }

    #[tokio::test]
    async fn push_events_produce_no_task_until_contract_v3() {
        let state = app_state_with_review(review_on());
        let event = GitHubEvent::Push(coven_github_api::PushEvent {
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            branch: Some("main".to_string()),
            before_sha: "aaa".to_string(),
            after_sha: "bbb".to_string(),
            deleted: false,
            forced: false,
            pusher_login: "octocat".to_string(),
            commit_count: 2,
        });
        assert!(event_to_task(&state, event).await.is_none());
    }
}

#[cfg(test)]
mod command_routing_tests {
    use super::tests::app_state;
    use super::*;
    use coven_github_api::PrReviewCommentEvent;

    fn pr_comment(body: &str) -> GitHubEvent {
        GitHubEvent::PullRequestReviewComment(PrReviewCommentEvent {
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            pr_number: 88,
            pr_title: "Add spell compiler cache".to_string(),
            comment_body: body.to_string(),
            commenter_login: "octocat".to_string(),
        })
    }

    #[tokio::test]
    async fn review_command_on_pr_creates_commanded_review() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody review"))
            .await
            .expect("review command should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::ReviewPullRequest {
                pr_number, reason, ..
            } => {
                assert_eq!(pr_number, 88);
                assert_eq!(reason, "command:review");
            }
            other => panic!("expected ReviewPullRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deepen_command_carries_its_verb_in_the_reason() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody deepen"))
            .await
            .expect("deepen command should create a task");
        assert!(
            matches!(task.kind, TaskKind::ReviewPullRequest { ref reason, .. } if reason == "command:deepen")
        );
    }

    #[tokio::test]
    async fn cancel_command_rides_a_gated_task_not_a_route_side_mutation() {
        let state = app_state();
        let queued = Task {
            id: "task-queued".to_string(),
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "t".to_string(),
                reason: "opened".to_string(),
            },
        };
        super::tests::seed_task(&state, "dl-cancel", &queued).await;

        let task = event_to_task(&state, pr_comment("@coven-cody cancel"))
            .await
            .expect("cancel should route");

        // The route must NOT tombstone anything — the worker does, after the
        // commander's write access is verified (issue #13).
        let states = state.store.task_states().await.expect("states");
        assert_eq!(
            states,
            vec![("task-queued".to_string(), "queued".to_string())],
            "queued review must be untouched until the gate passes"
        );
        match task.kind {
            TaskKind::CancelReviews { pr_number } => assert_eq!(pr_number, 88),
            other => panic!("expected CancelReviews, got {other:?}"),
        }
        assert_eq!(task.commander.as_deref(), Some("octocat"));
    }

    #[tokio::test]
    async fn memory_commands_are_acknowledged_but_deferred() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody remember we ship Fridays"))
            .await
            .expect("remember should acknowledge");
        // The acknowledgement is gated: it carries the commander so the
        // worker verifies write access before replying (issue #13).
        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::CommandReply { body, .. } => {
                assert!(body.contains("#6"));
                assert!(body.contains("Nothing was stored"));
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_command_reports_tracked_tasks_for_the_surface() {
        let state = app_state();
        let tracked = Task {
            id: "task-1".to_string(),
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "Add spell compiler cache".to_string(),
                reason: "opened".to_string(),
            },
        };
        super::tests::seed_task(&state, "dl-status", &tracked).await;

        let task = event_to_task(&state, pr_comment("@coven-cody status"))
            .await
            .expect("status should reply");
        match task.kind {
            TaskKind::CommandReply { body, .. } => {
                assert!(body.contains("Review PR #88"), "status body: {body}");
                assert!(body.contains("queued"), "status body: {body}");
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn review_command_on_an_issue_is_clarified() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(coven_github_api::IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody review".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("review on an issue should clarify");
        assert!(matches!(
            task.kind,
            TaskKind::CommandReply { ref body, .. } if body.contains("needs a pull request")
        ));
    }
}

#[cfg(test)]
mod delivery_idempotency_tests {
    //! Route-level proof of the persist-then-ack contract (issue #2): the
    //! delivery record exists before GitHub hears success, and a redelivered
    //! delivery id never dispatches twice.
    use super::tests::app_state;
    use super::*;
    use axum::extract::State;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    const SECRET: &str = "secret"; // matches app_state's webhook_secret

    fn signed_headers(event: &str, delivery_id: Option<&str>, body: &str) -> HeaderMap {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("hmac key");
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", event.parse().expect("header"));
        headers.insert("x-hub-signature-256", sig.parse().expect("header"));
        if let Some(id) = delivery_id {
            headers.insert("x-github-delivery", id.parse().expect("header"));
        }
        headers
    }

    fn assigned_payload() -> String {
        serde_json::json!({
            "action": "assigned",
            "issue": { "number": 42, "title": "Fix auth", "body": "b" },
            "assignee": { "login": "coven-cody[bot]" },
            "repository": { "name": "demo", "owner": { "login": "OpenCoven" } },
            "installation": { "id": 7 }
        })
        .to_string()
    }

    async fn call(
        state: &AppState,
        headers: HeaderMap,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = handle_webhook(
            State(state.clone()),
            headers,
            Bytes::from(body.to_string()),
        )
        .await
        .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn missing_delivery_id_is_rejected_and_nothing_persists() {
        let state = app_state();
        let body = assigned_payload();
        let (status, json) = call(&state, signed_headers("issues", None, &body), &body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "missing delivery id");
        assert!(state.store.task_states().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn redelivered_delivery_id_never_dispatches_twice() {
        let state = app_state();
        let body = assigned_payload();

        let (status, json) =
            call(&state, signed_headers("issues", Some("dl-1"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["duplicate"], serde_json::Value::Null);

        // GitHub redelivers the same delivery id.
        let (status, json) =
            call(&state, signed_headers("issues", Some("dl-1"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["duplicate"], true);

        // Exactly one durable task row — the queue IS the table, so a second
        // row is what "dispatching twice" would mean.
        let states = state.store.task_states().await.unwrap();
        assert_eq!(states.len(), 1, "one durable task: {states:?}");
        assert_eq!(states[0].1, "queued");

        // The delivery record ties the id to the routed task.
        let routing = state.store.delivery_routing("dl-1").await.unwrap();
        assert_eq!(
            routing.as_deref(),
            Some(format!("task:{}", states[0].0).as_str())
        );
    }

    #[tokio::test]
    async fn ping_and_unroutable_deliveries_are_recorded_as_ignored() {
        let state = app_state();

        let ping = r#"{"zen":"Keep it logically awesome.","hook_id":1}"#;
        let (status, json) =
            call(&state, signed_headers("ping", Some("dl-ping"), ping), ping).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["pong"], true);
        assert_eq!(
            state.store.delivery_routing("dl-ping").await.unwrap().as_deref(),
            Some("ignored:ping")
        );

        // An event no familiar routes: recorded, acknowledged, no task.
        let body = serde_json::json!({
            "action": "assigned",
            "issue": { "number": 1, "title": "t", "body": "b" },
            "assignee": { "login": "someone-else" },
            "repository": { "name": "demo", "owner": { "login": "OpenCoven" } },
            "installation": { "id": 7 }
        })
        .to_string();
        let (status, _) =
            call(&state, signed_headers("issues", Some("dl-2"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state.store.delivery_routing("dl-2").await.unwrap().as_deref(),
            Some("ignored:unroutable")
        );
        assert!(state.store.task_states().await.unwrap().is_empty());
    }
}

#[cfg(test)]
mod tenancy_tests {
    //! The tenant boundary on the task API (issue #3): token mode fails
    //! closed, a tenant sees only its own installation, and every read is
    //! audited.
    use super::tests::{app_state, seed_task};
    use super::*;
    use axum::extract::State;
    use coven_github_config::TenantToken;
    use std::sync::Arc;

    fn token_state(api: ApiConfig) -> AppState {
        let base = app_state();
        let mut config = (*base.config).clone();
        config.api = api;
        AppState {
            config: Arc::new(config),
            ..base
        }
    }

    fn two_tenant_api() -> ApiConfig {
        ApiConfig {
            mode: ApiMode::Token,
            service_token: Some("service-token-0123456789abcdef".to_string()),
            tenants: vec![
                TenantToken {
                    token: "tenant-one-0123456789abcdef".to_string(),
                    installation_id: 1,
                    repos: vec![],
                },
                TenantToken {
                    token: "tenant-two-0123456789abcdef".to_string(),
                    installation_id: 2,
                    repos: vec![],
                },
            ],
        }
    }

    fn task_for(id: &str, installation_id: u64, repo_name: &str) -> Task {
        Task {
            id: id.to_string(),
            installation_id,
            repo_owner: "OpenCoven".to_string(),
            repo_name: repo_name.to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: format!("task {id}"),
                issue_body: "b".to_string(),
            },
        }
    }

    async fn list(state: &AppState, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert(
                "authorization",
                format!("Bearer {token}").parse().expect("header"),
            );
        }
        let response = list_tasks(State(state.clone()), headers)
            .await
            .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    fn repos_of(json: &serde_json::Value) -> Vec<String> {
        json["tasks"]
            .as_array()
            .expect("tasks array")
            .iter()
            .map(|t| t["repo"].as_str().expect("repo").to_string())
            .collect()
    }

    #[tokio::test]
    async fn open_mode_lists_without_auth_for_local_development() {
        let state = app_state();
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;
        let (status, json) = list(&state, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["tasks"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn token_mode_fails_closed_and_reveals_nothing() {
        let state = token_state(two_tenant_api());
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;

        for bearer in [None, Some("wrong-token-0123456789abcdef")] {
            let (status, json) = list(&state, bearer).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer: {bearer:?}");
            assert_eq!(json["error"], "unauthorized");
            assert!(json.get("tasks").is_none(), "no data may leak: {json}");
        }

        let audit = state.store.api_audit_entries().await.expect("audit");
        assert_eq!(audit.len(), 2);
        for (caller, scope, action, result) in audit {
            assert_eq!(caller, "anonymous");
            assert_eq!(scope, "none");
            assert_eq!(action, "list_tasks");
            assert_eq!(result, "denied");
        }
    }

    #[tokio::test]
    async fn tenant_token_sees_only_its_own_installation() {
        let state = token_state(two_tenant_api());
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;
        seed_task(&state, "d2", &task_for("t2", 2, "beta")).await;

        // Installation 1's token must never see installation 2's task.
        let (status, json) = list(&state, Some("tenant-one-0123456789abcdef")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(repos_of(&json), vec!["OpenCoven/alpha".to_string()]);

        let (_, json) = list(&state, Some("tenant-two-0123456789abcdef")).await;
        assert_eq!(repos_of(&json), vec!["OpenCoven/beta".to_string()]);

        // The operator-wide service token sees both.
        let (_, json) = list(&state, Some("service-token-0123456789abcdef")).await;
        assert_eq!(json["tasks"].as_array().unwrap().len(), 2);

        let audit = state.store.api_audit_entries().await.expect("audit");
        assert_eq!(
            audit
                .iter()
                .map(|(caller, scope, _, result)| (caller.as_str(), scope.as_str(), result.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("tenant:1", "installation:1", "ok:1"),
                ("tenant:2", "installation:2", "ok:1"),
                ("service", "all", "ok:2"),
            ]
        );
    }

    #[tokio::test]
    async fn tenant_repo_scope_narrows_within_the_installation() {
        let mut api = two_tenant_api();
        api.tenants[0].repos = vec!["OpenCoven/alpha".to_string()];
        let state = token_state(api);
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;
        seed_task(&state, "d2", &task_for("t2", 1, "gamma")).await;

        let (_, json) = list(&state, Some("tenant-one-0123456789abcdef")).await;
        assert_eq!(repos_of(&json), vec!["OpenCoven/alpha".to_string()]);
    }
}
