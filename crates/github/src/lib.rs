//! GitHub API client: installation tokens, Check Runs, PRs, issue comments.

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod check_run;
pub mod installation;
pub mod pr;

/// Minimal GitHub event types parsed from webhooks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum GitHubEvent {
    IssueAssigned(IssueAssignedEvent),
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
