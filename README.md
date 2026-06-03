# coven-github

**Assign an issue to your familiar. Get a PR back.**

`coven-github` is the GitHub App adapter for [OpenCoven](https://opencoven.ai). It bridges GitHub's issue and pull-request workflow with the Coven harness — turning any Coven-configured familiar into a first-class GitHub coding agent, without black-box model lock-in.

```
GitHub issue assigned to @cody
  → coven-github webhook receiver
  → spawns coven-code session with issue context
  → Check Run shows live progress in GitHub UI
  → familiar opens branch, commits, pushes
  → PR opened and linked to original issue
  → Cave session available for live oversight
```

---

## Why

Every existing GitHub coding agent is a black box: GitHub's model, GitHub's context, GitHub's behavior. There's no concept of a *familiar* — no persistent identity, no memory, no composable skills, no operator-defined behavior.

`coven-github` flips that. Your familiar is yours: your model, your skills, your memory, your voice in the PR body. The GitHub App is just the ingress layer.

---

## Architecture

```
GitHub
  │ webhook (issue assigned / @mention / label)
  ▼
coven-github (this repo)
  │ validates HMAC · enqueues task · creates Check Run
  ▼
coven-code --headless --context session-brief.json
  │ agent loop: reads code · edits · runs tests
  │ emits structured progress events
  ▼
GitHub Check Run API     ← live status: "Cody: running tests…"
  │
  ▼
git push → GitHub PR     ← opened by coven-code's git tool
  │
  ▼
CovenCave oversight UI   ← watch session live, intervene, steer
```

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

🚧 **In development.** See [COVEN-GITHUB.md](COVEN-GITHUB.md) for the full product spec.

---

## Self-hosting

```bash
# Clone and build
git clone https://github.com/OpenCoven/coven-github
cd coven-github
cargo build --release

# Configure (see config/example.toml)
cp config/example.toml config/local.toml\n# Set: github_app_id, private_key_path, webhook_secret, familiar config

# Run
./target/release/coven-github serve --config config/local.toml
```

See [docs/self-hosting.md](docs/self-hosting.md) for full setup including GitHub App registration.

---

## Sponsor / Hosted Tier

`coven-github` is open source and self-hostable. OpenCoven offers a **hosted tier** for organizations that want managed infra, cloud familiar memory, and multi-familiar routing without running their own workers.

See [opencoven.ai/github](https://opencoven.ai/github) for hosted tier details.

---

## Related

- [coven-code](https://github.com/OpenCoven/coven-code) — execution runtime
- [coven-cave](https://github.com/OpenCoven/coven-cave) — oversight UI
- [cast-codes](https://github.com/OpenCoven/cast-codes) — local IDE with CastAgent

---

## License

GPL-3.0 — see [LICENSE](LICENSE).
