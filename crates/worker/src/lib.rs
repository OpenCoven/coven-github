//! Worker: pulls tasks from the queue, spawns coven-code sessions, streams progress.

use anyhow::Result;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

use coven_github_api::{
    check_run, installation, installation::TokenRole, pr, repo, ReviewEvidenceStatus, ReviewMode,
    SessionResult, SessionStatus, Task, TaskKind, DEFAULT_API_BASE_URL,
};
use coven_github_config::{Config, FamiliarConfig};
use coven_github_store::{Store, Terminal, TerminalState};

pub mod brief;
pub mod findings;
pub mod memory;
pub mod redact;
pub mod status_comment;

/// Base unit for exponential backoff between retry-safe coven-code attempts.
/// Attempt `n` sleeps `RETRY_BACKOFF_BASE * 2^n` (so 2s, 4s, … in production).
const RETRY_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Default Cave base URL used in familiar-voice comments when none is configured.
const DEFAULT_CAVE_BASE_URL: &str = "https://cave.opencoven.ai";

/// Runs the worker loop: claims queued tasks from the durable store and
/// executes them concurrently (issue #2). `notify` is a wake-up signal from
/// the webhook path; a poll-timeout backstops missed wake-ups.
pub async fn run(
    config: std::sync::Arc<Config>,
    store: Store,
    notify: std::sync::Arc<tokio::sync::Notify>,
) {
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(config.worker.concurrency));

    loop {
        // Hold capacity BEFORE claiming so a claimed task is never parked
        // behind a saturated pool while marked running.
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        match store.claim_next().await {
            Ok(Some(task)) => {
                let config = config.clone();
                let store = store.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = execute_task(&config, store, task).await {
                        error!("task execution error: {e:#}");
                    }
                });
            }
            Ok(None) => {
                drop(permit);
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
            Err(e) => {
                drop(permit);
                error!("failed to claim from the durable queue: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn execute_task(config: &Config, store: Store, task: Task) -> Result<()> {
    let api_base_url = config
        .github
        .api_base_url
        .as_deref()
        .unwrap_or(DEFAULT_API_BASE_URL);
    let private_key = std::fs::read_to_string(&config.github.private_key_path)?;
    let minter = Minter::App {
        api_base_url: api_base_url.to_string(),
        app_id: config.github.app_id,
        private_key,
        installation_id: task.installation_id,
        repo_name: task.repo_name.clone(),
    };
    execute_task_with_minter(config, store, task, &minter).await
}

/// Pre-flight outcome: ready to work, or declined at the permission gate.
enum Prepared {
    Ready {
        orchestration: String,
        targets: ResolvedTargets,
        check_id: u64,
    },
    Declined {
        orchestration: String,
    },
}

/// Task execution past minter construction; tests inject `Minter::Fixed`.
async fn execute_task_with_minter(
    config: &Config,
    store: Store,
    task: Task,
    minter: &Minter,
) -> Result<()> {
    let familiar = config
        .familiars
        .iter()
        .find(|f| f.id == task.familiar_id)
        .ok_or_else(|| anyhow::anyhow!("unknown familiar: {}", task.familiar_id))?;
    let api_base_url = config
        .github
        .api_base_url
        .as_deref()
        .unwrap_or(DEFAULT_API_BASE_URL);

    // Adapter-only command surfaces (issue #13): replies, acknowledgements,
    // and cancellations run without a coven-code session or Check Run — but
    // gated acknowledgements and cancellations still verify the commander's
    // write access first, so a drive-by comment earns a decline, not an act.
    match &task.kind {
        TaskKind::CommandReply { issue_number, body } => {
            let orchestration = minter.mint(TokenRole::Orchestration).await?;
            let reply = if commander_below_write(api_base_url, &orchestration, &task).await? {
                info!(task_id = %task.id, "declining gated reply for a commander without write access");
                decline_body(&task)
            } else {
                body.clone()
            };
            let marker = status_comment::marker(
                &familiar.id,
                &task.repo_owner,
                &task.repo_name,
                *issue_number,
            );
            status_comment::upsert(
                api_base_url,
                &orchestration,
                &task.repo_owner,
                &task.repo_name,
                *issue_number,
                &marker,
                &reply,
            )
            .await?;
            store
                .finish(
                    &task.id,
                    Terminal {
                        state: TerminalState::Completed,
                        ..Terminal::default()
                    },
                )
                .await?;
            return Ok(());
        }
        TaskKind::CancelReviews { pr_number } => {
            let orchestration = minter.mint(TokenRole::Orchestration).await?;
            let reply = if commander_below_write(api_base_url, &orchestration, &task).await? {
                info!(task_id = %task.id, "declining cancel for a commander without write access");
                decline_body(&task)
            } else {
                // The tombstone happens only past the gate: queued reviews of
                // this PR yield; in-flight work finishes (#8 covers staleness).
                let key = format!("{}/{}#{pr_number}", task.repo_owner, task.repo_name);
                let cancelled = store.cancel_queued(&key).await?;
                format!(
                    "Cancelled {cancelled} queued review(s) for PR #{pr_number}. Work already \
                     running will finish; `@{} review` re-arms the lane.",
                    familiar.bot_username.trim_end_matches("[bot]")
                )
            };
            let marker = status_comment::marker(
                &familiar.id,
                &task.repo_owner,
                &task.repo_name,
                *pr_number,
            );
            status_comment::upsert(
                api_base_url,
                &orchestration,
                &task.repo_owner,
                &task.repo_name,
                *pr_number,
                &marker,
                &reply,
            )
            .await?;
            store
                .finish(
                    &task.id,
                    Terminal {
                        state: TerminalState::Completed,
                        ..Terminal::default()
                    },
                )
                .await?;
            return Ok(());
        }
        _ => {}
    }

    info!(task_id = %task.id, familiar = %familiar.id, "starting task");

    // Pre-flight: installation token, ref resolution, and Check Run creation.
    // These run *before* the Check Run exists, so a failure here can't orphan a
    // check — but it would otherwise make the task vanish silently. Record it as
    // failed so it stays visible in Cave, then propagate.
    let prepared = async {
        // Adapter-held orchestration authority: resolve refs, drive the Check
        // Run, post progress comments. The agent never sees this token.
        let orchestration = minter.mint(TokenRole::Orchestration).await?;

        // Maintainer permission gate (issue #13): command-initiated work needs
        // write access on the repo before the adapter spends anything on it.
        if commander_below_write(api_base_url, &orchestration, &task).await? {
            return Ok(Prepared::Declined { orchestration });
        }

        // A now-authorized command review supersedes older queued reviews of
        // the same PR. Auto reviews tombstone at insert; command reviews wait
        // for this gate so unauthorized commenters can't displace queued work.
        if task.commander.is_some() {
            if let TaskKind::ReviewPullRequest { pr_number, .. } = &task.kind {
                let key = format!("{}/{}#{pr_number}", task.repo_owner, task.repo_name);
                store.supersede_queued_except(&key, &task.id).await?;
            }
        }

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
        Ok::<_, anyhow::Error>(Prepared::Ready {
            orchestration,
            targets,
            check_id,
        })
    }
    .await;

    let (orchestration, targets, check_id) = match prepared {
        Ok(Prepared::Ready {
            orchestration,
            targets,
            check_id,
        }) => (orchestration, targets, check_id),
        Ok(Prepared::Declined { orchestration }) => {
            // Below-write commander: decline on the status surface, do no work.
            info!(task_id = %task.id, "declining command from a commander without write access");
            if let Some(number) = surface_number(&task.kind) {
                let marker = status_comment::marker(
                    &familiar.id,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                );
                let body = decline_body(&task);
                if let Err(e) = status_comment::upsert(
                    api_base_url,
                    &orchestration,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                    &marker,
                    &body,
                )
                .await
                {
                    warn!(task_id = %task.id, "failed to post decline: {e:#}");
                }
            }
            store
                .finish(
                    &task.id,
                    Terminal {
                        state: TerminalState::Completed,
                        summary: Some("declined — maintainer commands need write access".into()),
                        ..Terminal::default()
                    },
                )
                .await?;
            return Ok(());
        }
        Err(e) => {
            error!(task_id = %task.id, "pre-flight failed before check run: {e:#}");
            store
                .finish(
                    &task.id,
                    Terminal {
                        state: TerminalState::Failed,
                        detail: Some(redact::redact(&format!("{e:#}"), &[])),
                        ..Terminal::default()
                    },
                )
                .await
                .ok();
            return Err(e);
        }
    };

    let repo = format!("{}/{}", task.repo_owner, task.repo_name);
    let check_run_url = format!("https://github.com/{repo}/runs/{check_id}");
    if let Err(e) = store.set_check_run_url(&task.id, &check_run_url).await {
        warn!(task_id = %task.id, "failed to record check run url: {e:#}");
    }

    // Everything past check creation is fallible but must not orphan the check
    // or leak the workspace. Run it, then finalize unconditionally below.
    let workspace = config.worker.workspace_root.join(&task.id);
    let outcome = run_and_publish(
        config,
        &task,
        familiar,
        minter,
        &orchestration,
        api_base_url,
        &targets,
        &workspace,
        check_id,
        &store,
    )
    .await;

    // Workspace cleanup ALWAYS runs — success or failure.
    tokio::fs::remove_dir_all(&workspace).await.ok();

    // The Check Run ALWAYS reaches a terminal conclusion; both arms complete it.
    match outcome {
        // Head moved mid-review (issue #8): the findings describe a commit the
        // PR no longer points at. Mark superseded everywhere — never publish
        // stale review output as if it covered the current head.
        Ok(published) if published.stale.is_some() => {
            let stale = published.stale.as_ref().expect("guarded by arm");
            store
                .finish(
                    &task.id,
                    Terminal {
                        state: TerminalState::Superseded,
                        summary: Some(format!(
                            "head moved {} -> {} mid-review",
                            stale.reviewed_sha, stale.current_sha
                        )),
                        ..Terminal::default()
                    },
                )
                .await
                .ok();
            if let Some(number) = surface_number(&task.kind) {
                let marker = status_comment::marker(
                    &familiar.id,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                );
                let body = format!(
                    "Status: superseded\n\nThe PR head moved from `{}` to `{}` while the \
                     review ran, so these findings no longer describe the current diff. \
                     The newer push is reviewed by its own event, or re-run with a \
                     `retry` command.",
                    stale.reviewed_sha, stale.current_sha
                );
                if let Err(e) = status_comment::upsert(
                    api_base_url,
                    &orchestration,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                    &marker,
                    &body,
                )
                .await
                {
                    warn!(task_id = %task.id, "failed to upsert superseded status: {e:#}");
                }
            }
            if let Err(e) = check_run::complete_with_base_url(
                api_base_url,
                &orchestration,
                &task.repo_owner,
                &task.repo_name,
                check_id,
                check_run::CheckConclusion::Neutral,
                "Stale",
                &format!(
                    "Reviewed {}, but the PR head is now {} — findings withheld as stale.",
                    stale.reviewed_sha, stale.current_sha
                ),
            )
            .await
            {
                error!(task_id = %task.id, "failed to finalize stale check run: {e:#}");
            }
        }
        Ok(published) => {
            let disp = disposition(&published.result);
            store
                .finish(&task.id, terminal_of(&published))
                .await
                .ok();
            // Findings pass the deterministic publication gates before any
            // surface sees them (issue #11): scope, severity policy, dedupe.
            // The digest always lands on the Check Run; policy can add the
            // status comment (advisory) or a blocking PR review verdict.
            let mut check_summary = published.result.summary.clone();
            let mut advisory: Option<String> = None;
            if let TaskKind::ReviewPullRequest { pr_number, .. } = &task.kind {
                let repo_key = format!("{}/{}", task.repo_owner, task.repo_name);
                let min_severity = config
                    .review
                    .min_severity_for(&repo_key)
                    .as_deref()
                    .and_then(findings::parse_severity);
                let outcome = findings::gate(
                    &published.result.review,
                    &published.changed_files,
                    min_severity,
                );
                let report = findings::render(&outcome);
                check_summary = format!("{check_summary}\n\n{report}");
                match config.review.publish_for(&repo_key).as_deref() {
                    Some("advisory_comment") => advisory = Some(report),
                    Some("request_changes") => {
                        // Blocking verdicts need write authority: mint the
                        // publication token only now, post-gates (issue #4).
                        let verdict = if outcome.published.is_empty() {
                            pr::ReviewVerdict::Comment
                        } else {
                            pr::ReviewVerdict::RequestChanges
                        };
                        match minter.mint(TokenRole::Publication).await {
                            Ok(publication) => {
                                if let Err(e) = pr::submit_review_with_base_url(
                                    api_base_url,
                                    &publication,
                                    &task.repo_owner,
                                    &task.repo_name,
                                    *pr_number,
                                    verdict,
                                    &check_summary,
                                )
                                .await
                                {
                                    warn!(task_id = %task.id, "failed to submit PR review: {e:#}");
                                }
                            }
                            Err(e) => {
                                warn!(task_id = %task.id, "failed to mint publication token for review verdict: {e:#}");
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Terminal state on the marker-backed status surface (issue #13).
            if let Some(number) = surface_number(&task.kind) {
                let marker = status_comment::marker(
                    &familiar.id,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                );
                let mut body =
                    final_status_body(
                        config,
                        &task.id,
                        &published.result,
                        published.opened_pr,
                        &published.cited_memory,
                    );
                if let Some(report) = &advisory {
                    body = format!("{body}\n\n{report}");
                }
                if let Err(e) = status_comment::upsert(
                    api_base_url,
                    &orchestration,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                    &marker,
                    &body,
                )
                .await
                {
                    warn!(task_id = %task.id, "failed to upsert final status: {e:#}");
                }
            }
            if let Err(e) = check_run::complete_with_base_url(
                api_base_url,
                &orchestration,
                &task.repo_owner,
                &task.repo_name,
                check_id,
                disp.conclusion,
                disp.title,
                &check_summary,
            )
            .await
            {
                error!(task_id = %task.id, "failed to finalize check run: {e:#}");
            }
        }
        Err(e) => {
            error!(task_id = %task.id, "session failed: {e:#}");
            store
                .finish(
                    &task.id,
                    Terminal {
                        state: TerminalState::Failed,
                        detail: Some(redact::redact(&format!("{e:#}"), &[&orchestration])),
                        ..Terminal::default()
                    },
                )
                .await
                .ok();
            if let Some(number) = surface_number(&task.kind) {
                let marker = status_comment::marker(
                    &familiar.id,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                );
                let body = redact::redact(
                    &format!("Status: failed\n\nTask failed: {e}"),
                    &[&orchestration],
                );
                if let Err(ce) = status_comment::upsert(
                    api_base_url,
                    &orchestration,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                    &marker,
                    &body,
                )
                .await
                {
                    warn!(task_id = %task.id, "failed to upsert failure status: {ce:#}");
                }
            }
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
    /// PR changed-file list fetched from live GitHub state (review tasks) —
    /// one input to the findings scope gate (issue #11).
    changed_files: Vec<String>,
    /// Set when a PR review's head moved while the session ran (issue #8):
    /// the findings were computed against `reviewed_sha`, but the PR is now
    /// at `current_sha`. Publication must mark the task superseded instead of
    /// presenting stale findings as current.
    stale: Option<StaleRefs>,
    /// Memory entry ids the review read and the adapter accepted — cited on
    /// the status surface for transparency (issue #6).
    cited_memory: Vec<String>,
}

/// Evidence of a mid-session head move on a reviewed PR.
struct StaleRefs {
    reviewed_sha: String,
    current_sha: String,
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
    #[cfg(test)]
    Fixed(std::collections::HashMap<TokenRole, String>),
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
            #[cfg(test)]
            Minter::Fixed(tokens) => tokens
                .get(&role)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no fixed token for role {role:?}")),
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
/// Maps the runtime's reported memory activity to audit rows, stamping each
/// with the adapter's verdict (`accepted` or `rejected:<reason>`) from the
/// validation pass (issue #6).
fn memory_activity_rows(
    installation_id: u64,
    repo: &str,
    task_id: &str,
    used: &coven_github_api::MemoryUsed,
    rejections: &[memory::MemoryRejection],
) -> Vec<coven_github_store::MemoryActivity> {
    let verdict = |op: memory::MemoryOp, target: &str| {
        rejections
            .iter()
            .find(|r| r.op == op && r.target == target)
            .map(|r| format!("rejected:{}", r.reason))
            .unwrap_or_else(|| "accepted".to_string())
    };
    let row = |op: &str, target: &str, scope: &str, outcome: String| {
        coven_github_store::MemoryActivity {
            at: String::new(),
            installation_id,
            repo: repo.to_string(),
            task_id: task_id.to_string(),
            op: op.to_string(),
            target: target.to_string(),
            scope: scope.to_string(),
            outcome,
        }
    };
    let mut rows = Vec::new();
    for entry in &used.read {
        rows.push(row(
            "read",
            &entry.id,
            &entry.scope,
            verdict(memory::MemoryOp::Read, &entry.id),
        ));
    }
    for write in &used.proposed {
        rows.push(row(
            "write",
            &write.key,
            &write.scope,
            verdict(memory::MemoryOp::Write, &write.key),
        ));
    }
    rows
}

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
    store: &Store,
) -> Result<Published> {
    // Provision ephemeral workspace and write the tokenless session brief.
    tokio::fs::create_dir_all(workspace).await?;
    // Hosted reviews carry tokenless changed-file context so the runtime can
    // prove coverage (issue #10); other task kinds brief without it.
    let review = match &task.kind {
        TaskKind::ReviewPullRequest {
            pr_number, reason, ..
        } => {
            let files = repo::get_pull_request_files_with_base_url(
                api_base_url,
                orchestration,
                &task.repo_owner,
                &task.repo_name,
                *pr_number,
            )
            .await?;
            let mut audit = config
                .review
                .audit_instruction_for(&format!("{}/{}", task.repo_owner, task.repo_name));
            // A `deepen` command widens the lens beyond the changed set.
            if reason == "command:deepen" {
                let depth = "Perform a deep review: inspect supporting files beyond the \
                             changed set and verify behavior with tests where possible.";
                audit = Some(match audit {
                    Some(existing) => format!("{existing}\n\n{depth}"),
                    None => depth.to_string(),
                });
            }
            Some(brief::ReviewContext {
                files,
                audit_instruction: audit,
            })
        }
        _ => None,
    };
    // Compute the memory governance policy (issue #6). Deny-by-default: unless
    // the installation opts memory in for this repo, this is None and no policy
    // is stamped, so the runtime does no memory work. Trust is derived from the
    // review target and actor: a fork PR is untrusted content and can never
    // write durable memory, overriding even a maintainer trigger.
    let repo_full = format!("{}/{}", task.repo_owner, task.repo_name);
    // Revoked memory for this repo (issue #6): the adapter refuses these on the
    // result and passes them to the runtime so it stops surfacing them.
    let denied = if config.memory.enabled_for(&repo_full) {
        store
            .revocations_for(task.installation_id, &repo_full)
            .await
            .unwrap_or_else(|e| {
                warn!(task_id = %task.id, "failed to load memory revocations: {e:#}");
                Vec::new()
            })
    } else {
        Vec::new()
    };
    let memory_policy = memory::compute_policy(memory::PolicyInputs {
        enabled: config.memory.enabled_for(&repo_full),
        installation_id: task.installation_id,
        repo: &repo_full,
        trust: memory::derive_trust(targets.head_is_fork, task.commander.is_some()),
        approval_required: config.memory.approval_required_for(&repo_full),
        retention_days: config.memory.retention_days,
        denied,
    });
    let brief = brief::build(
        task,
        familiar,
        workspace,
        &targets.default_branch,
        review.as_ref(),
        memory_policy
            .as_ref()
            .map(|p| serde_json::to_value(p).expect("memory policy serializes")),
    );
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

    // Best-effort status surface (issue #13): one marker-backed comment per
    // target, edited in place — a flaky comment API call must not abort the
    // task or orphan the Check Run.
    if let Some(number) = surface_number(&task.kind) {
        let marker =
            status_comment::marker(&familiar.id, &task.repo_owner, &task.repo_name, number);
        let start_msg = starting_comment(config, familiar, &task.id);
        if let Err(e) = status_comment::upsert(
            api_base_url,
            orchestration,
            &task.repo_owner,
            &task.repo_name,
            number,
            &marker,
            &start_msg,
        )
        .await
        {
            warn!(task_id = %task.id, "failed to upsert status comment: {e:#}");
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

    // Re-validate the runtime's reported memory activity against the policy we
    // granted (issue #6). The runtime's self-report is not trusted on its own:
    // any read or write outside scope — including a fork PR that tried to write
    // durable memory — is refused here before it can be persisted.
    let mut cited_memory: Vec<String> = Vec::new();
    if let (Some(policy), Some(used)) = (&memory_policy, &result.memory_used) {
        let rejections =
            memory::validate_memory_used(policy, used, |text| redact::redact(text, &[]) != text);
        if !rejections.is_empty() {
            warn!(
                task_id = %task.id,
                rejected = rejections.len(),
                "refused out-of-policy memory activity — not persisting those entries"
            );
        }
        // Record every reported read/write with the adapter's verdict so a
        // customer can inspect what memory a familiar used on their repo (#6).
        let activity =
            memory_activity_rows(task.installation_id, &repo_full, &task.id, used, &rejections);
        if let Err(e) = store.record_memory_activity(activity).await {
            warn!(task_id = %task.id, "failed to record memory activity: {e:#}");
        }
        // Cite the reads the adapter accepted (not refused/revoked) so the
        // review discloses which memory influenced it (issue #6).
        cited_memory = used
            .read
            .iter()
            .filter(|r| {
                !rejections
                    .iter()
                    .any(|rj| rj.op == memory::MemoryOp::Read && rj.target == r.id)
            })
            .map(|r| r.id.clone())
            .collect();
    }

    // Stale-ref gate (issue #8): review findings are only valid for the head
    // SHA that was actually reviewed. Re-fetch the PR before publishing; if
    // the head moved mid-session, surface the task as superseded rather than
    // presenting stale findings as current. The newer push's own event (or a
    // maintainer `retry`) re-reviews the fresh head.
    if let TaskKind::ReviewPullRequest { pr_number, .. } = &task.kind {
        let refs = repo::get_pull_request_refs_with_base_url(
            api_base_url,
            orchestration,
            &task.repo_owner,
            &task.repo_name,
            *pr_number,
        )
        .await?;
        if refs.head_sha != targets.head_sha {
            info!(
                task_id = %task.id,
                reviewed = %targets.head_sha,
                current = %refs.head_sha,
                "PR head moved during review — publishing as superseded"
            );
            return Ok(Published {
                result,
                opened_pr: None,
                changed_files: Vec::new(),
                stale: Some(StaleRefs {
                    reviewed_sha: targets.head_sha.clone(),
                    current_sha: refs.head_sha,
                }),
                cited_memory,
            });
        }
    }

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
                        task,
                        familiar,
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
                    if let Some(number) = surface_number(&task.kind) {
                        let marker = status_comment::marker(
                            &familiar.id,
                            &task.repo_owner,
                            &task.repo_name,
                            number,
                        );
                        let msg = redact::redact(
                            &format!(
                                "Status: failed\n\nI pushed `{branch}` but could not obtain publication credentials to open the PR: {e}"
                            ),
                            &[orchestration],
                        );
                        let _ = status_comment::upsert(
                            api_base_url,
                            orchestration,
                            &task.repo_owner,
                            &task.repo_name,
                            number,
                            &marker,
                            &msg,
                        )
                        .await;
                    }
                }
            }
        }
    }

    // Terminal state (done / needs input / failed) lands on the status surface
    // from execute_task's outcome handling.

    Ok(Published {
        result,
        opened_pr,
        changed_files: review.map(|r| r.files).unwrap_or_default(),
        stale: None,
        cited_memory,
    })
}

/// Opens the draft PR and posts the PR-opened comment with post-validation
/// publication authority. Best-effort: failures are surfaced on the issue
/// rather than failing the task, since the branch is already pushed.
async fn open_draft_pr(
    task: &Task,
    familiar: &FamiliarConfig,
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
        // The final status upsert in execute_task announces the opened PR.
        Ok(pr_num) => Some(pr_num),
        Err(e) => {
            // The branch is already pushed; the PR just didn't open. Surface it
            // rather than failing the whole task, so the work isn't lost from
            // the user's view.
            warn!(task_id = %task.id, "failed to open PR: {e:#}");
            if let Some(number) = surface_number(&task.kind) {
                let marker = status_comment::marker(
                    &familiar.id,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                );
                let msg = redact::redact(
                    &format!(
                        "Status: failed\n\nI pushed `{branch}` but could not open the PR automatically: {e}. Open the branch manually or check the App's pull-request permission."
                    ),
                    &[publication],
                );
                let _ = status_comment::upsert(
                    api_base_url,
                    publication,
                    &task.repo_owner,
                    &task.repo_name,
                    number,
                    &marker,
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

/// Durable terminal record for a published session outcome (issue #2).
fn terminal_of(published: &Published) -> Terminal {
    let result = &published.result;
    let state = match result.status {
        SessionStatus::Failure => TerminalState::Failed,
        _ => TerminalState::Completed,
    };
    let result_status = match result.status {
        SessionStatus::Success => "success",
        SessionStatus::Partial => "partial",
        SessionStatus::Failure => "failure",
        SessionStatus::NeedsInput => "needs_input",
    };
    Terminal {
        state,
        result_status: Some(result_status.to_string()),
        branch: result.branch.clone(),
        pr_number: published.opened_pr,
        summary: Some(result.summary.clone()),
        detail: None,
    }
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

/// Terminal body for the marker-backed status surface.
fn final_status_body(
    config: &Config,
    task_id: &str,
    result: &SessionResult,
    opened_pr: Option<u64>,
    cited_memory: &[String],
) -> String {
    let session = cave_session_url(config, task_id);
    let body = match result.status {
        SessionStatus::NeedsInput => format!(
            "Status: needs input\n\n{}\n\nReply on this thread to continue. Session: {session}",
            result.summary
        ),
        SessionStatus::Failure => format!(
            "Status: failed\n\n{}\n\nSession: {session}",
            result.summary
        ),
        SessionStatus::Success | SessionStatus::Partial => match opened_pr {
            Some(pr_number) => format!(
                "Status: done\n\n{}\n\nPR #{pr_number} opened. Session: {session}",
                result.summary
            ),
            None => format!(
                "Status: done\n\n{}\n\nSession: {session}",
                result.summary
            ),
        },
    };
    // Disclose which memory entries influenced this review (issue #6).
    if cited_memory.is_empty() {
        body
    } else {
        let cited = cited_memory
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{body}\n\nMemory used: {cited}")
    }
}

fn pr_title(result: &SessionResult, task: &Task) -> String {
    format!(
        "{} (#{} via Coven)",
        result.summary,
        surface_number(&task.kind).unwrap_or(0)
    )
}

/// True when the task carries a commander whose repository permission is
/// below write (issue #13). Auto-triggered tasks (no commander) always pass.
async fn commander_below_write(
    api_base_url: &str,
    orchestration: &str,
    task: &Task,
) -> Result<bool> {
    let Some(commander) = &task.commander else {
        return Ok(false);
    };
    let permission = repo::get_collaborator_permission_with_base_url(
        api_base_url,
        orchestration,
        &task.repo_owner,
        &task.repo_name,
        commander,
    )
    .await?;
    Ok(!matches!(permission.as_str(), "admin" | "maintain" | "write"))
}

/// Status-surface body for a below-write commander.
fn decline_body(task: &Task) -> String {
    format!(
        "Status: declined\n\nMaintainer commands need write access to {}/{}.",
        task.repo_owner, task.repo_name
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
        TaskKind::ReviewPullRequest {
            pr_number,
            pr_title,
            ..
        } => format!("Review PR #{pr_number}: {pr_title}"),
        TaskKind::CommandReply { issue_number, .. } => format!("Reply on #{issue_number}"),
        TaskKind::CancelReviews { pr_number } => {
            format!("Cancel queued reviews on PR #{pr_number}")
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
    /// True when this task reviews a fork PR — untrusted content that can never
    /// write durable memory (issue #6). Always false for non-PR tasks.
    head_is_fork: bool,
}

/// Resolves the repository default branch and the immutable target refs a task
/// operates on. Issue tasks target the default branch tip; PR review-comment
/// tasks target the PR's own head/base refs.
async fn resolve_targets(api_base_url: &str, token: &str, task: &Task) -> Result<ResolvedTargets> {
    let meta = repo::get_repo_with_base_url(api_base_url, token, &task.repo_owner, &task.repo_name)
        .await?;

    match &task.kind {
        TaskKind::AddressReviewComment { pr_number, .. }
        | TaskKind::ReviewPullRequest { pr_number, .. } => {
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
                head_is_fork: refs.head_is_fork,
            })
        }
        TaskKind::FixIssue { .. }
        | TaskKind::RespondToMention { .. }
        | TaskKind::CommandReply { .. }
        | TaskKind::CancelReviews { .. } => {
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
                head_is_fork: false,
            })
        }
    }
}

/// The issue/PR conversation number a task's status surface lives on. PR
/// conversation comments ride the issues API, so PR numbers work directly.
fn surface_number(kind: &TaskKind) -> Option<u64> {
    match kind {
        TaskKind::FixIssue { issue_number, .. }
        | TaskKind::RespondToMention { issue_number, .. }
        | TaskKind::CommandReply { issue_number, .. } => Some(*issue_number),
        TaskKind::AddressReviewComment { pr_number, .. }
        | TaskKind::ReviewPullRequest { pr_number, .. }
        | TaskKind::CancelReviews { pr_number } => Some(*pr_number),
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
            memory_used: None,
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
            review: coven_github_config::ReviewConfig::default(),
            storage: coven_github_config::StorageConfig::default(),
            memory: coven_github_config::MemoryConfig::default(),
            api: coven_github_config::ApiConfig::default(),
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

        let result = SessionResult {
            contract_version: HEADLESS_CONTRACT_VERSION.to_string(),
            status: SessionStatus::Success,
            branch: Some("cody/fix".to_string()),
            commits: vec![],
            files_changed: vec![],
            summary: "Fixed the auth refresh.".to_string(),
            pr_body: "body".to_string(),
            review: ReviewResult::none(),
            exit_reason: None,
            memory_used: None,
        };
        let done = final_status_body(&config, "task-42", &result, Some(17), &[]);
        assert!(done.starts_with("Status: done"));
        assert!(done.contains("PR #17 opened"));
        assert!(done.contains("https://cave.example.test/sessions/task-42"));
        assert!(
            !done.contains('✅') && !done.contains('→'),
            "status body should stay concise and actionable: {done}"
        );
        assert!(!done.contains("Memory used"), "no citation without memory");

        // A review that read memory cites the entries it used (issue #6).
        let cited = final_status_body(
            &config,
            "task-42",
            &result,
            Some(17),
            &["repo/acme/billing/conventions/x".to_string()],
        );
        assert!(cited.contains("Memory used: `repo/acme/billing/conventions/x`"));

        let mut needs_input = result.clone();
        needs_input.status = SessionStatus::NeedsInput;
        let waiting = final_status_body(&config, "task-42", &needs_input, None, &[]);
        assert!(waiting.starts_with("Status: needs input"));
        assert!(waiting.contains("Reply on this thread"));
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
            review: coven_github_config::ReviewConfig::default(),
            storage: coven_github_config::StorageConfig::default(),
            memory: coven_github_config::MemoryConfig::default(),
            api: coven_github_config::ApiConfig::default(),
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

#[cfg(all(test, unix))]
mod publication_tests {
    use super::*;
    use coven_github_api::installation::TokenRole;
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ORCHESTRATION: &str = "ghs_orchestration0000000000000000000000";
    const AGENT_GIT: &str = "ghs_agentgit0000000000000000000000000000";
    const PUBLICATION: &str = "ghs_publication0000000000000000000000000";

    fn fixed_minter() -> Minter {
        Minter::Fixed(HashMap::from([
            (TokenRole::Orchestration, ORCHESTRATION.to_string()),
            (TokenRole::AgentGit, AGENT_GIT.to_string()),
            (TokenRole::Publication, PUBLICATION.to_string()),
        ]))
    }

    #[tokio::test]
    async fn publication_uses_post_validation_token_and_leaks_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/OpenCoven/demo/check-runs/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/pulls"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(serde_json::json!({"number": 17})),
            )
            .mount(&server)
            .await;

        // Fake coven-code: records the git token it was handed, then emits a
        // result that tries to leak that token through free-text fields.
        let script = r#"#!/usr/bin/env bash
printf '%s' "$COVEN_GIT_TOKEN" > "$(dirname "$5")/seen-token"
cat > "$5" <<EOF
{"contract_version":"2","status":"success","branch":"cody/fix-42","commits":[{"sha":"a1","message":"msg $COVEN_GIT_TOKEN"}],"files_changed":[],"summary":"done $COVEN_GIT_TOKEN","pr_body":"body $COVEN_GIT_TOKEN","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}
EOF
exit 0
"#;
        let root =
            std::env::temp_dir().join(format!("coven-github-pub-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test dir should be created");
        let script_path = root.join("fake-coven-code.sh");
        fs::write(&script_path, script).expect("script should be written");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("script should be executable");

        let familiar = FamiliarConfig {
            id: "cody".to_string(),
            display_name: "Cody".to_string(),
            bot_username: "coven-cody[bot]".to_string(),
            model: None,
            skills: vec![],
            trigger_labels: vec![],
        };
        let config = Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: None,
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("private.pem"),
                webhook_secret: "secret".to_string(),
                api_base_url: Some(server.uri()),
            },
            worker: WorkerConfig {
                concurrency: 1,
                coven_code_bin: script_path,
                workspace_root: root.clone(),
                timeout_secs: 30,
                max_retries: 0,
            },
            familiars: vec![familiar.clone()],
            review: coven_github_config::ReviewConfig::default(),
            storage: coven_github_config::StorageConfig::default(),
            memory: coven_github_config::MemoryConfig::default(),
            api: coven_github_config::ApiConfig::default(),
        };
        let task = Task {
            id: "task-pub".to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "t".to_string(),
                issue_body: "b".to_string(),
            },
        };
        let targets = ResolvedTargets {
            default_branch: "main".to_string(),
            base_ref: "main".to_string(),
            head_sha: "abc123".to_string(),
            head_is_fork: false,
        };
        let workspace = root.join("ws");
        let store = coven_github_store::Store::open_in_memory().expect("store");

        let published = run_and_publish(
            &config,
            &task,
            &familiar,
            &fixed_minter(),
            ORCHESTRATION,
            &server.uri(),
            &targets,
            &workspace,
            7,
            &store,
        )
        .await
        .expect("publication should succeed");

        assert_eq!(published.opened_pr, Some(17));

        // The agent received exactly the AgentGit-scoped token.
        let seen = fs::read_to_string(workspace.join("seen-token"))
            .expect("fake coven-code should record its token");
        assert_eq!(seen, AGENT_GIT);

        // The sanitized envelope carries no live token values.
        assert!(!published.result.summary.contains(AGENT_GIT));
        assert!(published.result.summary.contains(redact::REDACTED));
        assert!(!published.result.pr_body.contains(AGENT_GIT));

        // No outgoing GitHub payload contains any token value…
        let requests = server.received_requests().await.expect("requests recorded");
        assert!(!requests.is_empty());
        for request in &requests {
            let body = String::from_utf8_lossy(&request.body);
            for token in [ORCHESTRATION, AGENT_GIT, PUBLICATION] {
                assert!(
                    !body.contains(token),
                    "{} {} leaked a token: {body}",
                    request.method,
                    request.url
                );
            }
        }
        // …and each endpoint was called with the authority its phase allows.
        let auth_of = |p: &str, m: &str| -> Vec<String> {
            requests
                .iter()
                .filter(|r| r.url.path() == p && r.method.as_str() == m)
                .map(|r| {
                    r.headers
                        .get("authorization")
                        .expect("authorization header present")
                        .to_str()
                        .expect("ascii header")
                        .to_string()
                })
                .collect()
        };
        assert_eq!(
            auth_of("/repos/OpenCoven/demo/pulls", "POST"),
            vec![format!("Bearer {PUBLICATION}")]
        );
        assert_eq!(
            auth_of("/repos/OpenCoven/demo/check-runs/7", "PATCH"),
            vec![format!("Bearer {ORCHESTRATION}")]
        );
        // One marker-backed status comment, posted with orchestration authority;
        // the terminal status edit happens in execute_task, not here.
        assert_eq!(
            auth_of("/repos/OpenCoven/demo/issues/42/comments", "POST"),
            vec![format!("Bearer {ORCHESTRATION}")]
        );

        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod stale_ref_tests {
    use super::*;
    use coven_github_api::installation::TokenRole;
    use coven_github_api::tasks::TaskListStatus;
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ORCHESTRATION: &str = "ghs_orchestration0000000000000000000000";
    const AGENT_GIT: &str = "******";

    fn fixed_minter() -> Minter {
        Minter::Fixed(HashMap::from([
            (TokenRole::Orchestration, ORCHESTRATION.to_string()),
            (TokenRole::AgentGit, AGENT_GIT.to_string()),
        ]))
    }

    fn pr_refs_body(head_sha: &str) -> serde_json::Value {
        serde_json::json!({
            "head": { "ref": "feat/change", "sha": head_sha },
            "base": { "ref": "main", "sha": "base0000" }
        })
    }

    /// A hosted review whose PR head advances mid-session must complete the
    /// Check Run as neutral/Stale, surface `Status: superseded` on the status
    /// comment, and mark the task superseded — never publishing the findings
    /// as if they covered the current head (issue #8).
    #[tokio::test]
    async fn review_of_moved_head_is_published_as_superseded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"default_branch": "main"})),
            )
            .mount(&server)
            .await;
        // First fetch (target resolution) sees the reviewed head; the
        // pre-publish re-fetch sees that the head has moved on.
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/pulls/88"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr_refs_body("sha-reviewed")))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/pulls/88"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr_refs_body("sha-moved")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/pulls/88/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "filename": "src/lib.rs" }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 7})))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/OpenCoven/demo/check-runs/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/issues/88/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/issues/88/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;

        // Contract-conformant review result with a finding that must NOT be
        // presented as current once the head has moved.
        let script = r#"#!/usr/bin/env bash
cat > "$5" <<EOF
{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"Found one issue in src/lib.rs.","pr_body":"","review":{"mode":"pull_request","evidence_status":"complete","reviewed_files":["src/lib.rs"],"supporting_files":[],"findings":[{"severity":"medium","file":"src/lib.rs","line":10,"title":"Off-by-one","body":"Loop bound skips the last element.","recommendation":null}],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}
EOF
exit 0
"#;
        let root =
            std::env::temp_dir().join(format!("coven-github-stale-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test dir should be created");
        let script_path = root.join("fake-coven-code.sh");
        fs::write(&script_path, script).expect("script should be written");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("script should be executable");

        let config = Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: None,
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("/nonexistent/never-read.pem"),
                webhook_secret: "secret".to_string(),
                api_base_url: Some(server.uri()),
            },
            worker: WorkerConfig {
                concurrency: 1,
                coven_code_bin: script_path,
                workspace_root: root.clone(),
                timeout_secs: 30,
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
            review: coven_github_config::ReviewConfig::default(),
            storage: coven_github_config::StorageConfig::default(),
            memory: coven_github_config::MemoryConfig::default(),
            api: coven_github_config::ApiConfig::default(),
        };
        let task = Task {
            id: "task-stale".to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "t".to_string(),
                reason: "synchronize".to_string(),
            },
        };
        let store = Store::open_in_memory().expect("store");
        // Seed the durable queued row the webhook path would have written.
        store
            .record_delivery(
                coven_github_store::Delivery {
                    delivery_id: "dl-stale".to_string(),
                    event: "pull_request".to_string(),
                    action: Some("synchronize".to_string()),
                    installation_id: Some(1),
                    repo: Some("OpenCoven/demo".to_string()),
                    payload_hash: "h".to_string(),
                },
                coven_github_store::Routing::Task(&task),
            )
            .await
            .expect("seed task row");

        execute_task_with_minter(&config, store.clone(), task, &fixed_minter())
            .await
            .expect("stale review must complete cleanly");

        // Cave sees the honest terminal state.
        let items = store.cave_list(HashMap::new(), None).await.expect("list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, TaskListStatus::Superseded);

        let requests = server.received_requests().await.expect("requests recorded");

        // The Check Run reached neutral/Stale — not success/Done.
        let check_patches: Vec<String> = requests
            .iter()
            .filter(|r| {
                r.method.as_str() == "PATCH"
                    && r.url.path() == "/repos/OpenCoven/demo/check-runs/7"
            })
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        let terminal = check_patches
            .last()
            .expect("the check run must reach a terminal state");
        assert!(terminal.contains("\"neutral\""), "conclusion: {terminal}");
        assert!(terminal.contains("Stale"), "title: {terminal}");
        assert!(terminal.contains("sha-reviewed") && terminal.contains("sha-moved"));
        assert!(
            !check_patches.iter().any(|b| b.contains("\"success\"")),
            "stale findings must never publish as a successful review"
        );

        // The status surface says superseded, and the finding text was withheld.
        let comment_posts: Vec<String> = requests
            .iter()
            .filter(|r| {
                r.method.as_str() == "POST"
                    && r.url.path() == "/repos/OpenCoven/demo/issues/88/comments"
            })
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert!(
            comment_posts.iter().any(|b| b.contains("Status: superseded")),
            "status surface must say superseded: {comment_posts:?}"
        );
        assert!(
            !comment_posts.iter().any(|b| b.contains("Off-by-one")),
            "stale findings must not land on the status surface"
        );

        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod command_and_marker_tests {
    use super::*;
    use coven_github_api::installation::TokenRole;
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ORCHESTRATION: &str = "ghs_orchestration0000000000000000000000";

    fn fixed_minter() -> Minter {
        Minter::Fixed(HashMap::from([(
            TokenRole::Orchestration,
            ORCHESTRATION.to_string(),
        )]))
    }

    fn config(api_base_url: String) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: None,
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("/nonexistent/never-read.pem"),
                webhook_secret: "secret".to_string(),
                api_base_url: Some(api_base_url),
            },
            worker: WorkerConfig {
                concurrency: 1,
                // Would fail loudly if any of these paths spawned a session.
                coven_code_bin: PathBuf::from("/nonexistent/coven-code"),
                workspace_root: PathBuf::from("/nonexistent/workspaces"),
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
            review: coven_github_config::ReviewConfig::default(),
            storage: coven_github_config::StorageConfig::default(),
            memory: coven_github_config::MemoryConfig::default(),
            api: coven_github_config::ApiConfig::default(),
        }
    }

    fn task(kind: TaskKind, commander: Option<&str>) -> Task {
        Task {
            id: "task-cmd".to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: commander.map(str::to_string),
            kind,
        }
    }

    #[tokio::test]
    async fn command_reply_upserts_without_spawning_coven_code() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;

        execute_task_with_minter(
            &config(server.uri()),
            Store::open_in_memory().expect("store"),
            task(
                TaskKind::CommandReply {
                    issue_number: 42,
                    body: "Tasks for OpenCoven/demo#42: none".to_string(),
                },
                None,
            ),
            &fixed_minter(),
        )
        .await
        .expect("command reply should post cleanly");

        let requests = server.received_requests().await.expect("requests recorded");
        let posted = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .expect("reply should be posted");
        let body = String::from_utf8_lossy(&posted.body);
        assert!(
            body.contains("<!-- coven:cody:OpenCoven/demo#42 -->"),
            "reply must carry the marker: {body}"
        );
        assert!(body.contains("Tasks for OpenCoven/demo#42"));
        // No Check Run, no session, no other GitHub calls.
        assert_eq!(requests.len(), 2, "GET comments + POST comment only");
    }

    #[tokio::test]
    async fn existing_marker_comment_is_edited_in_place() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": 5, "body": "unrelated first comment", "user": { "login": "octocat" } },
                { "id": 7, "body": "<!-- coven:cody:OpenCoven/demo#42 -->\nStatus: working", "user": { "login": "coven-cody[bot]" } }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/OpenCoven/demo/issues/comments/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        execute_task_with_minter(
            &config(server.uri()),
            Store::open_in_memory().expect("store"),
            task(
                TaskKind::CommandReply {
                    issue_number: 42,
                    body: "Status: done".to_string(),
                },
                None,
            ),
            &fixed_minter(),
        )
        .await
        .expect("upsert should edit in place");

        let requests = server.received_requests().await.expect("requests recorded");
        let patch = requests
            .iter()
            .find(|r| r.method.as_str() == "PATCH")
            .expect("existing marker comment must be edited, not duplicated");
        let body = String::from_utf8_lossy(&patch.body);
        assert!(body.contains("Status: done"));
        assert!(
            !requests.iter().any(|r| r.method.as_str() == "POST"),
            "no duplicate comment may be created"
        );
    }

    #[tokio::test]
    async fn below_write_commander_is_declined_before_any_work() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/collaborators/drive-by/permission"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"permission": "read"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/issues/42/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;

        let store = Store::open_in_memory().expect("store");
        let task = task(
            TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "t".to_string(),
                issue_body: "b".to_string(),
            },
            Some("drive-by"),
        );
        // Seed the durable queued row the webhook path would have written.
        store
            .record_delivery(
                coven_github_store::Delivery {
                    delivery_id: "dl-declined".to_string(),
                    event: "issue_comment".to_string(),
                    action: Some("created".to_string()),
                    installation_id: Some(1),
                    repo: Some("OpenCoven/demo".to_string()),
                    payload_hash: "h".to_string(),
                },
                coven_github_store::Routing::Task(&task),
            )
            .await
            .expect("seed task row");
        execute_task_with_minter(&config(server.uri()), store.clone(), task, &fixed_minter())
            .await
            .expect("a declined command is not an error");

        let requests = server.received_requests().await.expect("requests recorded");
        assert!(
            !requests.iter().any(|r| r.url.path().contains("check-runs")),
            "no Check Run may be created for a declined command"
        );
        let posted = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .expect("decline should land on the status surface");
        let body = String::from_utf8_lossy(&posted.body);
        assert!(
            body.contains("Status: declined"),
            "decline body: {body}"
        );
        // The durable record stays honest: terminal, with the decline noted.
        let states = store.task_states().await.expect("states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].1, "completed");
    }
    async fn seed(store: &Store, delivery_id: &str, task: &Task) {
        store
            .record_delivery(
                coven_github_store::Delivery {
                    delivery_id: delivery_id.to_string(),
                    event: "issue_comment".to_string(),
                    action: Some("created".to_string()),
                    installation_id: Some(1),
                    repo: Some("OpenCoven/demo".to_string()),
                    payload_hash: "h".to_string(),
                },
                coven_github_store::Routing::Task(task),
            )
            .await
            .expect("seed task row");
    }

    fn permission_mock(login: &str, permission: &str) -> Mock {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/OpenCoven/demo/collaborators/{login}/permission"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "permission": permission })),
            )
    }

    fn comment_mocks() -> (Mock, Mock) {
        (
            Mock::given(method("GET"))
                .and(path("/repos/OpenCoven/demo/issues/88/comments"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([]))),
            Mock::given(method("POST"))
                .and(path("/repos/OpenCoven/demo/issues/88/comments"))
                .respond_with(
                    ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})),
                ),
        )
    }

    fn queued_review(id: &str) -> Task {
        task(
            TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "t".to_string(),
                reason: "synchronize".to_string(),
            },
            None,
        )
        .with_id(id)
    }

    /// A drive-by `cancel` must decline at the gate and leave queued reviews
    /// untouched (issue #13 privilege fix).
    #[tokio::test]
    async fn drive_by_cancel_is_declined_and_tombstones_nothing() {
        let server = MockServer::start().await;
        permission_mock("drive-by", "read").mount(&server).await;
        let (get_comments, post_comment) = comment_mocks();
        get_comments.mount(&server).await;
        post_comment.mount(&server).await;

        let store = Store::open_in_memory().expect("store");
        seed(&store, "dl-q", &queued_review("victim")).await;
        let cancel = task(TaskKind::CancelReviews { pr_number: 88 }, Some("drive-by"))
            .with_id("cancel-task");
        seed(&store, "dl-c", &cancel).await;

        execute_task_with_minter(&config(server.uri()), store.clone(), cancel, &fixed_minter())
            .await
            .expect("declined cancel is not an error");

        let states: std::collections::HashMap<String, String> =
            store.task_states().await.unwrap().into_iter().collect();
        assert_eq!(states["victim"], "queued", "no tombstone below the gate");
        assert_eq!(states["cancel-task"], "completed");
        let requests = server.received_requests().await.expect("requests");
        let posted = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .expect("decline posted");
        let body = String::from_utf8_lossy(&posted.body);
        assert!(body.contains("Status: declined"), "body: {body}");
    }

    /// A maintainer `cancel` passes the gate, tombstones the queued review,
    /// and acknowledges with the count.
    #[tokio::test]
    async fn maintainer_cancel_tombstones_queued_reviews_past_the_gate() {
        let server = MockServer::start().await;
        permission_mock("octocat", "admin").mount(&server).await;
        let (get_comments, post_comment) = comment_mocks();
        get_comments.mount(&server).await;
        post_comment.mount(&server).await;

        let store = Store::open_in_memory().expect("store");
        seed(&store, "dl-q", &queued_review("victim")).await;
        let cancel =
            task(TaskKind::CancelReviews { pr_number: 88 }, Some("octocat")).with_id("cancel-task");
        seed(&store, "dl-c", &cancel).await;

        execute_task_with_minter(&config(server.uri()), store.clone(), cancel, &fixed_minter())
            .await
            .expect("cancel should succeed");

        let states: std::collections::HashMap<String, String> =
            store.task_states().await.unwrap().into_iter().collect();
        assert_eq!(states["victim"], "superseded");
        let requests = server.received_requests().await.expect("requests");
        let posted = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .expect("ack posted");
        let body = String::from_utf8_lossy(&posted.body);
        assert!(body.contains("Cancelled 1 queued review"), "body: {body}");
        assert!(
            !requests.iter().any(|r| r.url.path().contains("check-runs")),
            "cancel is adapter-only — no Check Run"
        );
    }

    /// Memory acknowledgements carry the commander and decline below write.
    #[tokio::test]
    async fn drive_by_memory_ack_is_declined() {
        let server = MockServer::start().await;
        permission_mock("drive-by", "read").mount(&server).await;
        let (get_comments, post_comment) = comment_mocks();
        get_comments.mount(&server).await;
        post_comment.mount(&server).await;

        execute_task_with_minter(
            &config(server.uri()),
            Store::open_in_memory().expect("store"),
            task(
                TaskKind::CommandReply {
                    issue_number: 88,
                    body: "Noted, but memory persistence is not wired up yet".to_string(),
                },
                Some("drive-by"),
            ),
            &fixed_minter(),
        )
        .await
        .expect("declined ack is not an error");

        let requests = server.received_requests().await.expect("requests");
        let posted = requests
            .iter()
            .find(|r| r.method.as_str() == "POST")
            .expect("reply posted");
        let body = String::from_utf8_lossy(&posted.body);
        assert!(body.contains("Status: declined"), "body: {body}");
        assert!(!body.contains("Noted"), "the ungated ack must not leak");
    }
}

/// Test-only convenience for retargeting a task id.
#[cfg(test)]
trait WithId {
    fn with_id(self, id: &str) -> Self;
}
#[cfg(test)]
impl WithId for Task {
    fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }
}

#[cfg(test)]
mod publication_gate_tests {
    //! End-to-end proof of the findings publication gates (issue #11):
    //! out-of-scope, duplicate, and below-threshold findings are withheld,
    //! the digest is honest about it, and the `request_changes` /
    //! `advisory_comment` policy modes route the verdict correctly.
    use super::*;
    use coven_github_api::installation::TokenRole;
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ORCHESTRATION: &str = "ghs_orchestration0000000000000000000000";
    const AGENT_GIT: &str = "ghs_agentgit000000000000000000000000000";
    const PUBLICATION: &str = "ghs_publication0000000000000000000000000";

    fn fixed_minter() -> Minter {
        Minter::Fixed(HashMap::from([
            (TokenRole::Orchestration, ORCHESTRATION.to_string()),
            (TokenRole::AgentGit, AGENT_GIT.to_string()),
            (TokenRole::Publication, PUBLICATION.to_string()),
        ]))
    }

    /// Review result with one publishable HIGH finding plus one duplicate,
    /// one out-of-scope file, and one INFO nit for the threshold gate.
    const RESULT_JSON: &str = r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"Reviewed the change.","pr_body":"","review":{"mode":"pull_request","evidence_status":"complete","reviewed_files":["src/lib.rs"],"supporting_files":[],"findings":[{"severity":"high","file":"src/lib.rs","line":10,"title":"Off-by-one","body":"Loop bound skips the last element.","recommendation":null},{"severity":"high","file":"src/lib.rs","line":10,"title":"Off-by-one","body":"Loop bound skips the last element.","recommendation":null},{"severity":"critical","file":"secrets/vault.rs","line":null,"title":"Speculative","body":"Never consulted this file.","recommendation":null},{"severity":"info","file":"src/lib.rs","line":20,"title":"Nit","body":"Prefer a doc comment.","recommendation":null}],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null}"#;

    async fn run_review(policy: coven_github_config::ReviewConfig) -> Vec<wiremock::Request> {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"default_branch": "main"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/pulls/88"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "head": { "ref": "feat/x", "sha": "stable-sha" },
                "base": { "ref": "main", "sha": "base-sha" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/pulls/88/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "filename": "src/lib.rs" }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/check-runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 7})))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/OpenCoven/demo/check-runs/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/OpenCoven/demo/issues/88/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/issues/88/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/OpenCoven/demo/pulls/88/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;

        let script = format!("#!/usr/bin/env bash\ncat > \"$5\" <<'RESULT'\n{RESULT_JSON}\nRESULT\nexit 0\n");
        let root =
            std::env::temp_dir().join(format!("coven-github-gates-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test dir");
        let script_path = root.join("fake-coven-code.sh");
        fs::write(&script_path, script).expect("script written");
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod");

        let config = Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: None,
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("/nonexistent/never-read.pem"),
                webhook_secret: "secret".to_string(),
                api_base_url: Some(server.uri()),
            },
            worker: WorkerConfig {
                concurrency: 1,
                coven_code_bin: script_path,
                workspace_root: root.clone(),
                timeout_secs: 30,
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
            review: policy,
            storage: coven_github_config::StorageConfig::default(),
            memory: coven_github_config::MemoryConfig::default(),
            api: coven_github_config::ApiConfig::default(),
        };
        let task = Task {
            id: "task-gates".to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "t".to_string(),
                reason: "synchronize".to_string(),
            },
        };

        execute_task_with_minter(
            &config,
            Store::open_in_memory().expect("store"),
            task,
            &fixed_minter(),
        )
        .await
        .expect("review must publish cleanly");

        let requests = server.received_requests().await.expect("requests");
        let _ = fs::remove_dir_all(root);
        requests
    }

    fn policy(min_severity: Option<&str>, publish: Option<&str>) -> coven_github_config::ReviewConfig {
        coven_github_config::ReviewConfig {
            familiar: Some("cody".to_string()),
            pull_request: true,
            include_drafts: false,
            audit_instruction: None,
            min_severity: min_severity.map(str::to_string),
            publish: publish.map(str::to_string),
            repos: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn gated_digest_lands_on_the_check_run_with_honest_counts() {
        let requests = run_review(policy(Some("medium"), None)).await;
        let terminal = requests
            .iter()
            .filter(|r| {
                r.method.as_str() == "PATCH"
                    && r.url.path() == "/repos/OpenCoven/demo/check-runs/7"
            })
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .next_back()
            .expect("terminal check patch");
        assert!(terminal.contains("Off-by-one"), "digest published: {terminal}");
        assert!(
            !terminal.contains("Speculative"),
            "out-of-scope finding must be withheld: {terminal}"
        );
        assert!(
            !terminal.contains("Prefer a doc comment"),
            "below-threshold finding must be withheld: {terminal}"
        );
        assert!(
            terminal.contains("3 finding(s) withheld"),
            "withheld counts must be stated: {terminal}"
        );
        // Default mode: no PR review submitted, no advisory digest on comment.
        assert!(
            !requests
                .iter()
                .any(|r| r.url.path() == "/repos/OpenCoven/demo/pulls/88/reviews"),
            "check_run mode must not submit a PR review"
        );
    }

    #[tokio::test]
    async fn request_changes_mode_submits_a_blocking_review_with_write_authority() {
        let requests = run_review(policy(None, Some("request_changes"))).await;
        let review_post = requests
            .iter()
            .find(|r| {
                r.method.as_str() == "POST"
                    && r.url.path() == "/repos/OpenCoven/demo/pulls/88/reviews"
            })
            .expect("PR review must be submitted");
        let body = String::from_utf8_lossy(&review_post.body);
        assert!(body.contains("REQUEST_CHANGES"), "verdict: {body}");
        assert!(body.contains("Off-by-one"), "digest in verdict body: {body}");
        // The verdict is write-authority work: publication token, never
        // orchestration (issue #4 boundary).
        let auth = review_post
            .headers
            .get("authorization")
            .expect("auth header")
            .to_str()
            .expect("ascii");
        assert_eq!(auth, format!("Bearer {PUBLICATION}"));
    }

    #[tokio::test]
    async fn advisory_mode_appends_the_digest_to_the_status_comment() {
        let requests = run_review(policy(None, Some("advisory_comment"))).await;
        let comment_bodies: Vec<String> = requests
            .iter()
            .filter(|r| {
                r.method.as_str() == "POST"
                    && r.url.path() == "/repos/OpenCoven/demo/issues/88/comments"
            })
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .collect();
        assert!(
            comment_bodies
                .iter()
                .any(|b| b.contains("Status: done") && b.contains("Off-by-one")),
            "advisory digest must ride the status comment: {comment_bodies:?}"
        );
    }
}
