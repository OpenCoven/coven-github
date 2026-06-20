# Familiar Contract for GitHub Work

`coven-github` is not valuable because it can call the GitHub API. The value is that a team can deploy a familiar: a known, persistent, context-aware operator that understands the repo, the team's standards, and when to stop for human judgment.

The GitHub App should make that trust visible.

## Promise

A GitHub familiar should:

- Carry persistent identity across issues, PRs, repositories, and review cycles.
- Use the team's configured model, skills, memory, and operating rules.
- Explain what it changed and why in the familiar's voice.
- Prefer draft PRs and Cave oversight for non-trivial work.
- Treat ambiguity as a reason to ask, not a reason to guess.
- Preserve repo hygiene: small branches, tested changes, clear failure states.
- Make recovery easy when the task cannot be completed.

## Behavioral Guarantees

| Guarantee | Product behavior |
|---|---|
| Context continuity | The familiar can use organization/repo memory and prior task history when enabled. |
| Team fit | Routing and skills are configured per installation, repository, and familiar. |
| Human control | Cave oversight links appear in Check Runs, comments, and task state. |
| Failure transparency | Every task ends in a visible state: review, done, needs input, failed, or timed out. |
| Minimal surprise | Familiars open draft PRs by default until the team explicitly promotes automation. |
| No self-trigger loops | Bot-authored comments do not retrigger the same familiar. |
| Bounded execution | Worker timeout, retry, and isolation rules are enforced. |

## Why This Beats Generic Agents

Generic coding agents optimize for a single task. Familiars optimize for an ongoing working relationship.

That matters most in PR clearing:

- A generic agent can satisfy a prompt; a familiar can remember the team's release posture.
- A generic agent can run tests; a familiar can know which tests are trusted signal.
- A generic agent can make edits; a familiar can know when the change needs a design note, a migration path, or a human decision.
- A generic agent can produce output; a familiar can build trust over repeated work.

## Operational Requirements

To make the familiar promise real, hosted `coven-github` needs:

1. Tenant-scoped familiar routing.
2. Durable task history and event idempotency.
3. Cave oversight as the default review surface.
4. Familiar memory boundaries that are opt-in, inspectable, and revocable.
5. Audit logs for task acceptance, execution, retries, timeout, PR creation, and human intervention.
6. Clear tier limits so teams know when a familiar is operating as a draft helper versus an autonomous maintainer.

## Launch Rule

Do not sell "autonomous code changes" first. Sell "a trusted familiar that drafts PRs under your team's control."

Autonomy can expand after the service proves reliability through visible oversight, repeatable failure handling, and team-specific context.
