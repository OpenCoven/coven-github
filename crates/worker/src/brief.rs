//! Session brief builder: converts a Task into the JSON context injected into coven-code.

use serde::{Deserialize, Serialize};
use std::path::Path;

use coven_github_api::{Task, TaskKind};
use coven_github_config::FamiliarConfig;

/// The session-brief.json schema injected into coven-code --headless.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionBrief {
    pub trigger: String,
    pub repo: RepoBrief,
    pub task: TaskBrief,
    pub familiar: FamiliarBrief,
    pub workspace: WorkspaceBrief,
    pub auth: AuthBrief,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RepoBrief {
    pub owner: String,
    pub name: String,
    pub clone_url: String,
    pub default_branch: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskBrief {
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

#[derive(Debug, Serialize, Deserialize)]
pub struct FamiliarBrief {
    pub id: String,
    pub display_name: String,
    pub model: Option<String>,
    pub skills: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceBrief {
    pub root: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthBrief {
    pub token: String,
}

/// Build a session brief from a Task and familiar config.
pub fn build(
    task: &Task,
    familiar: &FamiliarConfig,
    token: &str,
    workspace: &Path,
) -> SessionBrief {
    let trigger = match &task.kind {
        TaskKind::FixIssue { .. } => "issue_assigned",
        TaskKind::AddressReviewComment { .. } => "pr_review_comment",
        TaskKind::RespondToMention { .. } => "issue_mention",
    };

    let task_brief = match &task.kind {
        TaskKind::FixIssue {
            issue_number,
            issue_title,
            issue_body,
        } => TaskBrief::FixIssue {
            issue_number: *issue_number,
            issue_title: issue_title.clone(),
            issue_body: issue_body.clone(),
        },
        TaskKind::AddressReviewComment {
            pr_number,
            comment_body,
            diff_hunk,
        } => TaskBrief::AddressReviewComment {
            pr_number: *pr_number,
            comment_body: comment_body.clone(),
            diff_hunk: diff_hunk.clone(),
        },
        TaskKind::RespondToMention {
            issue_number,
            comment_body,
        } => TaskBrief::RespondToMention {
            issue_number: *issue_number,
            comment_body: comment_body.clone(),
        },
    };

    SessionBrief {
        trigger: trigger.to_string(),
        repo: RepoBrief {
            owner: task.repo_owner.clone(),
            name: task.repo_name.clone(),
            clone_url: format!(
                "https://x-access-token:{}@github.com/{}/{}.git",
                token, task.repo_owner, task.repo_name
            ),
            default_branch: "main".to_string(),
        },
        task: task_brief,
        familiar: FamiliarBrief {
            id: familiar.id.clone(),
            display_name: familiar.display_name.clone(),
            model: familiar.model.clone(),
            skills: familiar.skills.clone(),
        },
        workspace: WorkspaceBrief {
            root: workspace.to_string_lossy().to_string(),
        },
        auth: AuthBrief {
            token: token.to_string(),
        },
    }
}
