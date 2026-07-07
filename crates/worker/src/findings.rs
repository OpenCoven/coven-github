//! Deterministic publication gates for review findings (issue #11).
//!
//! Agent judgment produces findings; the adapter decides what publishes.
//! Every finding passes a gate chain before reaching any GitHub surface:
//!
//! 1. **Scope** — the file must appear in the reviewed / supporting /
//!    changed-file sets the session actually consulted. Findings about files
//!    the review never touched are speculative and are withheld.
//! 2. **Severity threshold** — repo policy can set a minimum severity;
//!    anything below is filtered (still counted, so the surface stays honest).
//! 3. **Duplicates** — identical `(file, line, title)` findings collapse.
//!
//! The rendered digest always states how many findings were withheld and
//! why, so a quiet report is distinguishable from a filtered one.
//! (A confidence axis is specified for headless contract v3; v2 findings
//! carry severity only.)

use std::collections::HashSet;

use coven_github_api::{ReviewFinding, ReviewResult, ReviewSeverity};

/// Result of running the gate chain over a review's findings.
pub struct GateOutcome {
    /// Findings that may publish, highest severity first.
    pub published: Vec<ReviewFinding>,
    pub dropped_out_of_scope: usize,
    pub dropped_below_threshold: usize,
    pub dropped_duplicates: usize,
}

impl GateOutcome {
    pub fn dropped_total(&self) -> usize {
        self.dropped_out_of_scope + self.dropped_below_threshold + self.dropped_duplicates
    }
}

/// Applies the gate chain. `changed_files` is the PR's changed set from live
/// GitHub state; `min_severity` comes from repo policy (`None` = publish all
/// severities).
pub fn gate(
    review: &ReviewResult,
    changed_files: &[String],
    min_severity: Option<ReviewSeverity>,
) -> GateOutcome {
    let in_scope: HashSet<&str> = review
        .reviewed_files
        .iter()
        .chain(review.supporting_files.iter())
        .chain(changed_files.iter())
        .map(String::as_str)
        .collect();
    let threshold = min_severity.map(rank);

    let mut outcome = GateOutcome {
        published: Vec::new(),
        dropped_out_of_scope: 0,
        dropped_below_threshold: 0,
        dropped_duplicates: 0,
    };
    let mut seen: HashSet<(String, Option<u64>, String)> = HashSet::new();

    for finding in &review.findings {
        if !in_scope.contains(finding.file.as_str()) {
            outcome.dropped_out_of_scope += 1;
            continue;
        }
        if let Some(threshold) = threshold {
            if rank(finding.severity.clone()) < threshold {
                outcome.dropped_below_threshold += 1;
                continue;
            }
        }
        if !seen.insert((
            finding.file.clone(),
            finding.line,
            finding.title.clone(),
        )) {
            outcome.dropped_duplicates += 1;
            continue;
        }
        outcome.published.push(finding.clone());
    }

    outcome
        .published
        .sort_by_key(|f| std::cmp::Reverse(rank(f.severity.clone())));
    outcome
}

/// Renders the gated findings as a markdown digest for Check Run output and
/// (in advisory mode) the status comment. Bounded well under GitHub's 64 KiB
/// Check Run summary limit.
pub fn render(outcome: &GateOutcome) -> String {
    const MAX_LEN: usize = 48_000;

    let mut out = String::new();
    if outcome.published.is_empty() {
        out.push_str("**Findings:** none published.");
    } else {
        out.push_str(&format!(
            "**Findings ({} published):**\n",
            outcome.published.len()
        ));
        for finding in &outcome.published {
            let location = match finding.line {
                Some(line) => format!("`{}:{line}`", finding.file),
                None => format!("`{}`", finding.file),
            };
            let entry = match &finding.recommendation {
                Some(rec) => format!(
                    "- **{}** {location} — {}\n  {}\n  _Recommendation: {}_\n",
                    severity_label(&finding.severity),
                    finding.title,
                    finding.body,
                    rec
                ),
                None => format!(
                    "- **{}** {location} — {}\n  {}\n",
                    severity_label(&finding.severity),
                    finding.title,
                    finding.body
                ),
            };
            if out.len() + entry.len() > MAX_LEN {
                out.push_str("- _…digest truncated._\n");
                break;
            }
            out.push_str(&entry);
        }
    }
    if outcome.dropped_total() > 0 {
        out.push_str(&format!(
            "\n_{} finding(s) withheld by publication gates: {} out of scope, {} below the severity threshold, {} duplicate(s)._",
            outcome.dropped_total(),
            outcome.dropped_out_of_scope,
            outcome.dropped_below_threshold,
            outcome.dropped_duplicates,
        ));
    }
    out
}

/// Parses a policy severity string (`info` … `critical`).
pub fn parse_severity(value: &str) -> Option<ReviewSeverity> {
    match value.to_ascii_lowercase().as_str() {
        "info" => Some(ReviewSeverity::Info),
        "low" => Some(ReviewSeverity::Low),
        "medium" => Some(ReviewSeverity::Medium),
        "high" => Some(ReviewSeverity::High),
        "critical" => Some(ReviewSeverity::Critical),
        _ => None,
    }
}

fn rank(severity: ReviewSeverity) -> u8 {
    match severity {
        ReviewSeverity::Info => 0,
        ReviewSeverity::Low => 1,
        ReviewSeverity::Medium => 2,
        ReviewSeverity::High => 3,
        ReviewSeverity::Critical => 4,
    }
}

fn severity_label(severity: &ReviewSeverity) -> &'static str {
    match severity {
        ReviewSeverity::Info => "INFO",
        ReviewSeverity::Low => "LOW",
        ReviewSeverity::Medium => "MEDIUM",
        ReviewSeverity::High => "HIGH",
        ReviewSeverity::Critical => "CRITICAL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{ReviewEvidenceStatus, ReviewMode};

    fn finding(file: &str, line: Option<u64>, title: &str, severity: ReviewSeverity) -> ReviewFinding {
        ReviewFinding {
            severity,
            file: file.to_string(),
            line,
            title: title.to_string(),
            body: "body".to_string(),
            recommendation: None,
        }
    }

    fn review(findings: Vec<ReviewFinding>) -> ReviewResult {
        ReviewResult {
            mode: ReviewMode::PullRequest,
            evidence_status: ReviewEvidenceStatus::Complete,
            reviewed_files: vec!["src/lib.rs".to_string()],
            supporting_files: vec!["src/util.rs".to_string()],
            findings,
            tests_run: vec![],
            no_findings_reason: None,
            limitations: vec![],
        }
    }

    #[test]
    fn out_of_scope_findings_are_withheld() {
        let outcome = gate(
            &review(vec![
                finding("src/lib.rs", Some(1), "in scope", ReviewSeverity::Low),
                finding("secrets/vault.rs", Some(1), "speculative", ReviewSeverity::Critical),
            ]),
            &["src/changed.rs".to_string()],
            None,
        );
        assert_eq!(outcome.published.len(), 1);
        assert_eq!(outcome.published[0].file, "src/lib.rs");
        assert_eq!(outcome.dropped_out_of_scope, 1);
    }

    #[test]
    fn changed_and_supporting_files_count_as_scope() {
        let outcome = gate(
            &review(vec![
                finding("src/changed.rs", None, "on the diff", ReviewSeverity::Low),
                finding("src/util.rs", None, "supporting", ReviewSeverity::Low),
            ]),
            &["src/changed.rs".to_string()],
            None,
        );
        assert_eq!(outcome.published.len(), 2);
        assert_eq!(outcome.dropped_total(), 0);
    }

    #[test]
    fn severity_threshold_filters_and_counts() {
        let outcome = gate(
            &review(vec![
                finding("src/lib.rs", Some(1), "nit", ReviewSeverity::Info),
                finding("src/lib.rs", Some(2), "bug", ReviewSeverity::High),
            ]),
            &[],
            Some(ReviewSeverity::Medium),
        );
        assert_eq!(outcome.published.len(), 1);
        assert_eq!(outcome.published[0].title, "bug");
        assert_eq!(outcome.dropped_below_threshold, 1);
    }

    #[test]
    fn duplicates_collapse_and_output_orders_by_severity() {
        let outcome = gate(
            &review(vec![
                finding("src/lib.rs", Some(1), "same", ReviewSeverity::Low),
                finding("src/lib.rs", Some(1), "same", ReviewSeverity::Low),
                finding("src/lib.rs", Some(9), "worse", ReviewSeverity::Critical),
            ]),
            &[],
            None,
        );
        assert_eq!(outcome.dropped_duplicates, 1);
        assert_eq!(
            outcome
                .published
                .iter()
                .map(|f| f.title.as_str())
                .collect::<Vec<_>>(),
            vec!["worse", "same"]
        );
    }

    #[test]
    fn render_reports_published_and_withheld_honestly() {
        let outcome = gate(
            &review(vec![
                finding("src/lib.rs", Some(10), "Off-by-one", ReviewSeverity::High),
                finding("elsewhere.rs", None, "speculative", ReviewSeverity::Low),
            ]),
            &[],
            None,
        );
        let text = render(&outcome);
        assert!(text.contains("**HIGH** `src/lib.rs:10` — Off-by-one"), "{text}");
        assert!(text.contains("1 finding(s) withheld"), "{text}");
        assert!(text.contains("1 out of scope"), "{text}");
    }

    #[test]
    fn render_names_the_empty_case() {
        let outcome = gate(&review(vec![]), &[], None);
        assert_eq!(render(&outcome), "**Findings:** none published.");
    }

    #[test]
    fn digest_is_bounded() {
        let many: Vec<ReviewFinding> = (0..5000)
            .map(|i| {
                finding(
                    "src/lib.rs",
                    Some(i),
                    &format!("finding number {i} with a reasonably long title"),
                    ReviewSeverity::Medium,
                )
            })
            .collect();
        let text = render(&gate(&review(many), &[], None));
        assert!(text.len() < 50_000, "digest must stay bounded: {}", text.len());
        assert!(text.contains("digest truncated"));
    }

    #[test]
    fn severity_strings_parse_and_reject_garbage() {
        assert_eq!(parse_severity("HIGH"), Some(ReviewSeverity::High));
        assert_eq!(parse_severity("info"), Some(ReviewSeverity::Info));
        assert_eq!(parse_severity("everything"), None);
    }
}
