# Reference demo: the operating loop, end to end

This is the ClawSweeper-style reference demo of the coven-github operating
loop (issue #19): the **real adapter binary**, driven by signed webhook
deliveries, publishing to an in-memory GitHub API stub through a
contract-conformant fake `coven-code` runtime. No network, no GitHub App
registration, no credentials — one command, about a minute, self-verifying.

```bash
examples/demo/run-demo.sh
```

A green exit code *is* the demo's claim: every property below is asserted
programmatically against the stub's recorded state, not eyeballed. Set
`KEEP=1` to keep the scratch directory (config, server log, stub log) for
inspection.

## Why "ClawSweeper-style"

ClawSweeper is the reference pattern for conservative GitHub automation in the
OpenClaw ecosystem (see [`DESIGN.md`](../DESIGN.md)): narrow promises, durable
state, marker-backed comments edited in place, explicit maintainer commands,
and deterministic gates before mutation. This demo shows coven-github honoring
each of those disciplines with its own machinery.

## What runs

| Piece | Real or stand-in | Role |
|---|---|---|
| `coven-github serve` | **Real** — the same binary you self-host | Webhook receiver, command router, worker pool, publication |
| Webhook deliveries | Real HMAC-SHA256 signatures, GitHub payload shapes | Drive every act |
| GitHub API | [`github-stub.py`](../examples/demo/github-stub.py) — in-memory stand-in | Records every mutation in an audit trail |
| `coven-code --headless` | [`fake-coven-code`](../examples/demo/fake-coven-code) — headless contract v2 conformant | Fabricates a familiar-voice fix instead of running a model |
| App credentials | Throwaway RSA key + random webhook secret, generated per run | Prove the real JWT → installation-token path |

The stub answers exactly the API surface the adapter uses — scoped token
minting, repo metadata, branch resolution, collaborator permission, Check
Runs, issue comments, pull requests — and exposes its world state at
`/_demo/state` and the mutation log at `/_demo/audit`.

## The acts

**Act 1 — issue assigned: the full loop.** `octocat` assigns issue #42 to
`@coven-cody`. The adapter mints an **orchestration** token, resolves the
default branch and head SHA from live state, creates a Check Run against that
immutable SHA, posts the one marker-backed status comment, flips the check to
`in_progress`, mints a separate **agent-git** token injected only via
`COVEN_GIT_TOKEN` (the fake runtime verifies the brief itself is tokenless),
runs the session, then mints a **publication** token and opens a draft PR back
to the issue — in Cody's voice. Asserted: one comment, edited in place to
`Status: done`; Check Run concluded `success`; PR is a draft; three distinct
token scopes minted.

**Act 2 — casual mention.** "thanks @coven-cody, great work on this!" triggers
*nothing*. Asserted: the audit trail did not grow by a single API call.

**Act 3 — self-trigger loop guard.** The familiar's own comment containing
`@coven-cody status` is ignored. Asserted: audit unchanged.

**Act 4 — unknown verb.** `@coven-cody explain` is a command-position mention
with an unknown verb, so the familiar replies with the real command list
instead of guessing. Asserted: the clarification *edited the same status
comment* — still exactly one surface.

**Act 5 — steering: `status`.** Answered from the durable task store (the same
state Cave polls), listing the issue's tasks and their lifecycle states.

**Act 6 — permission gate.** `mallory` (read-only) comments
`@coven-cody retry`. The worker checks collaborator permission *pre-flight*
and declines on the status surface. Asserted: `Status: declined` and **no new
Check Run** — no session was spent below the write-access bar.

**Act 7 — steering: `retry`.** `octocat` (admin) retries. A second full run
executes: second Check Run to `success`, second draft PR. Asserted: **still
exactly one status comment** — repeated runs never stack duplicates.

## The closing surfaces

After the acts, the demo prints two oversight views:

- **The audit trail** — every GitHub mutation in order, attributed to the
  token role that performed it. You can watch the authority split from
  issue #4 in action: `orchestration` drives checks and comments,
  `publication` opens the PR, `agent-git` never touches the API at all.
- **The Cave view** — `GET /api/github/tasks`, the adapter's task API that
  the CovenCave dashboard (issue #18) polls: task ids, branches, PR links,
  Check Run links, session ids, lifecycle status.

Cave *intervention* — pausing or steering a live session from the dashboard —
lands with #18; today the oversight loop closes through the maintainer
commands demonstrated above plus the session deep links on every Check Run
and status comment.

## Sample audit trail

```text
  1  app-jwt         mint orchestration token (repo-scoped: demo-service)
  2  orchestration   read repo metadata OpenCoven/demo-service
  3  orchestration   resolve branch 'main' head SHA
  4  orchestration   create check run 1001 'Cody — Fix issue #42: …' (queued)
  5  orchestration   list #42 comments (0 found)
  6  orchestration   post comment 5001 on #42
  7  orchestration   check run 1001 -> in_progress ('Running')
  8  app-jwt         mint agent-git token (repo-scoped: demo-service)
  9  app-jwt         mint publication token (repo-scoped: demo-service)
 10  publication     open draft PR #101 (cody/fix-issue-42 -> main)
 11  orchestration   list #42 comments (1 found)
 12  orchestration   edit comment 5001 in place (edit #1: 'Status: done')
 13  orchestration   check run 1001 -> success ('Done')
```

## Requirements

`cargo`, `python3` (stdlib only), `openssl`, `curl`. The script builds the
adapter with `cargo build -p coven-github`, picks free ports, and cleans up
after itself (scratch dir is kept on failure, or with `KEEP=1`).
