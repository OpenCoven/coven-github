# coven-github Roadmap

This roadmap operationalizes `coven-github` as a hosted service funded by teams that want trusted familiar-driven GitHub work.

## Strategic Thesis

The market already has generic coding agents. OpenCoven's advantage is the familiar:

- A known teammate, not a disposable bot.
- Context-aware across repos, issues, reviews, and team norms.
- Governed by skills, memory, and a visible trust contract.
- Watched and steered through Cave instead of hidden in a black box.

The hosted product should monetize managed reliability and trust continuity: durable task infrastructure, worker isolation, auditability, familiar memory, and multi-familiar routing.

## Milestone 1: Honest Self-Hosted Adapter

Goal: a motivated user can run the adapter and understand exactly what works.

- Implement issue assignment, label, issue mention, and PR review comment triggers.
- Enforce webhook HMAC validation and bot self-comment suppression.
- Enforce worker timeout behavior.
- Keep README status honest about implemented, partial, and planned capabilities.
- Publish security, isolation, self-hosting, hosted-vs-self-hosted, and familiar contract docs.

## Milestone 2: Hosted Control Plane

Goal: support real hosted installations without losing task state or leaking tenant context.

- Persistent task store.
- Durable queue.
- GitHub delivery idempotency.
- Installation-scoped familiar routing.
- Tenant-scoped task API auth for Cave.
- Task audit log and terminal states.

## Milestone 3: GitHub Correctness

Goal: make the GitHub App reliable across normal repositories.

- Resolve repository default branch through the GitHub API.
- Resolve Check Run head SHA instead of using placeholders.
- Use the repo default branch for PR base and session brief.
- Capture review-comment diff hunk context.
- Add transient GitHub API retry classification.
- Add webhook fixture tests for all supported triggers.

## Milestone 4: Hosted Worker Fleet

Goal: make familiar execution safe enough to charge for.

- Containerized worker backend.
- CPU, memory, disk, network, and timeout limits.
- Workspace cleanup guarantees.
- Token redaction and secret handling tests.
- Usage metering by installation, repo, familiar, and task runtime.
- Tier limits and concurrency controls.

## Milestone 5: Monetization Surface

Goal: make the value legible and buyable.

- `opencoven.ai/github` landing page.
- Hosted beta waitlist.
- Pricing: Community, Hosted Starter, Hosted Team, Hosted Dedicated.
- Cave dashboard for task history, familiar routing, usage, and audit events.
- Demo assets: issue assignment to Check Run, draft PR back to issue, Cave oversight intervention.

## Current Focus

1. Land the hosted MVP hardening branch.
2. Build persistent task state and idempotency.
3. Move familiar routing from global TOML toward installation-scoped config.
4. Make Cave oversight central in the public story and product loop.
