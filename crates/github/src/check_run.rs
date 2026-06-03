//! GitHub Check Runs API client.

use anyhow::Result;

/// Check Run status values.
#[derive(Debug, Clone)]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

/// Check Run conclusion values (used when status = Completed).
#[derive(Debug, Clone)]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    ActionRequired,
}

/// Creates a new Check Run and returns its ID.
pub async fn create(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    head_sha: &str,
    name: &str,
    details_url: Option<&str>,
) -> Result<u64> {
    // TODO: implement via reqwest + GitHub REST API
    // POST /repos/{owner}/{repo}/check-runs
    tracing::info!(
        repo = %format!("{repo_owner}/{repo_name}"),
        name,
        "creating check run"
    );
    Ok(0) // placeholder
}

/// Updates an existing Check Run with progress output.
pub async fn update(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    check_run_id: u64,
    status: CheckStatus,
    title: &str,
    summary: &str,
) -> Result<()> {
    tracing::info!(check_run_id, title, "updating check run");
    Ok(()) // placeholder
}

/// Completes a Check Run.
pub async fn complete(
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    check_run_id: u64,
    conclusion: CheckConclusion,
    title: &str,
    summary: &str,
) -> Result<()> {
    tracing::info!(check_run_id, title, "completing check run");
    Ok(()) // placeholder
}
