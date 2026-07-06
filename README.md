# coven-github

**Assign an issue to your familiar. Get a PR back.**

`coven-github` is the GitHub App adapter for [OpenCoven](https://opencoven.ai). It routes GitHub issues, labels, mentions, and review comments into a Coven familiar, then publishes progress through Check Runs, issue comments, draft PRs, and CovenCave session links.

```mermaid
flowchart LR
    issue[GitHub issue, label, mention, or review comment]
    app[coven-github GitHub App]
    worker[coven-github worker]
    familiar[coven-code familiar session]
    check[GitHub Check Run]
    pr[Draft pull request]
    cave[CovenCave oversight]

    issue --> app
    app --> worker
    worker --> familiar
    worker --> check
    familiar --> pr
    worker --> cave
    check --> reviewer[Maintainer]
    pr --> reviewer
    cave --> reviewer
```

---

## What it does

- Accepts GitHub App webhook deliveries and verifies their HMAC signature.
- Routes configured triggers to a familiar by bot username or label.
- Runs `coven-code --headless` with a tokenless session brief.
- Posts Check Run state, direct Cave session links, and draft PRs when the run produces commits.

See [Architecture Diagrams](docs/architecture.md), [Design](DESIGN.md), [Hosted OpenCoven](HOSTED.md), [Familiar Contract](FAMILIAR-CONTRACT.md), [Roadmap](ROADMAP.md), and [Hosted vs self-hosted](docs/hosted-vs-self-hosted.md) for the operational plan.

---

## Architecture

```mermaid
flowchart TB
    subgraph github[GitHub]
        trigger[Issue assignment<br/>trigger label<br/>@mention<br/>review comment]
        checks[Check Run]
        pull[Draft PR]
    end

    subgraph adapter[coven-github]
        webhook[Webhook receiver<br/>HMAC validation<br/>event parsing]
        routing[Familiar routing<br/>bot username<br/>trigger labels]
        tasks[Task queue/store<br/>status and audit]
        runner[Worker<br/>session brief<br/>timeout enforcement]
    end

    subgraph runtime[OpenCoven runtime]
        session[coven-code --headless]
        result[Result envelope<br/>summary, branch, evidence]
    end

    cave[CovenCave oversight<br/>live session and intervention]

    trigger --> webhook
    webhook --> routing
    routing --> tasks
    tasks --> runner
    runner --> session
    session --> result
    result --> runner
    runner --> checks
    runner --> pull
    runner --> cave
```

For deeper system, sequence, state, security-boundary, and hosted deployment diagrams, read [docs/architecture.md](docs/architecture.md).

### Components

| Component | Location | Role |
|---|---|---|
| `crates/webhook` | this repo | Webhook receiver: HMAC validation, event parsing, queue publish |
| `crates/worker` | this repo | Task runner: spawns coven-code, streams progress, posts Check Runs |
| `crates/github` | this repo | GitHub API client: installations, Check Runs, PRs, comments |
| `crates/config` | this repo | Familiar config, installation registry, model routing |
| `coven-github-webhook` | [OpenCoven/coven-github-webhook](https://github.com/OpenCoven/coven-github-webhook) | TypeScript deployment bundle for the hosted/self-hostable webhook entrypoint |
| `coven-code` | [OpenCoven/coven-code](https://github.com/OpenCoven/coven-code) | Execution runtime (headless mode) |
| `CovenCave` | [OpenCoven/coven-cave](https://github.com/OpenCoven/coven-cave) | Oversight UI |

---

## Triggers

Implemented lanes:

| Trigger | Action |
|---|---|
| Issue assigned to bot user (`@cody`) | Agent picks up issue, opens PR |
| `coven:` label applied to issue | Same as above |
| Maintainer command in a comment (`@cody <command>`) | See the command table below |
| PR opened / synchronize / reopened / ready_for_review | Automatic hosted review when the `[review]` policy enables the lane (drafts skipped by default; newer pushes supersede queued reviews of the same PR) |
| Review label applied to a PR | Explicit per-PR review opt-in — works even with the automatic lane off, including drafts |

Planned lanes:

| Trigger | Status |
|---|---|
| Push / commit-range review | `push` events are parsed and typed with fixtures today; execution needs a PR-less task kind, which ships with headless contract v3 |
| Advisory / blocking publication gates | Issue #11 |

## Maintainer commands

A mention only acts when it is the **first token of the comment**, followed by a
command verb — `@cody review`, `@cody fix: the lint is failing`. Casual
mentions mid-sentence trigger nothing, and an unknown verb in command position
gets a clarification reply instead of launching work. Every command except
`status` requires **write access** to the repository; the familiar's own
comments never re-trigger it.

| Command | On an issue | On a PR |
|---|---|---|
| `review` | Clarification (needs a PR) | Hosted review of the PR |
| `fix [text]` | Fix the issue (opens a PR) | Address the feedback in the comment |
| `deepen` | Clarification | Re-review with a wider lens (supporting files, tests) |
| `retry` | Re-run the fix lane | Re-run the review |
| `cancel` | Clarification (PR reviews only) | Cancel queued reviews for the PR (in-flight work finishes) |
| `remember` / `forget` | Acknowledged; persistence lands with the memory governance contract (#6) | Same |
| `status` | Current task state for this thread | Same |

Each familiar keeps **one marker-backed status comment per issue/PR**, edited
in place through the task lifecycle (working → done / needs input / failed),
with links to the Check Run, PR, and Cave session — repeated runs never stack
duplicate comments.

---

## Status

🚧 **In development.** The repo has the first GitHub App adapter path wired, but hosted production readiness is still being built. See [COVEN-GITHUB.md](COVEN-GITHUB.md) for the roadmap-level product spec.

| Capability | Status | Notes |
|---|---|---|
| Webhook HMAC validation | Implemented | Rejects unsigned or invalid GitHub webhook payloads. |
| Issue assignment trigger | Implemented | Routes matching bot assignees to configured familiars. |
| Label trigger | Implemented | Routes configured `trigger_labels` such as `coven:fix`. |
| Maintainer command protocol | Implemented | Typed `@familiar <verb>` grammar; casual mentions ignored; write-access gate; self-comments never re-trigger. |
| Marker-backed status comments | Implemented | One edited-in-place status surface per issue/PR; no duplicate bot comments. |
| PR lifecycle review trigger | Implemented | Policy-gated auto-review on opened/synchronize/reopened/ready_for_review plus label opt-in; familiar-authored PRs are never auto-reviewed. |
| Push / commit review trigger | Partial | Events parsed and typed with fixtures; execution lane needs headless contract v3. |
| GitHub App installation tokens | Implemented | Mints installation access tokens from the App private key. |
| Check Run creation and completion | Partial | Creates and updates Check Runs against the resolved target head SHA; stale-ref revalidation before publish is still planned. |
| Headless execution contract | Locked (v1) | Brief, result envelope, exit codes, and git-auth channel are pinned in [`docs/headless-contract.md`](docs/headless-contract.md) with JSON Schemas, golden fixtures, and a conformance test. |
| `coven-code --headless` execution | Partial | Worker spawns headless sessions with a tokenless session brief and enforces task timeouts; result quality depends on the runtime. |
| Pull request creation | Partial | Opens draft PRs from session results against the repository's resolved default/base branch. |
| CovenCave task polling | Partial | In-memory task API exists for local oversight; hosted control-plane auth and persistence are planned. |
| Durable queue / task store | Planned | Required for hosted reliability and restarts. |
| Hosted tier | Planned | See [Hosted vs self-hosted](docs/hosted-vs-self-hosted.md). |
| Familiar trust contract | Planned | See [Familiar Contract](FAMILIAR-CONTRACT.md). |

---

## Self-hosting

```bash
# Clone and build
git clone https://github.com/OpenCoven/coven-github
cd coven-github
cargo build --release

# Configure
cp config/example.toml config/local.toml

# Fill in config/local.toml, then validate it.
# doctor prints one next step for every error or warning.
./target/release/coven-github doctor --config config/local.toml

# Run
./target/release/coven-github serve --config config/local.toml
```

Prefer containers? A multi-stage [`Dockerfile`](Dockerfile) and
[`compose.yaml`](compose.yaml) ship in the repo root.

See [docs/self-hosting.md](docs/self-hosting.md) for GitHub App registration, permissions, smoke tests, and troubleshooting. For a minimal familiar route, start from [`examples/familiar-github-starter`](examples/familiar-github-starter/).

For a lightweight TypeScript deployment entrypoint that follows this app
contract, use
[`OpenCoven/coven-github-webhook`](https://github.com/OpenCoven/coven-github-webhook)
with its `config/example-policy.json` and connection guide.

---

## Sponsor / Hosted Tier

`coven-github` is open source and self-hostable. OpenCoven offers a **hosted tier** for organizations that want managed infra, cloud familiar memory, and multi-familiar routing without running their own workers.

See [Hosted OpenCoven](HOSTED.md) and [Hosted vs self-hosted](docs/hosted-vs-self-hosted.md) for the service shape, security boundaries, and buyer packaging.

---

## Related

- [coven-code](https://github.com/OpenCoven/coven-code) — execution runtime
- [coven-github-webhook](https://github.com/OpenCoven/coven-github-webhook) — TypeScript webhook deployment bundle
- [coven-cave](https://github.com/OpenCoven/coven-cave) — oversight UI
- [cast-codes](https://github.com/OpenCoven/cast-codes) — local IDE with CastAgent

---

## License

GPL-3.0 — see [LICENSE](LICENSE).
