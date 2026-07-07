//! Axum route handlers for the webhook endpoint.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use tracing::{info, warn};

use coven_github_api::{tasks::TaskStore, GitHubEvent, Task, TaskKind};
use coven_github_config::Config;

use crate::{
    commands::{parse_mention, Command, MentionKind, COMMAND_LIST},
    events::{parse_event, WebhookPayload},
    verify_signature,
};
use coven_github_api::tasks::TaskListStatus;

/// Shared application state passed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: std::sync::Arc<Config>,
    /// Channel for dispatching tasks to the worker pool.
    pub task_tx: tokio::sync::mpsc::Sender<Task>,
    pub task_store: TaskStore,
}

/// GET /api/github/tasks — current task state for CovenCave polling.
pub async fn list_tasks(State(state): State<AppState>) -> impl IntoResponse {
    let tasks = state.task_store.list().await;
    Json(json!({ "ok": true, "tasks": tasks })).into_response()
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

    // 2. Parse event type header.
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

    // `ping` is GitHub's webhook-configuration handshake — acknowledge it
    // explicitly so operators get a clear signal the endpoint is wired up.
    if matches!(event, GitHubEvent::Ping) {
        info!("webhook ping received — endpoint configured");
        return (StatusCode::OK, Json(json!({"ok": true, "pong": true}))).into_response();
    }

    // 4. Route event → task.
    if let Some(task) = event_to_task(&state, event).await {
        let task_id = task.id.clone();
        // Register auto-reviews BEFORE enqueueing so the worker can never
        // dequeue a task that a newer event has already superseded (#10).
        if let TaskKind::ReviewPullRequest { pr_number, .. } = &task.kind {
            let repo = format!("{}/{}", task.repo_owner, task.repo_name);
            state
                .task_store
                .register_pr_review(&repo, *pr_number, &task_id)
                .await;
        }
        if state.task_tx.try_send(task).is_err() {
            warn!(task_id, "task queue full — dropping task");
        } else {
            info!(task_id, "task enqueued");
        }
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
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

/// Maps a typed maintainer command to a task. Work commands carry the
/// commander for the worker's permission gate; replies carry none.
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
        Command::Cancel if s.on_pull_request => {
            // Tombstone queued reviews for this PR. In-flight work is not
            // interrupted (documented limitation); the next review command or
            // PR event re-arms the lane.
            state
                .task_store
                .register_pr_review(&repo, s.number, &format!("cancelled:{}", uuid::Uuid::new_v4()))
                .await;
            reply(format!(
                "Cancelled queued reviews for PR #{}. Work already running will finish; `@{} review` re-arms the lane.",
                s.number,
                familiar.bot_username.trim_end_matches("[bot]")
            ))
        }
        Command::Cancel => reply("`cancel` currently applies to queued pull-request reviews only.".to_string()),
        Command::Remember { .. } | Command::Forget { .. } => reply(
            "Noted, but memory persistence is not wired up yet — it lands with the hosted \
             memory governance contract (#6). Nothing was stored or deleted."
                .to_string(),
        ),
        Command::Status => {
            let items = state.task_store.list().await;
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
        let (task_tx, _task_rx) = tokio::sync::mpsc::channel(1);
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
            }),
            task_tx,
            task_store: TaskStore::default(),
        }
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
    async fn cancel_command_tombstones_queued_reviews_and_acknowledges() {
        let state = app_state();
        state
            .task_store
            .register_pr_review("OpenCoven/coven-code", 88, "task-queued")
            .await;

        let task = event_to_task(&state, pr_comment("@coven-cody cancel"))
            .await
            .expect("cancel should acknowledge");

        assert!(
            !state
                .task_store
                .is_current_pr_review("OpenCoven/coven-code", 88, "task-queued")
                .await,
            "queued review must be superseded by the cancel tombstone"
        );
        match task.kind {
            TaskKind::CommandReply { issue_number, body } => {
                assert_eq!(issue_number, 88);
                assert!(body.contains("Cancelled queued reviews"));
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_commands_are_acknowledged_but_deferred() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody remember we ship Fridays"))
            .await
            .expect("remember should acknowledge");
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
        state.task_store.mark_running(&tracked, "Cody", None).await;

        let task = event_to_task(&state, pr_comment("@coven-cody status"))
            .await
            .expect("status should reply");
        match task.kind {
            TaskKind::CommandReply { body, .. } => {
                assert!(body.contains("Review PR #88"), "status body: {body}");
                assert!(body.contains("running"), "status body: {body}");
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
