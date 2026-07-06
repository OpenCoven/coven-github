use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::{SessionResult, SessionStatus, Task, TaskKind};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskListStatus {
    Running,
    Review,
    Done,
    Failed,
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

#[derive(Debug, Clone, Default)]
pub struct TaskStore {
    inner: Arc<RwLock<HashMap<String, TaskListItem>>>,
    /// Latest auto-review task per "owner/repo#pr". Newer PR events supersede
    /// queued reviews for the same PR (issue #10): the webhook registers the
    /// newest task id before enqueueing, and the worker consults this at
    /// dequeue and silently skips stale entries.
    review_heads: Arc<RwLock<HashMap<String, String>>>,
}

impl TaskStore {
    pub async fn register_pr_review(&self, repo: &str, pr_number: u64, task_id: &str) {
        self.review_heads
            .write()
            .await
            .insert(format!("{repo}#{pr_number}"), task_id.to_string());
    }

    /// True when `task_id` is still the newest registered review for the PR.
    /// Unregistered tasks are current by definition (e.g. after a restart).
    pub async fn is_current_pr_review(&self, repo: &str, pr_number: u64, task_id: &str) -> bool {
        self.review_heads
            .read()
            .await
            .get(&format!("{repo}#{pr_number}"))
            .is_none_or(|current| current == task_id)
    }

    pub async fn mark_running(
        &self,
        task: &Task,
        familiar_name: &str,
        check_run_url: Option<String>,
    ) {
        let mut items = self.inner.write().await;
        let item = items
            .entry(task.id.clone())
            .or_insert_with(|| task_list_item(task, familiar_name));
        item.status = TaskListStatus::Running;
        item.check_run_url = check_run_url;
        item.updated_at = now_rfc3339();
    }

    pub async fn mark_complete(
        &self,
        task_id: &str,
        repo: &str,
        result: &SessionResult,
        pr_number: Option<u64>,
    ) {
        let mut items = self.inner.write().await;
        if let Some(item) = items.get_mut(task_id) {
            item.branch = result.branch.clone();
            item.pr_number = pr_number;
            item.pr_url =
                pr_number.map(|number| format!("https://github.com/{repo}/pull/{number}"));
            item.status = match result.status {
                SessionStatus::Success if pr_number.is_some() => TaskListStatus::Review,
                SessionStatus::Success | SessionStatus::Partial => TaskListStatus::Done,
                SessionStatus::NeedsInput => TaskListStatus::Review,
                SessionStatus::Failure => TaskListStatus::Failed,
            };
            item.updated_at = now_rfc3339();
        }
    }

    pub async fn mark_failed(&self, task_id: &str) {
        let mut items = self.inner.write().await;
        if let Some(item) = items.get_mut(task_id) {
            item.status = TaskListStatus::Failed;
            item.updated_at = now_rfc3339();
        }
    }

    /// Records a task as failed, inserting it if it was never marked running.
    ///
    /// Used for pre-flight failures (token, ref resolution, Check Run creation)
    /// that happen before [`mark_running`](Self::mark_running), so the task is
    /// still visible in Cave as failed rather than vanishing silently.
    pub async fn register_failed(&self, task: &Task, familiar_name: &str) {
        let mut items = self.inner.write().await;
        let item = items
            .entry(task.id.clone())
            .or_insert_with(|| task_list_item(task, familiar_name));
        item.status = TaskListStatus::Failed;
        item.updated_at = now_rfc3339();
    }

    pub async fn list(&self) -> Vec<TaskListItem> {
        let mut items: Vec<_> = self.inner.read().await.values().cloned().collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items
    }
}

fn task_list_item(task: &Task, familiar_name: &str) -> TaskListItem {
    let (issue_number, issue_title) = match &task.kind {
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
    };

    TaskListItem {
        id: task.id.clone(),
        repo: format!("{}/{}", task.repo_owner, task.repo_name),
        issue_number,
        issue_title,
        branch: None,
        pr_number: None,
        pr_url: None,
        status: TaskListStatus::Running,
        familiar_id: task.familiar_id.clone(),
        familiar_name: familiar_name.to_string(),
        session_id: Some(task.id.clone()),
        updated_at: now_rfc3339(),
        check_run_url: None,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn task_store_tracks_running_and_review_state() {
        let store = TaskStore::default();
        let task = task();

        store
            .mark_running(
                &task,
                "Cody",
                Some("https://github.com/OpenCoven/coven-code/runs/7".to_string()),
            )
            .await;
        let running = store.list().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].status, TaskListStatus::Running);
        assert_eq!(running[0].repo, "OpenCoven/coven-code");
        assert_eq!(running[0].issue_number, 42);

        store
            .mark_complete(
                "task-1",
                "OpenCoven/coven-code",
                &SessionResult {
                    contract_version: crate::HEADLESS_CONTRACT_VERSION.to_string(),
                    status: SessionStatus::Success,
                    branch: Some("cody/fix-auth".to_string()),
                    commits: vec![],
                    files_changed: vec![],
                    summary: "Done".to_string(),
                    pr_body: "Body".to_string(),
                    review: crate::ReviewResult::none(),
                    exit_reason: None,
                },
                Some(9),
            )
            .await;

        let review = store.list().await;
        assert_eq!(review[0].status, TaskListStatus::Review);
        assert_eq!(review[0].pr_number, Some(9));
        assert_eq!(
            review[0].pr_url.as_deref(),
            Some("https://github.com/OpenCoven/coven-code/pull/9")
        );
    }

    #[tokio::test]
    async fn newer_pr_review_registration_supersedes_older_tasks() {
        let store = TaskStore::default();

        // Unregistered tasks are current (e.g. adapter restarted mid-queue).
        assert!(
            store
                .is_current_pr_review("OpenCoven/coven-code", 88, "task-a")
                .await
        );

        store
            .register_pr_review("OpenCoven/coven-code", 88, "task-a")
            .await;
        store
            .register_pr_review("OpenCoven/coven-code", 88, "task-b")
            .await;

        assert!(
            !store
                .is_current_pr_review("OpenCoven/coven-code", 88, "task-a")
                .await,
            "older queued review must be superseded"
        );
        assert!(
            store
                .is_current_pr_review("OpenCoven/coven-code", 88, "task-b")
                .await
        );
        // A different PR in the same repo is unaffected.
        assert!(
            store
                .is_current_pr_review("OpenCoven/coven-code", 89, "task-a")
                .await
        );
    }

    #[tokio::test]
    async fn register_failed_inserts_a_failed_task_when_never_running() {
        // A pre-flight failure (token / ref resolution / Check Run creation)
        // happens before mark_running, so the task is not yet in the store.
        let store = TaskStore::default();
        store.register_failed(&task(), "Cody").await;

        let items = store.list().await;
        assert_eq!(items.len(), 1, "pre-flight failure must still be visible");
        assert_eq!(items[0].status, TaskListStatus::Failed);
        assert_eq!(items[0].issue_number, 42);
        assert_eq!(items[0].familiar_name, "Cody");
    }
}
