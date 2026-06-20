//! GitHub API client: installation tokens, Check Runs, PRs, issue comments.

use serde::{Deserialize, Serialize};

pub mod check_run;
pub mod installation;
pub mod pr;
pub mod tasks;

pub const DEFAULT_API_BASE_URL: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2026-03-10";

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubRequest {
    method: &'static str,
    path: String,
    body: serde_json::Value,
}

fn api_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("coven-github/0.1")
        .build()
        .map_err(Into::into)
}

async fn send_json(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    request: GitHubRequest,
) -> anyhow::Result<reqwest::Response> {
    let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
    let response = client
        .request(method, api_url(base_url, &request.path))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .json(&request.body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API request failed with {status}: {body}");
    }

    Ok(response)
}

/// Minimal GitHub event types parsed from webhooks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum GitHubEvent {
    IssueAssigned(IssueAssignedEvent),
    IssueLabeled(IssueLabeledEvent),
    IssueComment(IssueCommentEvent),
    PullRequestReviewComment(PrReviewCommentEvent),
    Unsupported { name: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueAssignedEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_body: String,
    pub assignee_login: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueLabeledEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_body: String,
    pub label_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueCommentEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub issue_number: u64,
    pub comment_body: String,
    pub commenter_login: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrReviewCommentEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub comment_body: String,
    pub commenter_login: String,
}

/// A task dispatched to the worker queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub kind: TaskKind,
    pub familiar_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    FixIssue {
        issue_number: u64,
        issue_title: String,
        issue_body: String,
    },
    AddressReviewComment {
        pr_number: u64,
        comment_body: String,
        diff_hunk: Option<String>,
    },
    RespondToMention {
        issue_number: u64,
        comment_body: String,
    },
}

/// Structured result envelope written by coven-code --headless.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionResult {
    pub status: SessionStatus,
    pub branch: Option<String>,
    pub commits: Vec<CommitInfo>,
    pub files_changed: Vec<String>,
    pub summary: String,
    pub pr_body: String,
    pub exit_reason: Option<ExitReason>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Success,
    Failure,
    Partial,
    NeedsInput,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TestFailure,
    AmbiguousSpec,
    GitConflict,
    InfraError,
}
