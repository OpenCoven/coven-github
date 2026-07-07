//! Cave-facing task projection types.
//!
//! Durable task state lives in `coven-github-store` (issue #2); this module
//! keeps the wire types the `/api/github/tasks` endpoint serves and the
//! shared mapping from a [`TaskKind`] to its conversation surface.

use serde::{Deserialize, Serialize};

use crate::TaskKind;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskListStatus {
    /// Accepted and durably recorded; not yet claimed by a worker.
    Queued,
    Running,
    Review,
    Done,
    Failed,
    /// The target moved while the task ran (e.g. a PR head advanced during a
    /// review) or a newer event replaced it; output withheld (issues #8/#10).
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListItem {
    pub id: String,
    pub repo: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub branch: Option<String>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub status: TaskListStatus,
    pub familiar_id: String,
    pub familiar_name: String,
    pub session_id: Option<String>,
    pub updated_at: String,
    pub check_run_url: Option<String>,
}

/// The issue/PR conversation a task surfaces on, with a human-readable title.
pub fn surface_of(kind: &TaskKind) -> (u64, String) {
    match kind {
        TaskKind::FixIssue {
            issue_number,
            issue_title,
            ..
        } => (*issue_number, issue_title.clone()),
        TaskKind::RespondToMention { issue_number, .. } => (
            *issue_number,
            format!("Respond to issue #{issue_number} mention"),
        ),
        TaskKind::AddressReviewComment { pr_number, .. } => {
            (*pr_number, format!("Address review on PR #{pr_number}"))
        }
        TaskKind::ReviewPullRequest {
            pr_number,
            pr_title,
            ..
        } => (*pr_number, format!("Review PR #{pr_number}: {pr_title}")),
        TaskKind::CommandReply { issue_number, .. } => {
            (*issue_number, format!("Reply on #{issue_number}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_of_names_every_task_kind() {
        let cases: Vec<(TaskKind, u64, &str)> = vec![
            (
                TaskKind::FixIssue {
                    issue_number: 42,
                    issue_title: "Fix auth".to_string(),
                    issue_body: "b".to_string(),
                },
                42,
                "Fix auth",
            ),
            (
                TaskKind::ReviewPullRequest {
                    pr_number: 88,
                    pr_title: "t".to_string(),
                    reason: "opened".to_string(),
                },
                88,
                "Review PR #88: t",
            ),
            (
                TaskKind::AddressReviewComment {
                    pr_number: 7,
                    comment_body: "c".to_string(),
                    diff_hunk: None,
                },
                7,
                "Address review on PR #7",
            ),
            (
                TaskKind::CommandReply {
                    issue_number: 3,
                    body: "b".to_string(),
                },
                3,
                "Reply on #3",
            ),
        ];
        for (kind, number, title) in cases {
            let (n, t) = surface_of(&kind);
            assert_eq!(n, number);
            assert_eq!(t, title);
        }
    }
}
