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
    events::{parse_event, WebhookPayload},
    verify_signature,
};

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
    if let Some(task) = event_to_task(&state, event) {
        let task_id = task.id.clone();
        if state.task_tx.try_send(task).is_err() {
            warn!(task_id, "task queue full — dropping task");
        } else {
            info!(task_id, "task enqueued");
        }
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

/// Maps a parsed event to a worker task, or returns None if not actionable.
fn event_to_task(state: &AppState, event: GitHubEvent) -> Option<Task> {
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
                kind: TaskKind::FixIssue {
                    issue_number: e.issue_number,
                    issue_title: e.issue_title,
                    issue_body: e.issue_body,
                },
            })
        }

        GitHubEvent::IssueComment(e) => {
            // Find a familiar mentioned in the comment body (skip the bot's own
            // comments to avoid self-trigger loops).
            let familiar = state.config.familiars.iter().find(|f| {
                e.commenter_login != f.bot_username && mentions(&e.comment_body, &f.bot_username)
            })?;

            // GitHub delivers PR conversation comments through `issue_comment`.
            // Route those through PR iteration so the familiar gets PR context
            // rather than issue context (the numbers coincide in GitHub).
            let kind = if e.on_pull_request {
                TaskKind::AddressReviewComment {
                    pr_number: e.issue_number,
                    comment_body: e.comment_body,
                    diff_hunk: None,
                }
            } else {
                TaskKind::RespondToMention {
                    issue_number: e.issue_number,
                    comment_body: e.comment_body,
                }
            };

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                kind,
            })
        }

        GitHubEvent::PullRequestReview(e) => {
            // A submitted review carries a summary body and a verdict. Trigger
            // on an explicit mention in the body, same as inline comments.
            let familiar = state.config.familiars.iter().find(|f| {
                e.reviewer_login != f.bot_username && mentions(&e.review_body, &f.bot_username)
            })?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                kind: TaskKind::AddressReviewComment {
                    pr_number: e.pr_number,
                    comment_body: e.review_body,
                    diff_hunk: None,
                },
            })
        }

        GitHubEvent::PullRequestReviewComment(e) => {
            let familiar = state.config.familiars.iter().find(|f| {
                e.commenter_login != f.bot_username && mentions(&e.comment_body, &f.bot_username)
            })?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                kind: TaskKind::AddressReviewComment {
                    pr_number: e.pr_number,
                    comment_body: e.comment_body,
                    diff_hunk: None,
                },
            })
        }

        // Routed through the review policy in the next change (issue #10).
        GitHubEvent::PullRequestChanged(_) | GitHubEvent::Push(_) => None,

        // `ping` is acknowledged at the handler; it never produces a task.
        GitHubEvent::Ping | GitHubEvent::Unsupported { .. } => None,
    }
}

/// Returns true if `body` mentions the familiar's `@handle` as a whole token.
///
/// `bot_username` is the GitHub App bot login (e.g. `coven-cody[bot]`); the
/// `[bot]` suffix is dropped since mentions are written `@coven-cody`. Matching
/// is boundary-aware: `@cody` inside `@codyx`, `@cody-2`, or `email@cody` does
/// not count, and `@coven-cody/team` (a team mention) is not a bot mention.
fn mentions(body: &str, bot_username: &str) -> bool {
    let handle = bot_username.trim_end_matches("[bot]");
    if handle.is_empty() {
        return false;
    }
    let needle = format!("@{handle}");
    let mut offset = 0;
    while let Some(pos) = body[offset..].find(&needle) {
        let start = offset + pos;
        let end = start + needle.len();
        let before = body[..start].chars().next_back();
        let after = body[end..].chars().next();
        // The character before `@` must be a separator (or start of string),
        // and the character after the handle must not continue an identifier.
        let boundary_before = before.is_none_or(|c| !c.is_alphanumeric() && c != '@');
        let boundary_after =
            after.is_none_or(|c| !(c.is_alphanumeric() || c == '-' || c == '_' || c == '/'));
        if boundary_before && boundary_after {
            return true;
        }
        offset = start + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{IssueCommentEvent, IssueLabeledEvent, TaskKind};
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::{path::PathBuf, sync::Arc};

    fn app_state() -> AppState {
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
                    trigger_labels: vec!["coven:fix".to_string()],
                }],
                review: coven_github_config::ReviewConfig::default(),
            }),
            task_tx,
            task_store: TaskStore::default(),
        }
    }

    #[test]
    fn labeled_issue_routes_to_familiar_trigger_label() {
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

    #[test]
    fn labeled_issue_ignores_unknown_labels() {
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
        );

        assert!(task.is_none());
    }

    #[test]
    fn issue_comment_ignores_bot_self_mentions() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                comment_body: "@coven-cody thanks, I opened a PR.".to_string(),
                commenter_login: "coven-cody[bot]".to_string(),
                on_pull_request: false,
            }),
        );

        assert!(task.is_none());
    }

    #[test]
    fn pr_review_comment_ignores_bot_self_mentions() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReviewComment(coven_github_api::PrReviewCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 7,
                comment_body: "@coven-cody please fix.".to_string(),
                commenter_login: "coven-cody[bot]".to_string(),
            }),
        );

        assert!(task.is_none());
    }

    #[test]
    fn issue_comment_on_pr_routes_to_pr_iteration() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 73,
                comment_body: "@coven-cody the lint is still failing".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: true,
            }),
        )
        .expect("a mention on a PR comment should create a task");

        match task.kind {
            TaskKind::AddressReviewComment {
                pr_number,
                diff_hunk,
                ..
            } => {
                assert_eq!(pr_number, 73);
                assert!(diff_hunk.is_none());
            }
            other => panic!("expected AddressReviewComment for a PR comment, got {other:?}"),
        }
    }

    #[test]
    fn issue_comment_on_issue_routes_to_respond_to_mention() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                comment_body: "@coven-cody can you take a look?".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .expect("a mention on an issue comment should create a task");

        match task.kind {
            TaskKind::RespondToMention { issue_number, .. } => assert_eq!(issue_number, 42),
            other => panic!("expected RespondToMention for an issue comment, got {other:?}"),
        }
    }

    #[test]
    fn submitted_review_mention_routes_to_pr_iteration() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReview(coven_github_api::PrReviewEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 73,
                review_body: "@coven-cody please add test coverage before merge.".to_string(),
                review_state: "changes_requested".to_string(),
                reviewer_login: "octocat".to_string(),
            }),
        )
        .expect("a mention in a review body should create a task");

        match task.kind {
            TaskKind::AddressReviewComment { pr_number, .. } => assert_eq!(pr_number, 73),
            other => panic!("expected AddressReviewComment for a review, got {other:?}"),
        }
    }

    #[test]
    fn submitted_review_without_mention_is_ignored() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReview(coven_github_api::PrReviewEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 73,
                review_body: "LGTM, nice work!".to_string(),
                review_state: "approved".to_string(),
                reviewer_login: "octocat".to_string(),
            }),
        );

        assert!(task.is_none());
    }

    #[test]
    fn ping_event_produces_no_task() {
        let state = app_state();
        assert!(event_to_task(&state, GitHubEvent::Ping).is_none());
    }

    #[test]
    fn mention_matching_is_boundary_aware() {
        // Exact handle (with the [bot] suffix stripped) matches.
        assert!(mentions("hey @coven-cody can you help", "coven-cody[bot]"));
        // Trailing identifier characters must not be swallowed into the handle.
        assert!(!mentions("ping @coven-codyx instead", "coven-cody[bot]"));
        assert!(!mentions("ping @coven-cody-2 instead", "coven-cody[bot]"));
        // A team mention (`@org/team`) is not a bot mention.
        assert!(!mentions("cc @coven-cody/maintainers", "coven-cody[bot]"));
        // An email-like local part is not a mention.
        assert!(!mentions("mail user@coven-cody.example", "coven-cody[bot]"));
        // Mention at end of string still matches.
        assert!(mentions("over to you @coven-cody", "coven-cody[bot]"));
    }
}
