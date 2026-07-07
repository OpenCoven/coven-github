# Hosted memory governance contract — design (issue #6)

Status: **proposed** — this document is the review surface for the contract
between `coven-github` (the adapter) and `coven-code` (the runtime) for
memory-backed familiars in hosted mode. Implementation follows in phased PRs,
coordinated across both repositories, once it is agreed.

It extends the [headless execution contract](headless-contract.md) and is
governed by the same normative language (MUST / MUST NOT / SHOULD / MAY, per
RFC 2119) and the same versioning discipline. Where prose in
[`FAMILIAR-CONTRACT.md`](../FAMILIAR-CONTRACT.md), [`HOSTED.md`](../HOSTED.md),
or [`docs/security.md`](security.md) disagrees, this file and the headless
contract win for the wire shape.

## Problem

Familiar memory is the hosted product's differentiator, and also its sharpest
data-boundary risk. The two sides know different things:

- **The adapter** (`coven-github`) is the only component that knows the
  installation id, account, repository (id, owner/name, visibility, default
  branch), the actor who triggered the work, the trigger/task kind, and the
  customer's policy.
- **The runtime** (`coven-code`) is the only component that loads, uses, and
  writes memory.

Nothing today carries the adapter's knowledge across that boundary. Without an
explicit contract, hosted memory silently defaults to whatever scoping the
runtime happens to use locally — path-scoped, user-scoped, or globally
familiar-scoped — instead of tenant/repo/policy-scoped. The failure modes are
concrete: a familiar reads memory from the wrong installation or customer;
untrusted pull-request content from a fork gets written into durable memory
and influences later reviews; a maintainer cannot tell which memory shaped a
review; a customer cannot inspect, revoke, or export what was learned about
their repositories.

## Goals

Mapped from issue #6's acceptance criteria:

- Every hosted `coven-code` invocation carries **explicit** memory policy — the
  runtime never infers scope.
- Hosted mode **fails closed**: missing tenant/repo scope means no memory reads
  or writes, not "fall back to local behavior".
- Untrusted PR/fork content **cannot** write durable memory under any policy.
- A review can **cite** the memory entries that influenced its output.
- Customers can **inspect and revoke** memory by installation and repository.

## Non-goals

- The memory storage engine, retrieval, and ranking inside `coven-code`. This
  contract governs *what crosses the boundary and under what authority*, not
  how the runtime indexes memory.
- The inspect/revoke **API surface** in Cave. It builds on tenant-scoped task
  API auth (#3) and the durable store (#2); this contract only requires that
  memory be **keyed** so that surface is buildable (see [Addressability](#addressability-inspect-and-revoke)).
- Usage metering of memory operations (#15) and audit/retention *tooling*
  (#12). This contract defines the retention *policy fields*; the enforcement
  tooling is separate.
- Local/self-hosted memory behavior. Self-hosters own their own store; the
  fail-closed hosted rules apply only when a `memory_policy` is present.

## Boundary model

The adapter is the **policy authority**; the runtime is the **policy
enforcer**. The adapter computes a `memory_policy` from installation policy,
repository, actor, and trust, and stamps it into the session brief. The runtime
MUST NOT read or write any memory outside what the policy grants, and MUST
report what it did. The adapter then **validates** the result envelope against
the same policy before publishing — defense in depth: a runtime bug or
compromise cannot silently exceed the grant, because the adapter re-checks.

```
 installation policy ─┐
 repository metadata ─┤
 actor + trust       ─┼──► adapter computes memory_policy ──► session brief
 trigger / task kind ─┘                                            │
                                                                   ▼
                                        coven-code loads/uses/writes memory
                                        strictly within memory_policy
                                                                   │
                                                                   ▼
                              result.memory_used  ◄── adapter VALIDATES
                              (read / proposed / rejected / approval)     │
                                                                   ▼
                                        adapter publishes; out-of-scope
                                        writes are refused, not persisted
```

Two invariants make this safe even if the runtime misbehaves:

1. **Deny by default.** Absence of a `memory_policy`, or `enabled: false`, means
   the runtime operates with no durable memory. A brief that omits the block is
   treated as memory-disabled, never as "unrestricted".
2. **Adapter re-validation.** The runtime's self-report is not trusted on its
   own. The adapter rejects (does not persist / does not publish) any memory
   write the policy did not authorize, and records the rejection.

## The `memory_policy` brief block

Added to `session-brief.json` as an **optional** object. Per the headless
contract's versioning rules an added optional field is backward-compatible, but
because the brief schema is `additionalProperties: false` (and the Rust
`SessionBrief` uses `deny_unknown_fields`), both sides MUST land the field in
the same coordinated contract update before the adapter emits it — exactly how
`review_context` and `audit_instruction` were introduced.

```json
{
  "memory_policy": {
    "enabled": true,
    "tenant_scope": {
      "installation_id": 42,
      "account_id": 1001,
      "account_login": "acme-inc"
    },
    "repo_scope": {
      "repo_id": 555,
      "owner": "acme-inc",
      "name": "billing",
      "visibility": "private",
      "default_branch": "main"
    },
    "branch_scope": {
      "base_ref": "main",
      "head_ref": "fix/tax-rounding",
      "same_repo": true
    },
    "trust_scope": "collaborator",
    "read_scopes": ["repo", "tenant_shared"],
    "write_scopes": ["repo"],
    "approval_required": true,
    "retention": {
      "max_age_days": 365,
      "delete_on_uninstall": true,
      "redact_secrets": true
    }
  }
}
```

| Field | Type | Meaning |
|---|---|---|
| `enabled` | bool | Master switch. `false` (or block absent) → no reads, no writes. |
| `tenant_scope` | object | Installation id + account. **Required** when `enabled`. Missing → the runtime MUST refuse all memory. |
| `repo_scope` | object | Repository identity and visibility. **Required** when `enabled`. |
| `branch_scope` | object | Refs in play and whether the head is same-repo (vs a fork). Drives trust. |
| `trust_scope` | enum | Actor trust: `maintainer` \| `collaborator` \| `external` \| `fork_pr` \| `scheduled`. See [Trust model](#trust-model). |
| `read_scopes` | string[] | Memory namespaces the runtime MAY read: `repo`, `tenant_shared`, `familiar_global` (subset only when policy allows). Empty → read nothing. |
| `write_scopes` | string[] | Namespaces the runtime MAY propose/write. Empty → propose nothing durable. |
| `approval_required` | bool | When true, written memory is `pending` until a maintainer approves (ties to the `@familiar remember` command, #13). |
| `retention` | object | `max_age_days`, `delete_on_uninstall`, `redact_secrets`. |

### Scope enums

- **`read_scopes` / `write_scopes`** name namespaces, never raw paths. The
  runtime resolves a namespace to concrete keys itself, but every resolved key
  MUST be prefixed by the tenant + repo coordinates (see
  [Addressability](#addressability-inspect-and-revoke)). `tenant_shared` is
  cross-repo memory *within one installation*; `familiar_global` is
  cross-tenant and is **never** granted in hosted mode by default.
- A policy MUST NOT grant a write scope broader than its read scope for the
  same invocation.

## Trust model

`trust_scope` is computed by the adapter from the actor and the trigger — the
runtime never decides trust. It gates the default read/write grants:

| `trust_scope` | Who | Default read | Default durable write |
|---|---|---|---|
| `maintainer` | admin/maintain/write actor on the target repo (see #13 permission gate) | `repo`, `tenant_shared` | `repo` (subject to `approval_required`) |
| `collaborator` | triage/read collaborator | `repo` | none (may *propose*, needs approval) |
| `external` | non-collaborator commenter | none | none |
| `fork_pr` | PR whose head is a fork (`branch_scope.same_repo == false`) | `repo` (read-only context) | **none — hard rule** |
| `scheduled` | Branch Gardener / cron (#14) | `repo` | `repo` (system-authored, no untrusted input) |

The load-bearing rule, restated because it is the issue's sharpest risk:

> **Untrusted PR content can never write durable memory.** For `fork_pr` and
> `external`, `write_scopes` MUST be empty regardless of other policy, and the
> adapter MUST reject any memory write in the result envelope. Fork content may
> *inform* a review but MUST NOT *persist* into memory that shapes future
> reviews — otherwise a hostile fork PR is a memory-poisoning vector.

## Result envelope: `memory_used`

The runtime reports memory activity in `result.json` as an **optional** object
(same additive-field discipline as the brief). Non-memory runs omit it or set
`enabled: false`.

```json
{
  "memory_used": {
    "enabled": true,
    "read": [
      { "id": "repo/acme-inc/billing/conventions/rounding", "scope": "repo" }
    ],
    "proposed": [
      { "key": "repo/acme-inc/billing/conventions/tax-tables",
        "summary": "Tax tables live in src/tax/tables.rs, not the DB.",
        "scope": "repo", "approval": "pending" }
    ],
    "rejected": [
      { "summary": "Reviewer's personal email is …",
        "scope": "repo", "reason": "pii" }
    ]
  }
}
```

| Field | Type | Meaning |
|---|---|---|
| `read` | array | Memory entries loaded, by id + scope. Powers **citation**. |
| `proposed` | array | Candidate writes with `approval` ∈ `pending` \| `applied` \| `auto`. |
| `rejected` | array | Candidates the runtime itself declined, with `reason` (`pii`, `secret`, `out_of_scope`, `low_confidence`). |

The adapter uses `read` to cite influencing memory in the Check Run / PR
surface (#13's marker-backed status comment is the natural home), and validates
`proposed` against `write_scopes` before anything is persisted.

## Addressability: inspect and revoke

Every durable memory key MUST be prefixed with its owning coordinates:

```
repo/<owner>/<name>/<namespace>/<key>
tenant/<installation_id>/<namespace>/<key>
```

This is what makes "inspect and revoke by installation and repository"
mechanically possible: Cave (once #3 lands the auth) enumerates `repo/<o>/<n>/*`
to show a customer what a familiar knows about their repo, and deletes by
prefix to revoke. `delete_on_uninstall` is a prefix delete over
`tenant/<installation_id>/*` and every `repo/*` the installation covered. A key
that is not prefix-addressable is non-conformant and MUST be rejected.

## Redaction

Memory writes pass through the adapter's existing secret scrubbing (the
`redact` module from #4) before persistence when `retention.redact_secrets` is
true (always, in hosted mode). A proposed memory whose text still matches a
token/credential pattern after scrubbing is rejected with `reason: "secret"`.
This closes the loop with the token-leak boundary: credentials cannot enter
durable memory any more than they can enter a brief or a comment.

## Adapter enforcement points

1. **Brief construction** (`crates/worker/src/brief.rs`): compute and stamp
   `memory_policy`. Deny-by-default — `enabled` only when the installation
   policy opts in **and** tenant + repo scope are known **and** `trust_scope`
   permits. `fork_pr`/`external` never get write scopes.
2. **Result validation** (`crates/worker/src/lib.rs`, alongside the existing
   `validate_result_contract`): reject the envelope's memory writes that fall
   outside `write_scopes`, that target a non-prefixed key, or that fail
   redaction; record each rejection. Out-of-scope writes are a **hard fail** of
   the memory portion — the review still publishes, but the write does not, and
   the rejection is auditable.
3. **Approval routing** (#13): `approval == pending` writes surface to the
   maintainer via the `@familiar remember` / `forget` commands, which today
   no-op with a "lands with #6" acknowledgement. This contract is the write
   path those commands will drive.

## Configuration surface

Installation memory policy extends the existing per-repo policy shape (the
`[review]` precedent from #10):

```toml
[memory]
enabled = false            # hosted opt-in; off by default
approval_required = true
retention_days = 365
[memory.repos."acme-inc/billing"]
enabled = true             # per-repo override
```

`doctor` validates that `[memory] enabled = true` is paired with a hosted
deployment (it is inert for self-hosted single-tenant use) and that retention
values are sane.

## What changes where (implementation preview)

| Component | Change |
|---|---|
| `docs/contracts/session-brief.schema.json` | add optional `memory_policy` (coordinated bump with coven-code) |
| `docs/contracts/result.schema.json` | add optional `memory_used` |
| `crates/config` | `[memory]` policy + per-repo overrides + doctor checks |
| `crates/worker/src/brief.rs` | compute + stamp `memory_policy` (deny-by-default, trust-gated) |
| `crates/worker/src/lib.rs` | validate `memory_used`; reject out-of-scope/unredacted writes |
| `crates/github` | `trust_scope` derivation from actor permission (reuses #13's permission lookup) |
| Cave (`#3` + `#18`) | inspect/revoke surface over prefix-addressable keys |

## Test plan (maps to the issue's criteria)

- **Explicit policy present:** every hosted invocation's brief carries
  `memory_policy`; a fake `coven-code` asserts the fields it received.
- **Fail-closed:** brief with missing tenant or repo scope → `enabled` forced
  false; runtime performs no memory op.
- **Fork cannot write:** a `fork_pr` result envelope that proposes a durable
  write → adapter rejects the write, publishes the review, records the
  rejection. Same for `external`.
- **Cross-installation / cross-repo isolation:** a result whose `read`/`proposed`
  keys are not prefixed by the invocation's own tenant+repo → rejected.
- **Citation:** a review that read memory surfaces those ids on the status
  comment.
- **Redaction:** a proposed memory containing a token pattern → rejected
  `secret` after scrubbing.
- **Memory-disabled:** policy `enabled=false` → no `memory_used` writes accepted.

## Phased PRs (coordinated with coven-code)

1. **Contract + schemas.** This doc, plus the additive `memory_policy` /
   `memory_used` schema fields and Rust types. **Done** (adapter side; the
   coven-code side accepts/emits them independently).
2. **Adapter policy + enforcement.** `[memory]` config, deny-by-default brief
   stamping with trust gating (incl. fork-PR never-write), and result-envelope
   validation. **Done.**
3. **Inspect / revoke.** Tenant-scoped `GET /api/github/memory` inspect over
   prefix-addressable keys; `POST /api/github/memory/revoke` with adapter-side
   enforcement (revoked keys refused on future reads/writes) plus a denial list
   forwarded to the runtime; `delete_on_uninstall`; read citation on the review
   surface; and **retention expiry** — a periodic server sweep drops audit rows
   past `retention_days` (revocations are never expired), while the same horizon
   is forwarded to the runtime for the memory bytes themselves. **Done.**

The adapter side is complete. What is inherently bilateral — the runtime
honoring the `denied` list and physically deleting revoked bytes — is the
coven-code counterpart; the adapter's refusal is the standalone guarantee, so a
revoked memory can never influence a review regardless of the runtime.
