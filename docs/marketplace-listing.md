# GitHub Marketplace listing plan

How the hosted OpenCoven GitHub App gets listed, priced, and paid on GitHub
Marketplace. The tier matrix and dollar figures live in
[pricing.md](pricing.md); the enforcement behind every promise — including
purchase-driven entitlements — is already in the adapter. This document is
the launch runbook for the listing itself.

## Why Marketplace (and not Stripe first)

- The buyer's unit is the **GitHub App installation** — exactly what
  Marketplace sells. Choose plan → install → billed on the existing GitHub
  invoice. No card form, no separate account, no procurement detour.
- `marketplace_purchase` webhooks feed the adapter's entitlement tables
  directly (account plan + installation mapping), so a purchase provisions
  service with **zero operator action**.
- Native 14-day free trials match the launch posture in
  [pricing.md](pricing.md) without building trial logic.

**Fallback:** if listing review stalls, Stripe Checkout keyed to the
installation id ships in days — the entitlement layer is billing-source
agnostic (`account_plans.source`), only the webhook origin differs.

## Plans to publish

| Marketplace plan | Price | Trial | Maps to |
|---|---|---|---|
| Hosted Starter | $99/mo (flat) | 14 days | `starter`: 1 familiar, 25 tasks/day, 1 concurrent |
| Hosted Team | $399/mo (flat) | 14 days | `team`: up to 4 familiars, 150 tasks/day, 4 concurrent |

Notes:

- **Plan names must contain "Starter" / "Team"** — the adapter classifies
  plan names by substring, and unknown names fail safe to Starter limits.
  Keep tier words in any future rename.
- **Hosted Dedicated is not listed.** Custom contracts, annual billing, and
  SLAs don't fit Marketplace plan mechanics; Dedicated is sales-led with
  direct invoicing, provisioned via an explicit `[[installations]]` entry
  (which always wins over plan defaults).
- No free Marketplace plan: Community (self-host) is the free tier.

## Requirements checklist

Marketplace listing requirements, mapped to their current state:

- [x] App handles `marketplace_purchase` events (purchase, change, cancel,
      trial) — adapter webhook route, idempotent + audited.
- [x] Plan limits enforced server-side — intake daily cap, claim-time
      concurrency, `require_plan` paid gate.
- [x] Customer data deleted on uninstall — delete-on-uninstall purge.
- [ ] App set **public** (the hosted App only; the self-host manifest in
      [app-manifest.json](app-manifest.json) stays private-by-default).
- [ ] Publisher verification for the OpenCoven org (profile complete,
      2FA enforced, domain verified).
- [ ] Support URL and contact email.
- [ ] Privacy policy and terms of service URLs (data boundaries are already
      written down in [HOSTED.md](../HOSTED.md) — the policy formalizes them).
- [ ] Pricing plans created in the listing matching the table above.
- [ ] Listing draft submitted for review.

## Listing copy (from HOSTED.md)

- **Name:** OpenCoven — hosted familiars for GitHub
- **Tagline:** Assign it like a teammate. Get a PR back.
- **Description:** OpenCoven lets your team deploy a trusted familiar to
  GitHub. It knows your repo context, follows your skills and review norms,
  drafts PRs under Cave oversight, and gets better as it works with your
  team. Bring your own model keys — we never mark up tokens. What you buy is
  managed reliability: durable queue, isolated workers, audit trail, familiar
  memory, and multi-familiar routing.
- **Categories:** AI Assistants / Code review / Project management
- **Screenshots:** issue assignment → Check Run → familiar-voice draft PR →
  Cave dashboard (record from the reference demo, `examples/demo/run-demo.sh`).

## Hosted deployment configuration

The hosted control plane runs with:

```toml
[billing]
require_plan = true   # installations without an entitled plan (or an
                      # explicit [[installations]] entry) are recorded
                      # ignored:no_plan
```

Self-hosted deployments never set this; the default is off.

## Launch sequence

1. Verify the OpenCoven publisher org; add support/privacy/terms URLs.
2. Make the hosted App public; confirm `marketplace_purchase` deliveries
   reach the production webhook (Marketplace sends them to the App's
   webhook URL).
3. Create the two paid plans with 14-day trials; enable the OSS-organization
   discount decision from [pricing.md](pricing.md) open decisions if
   approved.
4. Submit the listing for review; run one end-to-end purchase in a sandbox
   org (purchase → trial → task accepted; cancel → `ignored:no_plan`).
5. Flip `require_plan = true` on the hosted deployment at listing go-live,
   with existing beta installations grandfathered via `[[installations]]`
   entries until they purchase.
