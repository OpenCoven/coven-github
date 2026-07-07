//! Branch classification and action planning.
//!
//! Exclude patterns are intentionally small: literal equality, a bare `*` that
//! matches everything, or a trailing-`*` prefix glob such as `release/*`. Other
//! `*` characters are treated literally.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchFacts {
    pub name: String,
    pub sha: String,
    pub protected: bool,
    pub ahead: u64,
    pub behind: u64,
    pub ahead_author_logins: Vec<String>,
    pub authors_truncated: bool,
    pub open_pr: Option<u64>,
    pub merged_pr: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchClass {
    Excluded,
    Merged,
    Dead,
    Active,
    Prless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Autonomy {
    Propose,
    PruneDead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GardenerPolicy {
    pub autonomy: Autonomy,
    pub default_branch: String,
    pub exclude: Vec<String>,
    pub draft_pr_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GardenPlan {
    pub prune: Vec<PruneAction>,
    pub would_prune: Vec<PruneAction>,
    pub surface: Vec<SurfaceAction>,
    pub active: Vec<String>,
    pub skipped: Vec<SkipReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneAction {
    pub branch: String,
    pub sha: String,
    pub reason: PruneReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PruneReason {
    Dead,
    Merged { pr: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceAction {
    pub branch: String,
    pub sha: String,
    pub draft_pr_label: Option<String>,
    pub pr_number: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipReason {
    pub branch: String,
    pub reason: SkipCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipCode {
    Excluded,
    BotOnlyPrless,
    Withheld,
}

pub fn classify(facts: &BranchFacts, policy: &GardenerPolicy) -> BranchClass {
    if facts.name == policy.default_branch
        || facts.protected
        || matches_exclude(&facts.name, &policy.exclude)
    {
        return BranchClass::Excluded;
    }

    if facts.open_pr.is_some() {
        return BranchClass::Active;
    }

    if facts.merged_pr.is_some() && facts.ahead == 0 {
        return BranchClass::Merged;
    }

    if facts.ahead == 0 {
        return BranchClass::Dead;
    }

    BranchClass::Prless
}

pub fn matches_exclude(name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| matches_exclude_pattern(name, pattern))
}

fn matches_exclude_pattern(name: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if let Some(prefix) = pattern.strip_suffix('*') {
        if !prefix.contains('*') {
            return name.starts_with(prefix);
        }
    }

    name == pattern
}

pub fn plan(facts: &[BranchFacts], policy: &GardenerPolicy) -> GardenPlan {
    let mut plan = GardenPlan::default();

    for branch in facts {
        match classify(branch, policy) {
            BranchClass::Excluded => plan.skipped.push(skip(branch, SkipCode::Excluded)),
            class @ (BranchClass::Merged | BranchClass::Dead) => {
                let action = prune_action(branch, class);
                match policy.autonomy {
                    Autonomy::Propose => plan.would_prune.push(action),
                    Autonomy::PruneDead => plan.prune.push(action),
                }
            }
            BranchClass::Active => plan.active.push(branch.name.clone()),
            BranchClass::Prless => {
                if is_proven_bot_only(branch) {
                    plan.skipped.push(skip(branch, SkipCode::BotOnlyPrless));
                } else {
                    plan.surface.push(SurfaceAction {
                        branch: branch.name.clone(),
                        sha: branch.sha.clone(),
                        draft_pr_label: policy.draft_pr_label.clone(),
                        pr_number: None,
                    });
                }
            }
        }
    }

    plan
}

fn prune_action(facts: &BranchFacts, class: BranchClass) -> PruneAction {
    debug_assert!(matches!(class, BranchClass::Dead | BranchClass::Merged));
    debug_assert_eq!(facts.ahead, 0);

    let reason = match class {
        BranchClass::Merged => PruneReason::Merged {
            pr: facts
                .merged_pr
                .expect("merged class requires a merged pull request number"),
        },
        BranchClass::Dead => PruneReason::Dead,
        BranchClass::Excluded | BranchClass::Active | BranchClass::Prless => {
            unreachable!("non-prune class passed to prune_action")
        }
    };

    PruneAction {
        branch: facts.name.clone(),
        sha: facts.sha.clone(),
        reason,
    }
}

fn skip(facts: &BranchFacts, reason: SkipCode) -> SkipReason {
    SkipReason {
        branch: facts.name.clone(),
        reason,
    }
}

fn is_proven_bot_only(facts: &BranchFacts) -> bool {
    !facts.authors_truncated
        && !facts.ahead_author_logins.is_empty()
        && facts
            .ahead_author_logins
            .iter()
            .all(|login| login.ends_with("[bot]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(autonomy: Autonomy) -> GardenerPolicy {
        GardenerPolicy {
            autonomy,
            default_branch: "main".to_string(),
            exclude: vec!["skip/*".to_string()],
            draft_pr_label: Some("gardener".to_string()),
        }
    }

    fn facts(name: &str) -> BranchFacts {
        BranchFacts {
            name: name.to_string(),
            sha: format!("sha-{name}"),
            protected: false,
            ahead: 1,
            behind: 0,
            ahead_author_logins: vec!["human".to_string()],
            authors_truncated: false,
            open_pr: None,
            merged_pr: None,
        }
    }

    #[test]
    fn classifies_every_branch_class_with_first_match_precedence() {
        let policy = policy(Autonomy::Propose);

        let mut excluded = facts("skip/release");
        excluded.ahead = 0;
        excluded.merged_pr = Some(10);
        assert_eq!(classify(&excluded, &policy), BranchClass::Excluded);

        let mut protected = facts("protected");
        protected.protected = true;
        protected.ahead = 0;
        assert_eq!(classify(&protected, &policy), BranchClass::Excluded);

        let mut default_branch = facts("main");
        default_branch.ahead = 42;
        assert_eq!(classify(&default_branch, &policy), BranchClass::Excluded);

        let mut merged = facts("merged");
        merged.ahead = 0;
        merged.merged_pr = Some(11);
        assert_eq!(classify(&merged, &policy), BranchClass::Merged);

        let mut dead = facts("dead");
        dead.ahead = 0;
        assert_eq!(classify(&dead, &policy), BranchClass::Dead);

        let mut active = facts("active");
        active.open_pr = Some(12);
        assert_eq!(classify(&active, &policy), BranchClass::Active);

        let prless = facts("prless");
        assert_eq!(classify(&prless, &policy), BranchClass::Prless);
    }

    #[test]
    fn open_pull_request_keeps_zero_ahead_branches_active() {
        let policy = policy(Autonomy::Propose);

        let mut open_pr_not_merged = facts("release-targeted");
        open_pr_not_merged.ahead = 0;
        open_pr_not_merged.open_pr = Some(21);
        assert_eq!(classify(&open_pr_not_merged, &policy), BranchClass::Active);

        let mut open_pr_with_merged_history = facts("reopened-release-targeted");
        open_pr_with_merged_history.ahead = 0;
        open_pr_with_merged_history.open_pr = Some(22);
        open_pr_with_merged_history.merged_pr = Some(20);
        assert_eq!(
            classify(&open_pr_with_merged_history, &policy),
            BranchClass::Active
        );
    }

    #[test]
    fn exclude_matcher_supports_literals_bare_star_and_trailing_star_only() {
        assert!(matches_exclude("release", &["release".to_string()]));
        assert!(matches_exclude("release/1.2", &["release/*".to_string()]));
        assert!(!matches_exclude("release", &["release/*".to_string()]));
        assert!(matches_exclude("anything", &["*".to_string()]));
        assert!(matches_exclude("a*b", &["a*b".to_string()]));
        assert!(!matches_exclude("axb", &["a*b".to_string()]));
    }

    #[test]
    fn bot_only_prless_branches_are_skipped_only_when_proven_bot_only() {
        let policy = policy(Autonomy::Propose);
        let mut all_bot = facts("bot-only");
        all_bot.ahead_author_logins =
            vec!["dependabot[bot]".to_string(), "renovate[bot]".to_string()];

        let mut mixed = facts("mixed");
        mixed.ahead_author_logins = vec!["dependabot[bot]".to_string(), "alice".to_string()];

        let mut truncated = facts("truncated");
        truncated.ahead_author_logins = vec!["dependabot[bot]".to_string()];
        truncated.authors_truncated = true;

        let mut empty = facts("empty");
        empty.ahead_author_logins.clear();

        let plan = plan(&[all_bot, mixed, truncated, empty], &policy);

        assert_eq!(
            plan.skipped,
            vec![SkipReason {
                branch: "bot-only".to_string(),
                reason: SkipCode::BotOnlyPrless,
            }]
        );
        let surfaced: Vec<_> = plan
            .surface
            .iter()
            .map(|action| action.branch.as_str())
            .collect();
        assert_eq!(surfaced, vec!["mixed", "truncated", "empty"]);
    }

    #[test]
    fn autonomy_controls_dead_and_merged_prune_buckets() {
        let mut dead = facts("dead");
        dead.ahead = 0;
        let mut merged = facts("merged");
        merged.ahead = 0;
        merged.merged_pr = Some(7);

        let propose = plan(&[dead.clone(), merged.clone()], &policy(Autonomy::Propose));
        assert!(propose.prune.is_empty());
        assert_eq!(propose.would_prune.len(), 2);

        let prune_dead = plan(&[dead, merged], &policy(Autonomy::PruneDead));
        assert_eq!(prune_dead.prune.len(), 2);
        assert!(prune_dead.would_prune.is_empty());
    }

    #[test]
    fn active_and_prless_branches_never_land_in_prune_buckets() {
        let mut active = facts("active");
        active.open_pr = Some(9);
        let prless = facts("prless");

        let prune_dead = plan(
            &[active.clone(), prless.clone()],
            &policy(Autonomy::PruneDead),
        );
        assert!(prune_dead.prune.is_empty());
        assert!(prune_dead.would_prune.is_empty());

        let propose = plan(&[active, prless], &policy(Autonomy::Propose));
        assert!(propose.prune.is_empty());
        assert!(propose.would_prune.is_empty());
    }

    #[test]
    fn plan_records_active_excluded_and_surface_actions() {
        let mut active = facts("active");
        active.open_pr = Some(5);
        let mut excluded = facts("skip/nope");
        excluded.ahead = 0;
        let prless = facts("feature");

        let plan = plan(&[active, excluded, prless], &policy(Autonomy::Propose));

        assert_eq!(plan.active, vec!["active".to_string()]);
        assert_eq!(
            plan.skipped,
            vec![SkipReason {
                branch: "skip/nope".to_string(),
                reason: SkipCode::Excluded,
            }]
        );
        assert_eq!(
            plan.surface,
            vec![SurfaceAction {
                branch: "feature".to_string(),
                sha: "sha-feature".to_string(),
                draft_pr_label: Some("gardener".to_string()),
                pr_number: None,
            }]
        );
    }
}
