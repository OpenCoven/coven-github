# Hosted vs Self-Hosted

`coven-github` stays open source so teams can run a Coven-native GitHub App in their own infrastructure. The hosted OpenCoven service should monetize the parts that are operationally expensive: secure workers, durable queues, cross-repo familiar memory, observability, and multi-familiar routing.

## Who Should Self-Host

Self-hosting is best for:

- Solo maintainers and small teams that already operate their own infrastructure.
- Teams that want BYOM and local CovenCave oversight without managed OpenCoven services.
- Organizations that need source-available control before committing to a paid hosted tier.
- Security reviewers validating the adapter before installing a hosted GitHub App.

Self-hosted users manage:

- GitHub App registration, private key storage, and webhook secret rotation.
- Worker hosts, logs, upgrades, and task retries.
- Model credentials and `coven-code` runtime installation.
- Workspace isolation and cleanup.
- Task persistence and queue durability once they move beyond the in-process development path.

## Who Should Use Hosted OpenCoven

Hosted is best for:

- Teams that want to assign GitHub issues to familiars without running worker infrastructure.
- Organizations that want managed uptime, usage controls, audit logs, and billing.
- Teams that need familiar memory across repositories and repeated PRs.
- Engineering leaders who want multiple specialist familiars: implementation, review, security, docs, release, and triage.

Hosted OpenCoven should manage:

- GitHub App installation and webhook ingress.
- Durable task queue, retries, and task history.
- Ephemeral worker environments with cleanup and timeout enforcement.
- Centralized usage metering, limits, and billing.
- Organization and repository routing policy.
- Familiar memory, skills, and model routing.
- CovenCave oversight links for live intervention.

## Pricing Shape

The initial pricing should be simple enough to buy without procurement drama:

| Tier | Buyer | Packaging |
|---|---|---|
| Community | OSS maintainers and self-hosters | Free self-hosted adapter, community support, one familiar per installation. |
| Hosted Starter | Indie teams and small shops | Monthly base fee with included task credits, one organization, one or two familiars, standard worker pool. |
| Hosted Team | Product teams | Higher task credits, cross-repo familiar memory, multi-familiar routing, team usage dashboard, priority support. |
| Hosted Dedicated | Security-sensitive orgs | Dedicated workers, private network options, stronger audit retention, custom limits, SLA, onboarding support. |

The sell is not cheaper tokens. The sell is an owned, inspectable coding agent pipeline with persistent familiar identity, self-host escape hatch, and enough managed reliability that teams can trust it with real backlog work.

## Familiar Advantage

The strongest hosted pitch is trust continuity. Teams are not buying a bot that can edit files; they are deploying a familiar that knows their context, standards, release posture, and repeated pain points.

Suggested positioning:

> Assign it like a teammate. Get a PR back. Your familiar knows the difference between good and good enough for your repo.

That makes Cave oversight a primary product surface, not a safety footnote. The buyer should see:

- who the familiar is,
- what team context it used,
- what it tried,
- what changed,
- what evidence it collected,
- where it needs human judgment.

This is the part generic GitHub coding agents do not have: identity, relationship, memory, and operational trust.

## Buyer Proof Needed Before Launch

- A public status matrix that separates implemented, partial, and planned capabilities.
- A security document covering private keys, installation tokens, model credentials, workspace cleanup, and data retention.
- A short operator guide that gets a self-hosted GitHub App to a successful webhook smoke test.
- A hosted beta CTA with a clear promise: "Assign an issue to Cody; get a draft PR back with a Cave oversight link."
- Two demo videos or GIFs: issue assignment to Check Run, then draft PR back to issue.
- A familiar contract page that explains behavioral guarantees, oversight, and failure transparency.

## Landing Page Brief

Recommended structure for `opencoven.ai/github`:

1. Headline: "Assign an issue to your familiar. Get a PR back."
2. Three-step flow: install GitHub App, configure familiar, assign issue or label.
3. Trust block: open source adapter, BYOM, familiar memory, Cave oversight, self-hostable.
4. Hosted/service block: managed workers, durable queues, usage limits, auditability, multi-familiar routing.
5. Security block: installation tokens, ephemeral workspaces, no user Git credentials, retention policy.
6. CTA pair: "Join hosted beta" and "Self-host from GitHub."
