//! Worker: pulls tasks from the queue, spawns coven-code sessions, streams progress.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

use coven_github_api::{
    check_run, installation, installation::TokenRole, pr, repo, tasks::TaskStore,
    ReviewEvidenceStatus, ReviewMode, SessionResult, SessionStatus, Task, TaskKind,
    DEFAULT_API_BASE_URL,
};
use coven_github_config::{Config, FamiliarConfig};

pub mod brief;
pub mod redact;

/// Base unit for exponential backoff between retry-safe coven-code attempts.
/// Attempt `n` sleeps `RETRY_BACKOFF_BASE * 2^n` (so 2s, 4s, … in production).
const RETRY_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Default Cave base URL used in familiar-voice comments when none is configured.
const DEFAULT_CAVE_BASE_URL: &str = "https://cave.opencoven.ai";

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
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            // The semaphore is only ever closed on shutdown; stop pulling tasks.
            Err(_) => break,
        };

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = execute_task(&config, task_store, task).await {
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

    // Pre-flight: installation token, ref resolution, and Check Run creation.
    // These run *before* the Check Run exists, so a failure here can't orphan a
    // check — but it would otherwise make the task vanish silently. Record it as
    // failed so it stays visible in Cave, then propagate.
    let prepared = async {
        let private_key = std::fs::read_to_string(&config.github.private_key_path)?;
        let minter = Minter::App {
            api_base_url: api_base_url.to_string(),
            app_id: config.github.app_id,
            private_key,
            installation_id: task.installation_id,
            repo_name: task.repo_name.clone(),
        };
        // Adapter-held orchestration authority: resolve refs, drive the Check
        // Run, post progress comments. The agent never sees this token.
        let orchestration = minter.mint(TokenRole::Orchestration).await?;

        // Resolve target refs and base branch from live GitHub state. Check Runs
        // must attach to an immutable commit SHA, and PRs must target the repo's
        // actual base branch rather than a hardcoded "main".
        let targets = resolve_targets(api_base_url, &orchestration, &task).await?;

        // Create Check Run against the resolved head SHA. From this point on the
        // Check Run MUST reach a terminal conclusion on every code path — a flaky
        // comment or PR API call must never leave a perpetually in-progress check
        // blocking merges on a real repo.
        let check_name = format!("{} — {}", familiar.display_name, task_title(&task.kind));
        let details_url = cave_session_url(config, &task.id);
        let check_id = check_run::create_with_base_url(
            api_base_url,
            &orchestration,
            &task.repo_owner,
            &task.repo_name,
            &targets.head_sha,
            &check_name,
            Some(details_url.as_str()),
        )
        .await?;
        Ok::<_, anyhow::Error>((minter, orchestration, targets, check_id))
    }
    .await;

    let (minter, orchestration, targets, check_id) = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            error!(task_id = %task.id, "pre-flight failed before check run: {e:#}");
            task_store
                .register_failed(&task, &familiar.display_name)
                .await;
            return Err(e);
        }
    };

    let repo = format!("{}/{}", task.repo_owner, task.repo_name);
    let check_run_url = Some(format!("https://github.com/{repo}/runs/{check_id}"));
    task_store
        .mark_running(&task, &familiar.display_name, check_run_url)
        .await;

    // Everything past check creation is fallible but must not orphan the check
    // or leak the workspace. Run it, then finalize unconditionally below.
    let workspace = config.worker.workspace_root.join(&task.id);
    let outcome = run_and_publish(
        config,
        &task,
        familiar,
        &minter,
        &orchestration,
        api_base_url,
        &targets,
        &workspace,
        check_id,
    )
    .await;

    // Workspace cleanup ALWAYS runs — success or failure.
    tokio::fs::remove_dir_all(&workspace).await.ok();

    // The Check Run ALWAYS reaches a terminal conclusion; both arms complete it.
    match outcome {
        Ok(published) => {
            let disp = disposition(&published.result);
            task_store
                .mark_complete(&task.id, &repo, &published.result, published.opened_pr)
                .await;
            if let Err(e) = check_run::complete_with_base_url(
                api_base_url,
                &orchestration,
                &task.repo_owner,
                &task.repo_name,
                check_id,
                disp.conclusion,
                disp.title,
                &published.result.summary,
            )
            .await
            {
                error!(task_id = %task.id, "failed to finalize check run: {e:#}");
            }
        }
        Err(e) => {
            error!(task_id = %task.id, "session failed: {e:#}");
            task_store.mark_failed(&task.id).await;
            if let Err(ce) = check_run::complete_with_base_url(
                api_base_url,
                &orchestration,
                &task.repo_owner,
                &task.repo_name,
                check_id,
                check_run::CheckConclusion::Failure,
                "Error",
                // Error chains can embed response bodies; scrub token-shaped
                // strings before they reach the published Check Run summary.
                &redact::redact(&format!("Task failed: {e}"), &[&orchestration]),
            )
            .await
            {
                error!(task_id = %task.id, "failed to finalize check run after error: {ce:#}");
            }
        }
    }

    Ok(())
}

/// Successful end-to-end publication of a session result.
struct Published {
    result: SessionResult,
    opened_pr: Option<u64>,
}

/// Mints repo-scoped installation tokens for one task. Tests substitute
/// `Fixed` so publication paths run without GitHub App credentials.
pub(crate) enum Minter {
    App {
        api_base_url: String,
        app_id: u64,
        private_key: String,
        installation_id: u64,
        repo_name: String,
    },
}

impl Minter {
    async fn mint(&self, role: TokenRole) -> Result<String> {
        match self {
            Minter::App {
                api_base_url,
                app_id,
                private_key,
                installation_id,
                repo_name,
            } => {
                installation::get_scoped_token_with_base_url(
                    api_base_url,
                    *app_id,
                    private_key,
                    *installation_id,
                    repo_name,
                    role,
                )
                .await
            }
        }
    }
}

/// Provisions the workspace, runs coven-code with the retry policy, and publishes
/// the outcome (PR + comments). Returns `Err` only for failures that should mark
/// the task — and complete the Check Run — as failed: workspace/brief I/O errors
/// and retry-safe session failures that exhausted the retry budget. Cosmetic
/// side-effects (comments, the in-progress transition, even PR opening) are
/// best-effort and logged rather than propagated.
#[allow(clippy::too_many_arguments)]
async fn run_and_publish(
    config: &Config,
    task: &Task,
    familiar: &FamiliarConfig,
    minter: &Minter,
    orchestration: &str,
    api_base_url: &str,
    targets: &ResolvedTargets,
    workspace: &Path,
    check_id: u64,
) -> Result<Published> {
    // Provision ephemeral workspace and write the tokenless session brief.
    tokio::fs::create_dir_all(workspace).await?;
    let brief = brief::build(task, familiar, workspace, &targets.default_branch);
    let brief_path = workspace.join("session-brief.json");
    let result_path = workspace.join("result.json");
    let brief_json = serde_json::to_string_pretty(&brief)?;
    // Belt-and-braces on top of the serialization guard test: refuse to hand
    // the agent a brief that somehow embeds a live credential.
    anyhow::ensure!(
        !redact::contains_live_token(&brief_json, &[orchestration]),
        "session brief contained a live token; refusing to write it"
    );
    tokio::fs::write(&brief_path, brief_json).await?;

    // Best-effort "starting" comment — a flaky comment API call must not abort
    // the task or orphan the Check Run.
    if let Some(issue_number) = task_issue_number(&task.kind) {
        let start_msg = starting_comment(config, familiar, &task.id);
        if let Err(e) = pr::post_comment_with_base_url(
            api_base_url,
            orchestration,
            &task.repo_owner,
            &task.repo_name,
            issue_number,
            &start_msg,
        )
        .await
        {
            warn!(task_id = %task.id, "failed to post starting comment: {e:#}");
        }
    }

    // Best-effort progress transition; the check is completed regardless below.
    if let Err(e) = check_run::update_with_base_url(
        api_base_url,
        orchestration,
        &task.repo_owner,
        &task.repo_name,
        check_id,
        check_run::CheckStatus::InProgress,
        "Running",
        "Familiar is working on the task.",
    )
    .await
    {
        warn!(task_id = %task.id, "failed to mark check in progress: {e:#}");
    }

    // The agent's only credential: contents:write on the target repo, minted
    // immediately before spawn and injected via COVEN_GIT_TOKEN (never JSON).
    let agent_git = minter.mint(TokenRole::AgentGit).await?;

    // Run coven-code. Only retry-safe failures (exit 2, timeout, signal) are
    // retried; exit 1 (gave up) and exit 3 (needs input) are terminal.
    let mut result = run_session(
        config,
        &brief_path,
        &result_path,
        &agent_git,
        config.worker.max_retries,
    )
    .await?;

    // Scrub token values and token-shaped strings from the envelope before
    // anything downstream persists or publishes it (task store, comments,
    // PR body, Check Run output).
    redact::sanitize_result(&mut result, &[orchestration, &agent_git]);

    // Publish according to the terminal disposition of the result.
    let disp = disposition(&result);
    let mut opened_pr = None;
    if disp.open_pr {
        if let Some(branch) = &result.branch {
            // Write authority for publication is minted only now — after the
            // envelope passed contract validation and sanitization (issue #4).
            match minter.mint(TokenRole::Publication).await {
                Ok(publication) => {
                    opened_pr = open_draft_pr(
                        config,
                        task,
                        api_base_url,
                        &publication,
                        targets,
                        &result,
                        branch,
                    )
                    .await;
                }
                Err(e) => {
                    warn!(task_id = %task.id, "failed to mint publication token: {e:#}");
                    if let Some(issue_number) = task_issue_number(&task.kind) {
                        let msg = redact::redact(
                            &format!(
                                "I pushed `{branch}` but could not obtain publication credentials to open the PR: {e}"
                            ),
                            &[orchestration],
                        );
                        let _ = pr::post_comment_with_base_url(
                            api_base_url,
                            orchestration,
                            &task.repo_owner,
                            &task.repo_name,
                            issue_number,
                            &msg,
                        )
                        .await;
                    }
                }
            }
        }
    }

    // Needs-input: surface the familiar's clarifying question on the issue/PR.
    if result.status == SessionStatus::NeedsInput {
        if let Some(issue_number) = task_issue_number(&task.kind) {
            let msg = format!("I need input before I can continue:\n\n{}", result.summary);
            if let Err(e) = pr::post_comment_with_base_url(
                api_base_url,
                orchestration,
                &task.repo_owner,
                &task.repo_name,
                issue_number,
                &msg,
            )
            .await
            {
                warn!(task_id = %task.id, "failed to post clarifying comment: {e:#}");
            }
        }
    }

    Ok(Published { result, opened_pr })
}

/// Opens the draft PR and posts the PR-opened comment with post-validation
/// publication authority. Best-effort: failures are surfaced on the issue
/// rather than failing the task, since the branch is already pushed.
async fn open_draft_pr(
    config: &Config,
    task: &Task,
    api_base_url: &str,
    publication: &str,
    targets: &ResolvedTargets,
    result: &SessionResult,
    branch: &str,
) -> Option<u64> {
    match pr::open_pull_request_with_base_url(
        api_base_url,
        publication,
        &task.repo_owner,
        &task.repo_name,
        branch,
        &targets.base_ref,
        &pr_title(result, task),
        &result.pr_body,
        true, // draft
    )
    .await
    {
        Ok(pr_num) => {
            if let Some(issue_number) = task_issue_number(&task.kind) {
                let msg = pr_opened_comment(config, &task.id, pr_num);
                if let Err(e) = pr::post_comment_with_base_url(
                    api_base_url,
                    publication,
                    &task.repo_owner,
                    &task.repo_name,
                    issue_number,
                    &msg,
                )
                .await
                {
                    warn!(task_id = %task.id, "failed to post PR comment: {e:#}");
                }
            }
            Some(pr_num)
        }
        Err(e) => {
            // The branch is already pushed; the PR just didn't open. Surface it
            // rather than failing the whole task, so the work isn't lost from
            // the user's view.
            warn!(task_id = %task.id, "failed to open PR: {e:#}");
            if let Some(issue_number) = task_issue_number(&task.kind) {
                let msg = redact::redact(
                    &format!(
                        "I pushed `{branch}` but could not open the PR automatically: {e}. Open the branch manually or check the App's pull-request permission."
                    ),
                    &[publication],
                );
                let _ = pr::post_comment_with_base_url(
                    api_base_url,
                    publication,
                    &task.repo_owner,
                    &task.repo_name,
                    issue_number,
                    &msg,
                )
                .await;
            }
            None
        }
    }
}

/// Terminal disposition of a completed session, derived purely from the result.
///
/// This refines the coarse "success or failure" prose in the headless contract
/// (`docs/headless-contract.md` §4) into the adapter's own Check Run UX. It is an
/// adapter-internal mapping, not part of the coven-code wire contract, so the
/// finer conclusions (`neutral`/`action_required`) require no contract bump:
///
/// | status      | conclusion        | opens PR   | rationale |
/// |-------------|-------------------|------------|-----------|
/// | success     | success           | if commits | work complete |
/// | partial     | neutral           | if commits | progress made, not done — non-blocking |
/// | failure     | failure           | no         | agent gave up |
/// | needs_input | action_required   | no         | human must answer a question |
struct Disposition {
    conclusion: check_run::CheckConclusion,
    title: &'static str,
    open_pr: bool,
}

fn disposition(result: &SessionResult) -> Disposition {
    use check_run::CheckConclusion;

    // The adapter only opens a PR when there is a branch AND commits to review.
    let has_changes = result.branch.is_some() && !result.commits.is_empty();

    match result.status {
        SessionStatus::Success => Disposition {
            conclusion: CheckConclusion::Success,
            title: "Done",
            open_pr: has_changes,
        },
        SessionStatus::Partial => Disposition {
            conclusion: CheckConclusion::Neutral,
            title: "Partial",
            open_pr: has_changes,
        },
        SessionStatus::Failure => Disposition {
            conclusion: CheckConclusion::Failure,
            title: "Failed",
            open_pr: false,
        },
        SessionStatus::NeedsInput => Disposition {
            conclusion: CheckConclusion::ActionRequired,
            title: "Needs input",
            open_pr: false,
        },
    }
}

/// Outcome of a single coven-code invocation, classified per the exit-code
/// contract (`docs/headless-contract.md` §4).
enum Attempt {
    /// The runtime exited 0/1/3 and wrote a parseable `result.json`. Terminal:
    /// the adapter acts on `status`/`exit_reason` and MUST NOT retry.
    Completed(Box<SessionResult>),
    /// Exit 2, timeout, kill-by-signal, an unexpected exit code, or a spawn/read
    /// failure. Retry-safe per the contract.
    RetrySafe(anyhow::Error),
}

async fn run_session(
    config: &Config,
    brief_path: &Path,
    result_path: &Path,
    git_token: &str,
    max_retries: u32,
) -> Result<SessionResult> {
    run_session_with_backoff(
        config,
        brief_path,
        result_path,
        git_token,
        max_retries,
        RETRY_BACKOFF_BASE,
    )
    .await
}

/// Retry loop with an injectable backoff base so tests don't sleep for seconds.
/// Only `Attempt::RetrySafe` failures are retried; `Attempt::Completed` (exit
/// 0/1/3) is returned immediately even when the agent gave up (exit 1) — the
/// retry boundary is exit 2 / timeout / signal, never exit 1 or 3.
async fn run_session_with_backoff(
    config: &Config,
    brief_path: &Path,
    result_path: &Path,
    git_token: &str,
    max_retries: u32,
    backoff_base: Duration,
) -> Result<SessionResult> {
    let mut attempts = 0u32;
    loop {
        match run_coven_code(config, brief_path, result_path, git_token).await {
            Attempt::Completed(result) => return Ok(*result),
            Attempt::RetrySafe(e) if attempts < max_retries => {
                attempts += 1;
                warn!("coven-code attempt {attempts} hit a retry-safe failure ({e:#}), retrying…");
                tokio::time::sleep(backoff_base * 2u32.pow(attempts)).await;
            }
            Attempt::RetrySafe(e) => return Err(e),
        }
    }
}

async fn run_coven_code(
    config: &Config,
    brief_path: &Path,
    result_path: &Path,
    git_token: &str,
) -> Attempt {
    let child = Command::new(&config.worker.coven_code_bin)
        .arg("--headless")
        .arg("--context")
        .arg(brief_path)
        .arg("--output")
        .arg(result_path)
        // Git auth is injected via the environment, never written to the
        // session brief or any durable artifact (issue #4).
        .env("COVEN_GIT_TOKEN", git_token)
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(e) => return Attempt::RetrySafe(anyhow::anyhow!("failed to spawn coven-code: {e}")),
    };

    let status = match tokio::time::timeout(
        Duration::from_secs(config.worker.timeout_secs),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            return Attempt::RetrySafe(anyhow::anyhow!("failed to await coven-code: {e}"))
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Attempt::RetrySafe(anyhow::anyhow!(
                "coven-code timed out after {} seconds",
                config.worker.timeout_secs
            ));
        }
    };

    match status.code() {
        // Terminal outcomes. result.json MUST be present and parseable for these
        // exit codes; if it isn't, the runtime misbehaved — fall back to a
        // retry-safe failure rather than silently losing the task.
        Some(code @ (0 | 1 | 3)) => match read_result(result_path).await {
            Ok(result) => Attempt::Completed(Box::new(result)),
            Err(e) => Attempt::RetrySafe(anyhow::anyhow!(
                "coven-code exited {code} but result.json was unusable: {e}"
            )),
        },
        Some(2) => Attempt::RetrySafe(anyhow::anyhow!("coven-code infra error (exit 2)")),
        Some(code) => Attempt::RetrySafe(anyhow::anyhow!(
            "coven-code exited with unexpected code {code}"
        )),
        None => Attempt::RetrySafe(anyhow::anyhow!("coven-code killed by signal")),
    }
}

async fn read_result(result_path: &Path) -> Result<SessionResult> {
    let bytes = tokio::fs::read(result_path)
        .await
        .map_err(|_| anyhow::anyhow!("result.json not written by coven-code"))?;
    let result: SessionResult = serde_json::from_slice(&bytes)?;
    validate_result_contract(&result)?;
    Ok(result)
}

fn validate_result_contract(result: &SessionResult) -> Result<()> {
    if result.contract_version != coven_github_api::HEADLESS_CONTRACT_VERSION {
        anyhow::bail!(
            "unsupported result contract_version {}; expected {}",
            result.contract_version,
            coven_github_api::HEADLESS_CONTRACT_VERSION
        );
    }
    if result.status == SessionStatus::Success && result.exit_reason.is_some() {
        anyhow::bail!("result exit_reason must be null when status is success");
    }
    if matches!(
        result.status,
        SessionStatus::Failure | SessionStatus::NeedsInput
    ) && result.exit_reason.is_none()
    {
        anyhow::bail!(
            "result exit_reason is required when status is {:?}",
            result.status
        );
    }
    let is_review_mode = matches!(
        result.review.mode,
        ReviewMode::PullRequest | ReviewMode::ReviewComment
    );
    if is_review_mode {
        if result.review.evidence_status == ReviewEvidenceStatus::NotApplicable {
            anyhow::bail!(
                "review evidence_status not_applicable is invalid for {:?}",
                result.review.mode
            );
        }
        if result.review.evidence_status != ReviewEvidenceStatus::Missing
            && result.review.reviewed_files.is_empty()
        {
            anyhow::bail!(
                "reviewed_files is required for review mode {:?}",
                result.review.mode
            );
        }
        if result.review.evidence_status == ReviewEvidenceStatus::Complete
            && result.review.findings.is_empty()
            && result
                .review
                .no_findings_reason
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            anyhow::bail!("no_findings_reason is required when complete review findings are empty");
        }
    }
    if result.review.mode == ReviewMode::None
        && result.review.evidence_status != ReviewEvidenceStatus::NotApplicable
    {
        anyhow::bail!(
            "review evidence_status {:?} is invalid for none mode",
            result.review.evidence_status
        );
    }
    Ok(())
}

fn cave_base_url(config: &Config) -> &str {
    config
        .server
        .cave_base_url
        .as_deref()
        .unwrap_or(DEFAULT_CAVE_BASE_URL)
}

fn cave_session_url(config: &Config, task_id: &str) -> String {
    format!(
        "{}/sessions/{task_id}",
        cave_base_url(config).trim_end_matches('/')
    )
}

fn starting_comment(config: &Config, familiar: &FamiliarConfig, task_id: &str) -> String {
    format!(
        "{} is working on this.\n\nSession: {}\n\nI'll open a draft PR if the run produces reviewable changes.",
        familiar.display_name,
        cave_session_url(config, task_id)
    )
}

fn pr_opened_comment(config: &Config, task_id: &str, pr_number: u64) -> String {
    format!(
        "PR #{pr_number} opened.\n\nSession: {}",
        cave_session_url(config, task_id)
    )
}

fn pr_title(result: &SessionResult, task: &Task) -> String {
    format!(
        "{} (#{} via Coven)",
        result.summary,
        task_issue_number(&task.kind).unwrap_or(0)
    )
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

#[cfg(test)]
mod result_tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn read_result_rejects_unsupported_contract_version() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-version-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"1","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("v1 result must be rejected");
        assert!(
            format!("{error:#}").contains("unsupported result contract_version 1"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_missing_contract_version() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-missing-version-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("missing contract_version result must be rejected");
        assert!(
            format!("{error:#}").contains("missing field `contract_version`"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_success_with_exit_reason() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-success-exit-reason-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":"infra_error"}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("success result must reject non-null exit_reason");
        assert!(
            format!("{error:#}").contains("must be null when status is success"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_failure_without_exit_reason() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-failure-exit-reason-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"failure","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("non-success result must require exit_reason");
        assert!(
            format!("{error:#}").contains("exit_reason is required"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_unknown_root_field() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-extra-root-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null,"extra_root_field":"rejected"}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("unknown root field must be rejected");
        assert!(
            format!("{error:#}").contains("unknown field `extra_root_field`"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_unknown_review_field() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-extra-review-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[],"extra_review_field":"rejected"},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("unknown review field must be rejected");
        assert!(
            format!("{error:#}").contains("unknown field `extra_review_field`"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_not_applicable_evidence_for_review_modes() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-review-evidence-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"pull_request","evidence_status":"not_applicable","reviewed_files":["src/lib.rs"],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":"reviewed supplied file","limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("review result must reject not_applicable evidence");
        assert!(
            format!("{error:#}").contains("review evidence_status not_applicable is invalid"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_review_without_reviewed_files() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-review-files-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"pull_request","evidence_status":"complete","reviewed_files":[],"supporting_files":[],"findings":[{"severity":"low","file":"src/lib.rs","line":null,"title":"t","body":"b","recommendation":null}],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("review result must reject missing reviewed_files");
        assert!(
            format!("{error:#}").contains("reviewed_files is required"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_accepts_review_findings_with_reason() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-findings-reason-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"pull_request","evidence_status":"complete","reviewed_files":["src/lib.rs"],"supporting_files":[],"findings":[{"severity":"low","file":"src/lib.rs","line":null,"title":"t","body":"b","recommendation":null}],"tests_run":[],"no_findings_reason":"Also reviewed nearby context and found this issue.","limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let result = read_result(&path)
            .await
            .expect("findings with a reason should remain valid");
        assert_eq!(result.review.findings.len(), 1);
        assert_eq!(
            result.review.no_findings_reason.as_deref(),
            Some("Also reviewed nearby context and found this issue.")
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_accepts_partial_review_without_reason() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-review-reason-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"partial","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"pull_request","evidence_status":"partial","reviewed_files":["src/lib.rs"],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":["Review output was degraded before a clean-review conclusion."]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let result = read_result(&path)
            .await
            .expect("partial degraded review result should remain valid");
        assert_eq!(result.status, SessionStatus::Partial);
        assert!(result.exit_reason.is_none());
        assert!(result.review.findings.is_empty());
        assert!(result.review.no_findings_reason.is_none());

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_complete_review_without_reason() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-complete-review-reason-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"pull_request","evidence_status":"complete","reviewed_files":["src/lib.rs"],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":"   ","limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("complete empty review result must require a reason");
        assert!(
            format!("{error:#}").contains("complete review findings are empty"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn read_result_rejects_applicable_evidence_for_none_mode() {
        let path = std::env::temp_dir().join(format!(
            "coven-github-result-none-evidence-{}.json",
            uuid::Uuid::new_v4()
        ));
        fs::write(
            &path,
            r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"complete","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}"#,
        )
        .expect("result fixture should be written");

        let error = read_result(&path)
            .await
            .expect_err("none-mode result must reject applicable evidence");
        assert!(
            format!("{error:#}").contains("is invalid for none mode"),
            "unexpected error: {error:#}"
        );

        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod disposition_tests {
    use super::*;
    use coven_github_api::{CommitInfo, ReviewResult, HEADLESS_CONTRACT_VERSION};
    use coven_github_config::{GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::path::PathBuf;

    fn result(status: SessionStatus, branch: Option<&str>, commits: usize) -> SessionResult {
        SessionResult {
            contract_version: HEADLESS_CONTRACT_VERSION.to_string(),
            status,
            branch: branch.map(str::to_string),
            commits: (0..commits)
                .map(|i| CommitInfo {
                    sha: format!("sha{i}"),
                    message: "msg".to_string(),
                })
                .collect(),
            files_changed: vec![],
            summary: "summary".to_string(),
            pr_body: "body".to_string(),
            review: ReviewResult::none(),
            exit_reason: None,
        }
    }

    #[test]
    fn success_with_commits_opens_pr_and_concludes_success() {
        let disp = disposition(&result(SessionStatus::Success, Some("cody/fix"), 1));
        assert!(disp.open_pr);
        assert!(matches!(
            disp.conclusion,
            check_run::CheckConclusion::Success
        ));
    }

    #[test]
    fn success_without_commits_does_not_open_pr() {
        let disp = disposition(&result(SessionStatus::Success, None, 0));
        assert!(!disp.open_pr);
        assert!(matches!(
            disp.conclusion,
            check_run::CheckConclusion::Success
        ));
    }

    #[test]
    fn partial_with_commits_opens_pr_but_concludes_neutral() {
        let disp = disposition(&result(SessionStatus::Partial, Some("cody/fix"), 2));
        assert!(disp.open_pr);
        assert!(matches!(
            disp.conclusion,
            check_run::CheckConclusion::Neutral
        ));
    }

    #[test]
    fn failure_never_opens_pr_and_concludes_failure() {
        // Even if the agent left a branch behind, a failed session opens no PR.
        let disp = disposition(&result(SessionStatus::Failure, Some("cody/fix"), 3));
        assert!(!disp.open_pr);
        assert!(matches!(
            disp.conclusion,
            check_run::CheckConclusion::Failure
        ));
    }

    #[test]
    fn needs_input_concludes_action_required_without_pr() {
        let disp = disposition(&result(SessionStatus::NeedsInput, None, 0));
        assert!(!disp.open_pr);
        assert!(matches!(
            disp.conclusion,
            check_run::CheckConclusion::ActionRequired
        ));
    }

    fn config_with_cave(base_url: Option<&str>) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: base_url.map(str::to_string),
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("private.pem"),
                webhook_secret: "secret".to_string(),
                api_base_url: None,
            },
            worker: WorkerConfig {
                concurrency: 1,
                coven_code_bin: PathBuf::from("coven-code"),
                workspace_root: PathBuf::from("/tmp/coven-github-test"),
                timeout_secs: 30,
                max_retries: 1,
            },
            familiars: vec![],
        }
    }

    #[test]
    fn cave_session_url_targets_the_exact_session() {
        let config = config_with_cave(Some("https://cave.example.test/"));
        assert_eq!(
            cave_session_url(&config, "task-42"),
            "https://cave.example.test/sessions/task-42"
        );
    }

    #[test]
    fn cave_session_url_uses_hosted_default_when_unset() {
        let config = config_with_cave(None);
        assert_eq!(
            cave_session_url(&config, "task-42"),
            "https://cave.opencoven.ai/sessions/task-42"
        );
    }

    #[test]
    fn task_comments_are_direct_and_link_to_the_session() {
        let config = config_with_cave(Some("https://cave.example.test"));
        let familiar = FamiliarConfig {
            id: "cody".to_string(),
            display_name: "Cody".to_string(),
            bot_username: "coven-cody[bot]".to_string(),
            model: None,
            skills: vec![],
            trigger_labels: vec![],
        };

        let started = starting_comment(&config, &familiar, "task-42");
        assert!(started.contains("Cody is working on this"));
        assert!(started.contains("https://cave.example.test/sessions/task-42"));
        assert!(
            !started.contains('👋') && !started.contains('→'),
            "starting comment should be calm operator copy, not decorative chrome: {started}"
        );

        let opened = pr_opened_comment(&config, "task-42", 17);
        assert!(opened.contains("PR #17 opened"));
        assert!(opened.contains("https://cave.example.test/sessions/task-42"));
        assert!(
            !opened.contains('✅') && !opened.contains('→'),
            "PR comment should stay concise and actionable: {opened}"
        );
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::*;
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{Duration, Instant},
    };

    fn test_config(coven_code_bin: PathBuf, workspace_root: PathBuf, max_retries: u32) -> Config {
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
                // Generous default so exit-code tests never race the kill timer.
                // The timeout test overrides this to a short value on purpose.
                timeout_secs: 30,
                max_retries,
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

    /// Builds an isolated temp dir for one test and writes an executable script
    /// (the fake coven-code binary) into it. Returns (root, script_path).
    fn scratch(name: &str, script: &str) -> (PathBuf, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("coven-github-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test dir should be created");
        let script_path = root.join("fake-coven-code.sh");
        fs::write(&script_path, script).expect("script should be written");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("script should be executable");
        (root, script_path)
    }

    /// A minimal contract-valid result.json with the given status/exit_reason.
    fn result_json(status: &str, exit_reason: &str) -> String {
        format!(
            r#"{{"contract_version":"2","status":"{status}","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]}},"exit_reason":{exit_reason}}}"#
        )
    }

    #[tokio::test]
    async fn coven_code_process_is_stopped_after_configured_timeout() {
        let (root, script) = scratch("timeout-test", "#!/usr/bin/env bash\nsleep 5\n");
        let mut config = test_config(script, root.clone(), 0);
        // This test specifically exercises the kill-on-timeout path.
        config.worker.timeout_secs = 1;
        let brief_path = root.join("session-brief.json");
        let result_path = root.join("result.json");
        fs::write(&brief_path, "{}").expect("brief should be written");

        let started = Instant::now();
        let attempt = run_coven_code(&config, &brief_path, &result_path, "test-token").await;

        assert!(matches!(attempt, Attempt::RetrySafe(_)));
        assert!(
            started.elapsed().as_secs() < 3,
            "process should stop close to the configured timeout"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_zero_with_result_is_completed() {
        let body = result_json("success", "null");
        let script = format!("#!/usr/bin/env bash\ncat > \"$5\" <<'EOF'\n{body}\nEOF\nexit 0\n");
        let (root, path) = scratch("exit0", &script);
        let config = test_config(path, root.clone(), 0);
        let brief = root.join("session-brief.json");
        let result = root.join("result.json");
        fs::write(&brief, "{}").unwrap();

        let attempt = run_coven_code(&config, &brief, &result, "tok").await;
        match attempt {
            Attempt::Completed(r) => assert_eq!(r.status, SessionStatus::Success),
            Attempt::RetrySafe(e) => panic!("expected Completed, got RetrySafe: {e:#}"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_three_needs_input_is_completed_not_retried() {
        let body = result_json("needs_input", "\"ambiguous_spec\"");
        let script = format!("#!/usr/bin/env bash\ncat > \"$5\" <<'EOF'\n{body}\nEOF\nexit 3\n");
        let (root, path) = scratch("exit3", &script);
        let config = test_config(path, root.clone(), 0);
        let brief = root.join("session-brief.json");
        let result = root.join("result.json");
        fs::write(&brief, "{}").unwrap();

        let attempt = run_coven_code(&config, &brief, &result, "tok").await;
        match attempt {
            Attempt::Completed(r) => assert_eq!(r.status, SessionStatus::NeedsInput),
            Attempt::RetrySafe(e) => panic!("exit 3 must be terminal, got RetrySafe: {e:#}"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_two_is_retry_safe() {
        let (root, path) = scratch("exit2", "#!/usr/bin/env bash\nexit 2\n");
        let config = test_config(path, root.clone(), 0);
        let brief = root.join("session-brief.json");
        let result = root.join("result.json");
        fs::write(&brief, "{}").unwrap();

        let attempt = run_coven_code(&config, &brief, &result, "tok").await;
        assert!(matches!(attempt, Attempt::RetrySafe(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_one_failure_is_not_retried_and_surfaces_result() {
        // Exit 1 = agent gave up; the contract forbids retrying it. The script
        // records each invocation so we can assert it ran exactly once.
        let body = result_json("failure", "\"test_failure\"");
        let script = format!(
            "#!/usr/bin/env bash\necho x >> \"$(dirname \"$5\")/runs\"\ncat > \"$5\" <<'EOF'\n{body}\nEOF\nexit 1\n"
        );
        let (root, path) = scratch("exit1", &script);
        let config = test_config(path, root.clone(), 2); // budget of 2 retries
        let brief = root.join("session-brief.json");
        let result = root.join("result.json");
        fs::write(&brief, "{}").unwrap();

        let session = run_session_with_backoff(
            &config,
            &brief,
            &result,
            "tok",
            config.worker.max_retries,
            Duration::from_millis(1),
        )
        .await
        .expect("exit 1 yields a terminal result, not an error");
        assert_eq!(session.status, SessionStatus::Failure);

        let runs = fs::read_to_string(root.join("runs")).unwrap_or_default();
        assert_eq!(
            runs.lines().count(),
            1,
            "exit 1 must run exactly once (no retries)"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_two_retries_until_budget_exhausted_then_errors() {
        // Exit 2 is retry-safe: with max_retries=2 the binary runs 1 + 2 = 3
        // times before the session gives up with an error.
        let script =
            "#!/usr/bin/env bash\necho x >> \"$(dirname \"$5\")/runs\"\nexit 2\n".to_string();
        let (root, path) = scratch("exit2-retries", &script);
        let config = test_config(path, root.clone(), 2);
        let brief = root.join("session-brief.json");
        let result = root.join("result.json");
        fs::write(&brief, "{}").unwrap();

        let err = run_session_with_backoff(
            &config,
            &brief,
            &result,
            "tok",
            config.worker.max_retries,
            Duration::from_millis(1),
        )
        .await;
        assert!(err.is_err(), "exhausted retry budget should error");

        let runs = fs::read_to_string(root.join("runs")).unwrap_or_default();
        assert_eq!(
            runs.lines().count(),
            3,
            "exit 2 should run 1 + max_retries times"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exit_zero_without_result_file_is_retry_safe() {
        // A runtime that exits 0 but never writes result.json misbehaved; treat
        // it as retry-safe rather than crashing the task.
        let (root, path) = scratch("exit0-noresult", "#!/usr/bin/env bash\nexit 0\n");
        let config = test_config(path, root.clone(), 0);
        let brief = root.join("session-brief.json");
        let result = root.join("result.json");
        fs::write(&brief, "{}").unwrap();

        let attempt = run_coven_code(&config, &brief, &result, "tok").await;
        assert!(matches!(attempt, Attempt::RetrySafe(_)));
        let _ = fs::remove_dir_all(root);
    }
}
