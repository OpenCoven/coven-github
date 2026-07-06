//! Headless execution contract conformance.
//!
//! Pins the adapter's wire types to the locked contract in
//! `docs/headless-contract.md` via the golden fixtures in `docs/contracts/`.
//! If these fail, either the runtime contract drifted or the docs are stale —
//! fix one of them, do not just bless the test.

use coven_github_api::{
    ExitReason, ReviewEvidenceStatus, ReviewMode, SessionResult, SessionStatus,
    HEADLESS_CONTRACT_VERSION,
};
use coven_github_worker::brief::SessionBrief;

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/../../docs/contracts/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing fixture {path}: {e}"))
}

#[test]
fn golden_session_brief_deserializes_into_adapter_type() {
    let raw = fixture("session-brief.example.json");
    let brief: SessionBrief =
        serde_json::from_str(&raw).expect("golden brief must match SessionBrief");

    assert_eq!(brief.contract_version, HEADLESS_CONTRACT_VERSION);
    assert_eq!(brief.trigger, "issue_assigned");
    assert_eq!(brief.repo.owner, "OpenCoven");
    assert_eq!(brief.repo.default_branch, "main");
    assert_eq!(brief.familiar.id, "cody");

    // Round-trips without losing or inventing fields.
    let reserialized = serde_json::to_value(&brief).expect("brief reserializes");
    let original: serde_json::Value = serde_json::from_str(&raw).expect("fixture is valid json");
    assert_eq!(reserialized, original, "brief did not round-trip cleanly");
}

#[test]
fn hosted_review_session_brief_deserializes_optional_context() {
    let raw = r#"{
        "contract_version": "2",
        "trigger": "pr_review_comment",
        "repo": {
            "owner": "OpenCoven",
            "name": "coven-github",
            "clone_url": "https://github.com/OpenCoven/coven-github.git",
            "default_branch": "main"
        },
        "task": {
            "kind": "address_review_comment",
            "pr_number": 31,
            "comment_body": "review this",
            "diff_hunk": null
        },
        "familiar": {
            "id": "cody",
            "display_name": "Cody",
            "model": null,
            "skills": []
        },
        "workspace": {
            "root": "/tmp/coven"
        },
        "review_context": {
            "pr_number": 31,
            "files": [{ "path": "src/lib.rs" }]
        },
        "audit_instruction": "Inspect supplied changed-file patches."
    }"#;

    let brief: SessionBrief =
        serde_json::from_str(raw).expect("hosted review brief must match SessionBrief");

    assert_eq!(brief.trigger, "pr_review_comment");
    assert_eq!(
        brief
            .review_context
            .as_ref()
            .and_then(|context| context.get("pr_number"))
            .and_then(serde_json::Value::as_u64),
        Some(31)
    );
    assert_eq!(
        brief.audit_instruction.as_deref(),
        Some("Inspect supplied changed-file patches.")
    );
}

#[test]
fn golden_result_deserializes_into_adapter_type() {
    let raw = fixture("result.example.json");
    let result: SessionResult =
        serde_json::from_str(&raw).expect("golden result must match SessionResult");

    assert_eq!(result.contract_version, HEADLESS_CONTRACT_VERSION);
    assert_eq!(result.status, SessionStatus::Success);
    assert_eq!(result.review.mode, ReviewMode::None);
    assert_eq!(
        result.review.evidence_status,
        ReviewEvidenceStatus::NotApplicable
    );
    assert_eq!(result.branch.as_deref(), Some("cody/fix-issue-42"));
    assert_eq!(result.commits.len(), 1);
    assert_eq!(
        result.files_changed,
        vec!["src/auth/refresh.rs".to_string()]
    );
    assert!(result.exit_reason.is_none());
}

#[test]
fn result_without_contract_version_is_rejected() {
    let raw = r#"{
        "status": "failure",
        "branch": null,
        "commits": [],
        "files_changed": [],
        "summary": "Could not reproduce.",
        "pr_body": "",
        "review": {
            "mode": "none",
            "evidence_status": "not_applicable",
            "reviewed_files": [],
            "supporting_files": [],
            "findings": [],
            "tests_run": [],
            "no_findings_reason": null,
            "limitations": []
        },
        "exit_reason": "ambiguous_spec"
    }"#;
    let error = serde_json::from_str::<SessionResult>(raw)
        .expect_err("v2 result without contract_version must be rejected");

    assert!(
        error
            .to_string()
            .contains("missing field `contract_version`"),
        "unexpected error: {error}"
    );
}

#[test]
fn every_result_status_variant_is_wire_named_as_documented() {
    for (json, expected) in [
        ("success", SessionStatus::Success),
        ("failure", SessionStatus::Failure),
        ("partial", SessionStatus::Partial),
        ("needs_input", SessionStatus::NeedsInput),
    ] {
        let parsed: SessionStatus =
            serde_json::from_str(&format!("\"{json}\"")).expect("documented status must parse");
        assert_eq!(parsed, expected);
    }
}

#[test]
fn every_exit_reason_variant_is_wire_named_as_documented() {
    for (json, expected) in [
        ("test_failure", ExitReason::TestFailure),
        ("ambiguous_spec", ExitReason::AmbiguousSpec),
        ("git_conflict", ExitReason::GitConflict),
        ("infra_error", ExitReason::InfraError),
    ] {
        let parsed: ExitReason = serde_json::from_str(&format!("\"{json}\""))
            .expect("documented exit_reason must parse");
        assert_eq!(parsed, expected);
    }
}
