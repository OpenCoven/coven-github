//! GitHub Pull Request and issue comment API.

use anyhow::Result;

/// Opens a draft pull request. Returns the PR number.
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
    // TODO: POST /repos/{owner}/{repo}/pulls
    tracing::info!(
        repo = %format!("{repo_owner}/{repo_name}"),
        head = head_branch,
        title,
        "opening pull request"
    );
    Ok(0) // placeholder
}

/// Posts a comment on an issue or PR.
pub async fn post_comment(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    issue_number: u64,
    body: &str,
) -> Result<()> {
    // TODO: POST /repos/{owner}/{repo}/issues/{number}/comments
    tracing::info!(issue_number, "posting issue comment");
    Ok(()) // placeholder
}
