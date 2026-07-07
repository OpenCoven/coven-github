//! Token-pattern scanning and redaction for agent-visible artifacts (issue #4).
//!
//! Every free-text field of the result envelope passes through here before the
//! adapter persists or publishes it (task store, comments, PR bodies, Check
//! Run output). Redaction is belt-and-braces on top of scoped tokens: a leaked
//! value has already been reduced to single-repo authority, but it must still
//! never reach a durable store or GitHub output.

use coven_github_api::SessionResult;

/// Replacement marker for redacted credentials.
pub const REDACTED: &str = "[redacted-token]";

/// GitHub token prefixes: installation, classic PAT, OAuth, user-to-server,
/// refresh, fine-grained PAT.
const PREFIXES: [&str; 6] = ["ghs_", "ghp_", "gho_", "ghu_", "ghr_", "github_pat_"];

/// Minimum credential-body length before a prefix hit is treated as a token.
/// Real GitHub credential bodies are 30+ chars; this avoids redacting prose
/// that merely mentions a prefix.
const MIN_TOKEN_BODY: usize = 20;

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Replaces exact live token values, GitHub token patterns, and
/// `x-access-token:` URL credentials with [`REDACTED`].
pub fn redact(text: &str, live_tokens: &[&str]) -> String {
    let mut out = text.to_string();
    for token in live_tokens {
        if !token.is_empty() {
            out = out.replace(token, REDACTED);
        }
    }
    // Credentialed clone URLs: scrub the whole userinfo segment.
    out = redact_pattern(
        &out,
        "x-access-token:",
        |c| c != '@' && c != '"' && !c.is_whitespace(),
        1,
    );
    for prefix in PREFIXES {
        out = redact_pattern(&out, prefix, token_char, MIN_TOKEN_BODY);
    }
    out
}

/// True when `text` contains one of the adapter's live token values.
///
/// Deliberately does NOT pattern-match: user-authored task content may
/// legitimately quote token-shaped strings (e.g. an issue reporting a leak),
/// and that must not veto writing the session brief.
pub fn contains_live_token(text: &str, live_tokens: &[&str]) -> bool {
    live_tokens
        .iter()
        .any(|token| !token.is_empty() && text.contains(token))
}

/// Redacts every free-text field of a session result in place.
pub fn sanitize_result(result: &mut SessionResult, live_tokens: &[&str]) {
    fix(&mut result.summary, live_tokens);
    fix(&mut result.pr_body, live_tokens);
    fix_opt(&mut result.branch, live_tokens);
    for commit in &mut result.commits {
        fix(&mut commit.sha, live_tokens);
        fix(&mut commit.message, live_tokens);
    }
    for file in &mut result.files_changed {
        fix(file, live_tokens);
    }
    let review = &mut result.review;
    for file in review
        .reviewed_files
        .iter_mut()
        .chain(review.supporting_files.iter_mut())
    {
        fix(file, live_tokens);
    }
    for finding in &mut review.findings {
        fix(&mut finding.file, live_tokens);
        fix(&mut finding.title, live_tokens);
        fix(&mut finding.body, live_tokens);
        fix_opt(&mut finding.recommendation, live_tokens);
    }
    for test in &mut review.tests_run {
        fix(&mut test.command, live_tokens);
        fix_opt(&mut test.output_summary, live_tokens);
    }
    fix_opt(&mut review.no_findings_reason, live_tokens);
    for limitation in &mut review.limitations {
        fix(limitation, live_tokens);
    }
}

fn fix(text: &mut String, live_tokens: &[&str]) {
    let redacted = redact(text, live_tokens);
    if redacted != *text {
        *text = redacted;
    }
}

fn fix_opt(text: &mut Option<String>, live_tokens: &[&str]) {
    if let Some(text) = text {
        fix(text, live_tokens);
    }
}

/// Replaces every occurrence of `prefix` followed by at least `min_body`
/// consecutive `body_char` characters with [`REDACTED`].
fn redact_pattern(
    text: &str,
    prefix: &str,
    body_char: fn(char) -> bool,
    min_body: usize,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(prefix) {
        let (head, tail) = rest.split_at(idx);
        out.push_str(head);
        let after = &tail[prefix.len()..];
        let body_len: usize = after
            .chars()
            .take_while(|&c| body_char(c))
            .map(char::len_utf8)
            .sum();
        if body_len >= min_body {
            out.push_str(REDACTED);
            rest = &after[body_len..];
        } else {
            out.push_str(prefix);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{
        CommitInfo, ReviewEvidenceStatus, ReviewFinding, ReviewMode, ReviewResult,
        ReviewSeverity, ReviewTestRun, ReviewTestStatus, SessionResult, SessionStatus,
        HEADLESS_CONTRACT_VERSION,
    };

    #[test]
    fn live_token_values_are_replaced() {
        let out = redact("pushed with ghs_live123 ok", &["ghs_live123"]);
        assert_eq!(out, format!("pushed with {REDACTED} ok"));
    }

    #[test]
    fn github_token_patterns_are_redacted_without_knowing_the_value() {
        let text = "leaked ghs_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 in output";
        let out = redact(text, &[]);
        assert_eq!(out, format!("leaked {REDACTED} in output"));
    }

    #[test]
    fn x_access_token_url_credentials_are_redacted() {
        let text = "https://x-access-token:ghs_short@github.com/o/r.git";
        let out = redact(text, &[]);
        assert_eq!(out, format!("https://{REDACTED}@github.com/o/r.git"));
    }

    #[test]
    fn fine_grained_pat_pattern_is_redacted() {
        let out = redact("github_pat_11ABCDEFG0123456789abcdefghij tail", &[]);
        assert_eq!(out, format!("{REDACTED} tail"));
    }

    #[test]
    fn short_prefix_lookalikes_are_left_alone() {
        let text = "the ghs_ prefix and ghp_abc are not credentials";
        assert_eq!(redact(text, &[]), text);
    }

    #[test]
    fn contains_live_token_matches_exact_values_only() {
        assert!(contains_live_token(
            "brief with ghs_live123 inside",
            &["ghs_live123"]
        ));
        // Pattern-shaped strings in user-authored text (e.g. an issue body
        // quoting a leaked token) must NOT trip the live check — that would
        // let issue content veto task execution.
        assert!(!contains_live_token(
            "quotes ghs_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
            &["ghs_other"]
        ));
    }

    #[test]
    fn sanitize_result_scrubs_every_free_text_field() {
        let tok = "ghs_liveTOKENliveTOKENliveTOKEN000";
        let poison = |field: &str| format!("{field} leaks {tok}");
        let mut result = SessionResult {
            contract_version: HEADLESS_CONTRACT_VERSION.to_string(),
            status: SessionStatus::Success,
            branch: Some(poison("branch")),
            commits: vec![CommitInfo {
                sha: poison("sha"),
                message: poison("message"),
            }],
            files_changed: vec![poison("file")],
            summary: poison("summary"),
            pr_body: poison("pr_body"),
            review: ReviewResult {
                mode: ReviewMode::PullRequest,
                evidence_status: ReviewEvidenceStatus::Complete,
                reviewed_files: vec![poison("reviewed")],
                supporting_files: vec![poison("supporting")],
                findings: vec![ReviewFinding {
                    severity: ReviewSeverity::Low,
                    file: poison("finding-file"),
                    line: None,
                    title: poison("finding-title"),
                    body: poison("finding-body"),
                    recommendation: Some(poison("finding-rec")),
                }],
                tests_run: vec![ReviewTestRun {
                    command: poison("command"),
                    status: ReviewTestStatus::Passed,
                    output_summary: Some(poison("output")),
                }],
                no_findings_reason: Some(poison("reason")),
                limitations: vec![poison("limitation")],
            },
            exit_reason: None,
            memory_used: None,
        };

        sanitize_result(&mut result, &[tok]);

        let json = serde_json::to_string(&result).expect("result should serialize");
        assert!(!json.contains(tok), "sanitized result still leaked: {json}");
        assert!(json.contains(REDACTED));
    }
}
