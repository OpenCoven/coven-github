//! GitHub API client: installation tokens, Check Runs, PRs, issue comments.

use serde::{Deserialize, Serialize};

pub mod check_run;
pub mod installation;
pub mod pr;
pub mod repo;
pub mod tasks;

pub const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

/// Major version of the coven-code headless execution contract this adapter
/// speaks. See `docs/headless-contract.md`. Bump only on breaking changes.
pub const HEADLESS_CONTRACT_VERSION: &str = "2";

const GITHUB_API_VERSION: &str = "2026-03-10";

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitHubRequest {
    method: &'static str,
    path: String,
    body: serde_json::Value,
}

fn api_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

fn encode_ref_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("coven-github/0.1")
        .build()
        .map_err(Into::into)
}

async fn send_json(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    request: GitHubRequest,
) -> anyhow::Result<reqwest::Response> {
    let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
    let mut builder = client
        .request(method, api_url(base_url, &request.path))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION);
    // GET/metadata requests carry no body; only attach JSON for mutations.
    if !request.body.is_null() {
        builder = builder.json(&request.body);
    }
    let response = builder.send().await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub API request failed with {status}: {body}");
    }

    Ok(response)
}

/// Minimal GitHub event types parsed from webhooks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum GitHubEvent {
    IssueAssigned(IssueAssignedEvent),
    IssueLabeled(IssueLabeledEvent),
    IssueComment(IssueCommentEvent),
    PullRequestReview(PrReviewEvent),
    PullRequestReviewComment(PrReviewCommentEvent),
    PullRequestChanged(PrChangedEvent),
    Push(PushEvent),
    /// `ping` delivery GitHub sends when a webhook is first configured.
    Ping,
    Unsupported {
        name: String,
    },
}

/// Pull-request lifecycle change relevant to review triggers
/// (`pull_request` → opened / synchronize / reopened / ready_for_review /
/// labeled). Carries the refs at event time so review tasks pin an immutable
/// target (issue #10).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrChangedEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub pr_title: String,
    /// The webhook action that fired.
    pub action: String,
    /// Set for `labeled` actions.
    pub label_name: Option<String>,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub author_login: String,
    pub draft: bool,
}

/// Branch push. Parsed and typed today; the review execution lane ships with
/// headless contract v3 — v2 task kinds cannot express a PR-less review
/// (issue #10).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PushEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    /// `None` for refs outside `refs/heads/` (e.g. tag pushes).
    pub branch: Option<String>,
    pub before_sha: String,
    pub after_sha: String,
    pub deleted: bool,
    pub forced: bool,
    pub pusher_login: String,
    pub commit_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueAssignedEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_body: String,
    pub assignee_login: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueLabeledEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub issue_number: u64,
    pub issue_title: String,
    pub issue_body: String,
    pub label_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IssueCommentEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub issue_number: u64,
    /// Issue (or PR) title — needed when a `fix` command turns the comment
    /// into a FixIssue task (issue #13).
    pub issue_title: String,
    pub issue_body: String,
    pub comment_body: String,
    pub commenter_login: String,
    /// `issue_comment` fires for pull-request conversation comments as well as
    /// issue comments. GitHub flags the former with an `issue.pull_request`
    /// object; this lets routing send PR comments through PR iteration rather
    /// than issue-mention handling.
    pub on_pull_request: bool,
}

/// Top-level pull request review submission (`pull_request_review` → `submitted`).
///
/// Distinct from [`PrReviewCommentEvent`], which is a single inline comment on a
/// diff hunk. This carries the review summary body and verdict (state).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrReviewEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub review_body: String,
    /// Review verdict: `approved`, `changes_requested`, or `commented`.
    pub review_state: String,
    pub reviewer_login: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrReviewCommentEvent {
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub pr_number: u64,
    pub pr_title: String,
    pub comment_body: String,
    pub commenter_login: String,
}

/// A task dispatched to the worker queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub installation_id: u64,
    pub repo_owner: String,
    pub repo_name: String,
    pub kind: TaskKind,
    pub familiar_id: String,
    /// Login of the maintainer whose command initiated this task (issue #13).
    /// The worker checks their repo permission pre-flight and declines below
    /// `write`. `None` for tasks from non-command triggers.
    #[serde(default)]
    pub commander: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskKind {
    FixIssue {
        issue_number: u64,
        issue_title: String,
        issue_body: String,
    },
    AddressReviewComment {
        pr_number: u64,
        comment_body: String,
        diff_hunk: Option<String>,
    },
    RespondToMention {
        issue_number: u64,
        comment_body: String,
    },
    /// Adapter-initiated hosted review of a pull request (issue #10). Target
    /// refs are resolved live at execution; supersession — not ref pinning —
    /// keeps reviews current when the head moves.
    ReviewPullRequest {
        pr_number: u64,
        pr_title: String,
        /// What triggered the review: a webhook action (opened, synchronize, …)
        /// or a maintainer command (`command:review`, `command:deepen`, …).
        reason: String,
    },
    /// Adapter-only reply on an issue/PR conversation (issue #13): command
    /// acknowledgements, clarifications, status answers, permission declines.
    /// Executed without spawning coven-code.
    CommandReply {
        issue_number: u64,
        body: String,
    },
    /// Adapter-only cancellation of queued PR reviews (issue #13). The worker
    /// gates the commander's write access before tombstoning anything.
    CancelReviews {
        pr_number: u64,
    },
}

/// Structured result envelope written by coven-code --headless.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResult {
    /// Contract major version. Conformant producers MUST emit it. See
    /// `docs/headless-contract.md`.
    pub contract_version: String,
    pub status: SessionStatus,
    pub branch: Option<String>,
    pub commits: Vec<CommitInfo>,
    pub files_changed: Vec<String>,
    pub summary: String,
    pub pr_body: String,
    pub review: ReviewResult,
    pub exit_reason: Option<ExitReason>,
    /// Memory activity the runtime reports for hosted governance (issue #6).
    /// Absent/`None` when the run did no memory work. The adapter re-validates
    /// every proposed write against the invocation's memory policy before
    /// anything is persisted — see `docs/memory-contract.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used: Option<MemoryUsed>,
}

/// Runtime-reported memory activity for one session (issue #6).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryUsed {
    pub enabled: bool,
    /// Entries loaded and used — the basis for citing what shaped a review.
    #[serde(default)]
    pub read: Vec<MemoryEntryRef>,
    /// Candidate writes the runtime proposes (subject to adapter validation
    /// and, when the policy requires it, maintainer approval).
    #[serde(default)]
    pub proposed: Vec<ProposedMemory>,
    /// Candidates the runtime itself declined, with a reason.
    #[serde(default)]
    pub rejected: Vec<RejectedMemory>,
}

/// A memory entry the runtime read, by fully-qualified id and namespace scope.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryEntryRef {
    pub id: String,
    pub scope: String,
}

/// A memory write the runtime proposes.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProposedMemory {
    pub key: String,
    pub summary: String,
    pub scope: String,
    /// `pending` | `applied` | `auto`.
    pub approval: String,
}

/// A memory write the runtime declined on its own.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RejectedMemory {
    pub summary: String,
    pub scope: String,
    /// `pii` | `secret` | `out_of_scope` | `low_confidence`.
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Success,
    Failure,
    Partial,
    NeedsInput,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewResult {
    pub mode: ReviewMode,
    pub evidence_status: ReviewEvidenceStatus,
    pub reviewed_files: Vec<String>,
    pub supporting_files: Vec<String>,
    pub findings: Vec<ReviewFinding>,
    pub tests_run: Vec<ReviewTestRun>,
    pub no_findings_reason: Option<String>,
    pub limitations: Vec<String>,
}

impl ReviewResult {
    pub fn none() -> Self {
        Self {
            mode: ReviewMode::None,
            evidence_status: ReviewEvidenceStatus::NotApplicable,
            reviewed_files: Vec::new(),
            supporting_files: Vec::new(),
            findings: Vec::new(),
            tests_run: Vec::new(),
            no_findings_reason: None,
            limitations: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    None,
    PullRequest,
    ReviewComment,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvidenceStatus {
    NotApplicable,
    Complete,
    Partial,
    Missing,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFinding {
    pub severity: ReviewSeverity,
    pub file: String,
    pub line: Option<u64>,
    pub title: String,
    pub body: String,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTestRun {
    pub command: String,
    pub status: ReviewTestStatus,
    pub output_summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewTestStatus {
    Passed,
    Failed,
    NotRun,
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TestFailure,
    AmbiguousSpec,
    GitConflict,
    InfraError,
}

#[cfg(test)]
mod memory_result_tests {
    use super::*;

    const BASE: &str = r#"{"contract_version":"2","status":"success","branch":null,"commits":[],"files_changed":[],"summary":"s","pr_body":"","review":{"mode":"none","evidence_status":"not_applicable","reviewed_files":[],"supporting_files":[],"findings":[],"tests_run":[],"no_findings_reason":null,"limitations":[]},"exit_reason":null"#;

    #[test]
    fn result_without_memory_used_parses_as_none_and_omits_on_reserialize() {
        let json = format!("{BASE}}}");
        let result: SessionResult = serde_json::from_str(&json).expect("parses");
        assert!(result.memory_used.is_none());
        // Backward compatible: a memory-free result never grows the field.
        let out = serde_json::to_string(&result).expect("serializes");
        assert!(!out.contains("memory_used"), "unexpected field: {out}");
    }

    #[test]
    fn result_with_memory_used_round_trips() {
        let json = format!(
            r#"{BASE},"memory_used":{{"enabled":true,"read":[{{"id":"repo/o/r/conventions/x","scope":"repo"}}],"proposed":[{{"key":"repo/o/r/conventions/y","summary":"y","scope":"repo","approval":"pending"}}],"rejected":[]}}}}"#
        );
        let result: SessionResult = serde_json::from_str(&json).expect("parses");
        let mem = result.memory_used.as_ref().expect("memory_used present");
        assert!(mem.enabled);
        assert_eq!(mem.read[0].id, "repo/o/r/conventions/x");
        assert_eq!(mem.proposed[0].approval, "pending");

        let value = serde_json::to_value(&result).expect("serializes");
        assert_eq!(value["memory_used"]["proposed"][0]["scope"], "repo");
    }

    #[test]
    fn unknown_memory_field_is_rejected() {
        let json = format!(
            r#"{BASE},"memory_used":{{"enabled":true,"bogus":1}}}}"#
        );
        let err = serde_json::from_str::<SessionResult>(&json).expect_err("deny_unknown_fields");
        assert!(err.to_string().contains("bogus"), "unexpected error: {err}");
    }
}
