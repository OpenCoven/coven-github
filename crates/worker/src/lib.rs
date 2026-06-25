//! Worker: pulls tasks from the queue, spawns coven-code sessions, streams progress.

use anyhow::Result;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{error, info, warn};

use coven_github_api::{
    check_run, installation, pr, repo, tasks::TaskStore, SessionResult, SessionStatus, Task,
    TaskKind, DEFAULT_API_BASE_URL,
};
use coven_github_config::Config;

pub mod brief;

/// Runs the worker loop: pulls tasks and executes them concurrently.
pub async fn run(
    config: std::sync::Arc<Config>,
    task_store: TaskStore,
    mut task_rx: tokio::sync::mpsc::Receiver<Task>,
) {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.worker.concurrency));

    while let Some(task) = task_rx.recv().await {
        let config = config.clone();
        let task_store = task_store.clone();
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = execute_task(&config, task_store.clone(), task).await {
                error!("task execution error: {e:#}");
            }
        });
    }
}

async fn execute_task(config: &Config, task_store: TaskStore, task: Task) -> Result<()> {
    let familiar = config
        .familiars
        .iter()
        .find(|f| f.id == task.familiar_id)
        .ok_or_else(|| anyhow::anyhow!("unknown familiar: {}", task.familiar_id))?;

    info!(task_id = %task.id, familiar = %familiar.id, "starting task");
    let api_base_url = config
        .github
        .api_base_url
        .as_deref()
        .unwrap_or(DEFAULT_API_BASE_URL);

    // Get installation token.
    let private_key = std::fs::read_to_string(&config.github.private_key_path)?;
    let token = installation::get_token_with_base_url(
        api_base_url,
        config.github.app_id,
        &private_key,
        task.installation_id,
    )
    .await?;

    // Resolve target refs and base branch from live GitHub state. Check Runs
    // must attach to an immutable commit SHA, and PRs must target the repo's
    // actual base branch rather than a hardcoded "main".
    let targets = resolve_targets(api_base_url, &token, &task).await?;

    // Create Check Run against the resolved head SHA.
    let check_name = format!("{} — {}", familiar.display_name, task_title(&task.kind));
    let check_id = check_run::create_with_base_url(
        api_base_url,
        &token,
        &task.repo_owner,
        &task.repo_name,
        &targets.head_sha,
        &check_name,
        config
            .server
            .cave_base_url
            .as_deref()
            .map(|b| format!("{b}/sessions/{}", task.id))
            .as_deref(),
    )
    .await?;
    let repo = format!("{}/{}", task.repo_owner, task.repo_name);
    let check_run_url = Some(format!("https://github.com/{repo}/runs/{check_id}"));
    task_store
        .mark_running(&task, &familiar.display_name, check_run_url)
        .await;

    // Post "starting" comment.
    if let Some(issue_number) = task_issue_number(&task.kind) {
        let start_msg = format!(
            "👋 I'm **{}**, your Coven coding familiar. I'm on it — I'll open a PR once I've had a look.\n\n[Watch this session in CovenCave →]({})",
            familiar.display_name,
            config.server.cave_base_url.as_deref().unwrap_or("https://cave.opencoven.ai"),
        );
        pr::post_comment_with_base_url(
            api_base_url,
            &token,
            &task.repo_owner,
            &task.repo_name,
            issue_number,
            &start_msg,
        )
        .await?;
    }

    // Build session brief.
    let workspace = config.worker.workspace_root.join(&task.id);
    tokio::fs::create_dir_all(&workspace).await?;

    let brief = brief::build(&task, familiar, &workspace, &targets.default_branch);
    let brief_path = workspace.join("session-brief.json");
    let result_path = workspace.join("result.json");
    tokio::fs::write(&brief_path, serde_json::to_string_pretty(&brief)?).await?;

    // Spawn coven-code.
    check_run::update_with_base_url(
        api_base_url,
        &token,
        &task.repo_owner,
        &task.repo_name,
        check_id,
        check_run::CheckStatus::InProgress,
        "Running",
        "Familiar is working on the task…",
    )
    .await?;

    let result = run_with_retry(
        config,
        &brief_path,
        &result_path,
        &token,
        config.worker.max_retries,
    )
    .await;

    match result {
        Ok(session_result) => {
            let success = session_result.status == SessionStatus::Success
                || session_result.status == SessionStatus::Partial;

            // Open PR if we have commits.
            let mut opened_pr = None;
            if !session_result.commits.is_empty() {
                if let Some(branch) = &session_result.branch {
                    let pr_num = pr::open_pull_request_with_base_url(
                        api_base_url,
                        &token,
                        &task.repo_owner,
                        &task.repo_name,
                        branch,
                        &targets.base_ref,
                        &format!(
                            "{} (#{} via Coven)",
                            session_result.summary,
                            task_issue_number(&task.kind).unwrap_or(0)
                        ),
                        &session_result.pr_body,
                        true, // draft
                    )
                    .await?;
                    opened_pr = Some(pr_num);

                    if let Some(issue_number) = task_issue_number(&task.kind) {
                        pr::post_comment_with_base_url(
                            api_base_url,
                            &token,
                            &task.repo_owner,
                            &task.repo_name,
                            issue_number,
                            &format!(
                                "✅ PR #{pr_num} opened — [watch in CovenCave →]({})",
                                config
                                    .server
                                    .cave_base_url
                                    .as_deref()
                                    .unwrap_or("https://cave.opencoven.ai")
                            ),
                        )
                        .await?;
                    }
                }
            }
            task_store
                .mark_complete(&task.id, &repo, &session_result, opened_pr)
                .await;

            check_run::complete_with_base_url(
                api_base_url,
                &token,
                &task.repo_owner,
                &task.repo_name,
                check_id,
                if success {
                    check_run::CheckConclusion::Success
                } else {
                    check_run::CheckConclusion::Failure
                },
                if success { "Done" } else { "Incomplete" },
                &session_result.summary,
            )
            .await?;
        }
        Err(e) => {
            error!(task_id = %task.id, "session failed: {e:#}");
            task_store.mark_failed(&task.id).await;
            check_run::complete_with_base_url(
                api_base_url,
                &token,
                &task.repo_owner,
                &task.repo_name,
                check_id,
                check_run::CheckConclusion::Failure,
                "Error",
                &format!("Task failed: {e}"),
            )
            .await?;
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
    git_token: &str,
    max_retries: u32,
) -> Result<SessionResult> {
    let mut attempts = 0;
    loop {
        match run_coven_code(config, brief_path, result_path, git_token).await {
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
    git_token: &str,
) -> Result<SessionResult> {
    let mut child = Command::new(&config.worker.coven_code_bin)
        .arg("--headless")
        .arg("--context")
        .arg(brief_path)
        .arg("--output")
        .arg(result_path)
        // Git auth is injected via the environment, never written to the
        // session brief or any durable artifact (issue #4).
        .env("COVEN_GIT_TOKEN", git_token)
        .spawn()?;

    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(config.worker.timeout_secs),
        child.wait(),
    )
    .await
    {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!(
                "coven-code timed out after {} seconds",
                config.worker.timeout_secs
            );
        }
    };

    match status.code() {
        Some(0) => {}
        Some(2) => anyhow::bail!("coven-code infra error (exit 2) — retry-safe"),
        Some(3) => {
            // Agent needs clarification — read partial result.
        }
        Some(code) => anyhow::bail!("coven-code exited with code {code}"),
        None => anyhow::bail!("coven-code killed by signal"),
    }

    let result_bytes = tokio::fs::read(result_path)
        .await
        .map_err(|_| anyhow::anyhow!("result.json not written by coven-code"))?;
    let result: SessionResult = serde_json::from_slice(&result_bytes)?;
    Ok(result)
}

fn task_title(kind: &TaskKind) -> String {
    match kind {
        TaskKind::FixIssue {
            issue_title,
            issue_number,
            ..
        } => format!("Fix issue #{issue_number}: {issue_title}"),
        TaskKind::AddressReviewComment { pr_number, .. } => {
            format!("Address review on PR #{pr_number}")
        }
        TaskKind::RespondToMention { issue_number, .. } => {
            format!("Respond on issue #{issue_number}")
        }
    }
}

/// Refs resolved from live GitHub state for a task.
struct ResolvedTargets {
    /// Repository default branch (for the session brief).
    default_branch: String,
    /// Branch a draft PR should target.
    base_ref: String,
    /// Immutable commit SHA the Check Run attaches to.
    head_sha: String,
}

/// Resolves the repository default branch and the immutable target refs a task
/// operates on. Issue tasks target the default branch tip; PR review-comment
/// tasks target the PR's own head/base refs.
async fn resolve_targets(api_base_url: &str, token: &str, task: &Task) -> Result<ResolvedTargets> {
    let meta = repo::get_repo_with_base_url(api_base_url, token, &task.repo_owner, &task.repo_name)
        .await?;

    match &task.kind {
        TaskKind::AddressReviewComment { pr_number, .. } => {
            let refs = repo::get_pull_request_refs_with_base_url(
                api_base_url,
                token,
                &task.repo_owner,
                &task.repo_name,
                *pr_number,
            )
            .await?;
            Ok(ResolvedTargets {
                default_branch: meta.default_branch,
                base_ref: refs.base_ref,
                head_sha: refs.head_sha,
            })
        }
        TaskKind::FixIssue { .. } | TaskKind::RespondToMention { .. } => {
            let head_sha = repo::get_branch_sha_with_base_url(
                api_base_url,
                token,
                &task.repo_owner,
                &task.repo_name,
                &meta.default_branch,
            )
            .await?;
            Ok(ResolvedTargets {
                base_ref: meta.default_branch.clone(),
                default_branch: meta.default_branch,
                head_sha,
            })
        }
    }
}

fn task_issue_number(kind: &TaskKind) -> Option<u64> {
    match kind {
        TaskKind::FixIssue { issue_number, .. } => Some(*issue_number),
        TaskKind::RespondToMention { issue_number, .. } => Some(*issue_number),
        TaskKind::AddressReviewComment { .. } => None,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::{fs, os::unix::fs::PermissionsExt, time::Instant};

    fn test_config(coven_code_bin: PathBuf, workspace_root: PathBuf) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: None,
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("private.pem"),
                webhook_secret: "secret".to_string(),
                api_base_url: None,
            },
            worker: WorkerConfig {
                concurrency: 1,
                coven_code_bin,
                workspace_root,
                timeout_secs: 1,
                max_retries: 0,
            },
            familiars: vec![FamiliarConfig {
                id: "cody".to_string(),
                display_name: "Cody".to_string(),
                bot_username: "coven-cody[bot]".to_string(),
                model: None,
                skills: vec![],
                trigger_labels: vec![],
            }],
        }
    }

    #[tokio::test]
    async fn coven_code_process_is_stopped_after_configured_timeout() {
        let root = std::env::temp_dir().join(format!(
            "coven-github-timeout-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("test dir should be created");
        let script = root.join("slow-coven-code.sh");
        fs::write(&script, "#!/usr/bin/env bash\nsleep 5\n").expect("script should be written");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("script should be executable");

        let config = test_config(script, root.clone());
        let brief_path = root.join("session-brief.json");
        let result_path = root.join("result.json");
        fs::write(&brief_path, "{}").expect("brief should be written");

        let started = Instant::now();
        let result = run_coven_code(&config, &brief_path, &result_path, "test-token").await;

        assert!(result.is_err());
        assert!(
            started.elapsed().as_secs() < 3,
            "process should stop close to the configured timeout"
        );

        let _ = fs::remove_dir_all(root);
    }
}
