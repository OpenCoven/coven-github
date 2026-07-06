# AGENTS.md — coven-github

Guidance for **AI agents** (Codex, Claude Code, Hermes, and any Coven familiar)
opening pull requests against this repo. Humans: your canonical guide is
[`CONTRIBUTING.md`](CONTRIBUTING.md) — this is the agent-specific layer on top.

> **Read first:** [`README.md`](README.md) for what this repo is, and
> [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full contribution bar — including
> the **DCO sign-off requirement**, which is mandatory here.

---

## What this repo is (one line)

`coven-github` is the **GitHub App adapter** for OpenCoven (Rust): it routes
GitHub issues, labels, mentions, and review comments into a Coven familiar, then
publishes progress via Check Runs, issue comments, draft PRs, and CovenCave
session links.

## DCO — every commit must be signed off (mandatory)

This repo uses the **Developer Certificate of Origin**. Every commit must carry
a `Signed-off-by` trailer:

```sh
git commit -s -m "type: summary"
```

This produces `Signed-off-by: Your Name <your.email@example.com>`. Use a real
GitHub-linked identity. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the full DCO
text.

## Branch & PR workflow (all agents)

- **Never push to `main`.** Every change lands via a PR with green CI. Branch
  from current `origin/main`.
- **Fresh branch per task**; use a worktree if multiple sessions may touch this
  repo:
  ```sh
  git fetch origin main
  git worktree add -b <branch> /tmp/covengh-<branch> origin/main
  ```
- Keep the diff **scoped to one concern**; conventional-commit subjects
  (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`).
- After merge: delete the remote branch, remove your local worktree/branch.

## CI gates — run locally before opening the PR

CI (`.github/workflows/ci.yml`) rejects on any of these. Run them first:

```sh
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test --all
```

`-D warnings` has **no exceptions**. Fix lints; don't `#[allow(...)]` without a
justifying comment.

## Repo-specific invariants (don't break these)

- This is an **adapter**, not a familiar. Keep GitHub-webhook/App plumbing here;
  don't reimplement familiar/authority logic that belongs in `coven`.
- **Never commit App private keys, webhook secrets, or installation tokens.**
  The placeholder-secret list exists to keep test fixtures inert — keep real
  credentials out of the repo entirely.
- Surface progress through the GitHub-native primitives (Check Runs, comments,
  draft PRs) rather than inventing new side channels.

## Attribution — credit contributors correctly

When you re-land or build on someone else's work, **credit the human
contributor with a working GitHub-linked trailer** so they appear in the
contributors graph and on their profile:

```
Co-authored-by: Full Name <ID+username@users.noreply.github.com>
```

- Use the **numeric-id no-reply form**. Get the id with
  `gh api users/<login> --jq .id`.
- **Never** use a machine/`.local` email in a co-author trailer — it links to no
  account and gives **zero** credit.
- A commit can carry **both** `Signed-off-by:` (DCO, required) and
  `Co-authored-by:` (attribution) trailers — include both when re-landing a
  contributor's work.

## Secrets & safety

- Never commit secrets, tokens, or private emails. Use `*.noreply.github.com`
  for attribution.
- Don't disable CI gates or branch protection to land a change. If it can't go
  through a green PR, surface the blocker instead.

## Claude Code

`CLAUDE.md` points here — this file is the source of truth for both.
