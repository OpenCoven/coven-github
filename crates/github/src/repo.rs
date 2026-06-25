//! GitHub repository, branch, and pull request metadata client.
//!
//! Used to resolve the target refs a task operates on instead of relying on
//! placeholders like `"HEAD"` or a hardcoded `"main"` base branch.

use anyhow::Result;
use serde::Deserialize;

use crate::{client, send_json, GitHubRequest, DEFAULT_API_BASE_URL};

/// Repository metadata we care about for routing and publication.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoMetadata {
    pub default_branch: String,
}

/// Pull request refs needed to attach checks and open/update PRs correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRefs {
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
}

#[derive(Debug, Deserialize)]
struct BranchResponse {
    commit: CommitRef,
}

#[derive(Debug, Deserialize)]
struct CommitRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestMetaResponse {
    head: PrRef,
    base: PrRef,
}

#[derive(Debug, Deserialize)]
struct PrRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

/// Fetches repository metadata (default branch, etc.).
pub async fn get_repo(installation_token: &str, owner: &str, name: &str) -> Result<RepoMetadata> {
    get_repo_with_base_url(DEFAULT_API_BASE_URL, installation_token, owner, name).await
}

pub async fn get_repo_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
) -> Result<RepoMetadata> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_repo_request(owner, name),
    )
    .await?;
    Ok(response.json().await?)
}

/// Resolves the current commit SHA at the tip of a branch.
pub async fn get_branch_sha(
    installation_token: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<String> {
    get_branch_sha_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        owner,
        name,
        branch,
    )
    .await
}

pub async fn get_branch_sha_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<String> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_branch_request(owner, name, branch),
    )
    .await?;
    let body: BranchResponse = response.json().await?;
    Ok(body.commit.sha)
}

/// Fetches the head/base refs and SHAs for a pull request.
pub async fn get_pull_request_refs(
    installation_token: &str,
    owner: &str,
    name: &str,
    pr_number: u64,
) -> Result<PullRequestRefs> {
    get_pull_request_refs_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        owner,
        name,
        pr_number,
    )
    .await
}

pub async fn get_pull_request_refs_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    pr_number: u64,
) -> Result<PullRequestRefs> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_pull_request_request(owner, name, pr_number),
    )
    .await?;
    let body: PullRequestMetaResponse = response.json().await?;
    Ok(PullRequestRefs {
        head_ref: body.head.ref_name,
        head_sha: body.head.sha,
        base_ref: body.base.ref_name,
        base_sha: body.base.sha,
    })
}

fn get_repo_request(owner: &str, name: &str) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}"),
        body: serde_json::Value::Null,
    }
}

fn get_branch_request(owner: &str, name: &str, branch: &str) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/branches/{branch}"),
        body: serde_json::Value::Null,
    }
}

fn get_pull_request_request(owner: &str, name: &str, pr_number: u64) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/pulls/{pr_number}"),
        body: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_repo_request_targets_repo_endpoint() {
        let request = get_repo_request("octo", "repo");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo");
    }

    #[test]
    fn get_branch_request_targets_branch_endpoint() {
        let request = get_branch_request("octo", "repo", "develop");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/branches/develop");
    }

    #[test]
    fn get_pull_request_request_targets_pulls_endpoint() {
        let request = get_pull_request_request("octo", "repo", 7);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/pulls/7");
    }

    #[test]
    fn repo_metadata_deserializes_default_branch() {
        let meta: RepoMetadata =
            serde_json::from_value(json!({ "default_branch": "master", "id": 99 })).unwrap();
        assert_eq!(meta.default_branch, "master");
    }

    #[test]
    fn branch_response_extracts_commit_sha() {
        let body: BranchResponse =
            serde_json::from_value(json!({ "name": "main", "commit": { "sha": "abc123" } }))
                .unwrap();
        assert_eq!(body.commit.sha, "abc123");
    }

    #[test]
    fn pull_request_meta_extracts_head_and_base_refs() {
        let body: PullRequestMetaResponse = serde_json::from_value(json!({
            "head": { "ref": "feature", "sha": "headsha" },
            "base": { "ref": "develop", "sha": "basesha" }
        }))
        .unwrap();
        assert_eq!(body.head.ref_name, "feature");
        assert_eq!(body.head.sha, "headsha");
        assert_eq!(body.base.ref_name, "develop");
        assert_eq!(body.base.sha, "basesha");
    }
}
