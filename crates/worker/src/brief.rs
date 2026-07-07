//! Session brief builder: converts a Task into the JSON context injected into coven-code.

use serde::{Deserialize, Serialize};
use std::path::Path;

use coven_github_api::{Task, TaskKind, HEADLESS_CONTRACT_VERSION};
use coven_github_config::FamiliarConfig;

fn deserialize_contract_version<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let version = String::deserialize(deserializer)?;
    if version != HEADLESS_CONTRACT_VERSION {
        return Err(serde::de::Error::custom(format!(
            "unsupported session brief contract_version {}; expected {}",
            version, HEADLESS_CONTRACT_VERSION
        )));
    }
    Ok(version)
}

/// The session-brief.json schema injected into coven-code --headless.
///
/// The brief is intentionally tokenless: the agent receives read context only.
/// Git authentication is injected out-of-band (env / GIT_ASKPASS) and GitHub
/// write authority (comments, Check Runs, branches, PRs) stays with the adapter
/// behind its publication gate. See issue #4.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBrief {
    /// Contract major version this brief is written against. See
    /// `docs/headless-contract.md`.
    #[serde(deserialize_with = "deserialize_contract_version")]
    pub contract_version: String,
    pub trigger: String,
    pub repo: RepoBrief,
    pub task: TaskBrief,
    pub familiar: FamiliarBrief,
    pub workspace: WorkspaceBrief,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_context: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_instruction: Option<String>,
    /// Hosted memory governance policy (issue #6). Present only when the
    /// installation has opted memory in for this repo; absent → memory off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_policy: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoBrief {
    pub owner: String,
    pub name: String,
    pub clone_url: String,
    pub default_branch: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct FamiliarBrief {
    pub id: String,
    pub display_name: String,
    pub model: Option<String>,
    pub skills: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBrief {
    pub root: String,
}

/// Hosted-review evidence the adapter injects for ReviewPullRequest tasks
/// (issue #10): the changed-file list plus an optional operator instruction.
pub struct ReviewContext {
    pub files: Vec<String>,
    pub audit_instruction: Option<String>,
}

/// Build a tokenless session brief from a Task and familiar config.
///
/// `default_branch` is resolved from live GitHub repository metadata rather than
/// assuming `main` (see issue #9). `review` carries hosted-review context for
/// [`TaskKind::ReviewPullRequest`] tasks and is ignored for other kinds.
pub fn build(
    task: &Task,
    familiar: &FamiliarConfig,
    workspace: &Path,
    default_branch: &str,
    review: Option<&ReviewContext>,
    memory_policy: Option<serde_json::Value>,
) -> SessionBrief {
    let trigger = match &task.kind {
        TaskKind::FixIssue { .. } => "issue_assigned",
        TaskKind::AddressReviewComment { .. } => "pr_review_comment",
        TaskKind::RespondToMention { .. } => "issue_mention",
        // Contract v2 locks the trigger enum to the three values above; the
        // adapter-initiated review lane rides on pr_review_comment plus
        // review_context until native pull_request/push triggers land in v3.
        TaskKind::ReviewPullRequest { .. } => "pr_review_comment",
        // CommandReply and CancelReviews are executed adapter-side before
        // briefing (issue #13); these arms are safe fallbacks, not expected
        // paths.
        TaskKind::CommandReply { .. } => "issue_mention",
        TaskKind::CancelReviews { .. } => "issue_mention",
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
        TaskKind::CommandReply { issue_number, body } => TaskBrief::RespondToMention {
            issue_number: *issue_number,
            comment_body: body.clone(),
        },
        TaskKind::CancelReviews { pr_number } => TaskBrief::RespondToMention {
            issue_number: *pr_number,
            comment_body: format!("Cancel queued reviews for PR #{pr_number}."),
        },
        TaskKind::ReviewPullRequest {
            pr_number,
            pr_title,
            reason,
            ..
        } => TaskBrief::AddressReviewComment {
            pr_number: *pr_number,
            comment_body: format!(
                "Hosted review requested for PR #{pr_number} (\"{pr_title}\", trigger: {reason}). \
                 Review the changed files listed in review_context and report findings through \
                 the structured review evidence. Do not modify code."
            ),
            diff_hunk: None,
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
        review_context: review.map(|r| {
            serde_json::json!({
                "kind": "pull_request",
                "files": r
                    .files
                    .iter()
                    .map(|f| serde_json::json!({ "filename": f }))
                    .collect::<Vec<_>>(),
            })
        }),
        audit_instruction: review.and_then(|r| r.audit_instruction.clone()),
        memory_policy,
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
            commander: None,
        }
    }

    fn review_task() -> Task {
        Task {
            id: "task-2".to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "Add spell compiler cache".to_string(),
                reason: "synchronize".to_string(),
            },
        }
    }

    #[test]
    fn brief_uses_resolved_default_branch_not_hardcoded_main() {
        let brief = build(&task(), &familiar(), Path::new("/tmp/ws"), "develop", None, None);
        assert_eq!(brief.repo.default_branch, "develop");
    }

    #[test]
    fn brief_omits_memory_policy_by_default_and_stamps_it_when_present() {
        // No policy → the field is absent (memory off, deny-by-default).
        let plain = build(&task(), &familiar(), Path::new("/tmp/ws"), "main", None, None);
        assert!(plain.memory_policy.is_none());
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("memory_policy"), "unexpected field: {json}");

        // A policy → stamped verbatim into the brief.
        let policy = serde_json::json!({ "enabled": true, "repo": "acme/billing" });
        let stamped = build(
            &task(),
            &familiar(),
            Path::new("/tmp/ws"),
            "main",
            None,
            Some(policy.clone()),
        );
        assert_eq!(stamped.memory_policy.as_ref(), Some(&policy));
    }

    #[test]
    fn review_task_briefs_as_contract_v2_review_comment_with_context() {
        let review = ReviewContext {
            files: vec!["src/cache.rs".to_string(), "src/lib.rs".to_string()],
            audit_instruction: Some("Focus on eviction correctness.".to_string()),
        };
        let brief = build(
            &review_task(),
            &familiar(),
            Path::new("/tmp/ws"),
            "main",
            Some(&review),
            None,
        );

        // Contract v2 locks trigger/task enums — the review lane must ride on
        // the sanctioned pr_review_comment + review_context vehicle.
        assert_eq!(brief.trigger, "pr_review_comment");
        match &brief.task {
            TaskBrief::AddressReviewComment {
                pr_number,
                comment_body,
                diff_hunk,
            } => {
                assert_eq!(*pr_number, 88);
                assert!(comment_body.contains("Hosted review requested for PR #88"));
                assert!(comment_body.contains("synchronize"));
                assert!(diff_hunk.is_none());
            }
            other => panic!("expected AddressReviewComment brief, got {other:?}"),
        }
        let ctx = brief
            .review_context
            .as_ref()
            .expect("review context should be set");
        assert_eq!(ctx["kind"], "pull_request");
        assert_eq!(ctx["files"][0]["filename"], "src/cache.rs");
        assert_eq!(ctx["files"][1]["filename"], "src/lib.rs");
        assert_eq!(
            brief.audit_instruction.as_deref(),
            Some("Focus on eviction correctness.")
        );

        // The no-credential invariant holds for review briefs too (issue #4).
        let json = serde_json::to_string(&brief).expect("brief should serialize");
        assert!(!json.contains("\"token\""), "brief leaked a token: {json}");
    }

    #[test]
    fn non_review_tasks_carry_no_review_context() {
        let brief = build(&task(), &familiar(), Path::new("/tmp/ws"), "main", None, None);
        assert!(brief.review_context.is_none());
        assert!(brief.audit_instruction.is_none());
    }

    #[test]
    fn brief_serialization_never_contains_token_or_auth_fields() {
        let brief = build(&task(), &familiar(), Path::new("/tmp/ws"), "main", None, None);
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
