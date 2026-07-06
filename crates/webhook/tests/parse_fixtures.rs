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
    let event = parse("issues", include_str!("fixtures/issue_assigned.json"));
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

#[test]
fn pull_request_opened_fixture_parses() {
    let event = parse("pull_request", include_str!("fixtures/pull_request_opened.json"));

    match event {
        GitHubEvent::PullRequestChanged(e) => {
            assert_eq!(e.installation_id, 123);
            assert_eq!(e.repo_owner, "OpenCoven");
            assert_eq!(e.repo_name, "coven-code");
            assert_eq!(e.pr_number, 88);
            assert_eq!(e.pr_title, "Add spell compiler cache");
            assert_eq!(e.action, "opened");
            assert_eq!(e.label_name, None);
            assert_eq!(e.head_ref, "feat/spell-cache");
            assert_eq!(e.head_sha, "abc123def4567890abc123def4567890abc123de");
            assert_eq!(e.base_ref, "main");
            assert_eq!(e.author_login, "octocat");
            assert!(!e.draft);
        }
        other => panic!("expected PullRequestChanged, got {other:?}"),
    }
}

#[test]
fn pull_request_synchronize_fixture_carries_new_head_and_draft_flag() {
    let event = parse(
        "pull_request",
        include_str!("fixtures/pull_request_synchronize.json"),
    );

    match event {
        GitHubEvent::PullRequestChanged(e) => {
            assert_eq!(e.action, "synchronize");
            assert_eq!(e.head_sha, "f00dfacef00dfacef00dfacef00dfacef00dface");
            assert!(e.draft, "synchronize fixture is a draft PR");
        }
        other => panic!("expected PullRequestChanged, got {other:?}"),
    }
}

#[test]
fn pull_request_ready_for_review_fixture_parses() {
    let event = parse(
        "pull_request",
        include_str!("fixtures/pull_request_ready_for_review.json"),
    );

    match event {
        GitHubEvent::PullRequestChanged(e) => {
            assert_eq!(e.action, "ready_for_review");
            assert_eq!(e.pr_number, 91);
            assert_eq!(e.author_login, "hexadecimal-cat");
            assert!(!e.draft, "ready_for_review means the PR left draft state");
        }
        other => panic!("expected PullRequestChanged, got {other:?}"),
    }
}

#[test]
fn pull_request_closed_action_is_unsupported() {
    // Reuse the opened fixture body under a non-triggering action.
    let raw = include_str!("fixtures/pull_request_opened.json").replace("\"opened\"", "\"closed\"");
    let event = parse("pull_request", &raw);

    assert!(
        matches!(event, GitHubEvent::Unsupported { .. }),
        "closed must not trigger review, got {event:?}"
    );
}

#[test]
fn push_default_branch_fixture_parses() {
    let event = parse("push", include_str!("fixtures/push_default_branch.json"));

    match event {
        GitHubEvent::Push(e) => {
            assert_eq!(e.installation_id, 123);
            assert_eq!(e.repo_owner, "OpenCoven");
            assert_eq!(e.repo_name, "coven-code");
            assert_eq!(e.branch.as_deref(), Some("main"));
            assert_eq!(e.before_sha, "0001112223334445556667778889990001112223");
            assert_eq!(e.after_sha, "c0ffeec0ffeec0ffeec0ffeec0ffeec0ffeec0ff");
            assert!(!e.deleted);
            assert!(!e.forced);
            assert_eq!(e.pusher_login, "octocat");
            assert_eq!(e.commit_count, 2);
        }
        other => panic!("expected Push, got {other:?}"),
    }
}

#[test]
fn push_feature_branch_fixture_parses_forced_push() {
    let event = parse("push", include_str!("fixtures/push_feature_branch.json"));

    match event {
        GitHubEvent::Push(e) => {
            assert_eq!(e.branch.as_deref(), Some("feat/spell-cache"));
            assert!(e.forced);
            assert_eq!(e.commit_count, 1);
        }
        other => panic!("expected Push, got {other:?}"),
    }
}

#[test]
fn push_branch_deleted_fixture_is_flagged_deleted() {
    let event = parse("push", include_str!("fixtures/push_branch_deleted.json"));

    match event {
        GitHubEvent::Push(e) => {
            assert!(e.deleted);
            assert_eq!(e.after_sha, "0000000000000000000000000000000000000000");
            assert_eq!(e.commit_count, 0);
        }
        other => panic!("expected Push, got {other:?}"),
    }
}
