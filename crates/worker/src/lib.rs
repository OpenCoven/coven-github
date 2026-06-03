//! Worker: pulls tasks from the queue, spawns coven-code sessions, streams progress.

use anyhow::Result;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{error, info, warn};

use coven_github_api::{
    SessionResult, SessionStatus, Task, TaskKind,
    check_run, installation, pr,
};
use coven_github_config::{Config, FamiliarConfig};

pub mod brief;

/// Runs the worker loop: pulls tasks and executes them concurrently.
pub async fn run(config: std::sync::Arc<Config>, mut task_rx: tokio::sync::mpsc::Receiver<Task>) {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.worker.concurrency));

    while let Some(task) = task_rx.recv().await {
        let config = config.clone();
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = execute_task(&config, task).await {
                error!("task execution error: {e:#}");
            }
        });
    }
}

async fn execute_task(config: &Config, task: Task) -> Result<()> {
    let familiar = config.familiars.iter()
        .find(|f| f.id == task.familiar_id)
        .ok_or_else(|| anyhow::anyhow!("unknown familiar: {}", task.familiar_id))?;

    info!(task_id = %task.id, familiar = %familiar.id, "starting task");

    // Get installation token.
    let private_key = std::fs::read_to_string(&config.github.private_key_path)?;
    let token = installation::get_token(
        config.github.app_id,
        &private_key,
        task.installation_id,
    ).await?;

    // Create Check Run.
    let check_name = format!("{} — {}", familiar.display_name, task_title(&task.kind));
    let check_id = check_run::create(
        &token,
        &task.repo_owner,
        &task.repo_name,
        "HEAD", // TODO: resolve head SHA
        &check_name,
        config.server.cave_base_url.as_deref().map(|b| format!("{b}/sessions/{}", task.id)).as_deref(),
    ).await?;

    // Post "starting" comment.
    if let Some(issue_number) = task_issue_number(&task.kind) {
        let start_msg = format!(
            "👋 I'm **{}**, your Coven coding familiar. I'm on it — I'll open a PR once I've had a look.\n\n[Watch this session in CovenCave →]({})",
            familiar.display_name,
            config.server.cave_base_url.as_deref().unwrap_or("https://cave.opencoven.ai"),
        );
        pr::post_comment(&token, &task.repo_owner, &task.repo_name, issue_number, &start_msg).await?;
    }

    // Build session brief.
    let workspace = config.worker.workspace_root.join(&task.id);
    tokio::fs::create_dir_all(&workspace).await?;

    let brief = brief::build(&task, familiar, &token, &workspace);
    let brief_path = workspace.join("session-brief.json");
    let result_path = workspace.join("result.json");
    tokio::fs::write(&brief_path, serde_json::to_string_pretty(&brief)?).await?;

    // Spawn coven-code.
    check_run::update(
        &token, &task.repo_owner, &task.repo_name, check_id,
        check_run::CheckStatus::InProgress,
        "Running", "Familiar is working on the task…",
    ).await?;

    let result = run_with_retry(config, &brief_path, &result_path, config.worker.max_retries).await;

    match result {
        Ok(session_result) => {
            let success = session_result.status == SessionStatus::Success
                || session_result.status == SessionStatus::Partial;

            // Open PR if we have commits.
            if !session_result.commits.is_empty() {
                if let Some(branch) = &session_result.branch {
                    let pr_num = pr::open_pull_request(
                        &token,
                        &task.repo_owner, &task.repo_name,
                        branch, "main",
                        &format!("{} (#{} via Coven)", session_result.summary, task_issue_number(&task.kind).unwrap_or(0)),
                        &session_result.pr_body,
                        true, // draft
                    ).await?;

                    if let Some(issue_number) = task_issue_number(&task.kind) {
                        pr::post_comment(
                            &token, &task.repo_owner, &task.repo_name, issue_number,
                            &format!("✅ PR #{pr_num} opened — [watch in CovenCave →]({})",
                                config.server.cave_base_url.as_deref().unwrap_or("https://cave.opencoven.ai")),
                        ).await?;
                    }
                }
            }

            check_run::complete(
                &token, &task.repo_owner, &task.repo_name, check_id,
                if success { check_run::CheckConclusion::Success } else { check_run::CheckConclusion::Failure },
                if success { "Done" } else { "Incomplete" },
                &session_result.summary,
            ).await?;
        }
        Err(e) => {
            error!(task_id = %task.id, "session failed: {e:#}");
            check_run::complete(
                &token, &task.repo_owner, &task.repo_name, check_id,
                check_run::CheckConclusion::Failure,
                "Error",
                &format!("Task failed: {e}"),
            ).await?;
        }
    }

    // Clean up workspace.
    tokio::fs::remove_dir_all(&workspace).await.ok();
    Ok(())
}

async fn run_with_retry(
    config: &Config,
    brief_path: &PathBuf,
    result_path: &PathBuf,
    max_retries: u32,
) -> Result<SessionResult> {
    let mut attempts = 0;
    loop {
        match run_coven_code(config, brief_path, result_path).await {
            Ok(r) => return Ok(r),
            Err(e) if attempts < max_retries => {
                warn!("coven-code attempt {attempts} failed ({e:#}), retrying…");
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempts))).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn run_coven_code(
    config: &Config,
    brief_path: &PathBuf,
    result_path: &PathBuf,
) -> Result<SessionResult> {
    let status = Command::new(&config.worker.coven_code_bin)
        .arg("--headless")
        .arg("--context")
        .arg(brief_path)
        .arg("--output")
        .arg(result_path)
        .status()
        .await?;

    match status.code() {
        Some(0) => {}
        Some(2) => anyhow::bail!("coven-code infra error (exit 2) — retry-safe"),
        Some(3) => {
            // Agent needs clarification — read partial result.
        }
        Some(code) => anyhow::bail!("coven-code exited with code {code}"),
        None => anyhow::bail!("coven-code killed by signal"),
    }

    let result_bytes = tokio::fs::read(result_path).await
        .map_err(|_| anyhow::anyhow!("result.json not written by coven-code"))?;
    let result: SessionResult = serde_json::from_slice(&result_bytes)?;
    Ok(result)
}

fn task_title(kind: &TaskKind) -> String {
    match kind {
        TaskKind::FixIssue { issue_title, issue_number, .. } =>
            format!("Fix issue #{issue_number}: {issue_title}"),
        TaskKind::AddressReviewComment { pr_number, .. } =>
            format!("Address review on PR #{pr_number}"),
        TaskKind::RespondToMention { issue_number, .. } =>
            format!("Respond on issue #{issue_number}"),
    }
}

fn task_issue_number(kind: &TaskKind) -> Option<u64> {
    match kind {
        TaskKind::FixIssue { issue_number, .. } => Some(*issue_number),
        TaskKind::RespondToMention { issue_number, .. } => Some(*issue_number),
        TaskKind::AddressReviewComment { .. } => None,
    }
}
