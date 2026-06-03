//! Webhook event parsing: GitHub payload → typed events.

use serde::Deserialize;
use coven_github_api::{
    GitHubEvent, IssueAssignedEvent, IssueCommentEvent, PrReviewCommentEvent,
};

/// Raw GitHub webhook payload (partial — we only pull what we need).
#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub action: Option<String>,
    pub installation: Option<Installation>,
    pub repository: Option<Repository>,
    pub issue: Option<Issue>,
    pub comment: Option<Comment>,
    pub assignee: Option<User>,
    pub pull_request: Option<PullRequest>,
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
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub body: String,
    pub user: User,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    pub number: u64,
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
                    comment_body: comment.body.clone(),
                    commenter_login: comment.user.login.clone(),
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
                    comment_body: comment.body.clone(),
                    commenter_login: comment.user.login.clone(),
                });
            }
        }
        _ => {}
    }

    GitHubEvent::Unsupported {
        name: event_type.to_string(),
    }
}
