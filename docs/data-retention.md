# Data retention, artifacts, and audit (issue #12)

What `coven-github` durably retains, what it never retains, how it is redacted,
and how it is deleted — for both self-hosted operators and hosted OpenCoven.
Companion to [Security Model](security.md) and the
[Durable task store](durable-task-store.md).

## Principle

Retain enough to debug, bill, support, and satisfy customer trust — never raw
secrets, credentialed URLs, private repository contents, or unfiltered agent
transcripts. Everything the adapter writes to durable storage passes through
redaction first, and a test (`no_raw_token_survives_in_durable_stores`) scans
every adapter-generated stored field to prove no token or secret pattern
survives.

## Artifact classes

| Class | Retained? | Where | Notes |
|---|---|---|---|
| `task_metadata` | Yes (tenant-scoped) | `tasks` | id, installation, repo, familiar, kind, state, timestamps. |
| `publication_metadata` | Yes | `tasks` | branch, PR number, Check Run URL. |
| `agent_result` | Yes, **after redaction** | `tasks.summary`, `task_attempts.detail` | The result envelope is `sanitize_result`-scrubbed before any persist/publish. |
| `delivery_metadata` | Yes | `webhook_deliveries` | delivery id (idempotency), event/action, installation, repo, **payload hash only** — never the body. |
| `audit_events` | Yes | `api_audit`, table states below | See [Audit events](#audit-events). |
| `memory_activity` | Opt-in, retention-limited | `memory_activity` | Per-installation; see [issue #6](memory-contract.md). |
| `logs` / `transcripts` | Not persisted by the adapter | — | Streamed/redacted only; durable transcripts are out of scope until opt-in retention is designed. |
| `repo_checkout` | **Never** after task cleanup | ephemeral workspace | Workspace is deleted after every task. Container-isolated cleanup is issue #5. |
| `tokens` / `secrets` | **Never** | — | The brief is tokenless (#4); results, comments, Check Runs, and stored fields are redacted. |

## Redaction

`sanitize_result` scrubs every free-text field of the result envelope (summary,
PR body, branch, commit messages, review findings and evidence) before the
adapter stores or publishes anything; error detail written to
`task_attempts.detail`, Check Run summaries, and status comments passes through
`redact` too. Redaction replaces exact live token values **and** GitHub token
patterns (`ghs_`/`ghp_`/`gho_`/`ghu_`/`ghr_`/`github_pat_`) and
`x-access-token:` URL credentials. User-authored content (issue/comment bodies)
is deliberately *not* pattern-scrubbed — a maintainer may legitimately quote a
token-shaped string — so it is excluded from the durable audit-scan surface.

## Audit events

| Event | Recorded in |
|---|---|
| Webhook received / routed / ignored | `webhook_deliveries.routing` (`task:<id>` / `ignored:<reason>`) |
| Task queued / claimed / running / terminal | `tasks.state` + `task_attempts` |
| API read (task list, memory inspect, usage) | `api_audit` |
| Memory read/write decisions | `memory_activity` (with the adapter's accept/reject verdict) |
| Memory revocation | `memory_revocations` |
| Tenant data deletion on uninstall | `api_audit` (`delete_on_uninstall`, with counts) |

Installation-scoped tokens are minted per repository and role (#4); their
**permission class** is deterministic from the role, and the token **value** is
never logged or stored.

## Deletion and retention

- **Delete on uninstall** — an `installation` `deleted` webhook purges that
  tenant's memory (activity + revocations) and task artifacts (tasks,
  attempts, deliveries), and records an audited `delete_on_uninstall` entry.
  Idempotent: a redelivery finds nothing left.
- **Task-history retention** — `[storage] task_retention_days` runs a periodic
  server sweep that deletes terminal (completed/failed/superseded) tasks and
  their attempts past the horizon. In-flight tasks are never expired.
- **Memory retention** — `[memory] retention_days` sweeps memory audit rows;
  revocations are never expired (that would un-revoke memory). See #6.
- **On-demand revoke** — `POST /api/github/memory/revoke` (tenant-scoped).

## Self-hosted vs hosted

| | Self-hosted | Hosted OpenCoven |
|---|---|---|
| Store | Local SQLite; operator owns the volume and retention | Managed, tenant-isolated |
| Task API auth | `open` mode allowed for local dev | Tenant-scoped tokens, fail-closed (#3) |
| Retention | Optional (`task_retention_days` / `retention_days`) | Set by tier/policy |
| Memory | Operator-managed, off by default | Opt-in, per-installation, revocable |
| Worker isolation | May run on host | Container-isolated per task (#5) |

## Not yet covered

- Container-scoped artifact handling — `repo_checkout` cleanup guarantees and
  container log retention — lands with hosted worker isolation (#5).
- A unified append-only audit-event log (the tables above already provide the
  equivalent records; a single stream is a possible future consolidation).
- Durable, opt-in agent transcripts with their own redaction/retention policy.
