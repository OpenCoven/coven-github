//! Hosted memory governance policy (issue #6, `docs/memory-contract.md`).
//!
//! The adapter is the policy authority: it computes a [`MemoryPolicy`] from
//! installation policy, repository, and actor trust, stamps it into the session
//! brief, and — crucially — **re-validates** what the runtime reports it did
//! against that same policy. A runtime bug or compromise therefore cannot
//! silently exceed the grant, because [`validate_memory_used`] rejects any
//! read or write the policy did not authorize before it is persisted.

use serde::Serialize;

use coven_github_api::MemoryUsed;

/// Actor trust level, computed by the adapter — never inferred by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustScope {
    /// admin/maintain/write on the target repo (see the #13 permission gate).
    Maintainer,
    /// triage/read collaborator.
    Collaborator,
    /// Non-collaborator commenter.
    External,
    /// Pull request whose head is a fork — untrusted content.
    ForkPr,
    /// Branch Gardener / cron (#14): system-authored, no untrusted input.
    Scheduled,
}

/// A memory namespace. Never a raw path — keys are resolved under a namespace
/// and MUST be prefixed by the owning coordinates (see [`validate_memory_used`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    /// This repository's memory.
    Repo,
    /// Cross-repo memory within one installation.
    TenantShared,
    /// Cross-tenant — never granted in hosted mode by default.
    FamiliarGlobal,
}

impl MemoryScope {
    fn parse(s: &str) -> Option<MemoryScope> {
        match s {
            "repo" => Some(MemoryScope::Repo),
            "tenant_shared" => Some(MemoryScope::TenantShared),
            "familiar_global" => Some(MemoryScope::FamiliarGlobal),
            _ => None,
        }
    }
}

/// The computed policy the adapter stamps into the brief and validates against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryPolicy {
    pub enabled: bool,
    pub installation_id: u64,
    /// `owner/name` — the addressing authority for `repo`-scoped keys.
    pub repo: String,
    pub trust_scope: TrustScope,
    pub read_scopes: Vec<MemoryScope>,
    pub write_scopes: Vec<MemoryScope>,
    pub approval_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

/// Inputs the adapter derives before computing a policy.
pub struct PolicyInputs<'a> {
    /// Installation policy opt-in for this repo. `false` → deny by default.
    pub enabled: bool,
    pub installation_id: u64,
    /// `owner/name`.
    pub repo: &'a str,
    pub trust: TrustScope,
    pub approval_required: bool,
    pub retention_days: Option<u32>,
}

/// Computes the memory policy for one invocation, **deny-by-default**: returns
/// `None` when memory is not opted in, so an omitted brief block always means
/// "no memory", never "unrestricted". When enabled, read/write scopes are
/// granted by trust level per the contract's trust table.
pub fn compute_policy(input: PolicyInputs) -> Option<MemoryPolicy> {
    if !input.enabled {
        return None;
    }
    let (read_scopes, write_scopes) = grants(input.trust);
    // A collaborator may propose but every write needs approval; fork/external
    // never write. Force approval on whenever the trust level demands it, so a
    // permissive config cannot loosen the trust rule.
    let approval_required = input.approval_required || approval_forced(input.trust);
    Some(MemoryPolicy {
        enabled: true,
        installation_id: input.installation_id,
        repo: input.repo.to_string(),
        trust_scope: input.trust,
        read_scopes,
        write_scopes,
        approval_required,
        retention_days: input.retention_days,
    })
}

/// Derives the actor trust level for a memory decision (issue #6).
///
/// A fork PR is untrusted **content** — planted facts could be written into
/// durable memory to poison later reviews — so `head_is_fork` maps to
/// [`TrustScope::ForkPr`] and **overrides** the actor's own standing, even a
/// maintainer who triggered the review. Otherwise a commander (who already
/// passed the #13 write-access gate) is a maintainer, and auto-triggered work
/// gets the safe collaborator default.
pub fn derive_trust(head_is_fork: bool, has_commander: bool) -> TrustScope {
    if head_is_fork {
        TrustScope::ForkPr
    } else if has_commander {
        TrustScope::Maintainer
    } else {
        TrustScope::Collaborator
    }
}

/// Default `(read, write)` namespace grants per trust level (contract's table).
/// The load-bearing rule: `ForkPr` and `External` get **no** write scope, so a
/// hostile fork PR can never poison durable memory.
fn grants(trust: TrustScope) -> (Vec<MemoryScope>, Vec<MemoryScope>) {
    use MemoryScope::{Repo, TenantShared};
    match trust {
        TrustScope::Maintainer => (vec![Repo, TenantShared], vec![Repo]),
        TrustScope::Collaborator => (vec![Repo], vec![Repo]),
        TrustScope::External => (vec![], vec![]),
        TrustScope::ForkPr => (vec![Repo], vec![]),
        TrustScope::Scheduled => (vec![Repo], vec![Repo]),
    }
}

/// Trust levels whose writes must always be approval-gated regardless of config.
fn approval_forced(trust: TrustScope) -> bool {
    matches!(trust, TrustScope::Collaborator)
}

/// Why the adapter refused a memory read or write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRejection {
    pub op: MemoryOp,
    /// The offending key or id.
    pub target: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOp {
    Read,
    Write,
}

/// Re-validates the runtime's reported memory activity against `policy`.
/// Returns every read or write the adapter refuses; an empty vec means the
/// report is fully within policy. Callers MUST NOT persist a rejected write.
///
/// `looks_secret` decides whether a string still carries a credential after the
/// adapter's scrubbing — wired to the `redact` module by the caller so a token
/// can never enter durable memory.
pub fn validate_memory_used(
    policy: &MemoryPolicy,
    used: &MemoryUsed,
    looks_secret: impl Fn(&str) -> bool,
) -> Vec<MemoryRejection> {
    let mut rejections = Vec::new();
    if !used.enabled {
        return rejections;
    }

    for entry in &used.read {
        if let Some(reason) = check_access(policy, &entry.scope, &entry.id, &policy.read_scopes) {
            rejections.push(MemoryRejection {
                op: MemoryOp::Read,
                target: entry.id.clone(),
                reason,
            });
        }
    }

    for write in &used.proposed {
        if let Some(reason) = check_access(policy, &write.scope, &write.key, &policy.write_scopes) {
            rejections.push(MemoryRejection {
                op: MemoryOp::Write,
                target: write.key.clone(),
                reason,
            });
            continue;
        }
        if looks_secret(&write.summary) || looks_secret(&write.key) {
            rejections.push(MemoryRejection {
                op: MemoryOp::Write,
                target: write.key.clone(),
                reason: "write still carries a credential after redaction".to_string(),
            });
            continue;
        }
        if policy.approval_required && write.approval != "pending" {
            rejections.push(MemoryRejection {
                op: MemoryOp::Write,
                target: write.key.clone(),
                reason: format!(
                    "approval is required but write is '{}', not 'pending'",
                    write.approval
                ),
            });
        }
    }

    rejections
}

/// Checks one key/id against the granted scopes and its required prefix.
/// Returns `Some(reason)` on refusal, `None` when allowed.
fn check_access(
    policy: &MemoryPolicy,
    scope: &str,
    target: &str,
    granted: &[MemoryScope],
) -> Option<String> {
    let Some(scope) = MemoryScope::parse(scope) else {
        return Some(format!("unknown memory scope '{scope}'"));
    };
    if !granted.contains(&scope) {
        return Some(format!("scope '{scope:?}' is not granted by policy"));
    }
    let prefix = required_prefix(policy, scope);
    if !target.starts_with(&prefix) {
        return Some(format!(
            "key is not addressable under this invocation's scope (expected prefix '{prefix}')"
        ));
    }
    None
}

/// The mandatory key prefix that ties a memory entry to its owning coordinates,
/// so inspect/revoke-by-installation/repo is mechanically possible.
fn required_prefix(policy: &MemoryPolicy, scope: MemoryScope) -> String {
    match scope {
        MemoryScope::Repo => format!("repo/{}/", policy.repo),
        MemoryScope::TenantShared => format!("tenant/{}/", policy.installation_id),
        MemoryScope::FamiliarGlobal => "familiar/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{MemoryEntryRef, ProposedMemory};

    fn inputs(trust: TrustScope, enabled: bool) -> PolicyInputs<'static> {
        PolicyInputs {
            enabled,
            installation_id: 42,
            repo: "acme/billing",
            trust,
            approval_required: false,
            retention_days: Some(365),
        }
    }

    fn never_secret(_: &str) -> bool {
        false
    }

    fn proposed(scope: &str, key: &str, approval: &str) -> ProposedMemory {
        ProposedMemory {
            key: key.to_string(),
            summary: "a fact".to_string(),
            scope: scope.to_string(),
            approval: approval.to_string(),
        }
    }

    fn used_with(proposed: Vec<ProposedMemory>, read: Vec<MemoryEntryRef>) -> MemoryUsed {
        MemoryUsed {
            enabled: true,
            read,
            proposed,
            rejected: vec![],
        }
    }

    #[test]
    fn memory_disabled_yields_no_policy() {
        assert!(compute_policy(inputs(TrustScope::Maintainer, false)).is_none());
    }

    #[test]
    fn activity_rows_carry_the_adapter_verdict() {
        use crate::memory_activity_rows;
        let policy = compute_policy(inputs(TrustScope::ForkPr, true)).unwrap();
        let used = used_with(
            // A fork write is rejected (no write scope); the read is accepted.
            vec![proposed("repo", "repo/acme/billing/y", "pending")],
            vec![MemoryEntryRef {
                id: "repo/acme/billing/x".to_string(),
                scope: "repo".to_string(),
            }],
        );
        let rejections = validate_memory_used(&policy, &used, never_secret);
        let rows = memory_activity_rows(42, "acme/billing", "task-1", &used, &rejections);

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().find(|r| r.op == "read").unwrap().outcome,
            "accepted"
        );
        assert!(
            rows.iter()
                .find(|r| r.op == "write")
                .unwrap()
                .outcome
                .starts_with("rejected:"),
            "fork write should be recorded as rejected"
        );
    }

    #[test]
    fn fork_review_overrides_actor_trust() {
        // A fork PR is untrusted content: even a maintainer command reviewing it
        // is ForkPr (never writes), while a same-repo command is Maintainer.
        assert_eq!(derive_trust(true, true), TrustScope::ForkPr);
        assert_eq!(derive_trust(true, false), TrustScope::ForkPr);
        assert_eq!(derive_trust(false, true), TrustScope::Maintainer);
        assert_eq!(derive_trust(false, false), TrustScope::Collaborator);

        // And the fork grant carries through to no write scope.
        let policy = compute_policy(PolicyInputs {
            trust: derive_trust(true, true),
            ..inputs(TrustScope::Maintainer, true)
        })
        .unwrap();
        assert!(policy.write_scopes.is_empty(), "fork review must never write");
    }

    #[test]
    fn fork_pr_gets_read_but_never_write() {
        let policy = compute_policy(inputs(TrustScope::ForkPr, true)).unwrap();
        assert_eq!(policy.read_scopes, vec![MemoryScope::Repo]);
        assert!(
            policy.write_scopes.is_empty(),
            "a fork PR must never have a write scope"
        );

        // A fork that proposes a durable write is refused.
        let used = used_with(
            vec![proposed("repo", "repo/acme/billing/conventions/x", "pending")],
            vec![],
        );
        let rejections = validate_memory_used(&policy, &used, never_secret);
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].op, MemoryOp::Write);
    }

    #[test]
    fn external_actor_gets_nothing() {
        let policy = compute_policy(inputs(TrustScope::External, true)).unwrap();
        assert!(policy.read_scopes.is_empty());
        assert!(policy.write_scopes.is_empty());
    }

    #[test]
    fn maintainer_write_within_repo_scope_is_accepted() {
        let policy = compute_policy(inputs(TrustScope::Maintainer, true)).unwrap();
        let used = used_with(
            vec![proposed("repo", "repo/acme/billing/conventions/rounding", "pending")],
            vec![MemoryEntryRef {
                id: "repo/acme/billing/conventions/tables".to_string(),
                scope: "repo".to_string(),
            }],
        );
        assert!(validate_memory_used(&policy, &used, never_secret).is_empty());
    }

    #[test]
    fn cross_repo_key_is_rejected() {
        let policy = compute_policy(inputs(TrustScope::Maintainer, true)).unwrap();
        // Correct scope, but the key points at a different repo.
        let used = used_with(
            vec![proposed("repo", "repo/acme/OTHER/conventions/x", "pending")],
            vec![],
        );
        let rejections = validate_memory_used(&policy, &used, never_secret);
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].reason.contains("addressable"));
    }

    #[test]
    fn write_to_ungranted_scope_is_rejected() {
        let policy = compute_policy(inputs(TrustScope::Maintainer, true)).unwrap();
        // familiar_global is never granted.
        let used = used_with(
            vec![proposed("familiar_global", "familiar/cody/x", "pending")],
            vec![],
        );
        let rejections = validate_memory_used(&policy, &used, never_secret);
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].reason.contains("not granted"));
    }

    #[test]
    fn secret_bearing_write_is_rejected() {
        let policy = compute_policy(inputs(TrustScope::Maintainer, true)).unwrap();
        let used = used_with(
            vec![proposed("repo", "repo/acme/billing/notes/x", "pending")],
            vec![],
        );
        let rejections = validate_memory_used(&policy, &used, |_| true);
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].reason.contains("credential"));
    }

    #[test]
    fn collaborator_write_must_be_pending() {
        let policy = compute_policy(inputs(TrustScope::Collaborator, true)).unwrap();
        assert!(policy.approval_required, "collaborator writes are always gated");

        let auto = used_with(
            vec![proposed("repo", "repo/acme/billing/notes/x", "auto")],
            vec![],
        );
        let rejections = validate_memory_used(&policy, &auto, never_secret);
        assert_eq!(rejections.len(), 1);
        assert!(rejections[0].reason.contains("approval is required"));

        let pending = used_with(
            vec![proposed("repo", "repo/acme/billing/notes/x", "pending")],
            vec![],
        );
        assert!(validate_memory_used(&policy, &pending, never_secret).is_empty());
    }

    #[test]
    fn cross_tenant_read_is_rejected() {
        let policy = compute_policy(inputs(TrustScope::Maintainer, true)).unwrap();
        // tenant_shared is granted to maintainer, but the id is under another install.
        let used = used_with(
            vec![],
            vec![MemoryEntryRef {
                id: "tenant/999/shared/x".to_string(),
                scope: "tenant_shared".to_string(),
            }],
        );
        let rejections = validate_memory_used(&policy, &used, never_secret);
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].op, MemoryOp::Read);
    }

    #[test]
    fn disabled_memory_used_report_is_ignored() {
        let policy = compute_policy(inputs(TrustScope::Maintainer, true)).unwrap();
        let used = MemoryUsed {
            enabled: false,
            proposed: vec![proposed("repo", "bogus", "auto")],
            ..Default::default()
        };
        assert!(validate_memory_used(&policy, &used, never_secret).is_empty());
    }
}
