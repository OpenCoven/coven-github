//! GitHub Pull Request and issue comment API.

use anyhow::Result;
use serde::Deserialize;

use crate::{client, send_json, GitHubRequest, DEFAULT_API_BASE_URL};

#[derive(Debug, Deserialize)]
struct PullRequestResponse {
    number: u64,
}

/// Pull request summary for PRs found by head branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadPullRequest {
    /// Pull request number.
    pub number: u64,
    /// Pull request state (`open` or `closed`).
    pub state: String,
    /// Whether GitHub reports the PR as merged.
    pub merged: bool,
    /// Whether the PR is a draft.
    pub draft: bool,
}

#[derive(Debug, Deserialize)]
struct HeadPullRequestResponse {
    number: u64,
    state: String,
    merged_at: Option<String>,
    draft: bool,
}

impl From<HeadPullRequestResponse> for HeadPullRequest {
    fn from(value: HeadPullRequestResponse) -> Self {
        Self {
            number: value.number,
            state: value.state,
            merged: value.merged_at.is_some(),
            draft: value.draft,
        }
    }
}

/// Opens a draft pull request. Returns the PR number.
#[allow(clippy::too_many_arguments)]
pub async fn open_pull_request(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<u64> {
    tracing::info!(
        repo = %format!("{repo_owner}/{repo_name}"),
        head = head_branch,
        title,
        "opening pull request"
    );
    open_pull_request_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        head_branch,
        base_branch,
        title,
        body,
        draft,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn open_pull_request_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<u64> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        pull_request_request(
            repo_owner,
            repo_name,
            head_branch,
            base_branch,
            title,
            body,
            draft,
        ),
    )
    .await?;
    let body: PullRequestResponse = response.json().await?;
    Ok(body.number)
}

/// Lists pull requests whose head is `{owner}:{branch}`.
pub async fn list_pulls_by_head(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    branch: &str,
) -> Result<Vec<HeadPullRequest>> {
    list_pulls_by_head_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        branch,
    )
    .await
}

pub async fn list_pulls_by_head_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    branch: &str,
) -> Result<Vec<HeadPullRequest>> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        list_pulls_by_head_request(repo_owner, repo_name, branch),
    )
    .await?;
    let body: Vec<HeadPullRequestResponse> = response.json().await?;
    Ok(body.into_iter().map(HeadPullRequest::from).collect())
}

/// Posts a comment on an issue or PR.
pub async fn post_comment(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    body: &str,
) -> Result<()> {
    tracing::info!(issue_number, "posting issue comment");
    post_comment_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        issue_number,
        body,
    )
    .await
}

pub async fn post_comment_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    body: &str,
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        issue_comment_request(repo_owner, repo_name, issue_number, body),
    )
    .await?;
    Ok(())
}

/// Adds labels to an issue or PR.
pub async fn add_labels_to_issue(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    labels: &[String],
) -> Result<()> {
    add_labels_to_issue_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        issue_number,
        labels,
    )
    .await
}

pub async fn add_labels_to_issue_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    labels: &[String],
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        add_labels_to_issue_request(repo_owner, repo_name, issue_number, labels),
    )
    .await?;
    Ok(())
}

fn pull_request_request(
    repo_owner: &str,
    repo_name: &str,
    head_branch: &str,
    base_branch: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> GitHubRequest {
    GitHubRequest {
        method: "POST",
        path: format!("/repos/{repo_owner}/{repo_name}/pulls"),
        body: serde_json::json!({
            "title": title,
            "head": head_branch,
            "base": base_branch,
            "body": body,
            "draft": draft,
        }),
    }
}

fn list_pulls_by_head_request(repo_owner: &str, repo_name: &str, branch: &str) -> GitHubRequest {
    let branch = crate::encode_ref_component(branch);
    GitHubRequest {
        method: "GET",
        path: format!(
            "/repos/{repo_owner}/{repo_name}/pulls?state=all&head={repo_owner}:{branch}&per_page=100"
        ),
        body: serde_json::Value::Null,
    }
}

fn issue_comment_request(
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    body: &str,
) -> GitHubRequest {
    GitHubRequest {
        method: "POST",
        path: format!("/repos/{repo_owner}/{repo_name}/issues/{issue_number}/comments"),
        body: serde_json::json!({ "body": body }),
    }
}

fn add_labels_to_issue_request(
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    labels: &[String],
) -> GitHubRequest {
    GitHubRequest {
        method: "POST",
        path: format!("/repos/{repo_owner}/{repo_name}/issues/{issue_number}/labels"),
        body: serde_json::json!({ "labels": labels }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pull_request_request_targets_pulls_endpoint() {
        let request = pull_request_request(
            "octo",
            "repo",
            "coven/fix-7",
            "main",
            "Fix issue #7",
            "Body",
            true,
        );

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/pulls");
        assert_eq!(
            request.body,
            json!({
                "title": "Fix issue #7",
                "head": "coven/fix-7",
                "base": "main",
                "body": "Body",
                "draft": true
            })
        );
    }

    #[test]
    fn issue_comment_request_targets_comments_endpoint() {
        let request = issue_comment_request("octo", "repo", 7, "On it");

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/issues/7/comments");
        assert_eq!(request.body, json!({ "body": "On it" }));
    }

    #[test]
    fn list_pulls_by_head_request_targets_head_query() {
        let request = list_pulls_by_head_request("octo", "repo", "coven/fix-7");

        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            "/repos/octo/repo/pulls?state=all&head=octo:coven/fix-7&per_page=100"
        );
        assert!(request.body.is_null());
    }

    #[test]
    fn list_pulls_by_head_request_percent_encodes_query_delimiters() {
        let request = list_pulls_by_head_request("octo", "repo", "feature&fix#1");

        assert_eq!(
            request.path,
            "/repos/octo/repo/pulls?state=all&head=octo:feature%26fix%231&per_page=100"
        );
    }

    #[test]
    fn add_labels_to_issue_request_posts_label_names() {
        let labels = vec!["branch-gardener".to_string(), "automated".to_string()];
        let request = add_labels_to_issue_request("octo", "repo", 7, &labels);

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/issues/7/labels");
        assert_eq!(
            request.body,
            json!({ "labels": ["branch-gardener", "automated"] })
        );
    }

    #[test]
    fn head_pull_request_response_derives_merged_from_merged_at() {
        let body: Vec<HeadPullRequestResponse> = serde_json::from_value(json!([
            { "number": 7, "state": "closed", "merged_at": "2026-07-07T00:00:00Z", "draft": false },
            { "number": 8, "state": "open", "merged_at": null, "draft": true }
        ]))
        .unwrap();

        let pulls: Vec<_> = body.into_iter().map(HeadPullRequest::from).collect();
        assert_eq!(pulls[0].number, 7);
        assert_eq!(pulls[0].state, "closed");
        assert!(pulls[0].merged);
        assert!(!pulls[0].draft);
        assert_eq!(pulls[1].number, 8);
        assert_eq!(pulls[1].state, "open");
        assert!(!pulls[1].merged);
        assert!(pulls[1].draft);
    }
}

/// One issue/PR conversation comment, trimmed to what marker lookup needs.
#[derive(Debug, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub body: String,
    pub user: CommentUser,
}

#[derive(Debug, Deserialize)]
pub struct CommentUser {
    pub login: String,
}

/// Lists the first 100 conversation comments on an issue or PR (oldest first).
/// Marker-backed status comments are posted early in a thread, so a single
/// page suffices; threads beyond 100 comments fall back to posting fresh
/// (issue #13).
pub async fn list_comments_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
) -> Result<Vec<IssueComment>> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        list_comments_request(repo_owner, repo_name, issue_number),
    )
    .await?;
    Ok(response.json().await?)
}

/// Edits an existing conversation comment in place (issue #13).
pub async fn update_comment_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    comment_id: u64,
    body: &str,
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        update_comment_request(repo_owner, repo_name, comment_id, body),
    )
    .await?;
    Ok(())
}

fn list_comments_request(repo_owner: &str, repo_name: &str, issue_number: u64) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{repo_owner}/{repo_name}/issues/{issue_number}/comments?per_page=100"),
        body: serde_json::Value::Null,
    }
}

fn update_comment_request(
    repo_owner: &str,
    repo_name: &str,
    comment_id: u64,
    body: &str,
) -> GitHubRequest {
    GitHubRequest {
        method: "PATCH",
        path: format!("/repos/{repo_owner}/{repo_name}/issues/comments/{comment_id}"),
        body: serde_json::json!({ "body": body }),
    }
}

/// Verdict of an adapter-submitted PR review (issue #11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// Findings exist: block with requested changes.
    RequestChanges,
    /// Nothing actionable: a non-blocking comment review.
    Comment,
}

impl ReviewVerdict {
    fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            ReviewVerdict::Comment => "COMMENT",
        }
    }
}

/// Submits a pull request review with the given verdict and body
/// (`request_changes` publication mode, issue #11).
pub async fn submit_review_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    pr_number: u64,
    verdict: ReviewVerdict,
    body: &str,
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        submit_review_request(repo_owner, repo_name, pr_number, verdict, body),
    )
    .await?;
    Ok(())
}

fn submit_review_request(
    repo_owner: &str,
    repo_name: &str,
    pr_number: u64,
    verdict: ReviewVerdict,
    body: &str,
) -> GitHubRequest {
    GitHubRequest {
        method: "POST",
        path: format!("/repos/{repo_owner}/{repo_name}/pulls/{pr_number}/reviews"),
        body: serde_json::json!({ "event": verdict.as_str(), "body": body }),
    }
}

#[cfg(test)]
mod comment_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_comments_request_targets_issue_comments_endpoint() {
        let request = list_comments_request("octo", "repo", 42);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/issues/42/comments?per_page=100");
    }

    #[test]
    fn update_comment_request_patches_the_comment_by_id() {
        let request = update_comment_request("octo", "repo", 77, "new body");
        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/repos/octo/repo/issues/comments/77");
        assert_eq!(request.body["body"], json!("new body"));
    }

    #[test]
    fn submit_review_request_posts_the_verdict() {
        let request =
            submit_review_request("octo", "repo", 88, ReviewVerdict::RequestChanges, "digest");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/pulls/88/reviews");
        assert_eq!(request.body["event"], json!("REQUEST_CHANGES"));
        assert_eq!(request.body["body"], json!("digest"));
    }

    #[test]
    fn issue_comment_deserializes_id_body_and_author() {
        let comments: Vec<IssueComment> = serde_json::from_value(json!([
            { "id": 7, "body": "<!-- coven:cody:o/r#42 -->\nStatus: working", "user": { "login": "coven-cody[bot]" }, "created_at": "2026-07-06T00:00:00Z" }
        ]))
        .unwrap();
        assert_eq!(comments[0].id, 7);
        assert!(comments[0].body.contains("coven:cody"));
        assert_eq!(comments[0].user.login, "coven-cody[bot]");
    }
}
