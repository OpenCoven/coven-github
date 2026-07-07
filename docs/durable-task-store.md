# Durable task store and delivery idempotency — design (issue #2)

Status: **accepted** — phase 1 (store crate + delivery idempotency) is
implemented; phases 2–3 below are pending.

## Problem

The adapter accepts GitHub webhooks, maps them to tasks, and pushes them into
an in-process `tokio::mpsc` channel consumed by the worker pool
(`crates/webhook/src/routes.rs` → `crates/worker/src/lib.rs`). Three failure
modes make this unacceptable for a hosted App that promises reliable work:

1. **Silent drops.** A full channel logs `task queue full — dropping task`
   and still returns `200 OK`. GitHub treats 200 as delivered and never
   retries; the user sees nothing.
2. **Restart amnesia.** Queued and running tasks, task history, and the
   supersession registry all live in process memory
   (`crates/github/src/tasks.rs`). A deploy or crash loses everything, and
   Cave cannot reconstruct state.
3. **Duplicate deliveries.** GitHub redelivers webhooks (manual redelivery,
   timeouts, retries). Nothing deduplicates by `X-GitHub-Delivery`, so a
   redelivered event re-runs the task: duplicate sessions, comments, PRs.

## Goals

Mapped from issue #2's acceptance criteria:

- Every accepted webhook has a durable record before GitHub sees success.
- Replaying a delivery id never creates a duplicate task.
- Process restart loses no queued task and re-queues interrupted running
  tasks.
- A saturated worker pool delays work instead of dropping it.
- Task lifecycle states are explicit and queryable:
  `received`, `queued`, `running`, `completed`, `failed`, `ignored`
  (plus the existing `superseded` terminal state from #8/#10).
- Cave's `/api/github/tasks` survives restarts.

## Non-goals

- Multi-node worker fleets and distributed queues. One process owns the
  store; the hosted fleet architecture arrives with #5/#15 and can graduate
  the trait implementation without changing call sites.
- Tenant auth on the task API (#3), audit/retention policy (#12), and
  installation-scoped routing (#7) — they build on this store but are
  separate issues.
- Payload archival. We persist routing coordinates and a payload hash, not
  full webhook bodies (see `docs/security.md`; bodies can embed user
  content we don't want retained by default).

## Storage choice: embedded SQLite via `rusqlite`

- **Zero-infra self-hosting.** The compose stack stays single-container;
  self-hosters get durability for free with a mounted volume. This matches
  the repo's "honest self-hosted adapter" posture.
- **WAL mode** gives concurrent readers with a single writer — exactly the
  adapter's shape (webhook writes, worker claims, Cave reads).
- **`rusqlite` over `sqlx`:** the adapter's query surface is small and
  hand-written; rusqlite is synchronous (wrapped in `spawn_blocking`),
  adds no proc-macro build cost, and avoids an async pool we don't need
  at this concurrency.
- **Graduation path.** All access goes through one `TaskStore` type in a
  new `crates/store` crate. If hosted scale demands Postgres, the type
  becomes a trait with a second backend; call sites don't change.

Connection discipline: one writer connection guarded by a mutex, WAL,
`busy_timeout=5s`, `synchronous=NORMAL`, foreign keys on. Reads may share
the writer connection initially — the volume is tiny; optimizing read
concurrency is premature until Cave polling proves otherwise.

Schema versioning via `PRAGMA user_version` and in-crate forward-only
migrations run at startup.

## Config surface

```toml
[storage]
# Directory for durable adapter state (SQLite database + WAL files).
# The doctor command checks it is creatable/writable.
path = "data/coven-github.db"
```

Default `data/coven-github.db` relative to the working directory; compose
gains a named volume. `doctor` validates the parent directory exists or is
creatable and warns when the path sits on tmpfs.

## Schema

```sql
CREATE TABLE webhook_deliveries (
  delivery_id     TEXT PRIMARY KEY,          -- X-GitHub-Delivery
  event           TEXT NOT NULL,             -- X-GitHub-Event
  action          TEXT,                      -- payload action, if any
  installation_id INTEGER,
  repo            TEXT,                      -- owner/name when parseable
  payload_hash    TEXT NOT NULL,             -- sha256 of raw body
  routing         TEXT NOT NULL,             -- 'task:<id>' | 'ignored:<reason>'
  received_at     TEXT NOT NULL              -- RFC 3339
);

CREATE TABLE tasks (
  id              TEXT PRIMARY KEY,          -- uuid (also the session id)
  delivery_id     TEXT REFERENCES webhook_deliveries(delivery_id),
  installation_id INTEGER NOT NULL,
  repo            TEXT NOT NULL,             -- owner/name
  familiar_id     TEXT NOT NULL,
  kind            TEXT NOT NULL,             -- serde_json of TaskKind
  commander       TEXT,                      -- issue #13 permission gate
  state           TEXT NOT NULL,             -- queued|running|completed|failed|ignored|superseded
  supersede_key   TEXT,                      -- 'owner/repo#pr' for PR reviews
  attempts        INTEGER NOT NULL DEFAULT 0,
  -- Result surface for Cave (nullable until terminal):
  branch          TEXT,
  pr_number       INTEGER,
  check_run_url   TEXT,
  summary         TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
CREATE INDEX tasks_state_created ON tasks(state, created_at);
CREATE INDEX tasks_supersede ON tasks(supersede_key)
  WHERE supersede_key IS NOT NULL;

CREATE TABLE task_attempts (
  task_id    TEXT NOT NULL REFERENCES tasks(id),
  attempt    INTEGER NOT NULL,
  started_at TEXT NOT NULL,
  ended_at   TEXT,
  outcome    TEXT,                           -- 'completed' | failure category
  detail     TEXT,                           -- redacted error text
  PRIMARY KEY (task_id, attempt)
);
```

Notes:

- `tasks.kind` stores the existing `TaskKind` serde JSON — no parallel type
  hierarchy, and the wire shape is already versioned by the enum tags.
- `supersede_key` replaces the in-memory `review_heads` map: the newest
  review task for a PR supersedes older *queued* rows in the same
  transaction that inserts it (mid-flight staleness stays with the #8
  re-fetch gate).
- The Cave list (`TaskListItem`) becomes a straight query over `tasks`;
  the in-memory `TaskStore` in `crates/github/src/tasks.rs` is retired.
- `task_attempts.detail` passes through the existing `redact` scrubbing
  before persistence.

## Webhook path

```
1. Validate HMAC                                   (unchanged)
2. Read X-GitHub-Delivery.
     Missing → 400 {"error":"missing delivery id"}.
     GitHub always sends it; a caller that doesn't is not GitHub.
3. BEGIN IMMEDIATE
     INSERT webhook_deliveries ... ON CONFLICT(delivery_id) DO NOTHING
     If the row already existed → COMMIT, return 200 {"ok":true,
       "duplicate":true}. Nothing else happens: idempotency.
4. Route the event (existing event_to_task logic).
     Not actionable → record routing='ignored:<reason>', COMMIT, 200.
5. INSERT tasks (state='queued', supersede_key when review)
     + tombstone older queued reviews with the same supersede_key
       (state='superseded').
     Record routing='task:<id>'. COMMIT.
6. Notify the worker pool (tokio::sync::Notify — a wake-up, not a queue).
7. Return 200. Durable state exists before GitHub hears success.
```

The `mpsc` channel disappears. There is no "queue full": the queue is the
`tasks` table, and backpressure is worker-side (semaphore), not
acceptance-side.

Failure mode: if SQLite is unavailable the route returns **500**, GitHub
retries the delivery later, and the operator sees it in logs — strictly
better than acknowledging work we can't hold.

## Worker path

```
loop:
  claim = UPDATE tasks
            SET state='running', attempts=attempts+1, updated_at=now
          WHERE id = (SELECT id FROM tasks
                      WHERE state='queued' ORDER BY created_at LIMIT 1)
          RETURNING *;
  none → wait on Notify with a 5s timeout (poll fallback), continue.
  some → INSERT task_attempts row; execute (existing execute_task body);
         terminal update: state=completed|failed|superseded (+ result
         columns), close the attempt row.
```

Concurrency stays a semaphore around claims. Claims are atomic under
SQLite's writer lock; `RETURNING` (SQLite ≥ 3.35, bundled) keeps it one
statement.

**Restart recovery:** on startup, before serving —

```sql
UPDATE tasks SET state='queued'
WHERE state='running';
```

One process owns the store, so any `running` row at boot is an orphan of a
dead process. The attempt row stays closed as `interrupted`; the task gets
a fresh attempt when re-claimed. Tasks whose `attempts` already exceed
`worker.max_retries + 1` go to `failed` instead, so a crash-looping task
cannot poison the queue. (A re-run task re-uses its marker-backed status
comment (#13), so the user surface stays deduplicated even across a
restart.)

## What changes where

| Component | Change |
|---|---|
| new `crates/store` | `Store`: open/migrate, delivery insert-or-dup, task enqueue+tombstone, claim, terminal updates, attempt records, Cave list query, startup recovery |
| `crates/webhook/routes.rs` | Delivery-id gate; persist-then-ack; drop `task_tx`; `list_tasks` reads the store |
| `crates/worker/lib.rs` | Claim loop replaces `mpsc` recv; terminal states write to the store; supersession check reads `state='superseded'` instead of the in-memory registry |
| `crates/github/tasks.rs` | In-memory `TaskStore` retired; `TaskListItem`/`TaskListStatus` stay as the API projection |
| `crates/config` | `[storage] path` + doctor checks |
| `crates/server/main.rs` | Open store, run recovery, wire Notify |
| `compose.yaml` | Named volume for `/data` |
| `README.md` | Durable queue row: planned → implemented (only once true) |

## Test plan (maps to the issue's criteria)

- **Duplicate delivery:** same delivery id twice → one `tasks` row; second
  response flags `duplicate`; no second worker execution (wiremock: no
  second Check Run POST).
- **Missing delivery id:** 400, nothing persisted.
- **Queue-full behavior:** N tasks with concurrency 1 → all N eventually
  complete; none dropped (the old `try_send` drop test inverts).
- **Restart recovery:** enqueue, claim, drop the store handle mid-run,
  reopen → task is `queued` again with a closed `interrupted` attempt;
  exhausted-attempts variant lands in `failed`.
- **Supersession:** two review events for the same PR → older queued row
  `superseded`, only the newer claims.
- **Ignored routing:** unsupported event → delivery recorded with
  `ignored:` routing, no task row.
- **Cave continuity:** list query returns pre-restart terminal tasks.
- Demo (`examples/demo/run-demo.sh`) keeps passing — it exercises the full
  loop through the real binary.

## Phased PRs

1. **`crates/store` + delivery idempotency.** Store crate, migrations,
   config/doctor, webhook persist-then-ack with dedup. The mpsc stays for
   dispatch in this PR (tasks additionally recorded durably).
2. **Durable claims + recovery.** Replace mpsc with claim loop + Notify;
   startup recovery; supersession moves into the store; retire the
   in-memory `TaskStore`; Cave list reads SQLite.
3. **Truth pass.** README/HOSTED status updates, compose volume, demo
   assertion that a redelivered webhook does not duplicate comments.

Each PR keeps `cargo check/clippy/test` green and lands separately
mergeable.
