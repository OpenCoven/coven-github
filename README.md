# coven-github

**Assign an issue to your familiar. Get a PR back.**

`coven-github` is the GitHub App adapter for [OpenCoven](https://opencoven.ai). It bridges GitHub's issue and pull-request workflow with the Coven harness — turning any Coven-configured familiar into a first-class GitHub coding agent, without black-box model lock-in.

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

## Why

Every existing GitHub coding agent is a black box: GitHub's model, GitHub's context, GitHub's behavior. There's no concept of a *familiar* — no persistent identity, no memory, no composable skills, no operator-defined behavior.

`coven-github` flips that. Your familiar is yours: your model, your skills, your memory, your voice in the PR body. The GitHub App is just the ingress layer.

That is the product wedge: assign it like a teammate, get a PR back, and keep Cave oversight in the loop. A familiar should know the difference between "technically works" and "good enough for this repo, this team, and this moment."

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
| `coven-code` | [OpenCoven/coven-code](https://github.com/OpenCoven/coven-code) | Execution runtime (headless mode) |
| `CovenCave` | [OpenCoven/coven-cave](https://github.com/OpenCoven/coven-cave) | Oversight UI |

---

## Triggers (V1)

| Trigger | Action |
|---|---|
| Issue assigned to bot user (`@cody`) | Agent picks up issue, opens PR |
| `coven:` label applied to issue | Same as above |
| `@cody` mention in issue comment | Agent responds / iterates |
| PR review comment `@cody fix:` | Agent addresses review feedback |

---

## Status

🚧 **In development.** The repo has the first GitHub App adapter path wired, but hosted production readiness is still being built. See [COVEN-GITHUB.md](COVEN-GITHUB.md) for the roadmap-level product spec.

| Capability | Status | Notes |
|---|---|---|
| Webhook HMAC validation | Implemented | Rejects unsigned or invalid GitHub webhook payloads. |
| Issue assignment trigger | Implemented | Routes matching bot assignees to configured familiars. |
| Label trigger | Implemented | Routes configured `trigger_labels` such as `coven:fix`. |
| Issue / PR mention trigger | Implemented | Ignores familiar bot self-comments to avoid loops. |
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

# Configure (see config/example.toml)
cp config/example.toml config/local.toml
# Then set in config/local.toml: github.app_id, github.private_key_path,
# github.webhook_secret, worker.coven_code_bin, and a [[familiars]] block.

# Run
./target/release/coven-github serve --config config/local.toml
```

See [docs/self-hosting.md](docs/self-hosting.md) for full setup including GitHub App registration. For a minimal familiar route, start from [`examples/familiar-github-starter`](examples/familiar-github-starter/).

---

## Sponsor / Hosted Tier

`coven-github` is open source and self-hostable. OpenCoven offers a **hosted tier** for organizations that want managed infra, cloud familiar memory, and multi-familiar routing without running their own workers.

See [Hosted OpenCoven](HOSTED.md) and [Hosted vs self-hosted](docs/hosted-vs-self-hosted.md) for the service shape, security boundaries, and buyer packaging.

---

## Related

- [coven-code](https://github.com/OpenCoven/coven-code) — execution runtime
- [coven-cave](https://github.com/OpenCoven/coven-cave) — oversight UI
- [cast-codes](https://github.com/OpenCoven/cast-codes) — local IDE with CastAgent

---

## License

GPL-3.0 — see [LICENSE](LICENSE).
