# COVEN-GITHUB.md — Product Spec

*coven-github: Coven-native GitHub App coding agent*
*Authors: Cody 🦄 + Sage 🌿 · June 3, 2026*

---

## Vision

A GitHub App that turns any Coven-configured familiar into a first-class GitHub coding agent. Assign an issue to `@cody` (or any familiar bot user), and the familiar plans, edits, commits, and opens a pull request — with live oversight in CovenCave and no black-box model lock-in.

---

## Problem

Every existing GitHub coding agent is a closed system:

- **GitHub Copilot Workspace** — GitHub's model, GitHub's context window, GitHub's behavior. No familiar identity, no persistent memory, no operator skills. Paid gate (Pro+ / Enterprise).
- **Devin** — Full-VM agent, strong execution, but ~$500/month, proprietary, model-locked.
- **OpenHands** — Open source, model-agnostic, GitHub integration exists — but no identity, no skills, no cross-session memory.
- **Sweep AI** — Closest structural analogue (true GitHub App), but GPT-4-locked, no identity, effectively abandoned the GitHub-first path.

**No competitor has:** persistent familiar identity + composable skill system + cross-session memory + BYOM + open source + self-hostable + owned oversight UI.

That combination is unoccupied territory. Coven owns all five primitives already.

---

## Architecture

### Two-Layer Separation

```
Layer 1 — GitHub ingress (coven-github)
  Webhooks · auth · event routing · Check Runs · PR lifecycle

Layer 2 — Execution quality (coven-code)
  Agent loop · model · tools · memory · skills · output
```

Mixing these creates a monolith that is hard to test, deploy, and reason about.
`coven-github` is a thin adapter. `coven-code` is the runtime.

### Session Lifecycle

```
1. Webhook arrives
   → validate HMAC signature (reject invalid)
   → parse event: repo, ref, issue body/diff, assignee/label/mention

2. Task enqueued
   → create GitHub Check Run (status: in_progress)
   → post "starting…" comment on issue

3. Worker dequeues
   → provision ephemeral workspace (tmp dir or container)
   → clone repo via installation access token
   → write session-brief.json: issue body, repo context, familiar config

4. coven-code session spawned
   → --headless --context session-brief.json --output result.json
   → familiar reads code, edits files, runs tests, commits

5. Progress streaming
   → coven-code emits structured events (file_changed, test_run, etc.)
   → worker updates Check Run annotations in real time
   → "Cody: 3/8 tests passing…" visible inline in GitHub UI

6. Completion
   → coven-code pushes branch via installation token
   → worker opens draft PR (body: familiar summary + session link)
   → Check Run updated: completed / success or failure
   → issue comment: "PR #42 opened — watch in Cave →"

7. Iteration
   → PR review comment "@cody fix the type error on line 42"
   → re-triggers step 3 with review context appended to brief
```

### Infrastructure

| Component | Role |
|---|---|
| **Webhook receiver** | HTTP server; validates HMAC; publishes to task queue |
| **Task queue** | Decouples ingest from execution; Redis, SQS, or in-process (dev) |
| **Worker pool** | Pulls tasks; manages coven-code processes; streams progress |
| **Ephemeral workspaces** | Per-task isolated filesystems; Docker containers in production |
| **GitHub API client** | Installation tokens (1hr TTL + refresh); Check Runs; PRs; comments |
| **Config store** | Per-installation familiar config; model routing; secret storage |

---

## coven-code Delta: Headless Mode

The execution runtime needs the following additions to work as a GitHub App backend:

### `--headless` flag
- Disables ratatui TUI entirely
- Routes all output to stdout (structured JSON events) + `--output <result.json>`
- Exits 0 on success, non-0 on failure

### `--context <session-brief.json>`
Session brief schema:
```json
{
  "trigger": "issue_assigned",
  "repo": { "owner": "OpenCoven", "name": "coven-code", "clone_url": "...", "default_branch": "main" },
  "issue": { "number": 42, "title": "...", "body": "...", "labels": [] },
  "familiar": { "id": "cody", "model": "anthropic/claude-sonnet-4-6", "skills": ["systematic-debugging"] },
  "workspace": { "root": "/tmp/task-abc123" },
  "auth": { "token": "<installation_access_token>" }
}
```

### `--output <result.json>`
Result envelope schema:
```json
{
  "status": "success" | "failure" | "partial",
  "branch": "cody/fix-issue-42",
  "commits": [{ "sha": "...", "message": "..." }],
  "files_changed": ["src/auth.rs"],
  "summary": "Fixed OAuth token refresh by adding a 60-second clock skew buffer.",
  "pr_body": "## Summary\n\n...",
  "events": [...],
  "exit_reason": null | "test_failure" | "ambiguous_spec" | "git_conflict" | "infra_error"
}
```

### Git auth forwarding
- Accept `GIT_ASKPASS` or `GIT_TOKEN` env var for push operations
- Use installation access token — not user credentials

### Exit codes
```
0   — success: commits made, result.json written
1   — failure: agent gave up, result.json written with exit_reason
2   — infra error: workspace, git, or tool failure (retry-safe)
3   — ambiguous: agent needs clarification (posts comment, exits cleanly)
```

---

## GitHub App Registration

### Required Permissions

| Permission | Level |
|---|---|
| Contents | Read + Write |
| Issues | Read + Write |
| Pull requests | Read + Write |
| Checks | Write |
| Metadata | Read (baseline) |
| Workflows | Write (optional; needed if touching CI config) |

### Webhook Events

| Event | Use |
|---|---|
| `issues` → `assigned` | Primary task trigger |
| `issue_comment` → `created` | `@mention` and iteration |
| `pull_request_review_comment` → `created` | Review feedback iteration |
| `check_suite` / `check_run` | CI awareness |
| `push` | Branch tracking (optional) |

### Bot User

The GitHub App creates a bot user. Bot username configurable per installation.
Default: `coven-cody[bot]`. Orgs can configure `@cody`, `@nova`, etc. via familiar mapping.

---

## Familiar Identity in GitHub

The PR body and issue comments are written in the familiar's voice:

```markdown
## Hey, I'm Cody 🦄

I looked at issue #42 and here's what I found:

The OAuth token refresh path in `src/auth/refresh.rs` wasn't accounting for
clock skew between the client and the auth server. I added a 60-second buffer\nto the expiry check.

**Changed:** `src/auth/refresh.rs` (+12 / -3)
**Tests:** 8/8 passing (added 2 regression cases)

[Watch this session in CovenCave →](https://cave.opencoven.ai/sessions/abc123)
```

The `pr_body` field in `result.json` is generated by the familiar — not a template. This is familiar voice, not boilerplate.

---

## CovenCave Integration

### Coven Board — `coven-github` Tasks

A new task source in the Coven Board alongside manual sessions:

```
Inbox  |  Running  |  Review  |  GitHub

GitHub tab shows:
  ● coven-code #42 — Fix OAuth refresh          running    2m ago
    ↳ Cody · 3/8 tests passing · fix/issue-42
  ● cast-codes #18 — Implement spell compiler    review     18m ago
    ↳ Cody · PR #31 opened
  ● coven-cave #7  — Browser seam fix            done       1h ago
    ↳ Cody · merged as #88
```

Click any row → open Cave session for live oversight.

### Check Run Deep Link

Every Check Run summary includes a `details_url` pointing to the Cave session:
`https://cave.opencoven.ai/sessions/<id>` (or `localhost:3000/sessions/<id>` for self-hosted).

---

## Sponsor / Premium Tier

`coven-github` is open source and self-hostable. The hosted tier monetizes around managed infra and advanced orchestration.

### Open Source (self-hosted)
- Full GitHub App functionality
- BYOM
- Single familiar per installation
- Local CovenCave oversight
- Community support

### Sponsor Tier (GitHub Sponsors)
- **Small sponsor:** Early access + hosted worker credits (5 tasks/day)
- **Medium sponsor:** 50 tasks/day + cloud familiar memory (cross-repo)
- **Large sponsor / org:** Unlimited tasks + multi-familiar routing + dedicated worker + SLA

### Hosted Premium (Enterprise)
- Managed worker fleet (no infra to run)
- Multi-familiar routing (Nova dispatches Cody vs security familiar vs ops familiar)
- Cloud familiar memory — persistent cross-repo, cross-PR context
- Organization-wide installation
- Proactive PR review (familiar reviews incoming PRs unprompted)
- Priority model credits / usage bundling
- White-label bot username

---

## Recovery Design (Sweep's Lesson)

**80% of agent failures are infra, not reasoning.** Design for it from day one.

| Failure class | Behavior |
|---|---|
| Git conflict | Agent posts comment: "I hit a conflict on `main` — can you rebase and re-trigger?" |
| Test failure (fixable) | Agent iterates up to 3 times, then posts partial PR with test output |
| Test failure (unknown root cause) | Posts comment with test output + what it tried; exits with `exit_reason: test_failure` |
| Ambiguous spec | Posts clarifying question as issue comment; exits cleanly (code 3) |
| Infra error (container, git, OOM) | Retries up to 2 times; marks Check Run `failure` with infra note |
| Token expiry | Automatic refresh; transparent to agent session |

All failures are visible in CovenCave with full session replay.

---

## V1 Milestones

### M1 — coven-code headless mode (~3 days)
- `--headless`, `--context`, `--output` flags
- `result.json` envelope
- Git token forwarding
- Exit code contract
- Basic integration test: inject synthetic issue context → verify PR-shaped output

### M2 — coven-github webhook service (~4 days)
- GitHub App registration (manifest + private key handling)
- Webhook receiver with HMAC validation
- In-process task queue (Redis/SQS in production)
- Worker: spawns coven-code, streams progress
- Check Runs client: create / update / complete
- Issue comment: start + PR link

### M3 — PR lifecycle + iteration (~3 days)
- Branch push via installation token
- Draft PR opener with familiar-voice body
- PR review comment re-trigger
- `@cody` mention in issue comments

### M4 — CovenCave GitHub tab (~2 days)
- New task source in Coven Board
- Check Run deep link → Cave session
- Running/review/done states

### M5 — Docs + self-hosting guide (~2 days)
- `docs/self-hosting.md`
- GitHub App registration walkthrough
- `config/example.toml`
- Sponsor tier landing copy

**Total: ~2 weeks to a working V1 end-to-end.**

---

## V2 Backlog

- Multi-familiar routing (Nova dispatches based on issue labels / repo context)
- Cross-repo familiar memory
- Skill auto-detection from repo type (Rust → rust-expert skill, etc.)
- Organization-wide installation
- Proactive PR review (unprompted)
- `coven-github` CLI: `coven-github assign --issue 42 --familiar cody`
- Metrics dashboard in CovenCave (tasks completed, PR merge rate, avg time)

---

## Open Questions

1. **Bot username strategy** — single `coven-cody[bot]` app vs. per-org custom bot username (requires separate GitHub App per org)?
2. **Container isolation** — Docker per task vs. Fly Machines vs. GitHub Actions ephemeral runners for the execution environment?
3. **Memory persistence** — where does cross-PR familiar memory live in the hosted tier? Convex? Postgres? Coven's own storage?
4. **Model billing** — does the operator bring their own API key, or does OpenCoven proxy model calls in the hosted tier?

---

*Spec: Cody 🦄 + Sage 🌿 · June 3, 2026*
