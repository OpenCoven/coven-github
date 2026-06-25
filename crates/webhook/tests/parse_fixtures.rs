//! Fixture-driven parsing tests.
//!
//! Each fixture is a trimmed but structurally faithful GitHub webhook payload
//! captured from the real `X-GitHub-Event` deliveries we subscribe to. They
//! exercise `parse_event` end-to-end (raw JSON → typed `GitHubEvent`), guarding
//! against drift between GitHub's payload shape and our partial deserializers.

use coven_github_api::GitHubEvent;
use coven_github_webhook::events::{parse_event, WebhookPayload};

/// Parse a fixture string under the given `X-GitHub-Event` type.
fn parse(event_type: &str, raw: &str) -> GitHubEvent {
    let payload: WebhookPayload =
        serde_json::from_str(raw).expect("fixture should deserialize as a WebhookPayload");
    parse_event(event_type, &payload)
}

#[test]
fn issue_assigned_fixture_parses() {
    let event = parse(
        "issues",
        include_str!("fixtures/issue_assigned.json"),
    );
    match event {
        GitHubEvent::IssueAssigned(e) => {
            assert_eq!(e.installation_id, 987654);
            assert_eq!(e.repo_owner, "OpenCoven");
            assert_eq!(e.repo_name, "coven-code");
            assert_eq!(e.issue_number, 42);
            assert_eq!(e.issue_title, "Fix OAuth token refresh");
            assert_eq!(e.assignee_login, "coven-cody[bot]");
        }
        other => panic!("expected IssueAssigned, got {other:?}"),
    }
}

#[test]
fn issue_labeled_fixture_parses() {
    let event = parse("issues", include_str!("fixtures/issue_labeled.json"));
    match event {
        GitHubEvent::IssueLabeled(e) => {
            assert_eq!(e.issue_number, 42);
            assert_eq!(e.label_name, "coven:fix");
        }
        other => panic!("expected IssueLabeled, got {other:?}"),
    }
}

#[test]
fn issue_comment_fixture_parses_as_issue_comment() {
    let event = parse(
        "issue_comment",
        include_str!("fixtures/issue_comment_created.json"),
    );
    match event {
        GitHubEvent::IssueComment(e) => {
            assert_eq!(e.issue_number, 42);
            assert_eq!(e.commenter_login, "octocat");
            assert!(
                !e.on_pull_request,
                "a plain issue comment must not be flagged as a PR comment"
            );
            assert!(e.comment_body.contains("@coven-cody"));
        }
        other => panic!("expected IssueComment, got {other:?}"),
    }
}

#[test]
fn issue_comment_on_pr_fixture_is_flagged_as_pull_request() {
    // GitHub delivers PR conversation comments via `issue_comment`; the only
    // signal that distinguishes them is `issue.pull_request`.
    let event = parse(
        "issue_comment",
        include_str!("fixtures/issue_comment_on_pr.json"),
    );
    match event {
        GitHubEvent::IssueComment(e) => {
            assert_eq!(e.issue_number, 73);
            assert!(
                e.on_pull_request,
                "a comment carrying issue.pull_request must be flagged as a PR comment"
            );
        }
        other => panic!("expected IssueComment, got {other:?}"),
    }
}

#[test]
fn pull_request_review_fixture_parses() {
    let event = parse(
        "pull_request_review",
        include_str!("fixtures/pull_request_review_submitted.json"),
    );
    match event {
        GitHubEvent::PullRequestReview(e) => {
            assert_eq!(e.pr_number, 73);
            assert_eq!(e.review_state, "changes_requested");
            assert_eq!(e.reviewer_login, "octocat");
            assert!(e.review_body.contains("test coverage"));
        }
        other => panic!("expected PullRequestReview, got {other:?}"),
    }
}

#[test]
fn pull_request_review_comment_fixture_parses() {
    let event = parse(
        "pull_request_review_comment",
        include_str!("fixtures/pull_request_review_comment_created.json"),
    );
    match event {
        GitHubEvent::PullRequestReviewComment(e) => {
            assert_eq!(e.pr_number, 73);
            assert_eq!(e.commenter_login, "octocat");
        }
        other => panic!("expected PullRequestReviewComment, got {other:?}"),
    }
}

#[test]
fn ping_fixture_parses_as_ping() {
    let event = parse("ping", include_str!("fixtures/ping.json"));
    assert!(
        matches!(event, GitHubEvent::Ping),
        "ping delivery should parse as GitHubEvent::Ping, got {event:?}"
    );
}
