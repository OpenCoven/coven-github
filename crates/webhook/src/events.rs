//! Webhook event parsing: GitHub payload → typed events.

use coven_github_api::{
    GitHubEvent, IssueAssignedEvent, IssueCommentEvent, IssueLabeledEvent, PrChangedEvent,
    PrReviewCommentEvent, PrReviewEvent, PushEvent,
};
use serde::Deserialize;

/// Raw GitHub webhook payload (partial — we only pull what we need).
#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub action: Option<String>,
    pub installation: Option<Installation>,
    pub repository: Option<Repository>,
    pub issue: Option<Issue>,
    pub comment: Option<Comment>,
    pub review: Option<Review>,
    pub label: Option<Label>,
    pub assignee: Option<User>,
    pub pull_request: Option<PullRequest>,
    /// push: the updated ref, e.g. "refs/heads/main".
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    /// push: SHA of the ref before/after the push.
    pub before: Option<String>,
    pub after: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub forced: bool,
    #[serde(default)]
    pub commits: Vec<PushCommit>,
    pub sender: Option<User>,
}

#[derive(Debug, Deserialize)]
pub struct Installation {
    pub id: u64,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub name: String,
    pub owner: Owner,
}

#[derive(Debug, Deserialize)]
pub struct Owner {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    /// Present (non-null) only when the "issue" is actually a pull request.
    /// GitHub delivers PR conversation comments via the `issue_comment` event.
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub body: String,
    pub user: User,
}

#[derive(Debug, Deserialize)]
pub struct Review {
    /// A review can be submitted with no summary body (e.g. a bare approval).
    pub body: Option<String>,
    pub user: User,
    /// `approved`, `changes_requested`, or `commented`.
    pub state: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    /// Full PR metadata is present on `pull_request` events; review/comment
    /// events may omit it, so everything past `number` is optional.
    pub title: Option<String>,
    #[serde(default)]
    pub draft: bool,
    pub head: Option<GitRef>,
    pub base: Option<GitRef>,
    pub user: Option<User>,
}

#[derive(Debug, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub sha: String,
}

#[derive(Debug, Deserialize)]
pub struct PushCommit {
    pub id: String,
}

/// Parse a raw webhook into a typed `GitHubEvent`.
pub fn parse_event(event_type: &str, payload: &WebhookPayload) -> GitHubEvent {
    match event_type {
        "issues" if payload.action.as_deref() == Some("assigned") => {
            if let (Some(inst), Some(repo), Some(issue), Some(assignee)) = (
                &payload.installation,
                &payload.repository,
                &payload.issue,
                &payload.assignee,
            ) {
                return GitHubEvent::IssueAssigned(IssueAssignedEvent {
                    installation_id: inst.id,
                    repo_owner: repo.owner.login.clone(),
                    repo_name: repo.name.clone(),
                    issue_number: issue.number,
                    issue_title: issue.title.clone(),
                    issue_body: issue.body.clone().unwrap_or_default(),
                    assignee_login: assignee.login.clone(),
                });
            }
        }
        "issues" if payload.action.as_deref() == Some("labeled") => {
            if let (Some(inst), Some(repo), Some(issue), Some(label)) = (
                &payload.installation,
                &payload.repository,
                &payload.issue,
                &payload.label,
            ) {
                return GitHubEvent::IssueLabeled(IssueLabeledEvent {
                    installation_id: inst.id,
                    repo_owner: repo.owner.login.clone(),
                    repo_name: repo.name.clone(),
                    issue_number: issue.number,
                    issue_title: issue.title.clone(),
                    issue_body: issue.body.clone().unwrap_or_default(),
                    label_name: label.name.clone(),
                });
            }
        }
        "issue_comment" if payload.action.as_deref() == Some("created") => {
            if let (Some(inst), Some(repo), Some(issue), Some(comment)) = (
                &payload.installation,
                &payload.repository,
                &payload.issue,
                &payload.comment,
            ) {
                return GitHubEvent::IssueComment(IssueCommentEvent {
                    installation_id: inst.id,
                    repo_owner: repo.owner.login.clone(),
                    repo_name: repo.name.clone(),
                    issue_number: issue.number,
                    issue_title: issue.title.clone(),
                    issue_body: issue.body.clone().unwrap_or_default(),
                    comment_body: comment.body.clone(),
                    commenter_login: comment.user.login.clone(),
                    on_pull_request: issue.pull_request.is_some(),
                });
            }
        }
        "pull_request_review" if payload.action.as_deref() == Some("submitted") => {
            if let (Some(inst), Some(repo), Some(pr), Some(review)) = (
                &payload.installation,
                &payload.repository,
                &payload.pull_request,
                &payload.review,
            ) {
                return GitHubEvent::PullRequestReview(PrReviewEvent {
                    installation_id: inst.id,
                    repo_owner: repo.owner.login.clone(),
                    repo_name: repo.name.clone(),
                    pr_number: pr.number,
                    pr_title: pr.title.clone().unwrap_or_default(),
                    review_body: review.body.clone().unwrap_or_default(),
                    review_state: review.state.clone().unwrap_or_default(),
                    reviewer_login: review.user.login.clone(),
                });
            }
        }
        "pull_request_review_comment" if payload.action.as_deref() == Some("created") => {
            if let (Some(inst), Some(repo), Some(pr), Some(comment)) = (
                &payload.installation,
                &payload.repository,
                &payload.pull_request,
                &payload.comment,
            ) {
                return GitHubEvent::PullRequestReviewComment(PrReviewCommentEvent {
                    installation_id: inst.id,
                    repo_owner: repo.owner.login.clone(),
                    repo_name: repo.name.clone(),
                    pr_number: pr.number,
                    pr_title: pr.title.clone().unwrap_or_default(),
                    comment_body: comment.body.clone(),
                    commenter_login: comment.user.login.clone(),
                });
            }
        }
        "pull_request"
            if matches!(
                payload.action.as_deref(),
                Some("opened" | "synchronize" | "reopened" | "ready_for_review" | "labeled")
            ) =>
        {
            if let (Some(inst), Some(repo), Some(pr)) = (
                &payload.installation,
                &payload.repository,
                &payload.pull_request,
            ) {
                if let (Some(title), Some(head), Some(base), Some(user)) =
                    (&pr.title, &pr.head, &pr.base, &pr.user)
                {
                    return GitHubEvent::PullRequestChanged(PrChangedEvent {
                        installation_id: inst.id,
                        repo_owner: repo.owner.login.clone(),
                        repo_name: repo.name.clone(),
                        pr_number: pr.number,
                        pr_title: title.clone(),
                        action: payload.action.clone().unwrap_or_default(),
                        label_name: payload.label.as_ref().map(|l| l.name.clone()),
                        head_ref: head.git_ref.clone(),
                        head_sha: head.sha.clone(),
                        base_ref: base.git_ref.clone(),
                        author_login: user.login.clone(),
                        draft: pr.draft,
                    });
                }
            }
        }
        "push" => {
            if let (Some(inst), Some(repo), Some(git_ref), Some(before), Some(after), Some(sender)) = (
                &payload.installation,
                &payload.repository,
                &payload.git_ref,
                &payload.before,
                &payload.after,
                &payload.sender,
            ) {
                return GitHubEvent::Push(PushEvent {
                    installation_id: inst.id,
                    repo_owner: repo.owner.login.clone(),
                    repo_name: repo.name.clone(),
                    branch: git_ref.strip_prefix("refs/heads/").map(str::to_string),
                    before_sha: before.clone(),
                    after_sha: after.clone(),
                    deleted: payload.deleted,
                    forced: payload.forced,
                    pusher_login: sender.login.clone(),
                    commit_count: payload.commits.len(),
                });
            }
        }
        "ping" => return GitHubEvent::Ping,
        _ => {}
    }

    GitHubEvent::Unsupported {
        name: event_type.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_labeled_issue_as_label_event() {
        let payload: WebhookPayload = serde_json::from_value(json!({
            "action": "labeled",
            "installation": { "id": 123 },
            "repository": {
                "name": "coven-code",
                "owner": { "login": "OpenCoven" }
            },
            "issue": {
                "number": 42,
                "title": "Fix the spell compiler",
                "body": "It loses sigils."
            },
            "label": { "name": "coven:fix" }
        }))
        .expect("payload should deserialize");

        let event = parse_event("issues", &payload);

        match event {
            GitHubEvent::IssueLabeled(e) => {
                assert_eq!(e.installation_id, 123);
                assert_eq!(e.repo_owner, "OpenCoven");
                assert_eq!(e.repo_name, "coven-code");
                assert_eq!(e.issue_number, 42);
                assert_eq!(e.issue_title, "Fix the spell compiler");
                assert_eq!(e.issue_body, "It loses sigils.");
                assert_eq!(e.label_name, "coven:fix");
            }
            other => panic!("expected IssueLabeled, got {other:?}"),
        }
    }
}
