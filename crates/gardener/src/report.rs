//! Markdown reporting for branch gardener plans.

use serde::{Deserialize, Serialize};
use std::fmt::Write;

use crate::scan::{
    Autonomy, GardenPlan, PruneAction, PruneReason, SkipCode, SkipReason, SurfaceAction,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionCounts {
    pub pruned: usize,
    pub prune_skipped_moved: usize,
    pub surfaced: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunReport {
    plan: GardenPlan,
    executed: ExecutionCounts,
}

impl RunReport {
    pub fn from_plan(plan: &GardenPlan, executed: &ExecutionCounts) -> Self {
        Self {
            plan: plan.clone(),
            executed: *executed,
        }
    }

    pub fn summary_line(&self) -> String {
        format!(
            "gardener: pruned {}, surfaced {}, active {}, skipped {}, would-prune {}",
            self.executed.pruned,
            self.executed.surfaced,
            self.plan.active.len(),
            self.plan.skipped.len(),
            self.plan.would_prune.len()
        )
    }

    pub fn comment_body(&self, repo: &str, autonomy: Autonomy) -> String {
        let mut body = String::new();
        writeln!(body, "🌿 Branch Gardener for `{repo}`").expect("write markdown heading");
        writeln!(body).expect("write markdown spacer");
        writeln!(body, "Autonomy: `{}`", autonomy.as_str()).expect("write autonomy");
        writeln!(body).expect("write markdown spacer");
        writeln!(body, "| count | value |").expect("write table header");
        writeln!(body, "| --- | ---: |").expect("write table divider");
        writeln!(body, "| pruned | {} |", self.executed.pruned).expect("write pruned count");
        writeln!(
            body,
            "| prune-skipped-moved | {} |",
            self.executed.prune_skipped_moved
        )
        .expect("write skipped moved count");
        writeln!(body, "| surfaced | {} |", self.executed.surfaced).expect("write surfaced count");
        writeln!(body, "| active | {} |", self.plan.active.len()).expect("write active count");
        writeln!(body, "| skipped | {} |", self.plan.skipped.len()).expect("write skipped count");
        writeln!(body, "| would-prune | {} |", self.plan.would_prune.len())
            .expect("write would prune count");

        write_prune_section(&mut body, "Pruned", &self.plan.prune);
        write_prune_section(&mut body, "Would prune", &self.plan.would_prune);
        write_surface_section(&mut body, &self.plan.surface);
        write_skip_section(&mut body, &self.plan.skipped);

        body
    }
}

impl Autonomy {
    fn as_str(self) -> &'static str {
        match self {
            Autonomy::Propose => "propose",
            Autonomy::PruneDead => "prune-dead",
        }
    }
}

fn write_prune_section(body: &mut String, title: &str, actions: &[PruneAction]) {
    writeln!(body).expect("write markdown spacer");
    writeln!(body, "### {title}").expect("write prune heading");
    if actions.is_empty() {
        writeln!(body, "- _(none)_").expect("write empty prune list");
    } else {
        for action in actions {
            writeln!(body, "{}", format_prune_action(action)).expect("write prune action");
        }
    }
}

fn write_surface_section(body: &mut String, actions: &[SurfaceAction]) {
    writeln!(body).expect("write markdown spacer");
    writeln!(body, "### Surfaced").expect("write surface heading");
    if actions.is_empty() {
        writeln!(body, "- _(none)_").expect("write empty surface list");
    } else {
        for action in actions {
            writeln!(body, "{}", format_surface_action(action)).expect("write surface action");
        }
    }
}

fn write_skip_section(body: &mut String, skips: &[SkipReason]) {
    writeln!(body).expect("write markdown spacer");
    writeln!(body, "### Skipped").expect("write skipped heading");
    if skips.is_empty() {
        writeln!(body, "- _(none)_").expect("write empty skipped list");
    } else {
        for skip in skips {
            writeln!(body, "{}", format_skip(skip)).expect("write skipped branch");
        }
    }
}

fn format_prune_reason(reason: &PruneReason) -> String {
    match reason {
        PruneReason::Dead => "dead".to_string(),
        PruneReason::Merged { pr } => format!("merged PR #{pr}"),
    }
}

fn format_skip_reason(reason: SkipCode) -> &'static str {
    match reason {
        SkipCode::Excluded => "excluded",
        SkipCode::BotOnlyPrless => "bot-only-prless",
        SkipCode::Withheld => "withheld",
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn code_span(value: &str) -> String {
    let longest_run = longest_backtick_run(value);
    if longest_run == 0 {
        format!("`{value}`")
    } else {
        let fence = "`".repeat(longest_run + 1);
        format!("{fence} {value} {fence}")
    }
}

fn longest_backtick_run(value: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for ch in value.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn format_prune_action(action: &PruneAction) -> String {
    format!(
        "- {} @ `{}` — {}",
        code_span(&action.branch),
        short_sha(&action.sha),
        format_prune_reason(&action.reason)
    )
}

fn format_surface_action(action: &SurfaceAction) -> String {
    let pr = action
        .pr_number
        .map(|number| format!("draft PR #{number}"))
        .unwrap_or_else(|| "draft PR planned".to_string());
    let label = action
        .draft_pr_label
        .as_ref()
        .map(|label| format!(", label `{label}`"))
        .unwrap_or_default();

    format!(
        "- {} @ `{}` — {}{}",
        code_span(&action.branch),
        short_sha(&action.sha),
        pr,
        label
    )
}

fn format_skip(skip: &SkipReason) -> String {
    format!(
        "- {} — {}",
        code_span(&skip.branch),
        format_skip_reason(skip.reason)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{GardenPlan, PruneAction, PruneReason, SkipCode, SkipReason, SurfaceAction};

    fn sample_plan() -> GardenPlan {
        GardenPlan {
            prune: vec![PruneAction {
                branch: "dead".to_string(),
                sha: "abc123".to_string(),
                reason: PruneReason::Dead,
            }],
            would_prune: vec![PruneAction {
                branch: "merged".to_string(),
                sha: "def456".to_string(),
                reason: PruneReason::Merged { pr: 42 },
            }],
            surface: vec![SurfaceAction {
                branch: "feature".to_string(),
                sha: "fed987".to_string(),
                draft_pr_label: Some("gardener".to_string()),
                pr_number: Some(77),
            }],
            active: vec!["active".to_string(), "reviewed".to_string()],
            skipped: vec![SkipReason {
                branch: "main".to_string(),
                reason: SkipCode::Excluded,
            }],
        }
    }

    #[test]
    fn summary_line_uses_executed_counts_and_plan_counts() {
        let report = RunReport::from_plan(
            &sample_plan(),
            &ExecutionCounts {
                pruned: 3,
                prune_skipped_moved: 1,
                surfaced: 2,
            },
        );

        assert_eq!(
            report.summary_line(),
            "gardener: pruned 3, surfaced 2, active 2, skipped 1, would-prune 1"
        );
    }

    #[test]
    fn comment_body_contains_counts_and_branch_sections() {
        let report = RunReport::from_plan(
            &sample_plan(),
            &ExecutionCounts {
                pruned: 1,
                prune_skipped_moved: 1,
                surfaced: 1,
            },
        );

        let body = report.comment_body("OpenCoven/coven-github", Autonomy::PruneDead);

        assert!(body.contains("🌿 Branch Gardener for `OpenCoven/coven-github`"));
        assert!(body.contains("| pruned | 1 |"));
        assert!(body.contains("| prune-skipped-moved | 1 |"));
        assert!(body.contains("| would-prune | 1 |"));
        assert!(body.contains("`dead`"));
        assert!(body.contains("`merged`"));
        assert!(body.contains("PR #42"));
        assert!(body.contains("`feature`"));
        assert!(body.contains("PR #77"));
        assert!(body.contains("`main`"));
        assert!(body.contains("excluded"));
    }

    #[test]
    fn comment_body_uses_safe_code_spans_for_branch_names_with_backticks() {
        let plan = GardenPlan {
            prune: vec![PruneAction {
                branch: "pwn`@org/team".to_string(),
                sha: "abc123".to_string(),
                reason: PruneReason::Dead,
            }],
            would_prune: Vec::new(),
            surface: vec![SurfaceAction {
                branch: "two``ticks".to_string(),
                sha: "def456".to_string(),
                draft_pr_label: None,
                pr_number: None,
            }],
            active: Vec::new(),
            skipped: vec![SkipReason {
                branch: "skip`@all".to_string(),
                reason: SkipCode::Excluded,
            }],
        };
        let report = RunReport::from_plan(&plan, &ExecutionCounts::default());

        let body = report.comment_body("OpenCoven/coven-github", Autonomy::Propose);

        assert!(body.contains("`` pwn`@org/team ``"));
        assert!(body.contains("``` two``ticks ```"));
        assert!(body.contains("`` skip`@all ``"));
        assert!(!body.contains("- `pwn`@org/team`"));
        assert!(!body.contains("- `two``ticks`"));
        assert!(!body.contains("- `skip`@all`"));
    }
}
