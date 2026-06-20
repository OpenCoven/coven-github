# Hosted MVP Plan

This plan turns `coven-github` from a promising open-source adapter into a monetizable hosted service.

## Product Wedge

OpenCoven should not compete as "another coding agent." The wedge is an owned GitHub coding pipeline:

- Familiar identity instead of generic bot output.
- BYOM and skill routing instead of model lock-in.
- Self-hostable adapter for trust and adoption.
- Hosted worker fleet for teams that do not want to operate infra.
- CovenCave oversight so humans can watch, steer, and recover agent work.

The paid product should sell reliability, memory, routing, and trust. Token arbitrage is not the business.

The business frame from Astra: Cave oversight is the product surface, not a safety checkbox. Teams should feel like they are assigning work to a known teammate under their control, not gambling on a one-off agent run.

Ideal first buyers:

- Small engineering teams with more backlog than hands.
- Platform teams that need safe dependency, docs, and maintenance PRs.
- DevRel and open-source teams that triage issues across public repos.
- Security-conscious teams that want the self-host escape hatch before adopting hosted workers.

## Phase 1: Trustworthy Self-Hosted Adapter

Goal: a stranger can install the App, trigger a familiar, and understand failures.

- Keep README status honest with implemented, partial, and planned capabilities.
- Expand self-hosting docs with smoke tests and troubleshooting.
- Implement documented trigger labels.
- Prevent familiar bot self-comment loops.
- Enforce worker task timeouts.
- Add security and isolation docs.
- Add route and worker tests for the above.

Status: started in this branch.

## Phase 2: Hosted Control Plane Foundation

Goal: a hosted installation can survive restarts and safely separate tenants.

- Add persistent task store behind the existing task list API.
- Store GitHub delivery ID and event coordinates for idempotency.
- Add tenant model keyed by GitHub installation ID.
- Move familiar routing from global TOML to installation-scoped config.
- Protect `/api/github/tasks` with internal or tenant-scoped auth.
- Persist task states: queued, running, needs input, review, done, failed.
- Record Check Run, branch, PR, issue, and session links.
- Record the familiar identity, memory scope, skill pack, and oversight session for each task.

This is the first paid-service foundation. Without it, hosted cannot be trusted.

## Phase 3: Production GitHub Correctness

Goal: the adapter behaves correctly across real repositories.

- Resolve repository default branch through GitHub API.
- Resolve head SHA instead of using `HEAD` for Check Runs.
- Use repository default branch instead of hardcoded `main` for PR base and session brief.
- Add PR review comment diff hunk support.
- Add retry behavior for transient GitHub API errors.
- Add webhook tests for assigned, labeled, issue mentions, and PR review comments.

## Phase 4: Worker Isolation and Usage Metering

Goal: turn worker execution into a paid hosted unit.

- Add containerized worker backend.
- Keep process backend for local development.
- Enforce CPU, memory, disk, timeout, and cleanup rules.
- Meter task runtime, model provider, familiar, repo, and installation.
- Add per-tier concurrency and task-credit limits.
- Emit audit events for accepted, started, completed, failed, retried, and timed-out tasks.

## Phase 5: Monetization Surface

Goal: make buying obvious.

- Publish `opencoven.ai/github` landing page.
- Add hosted beta waitlist.
- Offer self-hosted install docs as the trust path.
- Package Hosted Starter, Hosted Team, and Hosted Dedicated.
- Add Cave dashboard views for task history, usage, and familiar routing.
- Add example PRs and demo videos for issue assignment to draft PR.

## Near-Term Engineering Backlog

1. Persistent task store and idempotency.
2. Tenant-scoped installation config.
3. Authenticated task API for Cave.
4. Default branch and head SHA resolution.
5. Container worker backend.
6. Hosted beta landing page and waitlist.

## Success Metrics

- Self-hosted user can complete the smoke test in under 15 minutes.
- Hosted beta user can install the GitHub App and trigger a visible task in under 5 minutes.
- Every accepted webhook has a durable task record.
- Every worker task has a terminal state.
- A failed task leaves enough evidence for the operator to know whether the failure was spec, code, model, GitHub API, or infrastructure.
