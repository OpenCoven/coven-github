//! Branch Gardener execution path for adapter-only garden run tasks.

use anyhow::Result;
use tracing::{info, warn};

use coven_github_api::{installation::TokenRole, pr, repo, Task};
use coven_github_config::{Config, FamiliarConfig};
use coven_github_gardener::{
    classify as classify_branch, matches_exclude, plan as plan_garden, Autonomy, BranchClass,
    BranchFacts, ExecutionCounts, GardenPlan, GardenerPolicy, RunReport, SkipCode, SkipReason,
};
use coven_github_store::{Store, Terminal, TerminalState};

use crate::{redact, status_comment, Minter};

async fn finish_garden(
    store: &Store,
    task: &Task,
    state: TerminalState,
    summary: impl Into<String>,
    detail: Option<String>,
) -> Result<()> {
    store
        .finish(
            &task.id,
            Terminal {
                state,
                summary: Some(summary.into()),
                detail,
                ..Terminal::default()
            },
        )
        .await
}

async fn upsert_garden_comment(
    api_base_url: &str,
    orchestration: &str,
    familiar: &FamiliarConfig,
    task: &Task,
    issue_number: u64,
    body: &str,
) -> Result<()> {
    let marker = status_comment::marker(
        &familiar.id,
        &task.repo_owner,
        &task.repo_name,
        issue_number,
    );
    status_comment::upsert(
        api_base_url,
        orchestration,
        &task.repo_owner,
        &task.repo_name,
        issue_number,
        &marker,
        body,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_failed_garden_run(
    api_base_url: &str,
    orchestration: &str,
    store: &Store,
    task: &Task,
    familiar: &FamiliarConfig,
    report_issue: Option<u64>,
    summary: &str,
    error: &anyhow::Error,
) -> Result<()> {
    warn!(task_id = %task.id, "branch gardener run failed: {error:#}");
    finish_garden(
        store,
        task,
        TerminalState::Failed,
        summary,
        Some(redact::redact(&format!("{error:#}"), &[orchestration])),
    )
    .await?;
    if let Some(number) = report_issue {
        let body = redact::redact(
            &format!("Status: failed\n\nBranch Gardener run failed: {summary}."),
            &[orchestration],
        );
        if let Err(comment_error) =
            upsert_garden_comment(api_base_url, orchestration, familiar, task, number, &body).await
        {
            warn!(task_id = %task.id, "failed to upsert gardener failure status: {comment_error:#}");
        }
    }
    Ok(())
}

async fn scan_garden_branches(
    api_base_url: &str,
    orchestration: &str,
    task: &Task,
    resolved: &coven_github_config::ResolvedGardenerPolicy,
) -> Result<(String, Vec<BranchFacts>)> {
    let metadata = repo::get_repo_with_base_url(
        api_base_url,
        orchestration,
        &task.repo_owner,
        &task.repo_name,
    )
    .await?;
    let default_branch = metadata.default_branch;
    let branches = repo::list_branches_with_base_url(
        api_base_url,
        orchestration,
        &task.repo_owner,
        &task.repo_name,
    )
    .await?;

    let mut facts = Vec::with_capacity(branches.len());
    for branch in branches {
        let excluded = branch.name == default_branch
            || branch.protected
            || matches_exclude(&branch.name, &resolved.exclude);
        if excluded {
            facts.push(BranchFacts {
                name: branch.name,
                sha: branch.commit.sha,
                protected: branch.protected,
                ahead: 0,
                behind: 0,
                ahead_author_logins: Vec::new(),
                authors_truncated: false,
                open_pr: None,
                merged_pr: None,
            });
            continue;
        }

        let compare = repo::compare_ahead_behind_with_base_url(
            api_base_url,
            orchestration,
            &task.repo_owner,
            &task.repo_name,
            &default_branch,
            &branch.name,
        )
        .await?;
        let pulls = pr::list_pulls_by_head_with_base_url(
            api_base_url,
            orchestration,
            &task.repo_owner,
            &task.repo_name,
            &branch.name,
        )
        .await?;
        facts.push(BranchFacts {
            name: branch.name,
            sha: branch.commit.sha,
            protected: branch.protected,
            ahead: compare.ahead_by,
            behind: compare.behind_by,
            ahead_author_logins: compare.author_logins,
            authors_truncated: compare.truncated,
            open_pr: pulls
                .iter()
                .find(|pull| pull.state == "open")
                .map(|pull| pull.number),
            merged_pr: pulls
                .iter()
                .find(|pull| pull.merged)
                .map(|pull| pull.number),
        });
    }

    Ok((default_branch, facts))
}

fn trace_garden_plan(facts: &[BranchFacts], policy: &GardenerPolicy) {
    for fact in facts {
        let class = classify_branch(fact, policy);
        let action = match class {
            BranchClass::Excluded => "skip",
            BranchClass::Merged | BranchClass::Dead => match policy.autonomy {
                Autonomy::Propose => "would_prune",
                Autonomy::PruneDead => "prune",
            },
            BranchClass::Active => "active",
            BranchClass::Prless
                if fact
                    .ahead_author_logins
                    .iter()
                    .all(|login| login.ends_with("[bot]"))
                    && !fact.ahead_author_logins.is_empty()
                    && !fact.authors_truncated =>
            {
                "skip"
            }
            BranchClass::Prless => "surface",
        };
        info!(
            branch = %fact.name,
            class = ?class,
            action,
            "branch gardener planned action"
        );
    }
}

fn surface_body(branch: &str, ahead: u64) -> String {
    format!(
        "Opened by the Branch Gardener to surface `{branch}` for maintainer review.\n\n\
         This branch is {ahead} commit(s) ahead of the default branch."
    )
}

async fn execute_garden_plan(
    api_base_url: &str,
    minter: &Minter,
    task: &Task,
    default_branch: &str,
    facts: &[BranchFacts],
    plan: &mut GardenPlan,
) -> Result<ExecutionCounts> {
    let mut counts = ExecutionCounts::default();

    if !plan.prune.is_empty() {
        let agent_git = minter.mint(TokenRole::AgentGit).await?;
        for action in &plan.prune {
            info!(
                branch = %action.branch,
                class = ?classify_branch(
                    facts
                        .iter()
                        .find(|fact| fact.name == action.branch)
                        .expect("planned prune action has source facts"),
                    &GardenerPolicy {
                        autonomy: Autonomy::PruneDead,
                        default_branch: default_branch.to_string(),
                        exclude: Vec::new(),
                        draft_pr_label: None,
                    },
                ),
                action = "prune",
                "branch gardener pruning branch"
            );
            let current_sha = match repo::get_branch_sha_with_base_url(
                api_base_url,
                &agent_git,
                &task.repo_owner,
                &task.repo_name,
                &action.branch,
            )
            .await
            {
                Ok(sha) => sha,
                Err(error) => {
                    counts.prune_skipped_moved += 1;
                    warn!(
                        task_id = %task.id,
                        branch = %action.branch,
                        "skipping branch prune because SHA re-check failed: {error:#}"
                    );
                    continue;
                }
            };

            if current_sha != action.sha {
                counts.prune_skipped_moved += 1;
                warn!(
                    task_id = %task.id,
                    branch = %action.branch,
                    scanned_sha = %action.sha,
                    current_sha = %current_sha,
                    "skipping branch prune because branch moved"
                );
                continue;
            }

            match repo::delete_ref_with_base_url(
                api_base_url,
                &agent_git,
                &task.repo_owner,
                &task.repo_name,
                &action.branch,
            )
            .await
            {
                Ok(()) => counts.pruned += 1,
                Err(error) => {
                    counts.prune_skipped_moved += 1;
                    warn!(
                        task_id = %task.id,
                        branch = %action.branch,
                        "skipping branch prune after delete failed: {error:#}"
                    );
                }
            }
        }
    }

    if !plan.surface.is_empty() {
        let publication = minter.mint(TokenRole::Publication).await?;
        let planned_surface = std::mem::take(&mut plan.surface);
        for mut action in planned_surface {
            info!(
                branch = %action.branch,
                class = ?BranchClass::Prless,
                action = "surface",
                "branch gardener surfacing branch"
            );
            let ahead = facts
                .iter()
                .find(|fact| fact.name == action.branch)
                .map(|fact| fact.ahead)
                .unwrap_or_default();
            match pr::open_pull_request_with_base_url(
                api_base_url,
                &publication,
                &task.repo_owner,
                &task.repo_name,
                &action.branch,
                default_branch,
                &format!("Surface branch {}", action.branch),
                &surface_body(&action.branch, ahead),
                true,
            )
            .await
            {
                Ok(number) => {
                    action.pr_number = Some(number);
                    counts.surfaced += 1;
                    if let Some(label) = &action.draft_pr_label {
                        if let Err(error) = pr::add_labels_to_issue_with_base_url(
                            api_base_url,
                            &publication,
                            &task.repo_owner,
                            &task.repo_name,
                            number,
                            std::slice::from_ref(label),
                        )
                        .await
                        {
                            warn!(
                                task_id = %task.id,
                                branch = %action.branch,
                                pr_number = number,
                                "failed to add branch gardener draft label: {error:#}"
                            );
                        }
                    }
                    plan.surface.push(action);
                }
                Err(error) => {
                    plan.skipped.push(SkipReason {
                        branch: action.branch.clone(),
                        reason: SkipCode::Withheld,
                    });
                    warn!(
                        task_id = %task.id,
                        branch = %action.branch,
                        "failed to surface branch as draft PR: {error:#}"
                    );
                }
            }
        }
    }

    Ok(counts)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_garden_run(
    config: &Config,
    api_base_url: &str,
    orchestration: &str,
    minter: &Minter,
    store: &Store,
    task: &Task,
    familiar: &FamiliarConfig,
    report_issue: Option<u64>,
) -> Result<()> {
    let repo_full = format!("{}/{}", task.repo_owner, task.repo_name);
    let resolved = config.gardener_policy(&repo_full);
    if !resolved.enabled {
        if let Some(number) = report_issue {
            let body = "Status: done\n\nBranch Gardener is not enabled for this repository. \
                        Enable the `gardener` policy in coven-github config for this repo before \
                        running it.";
            if let Err(error) =
                upsert_garden_comment(api_base_url, orchestration, familiar, task, number, body)
                    .await
            {
                warn!(task_id = %task.id, "failed to upsert disabled gardener status: {error:#}");
            }
        }
        finish_garden(
            store,
            task,
            TerminalState::Completed,
            "gardener disabled",
            None,
        )
        .await?;
        return Ok(());
    }

    let (default_branch, facts) =
        match scan_garden_branches(api_base_url, orchestration, task, &resolved).await {
            Ok(scan) => scan,
            Err(error) => {
                return finish_failed_garden_run(
                    api_base_url,
                    orchestration,
                    store,
                    task,
                    familiar,
                    report_issue,
                    "gardener scan failed",
                    &error,
                )
                .await;
            }
        };
    let policy = GardenerPolicy {
        autonomy: resolved.autonomy,
        default_branch: default_branch.clone(),
        exclude: resolved.exclude,
        draft_pr_label: resolved.draft_pr_label,
    };
    trace_garden_plan(&facts, &policy);
    let mut plan = plan_garden(&facts, &policy);

    let counts = match execute_garden_plan(
        api_base_url,
        minter,
        task,
        &default_branch,
        &facts,
        &mut plan,
    )
    .await
    {
        Ok(counts) => counts,
        Err(error) => {
            return finish_failed_garden_run(
                api_base_url,
                orchestration,
                store,
                task,
                familiar,
                report_issue,
                "gardener execution failed",
                &error,
            )
            .await;
        }
    };

    let report = RunReport::from_plan(&plan, &counts);
    let summary = report.summary_line();
    if let Some(number) = report_issue {
        let body = report.comment_body(&repo_full, policy.autonomy);
        if let Err(error) =
            upsert_garden_comment(api_base_url, orchestration, familiar, task, number, &body).await
        {
            warn!(task_id = %task.id, "failed to upsert gardener status: {error:#}");
        }
    }

    finish_garden(store, task, TerminalState::Completed, summary, None).await?;
    Ok(())
}
