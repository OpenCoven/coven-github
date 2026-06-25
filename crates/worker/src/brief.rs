//! Session brief builder: converts a Task into the JSON context injected into coven-code.

use serde::{Deserialize, Serialize};
use std::path::Path;

use coven_github_api::{Task, TaskKind, HEADLESS_CONTRACT_VERSION};
use coven_github_config::FamiliarConfig;

fn default_contract_version() -> String {
    HEADLESS_CONTRACT_VERSION.to_string()
}

/// The session-brief.json schema injected into coven-code --headless.
///
/// The brief is intentionally tokenless: the agent receives read context only.
/// Git authentication is injected out-of-band (env / GIT_ASKPASS) and GitHub
/// write authority (comments, Check Runs, branches, PRs) stays with the adapter
/// behind its publication gate. See issue #4.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionBrief {
    /// Contract major version this brief is written against. See
    /// `docs/headless-contract.md`.
    #[serde(default = "default_contract_version")]
    pub contract_version: String,
    pub trigger: String,
    pub repo: RepoBrief,
    pub task: TaskBrief,
    pub familiar: FamiliarBrief,
    pub workspace: WorkspaceBrief,
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

/// Build a tokenless session brief from a Task and familiar config.
///
/// `default_branch` is resolved from live GitHub repository metadata rather than
/// assuming `main` (see issue #9).
pub fn build(
    task: &Task,
    familiar: &FamiliarConfig,
    workspace: &Path,
    default_branch: &str,
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
        contract_version: HEADLESS_CONTRACT_VERSION.to_string(),
        trigger: trigger.to_string(),
        repo: RepoBrief {
            owner: task.repo_owner.clone(),
            name: task.repo_name.clone(),
            // No embedded credentials: git auth is injected out-of-band.
            clone_url: format!(
                "https://github.com/{}/{}.git",
                task.repo_owner, task.repo_name
            ),
            default_branch: default_branch.to_string(),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{Task, TaskKind};

    fn familiar() -> FamiliarConfig {
        FamiliarConfig {
            id: "cody".to_string(),
            display_name: "Cody".to_string(),
            bot_username: "coven-cody[bot]".to_string(),
            model: None,
            skills: vec![],
            trigger_labels: vec![],
        }
    }

    fn task() -> Task {
        Task {
            id: "task-1".to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
            },
        }
    }

    #[test]
    fn brief_uses_resolved_default_branch_not_hardcoded_main() {
        let brief = build(&task(), &familiar(), Path::new("/tmp/ws"), "develop");
        assert_eq!(brief.repo.default_branch, "develop");
    }

    #[test]
    fn brief_serialization_never_contains_token_or_auth_fields() {
        let brief = build(&task(), &familiar(), Path::new("/tmp/ws"), "main");
        let value = serde_json::to_value(&brief).expect("brief should serialize");
        let json = serde_json::to_string(&brief).expect("brief should serialize");

        // The brief must never carry credentials (issue #4). Check for the
        // serialized field keys rather than raw substrings, since free-text
        // task content may legitimately mention "auth"/"token".
        assert!(
            value.get("auth").is_none(),
            "brief leaked an auth field: {json}"
        );
        assert!(
            !json.contains("\"token\""),
            "brief leaked a token field: {json}"
        );
        assert!(
            !json.contains("x-access-token"),
            "clone_url leaked an embedded credential: {json}"
        );
    }
}
