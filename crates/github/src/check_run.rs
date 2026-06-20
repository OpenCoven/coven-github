//! GitHub Check Runs API client.

use anyhow::Result;
use serde::Deserialize;

use crate::{client, send_json, GitHubRequest, DEFAULT_API_BASE_URL};

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

#[derive(Debug, Deserialize)]
struct CheckRunResponse {
    id: u64,
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
    tracing::info!(
        repo = %format!("{repo_owner}/{repo_name}"),
        name,
        "creating check run"
    );
    create_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        head_sha,
        name,
        details_url,
    )
    .await
}

pub async fn create_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    head_sha: &str,
    name: &str,
    details_url: Option<&str>,
) -> Result<u64> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        create_request(repo_owner, repo_name, head_sha, name, details_url),
    )
    .await?;
    let body: CheckRunResponse = response.json().await?;
    Ok(body.id)
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
    update_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        check_run_id,
        status,
        title,
        summary,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn update_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    check_run_id: u64,
    status: CheckStatus,
    title: &str,
    summary: &str,
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        update_request(repo_owner, repo_name, check_run_id, status, title, summary),
    )
    .await?;
    Ok(())
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
    complete_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        repo_owner,
        repo_name,
        check_run_id,
        conclusion,
        title,
        summary,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn complete_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    repo_owner: &str,
    repo_name: &str,
    check_run_id: u64,
    conclusion: CheckConclusion,
    title: &str,
    summary: &str,
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        complete_request(
            repo_owner,
            repo_name,
            check_run_id,
            conclusion,
            title,
            summary,
        ),
    )
    .await?;
    Ok(())
}

fn create_request(
    repo_owner: &str,
    repo_name: &str,
    head_sha: &str,
    name: &str,
    details_url: Option<&str>,
) -> GitHubRequest {
    let mut body = serde_json::json!({
        "name": name,
        "head_sha": head_sha,
        "status": "queued",
    });
    if let Some(details_url) = details_url {
        body["details_url"] = serde_json::json!(details_url);
    }

    GitHubRequest {
        method: "POST",
        path: format!("/repos/{repo_owner}/{repo_name}/check-runs"),
        body,
    }
}

fn update_request(
    repo_owner: &str,
    repo_name: &str,
    check_run_id: u64,
    status: CheckStatus,
    title: &str,
    summary: &str,
) -> GitHubRequest {
    GitHubRequest {
        method: "PATCH",
        path: format!("/repos/{repo_owner}/{repo_name}/check-runs/{check_run_id}"),
        body: serde_json::json!({
            "status": status.as_str(),
            "output": {
                "title": title,
                "summary": summary,
            }
        }),
    }
}

fn complete_request(
    repo_owner: &str,
    repo_name: &str,
    check_run_id: u64,
    conclusion: CheckConclusion,
    title: &str,
    summary: &str,
) -> GitHubRequest {
    GitHubRequest {
        method: "PATCH",
        path: format!("/repos/{repo_owner}/{repo_name}/check-runs/{check_run_id}"),
        body: serde_json::json!({
            "status": "completed",
            "conclusion": conclusion.as_str(),
            "output": {
                "title": title,
                "summary": summary,
            }
        }),
    }
}

impl CheckStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl CheckConclusion {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Neutral => "neutral",
            Self::Cancelled => "cancelled",
            Self::ActionRequired => "action_required",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_request_targets_check_runs_with_expected_payload() {
        let request = create_request(
            "octo",
            "repo",
            "abc123",
            "Cody — Fix issue #7",
            Some("https://cave.example/sessions/task-7"),
        );

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/repos/octo/repo/check-runs");
        assert_eq!(
            request.body,
            json!({
                "name": "Cody — Fix issue #7",
                "head_sha": "abc123",
                "status": "queued",
                "details_url": "https://cave.example/sessions/task-7"
            })
        );
    }

    #[test]
    fn update_request_sets_output_without_conclusion() {
        let request = update_request(
            "octo",
            "repo",
            42,
            CheckStatus::InProgress,
            "Running",
            "Working",
        );

        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/repos/octo/repo/check-runs/42");
        assert_eq!(
            request.body,
            json!({
                "status": "in_progress",
                "output": {
                    "title": "Running",
                    "summary": "Working"
                }
            })
        );
    }

    #[test]
    fn complete_request_sets_completed_status_and_conclusion() {
        let request = complete_request(
            "octo",
            "repo",
            42,
            CheckConclusion::Success,
            "Done",
            "Ready",
        );

        assert_eq!(request.method, "PATCH");
        assert_eq!(request.path, "/repos/octo/repo/check-runs/42");
        assert_eq!(
            request.body,
            json!({
                "status": "completed",
                "conclusion": "success",
                "output": {
                    "title": "Done",
                    "summary": "Ready"
                }
            })
        );
    }
}
