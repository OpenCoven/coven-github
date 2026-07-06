//! GitHub Pull Request and issue comment API.

use anyhow::Result;
use serde::Deserialize;

use crate::{client, send_json, GitHubRequest, DEFAULT_API_BASE_URL};

#[derive(Debug, Deserialize)]
struct PullRequestResponse {
    number: u64,
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
