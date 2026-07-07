# Hosted pricing tiers (issue #17)

Status: **proposed** — numbers below are a launch proposal for review, not a
commitment. Everything a tier promises maps to an enforcement knob that
already exists in the adapter, so the price sheet can never drift ahead of
the product. The Marketplace listing runbook that publishes these tiers
lives in [marketplace-listing.md](marketplace-listing.md).

## Principles

1. **Flat monthly tiers with task caps.** No pure usage billing at launch —
   task duration and model-cost distribution are unknown until real
   installations run for a quarter (per `HOSTED.md`). The metering that
   would power usage billing already exists (`/api/github/usage`, #15);
   pricing can graduate later without re-instrumenting.
2. **The moat is trust continuity, not compute.** Tiers scale on the things
   a team feels — familiars, memory, oversight, retention, support — not on
   raw CPU minutes.
3. **BYOM everywhere.** Model keys are the customer's at every tier; we
   never mark up tokens. What hosted sells is managed reliability: durable
   queue, isolation, audit, routing, memory governance.
4. **Every limit is enforced, not promised.** Each line in the matrix below
   names the config/mechanism that enforces it today.

## The tiers

| | **Community** | **Hosted Starter** | **Hosted Team** | **Hosted Dedicated** |
|---|---|---|---|---|
| **Price (launch proposal)** | Free (self-host) | **$99/mo** | **$399/mo** | **from $2,000/mo** (annual) |
| Buyer | OSS maintainers, evaluators, local-first users | A small team with a backlog | Product/platform teams | Security-sensitive orgs |
| Familiars | Unlimited (you run it) | 1 | Up to 4, per-repo routing | Custom roster |
| Tasks / day | Unlimited (your hardware) | 25 | 150 | Custom |
| Concurrent tasks | Your config | 1 | 4 | Custom pool |
| Trigger surface | Everything | Assignment, labels, commands | + auto PR review lanes, per-repo trigger policy | Everything + custom lanes |
| Familiar memory | Self-managed | — | Team memory, approval-gated, 90-day retention | Custom scopes + retention, revoke/inspect APIs |
| Oversight | Local Cave | Cave session links | Cave dashboard + task/usage/audit APIs | Same + audit export |
| Worker isolation | Your choice (host or container backend) | Shared containerized pool | Shared containerized pool, higher limits | **Dedicated workers**, custom resource/network profile |
| Data retention | Your policy | 30 days task history | 90 days, purge API | Custom, contractual, delete-on-uninstall |
| Support | Community | Email | Priority email | SLA + onboarding |

## Why these numbers

- **$99 Starter** clears the "is this a real product" bar while staying
  below team-tool procurement friction; 25 tasks/day is generous for one
  familiar working one backlog but small enough to force graduating teams
  upward.
- **$399 Team** is priced against the alternative — a fraction of the cost
  of the engineer-hours the familiar absorbs, and in line with per-seat dev
  tooling for a ~10-person team without per-seat accounting overhead.
- **Dedicated from $2,000** covers a dedicated worker pool's real cost with
  margin and filters for organizations that genuinely need isolation,
  retention contracts, and an SLA. Annual only: this tier carries
  onboarding cost.
- Caps are deliberately **daily task counts**, not compute: they're legible
  to the buyer, already enforced at intake, and honest about what drives
  our cost (sessions started).

## Enforcement map (nothing here is aspirational)

| Promise | Mechanism |
|---|---|
| Task caps / concurrency | `[installations.limits] max_tasks_per_day` (intake gate, audited as `ignored:quota_exceeded`) and `max_concurrent` (claim-time gate) — #15 |
| Familiar count / routing | `[[installations]] familiars` allow-list + per-repo trigger policy — #7 |
| Tenant data boundary | Fail-closed token-scoped task/usage/memory APIs, per-read audit — #3 |
| Memory governance | `[memory]` opt-in, approval gates, retention_days, revoke + inspect — #6 |
| Worker isolation | Container backend: hardened profile, cpu/mem/pids/network limits — #5 |
| Retention / purge | Store retention sweep + tenant purge — #12 |
| Audit trail | Delivery/task/attempt records, `api_audit`, memory activity — #2/#3/#6 |
| Review quality gates | Severity threshold + publish modes per repo — #11 |

## What launch does NOT include

- Per-seat pricing (no seat concept in the adapter; installations are the
  unit).
- Metered/overage billing (metering exists; billing integration is post-beta
  once cost distribution is known).
- A free hosted tier. Community *is* the free tier — the adapter is open
  source and one command self-hosts it. A free hosted tier would sell our
  costliest resource (isolated compute) at zero against an already-free
  alternative.

## Open decisions for launch

1. Final dollar figures (the ratios matter more than the absolutes; 1 : 4 :
   20 is the proposal).
2. Trial policy: 14-day Starter trial vs. a permanently free "5 tasks/day"
   hosted sandbox. Proposal: **14-day trial**, no free hosted tier.
3. Whether Team includes the review `request_changes` blocking mode by
   default or as an opt-in (proposal: available at every hosted tier —
   policy, not pricing).
4. Nonprofit/OSS-org discount on Team (proposal: 50%).
