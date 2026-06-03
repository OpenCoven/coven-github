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

        GitHubEvent::IssueComment(e) => {
            // Find a familiar mentioned in the comment body.
            let familiar = state.config.familiars.iter().find(|f| {
                e.comment_body
                    .contains(&format!("@{}", f.bot_username.trim_end_matches("[bot]")))
            })?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                kind: TaskKind::RespondToMention {
                    issue_number: e.issue_number,
                    comment_body: e.comment_body,
                },
            })
        }

        GitHubEvent::PullRequestReviewComment(e) => {
            let familiar = state.config.familiars.iter().find(|f| {
                e.comment_body
                    .contains(&format!("@{}", f.bot_username.trim_end_matches("[bot]")))
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

        GitHubEvent::Unsupported { .. } => None,
    }
}
