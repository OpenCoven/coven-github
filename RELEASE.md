# Release checklist — coven-github

The first usable release (`v0.1.0`) ships only after every gate below passes.
Gates 1–5 are runnable from a clean checkout with no GitHub credentials. Gate 6
needs a disposable repo with the App installed and a live `coven-code` binary —
it is the human-in-the-loop step that authorizes the tag.

## Gates

```bash
# 1. Format
cargo fmt --all -- --check

# 2. Type/borrow check
cargo check --workspace

# 3. Lint (warnings are errors)
cargo clippy --workspace --all-targets -- -D warnings

# 4. Tests
cargo test --workspace

# 5. Docs smoke — config validation + webhook signature path
#    (boot the server against a throwaway config, then:)
cargo build --release -p coven-github
./target/release/coven-github doctor --config config/local.toml
scripts/smoke-webhook.sh http://localhost:3000/webhook "$WEBHOOK_SECRET"
```

Gate 5 detail: `doctor` must exit `0` on a filled-in config, and
`scripts/smoke-webhook.sh` must report unsigned → 401, bad signature → 401,
valid signature → 200. See [docs/self-hosting.md](docs/self-hosting.md).

## 6. Disposable-repo end-to-end (human-gated)

On a throwaway repo with the GitHub App installed and `worker.coven_code_bin`
pointing at a real `coven-code`:

1. Open an issue and assign it to the configured bot user (or apply a
   `trigger_labels` label such as `coven:fix`).
2. Confirm a Check Run appears and the familiar session starts.
3. Confirm a draft PR opens in the familiar's voice and links back to the issue.
4. Confirm the Check Run resolves to success/failure (not stuck).

Capture the issue/PR links in the release notes as evidence.

## Cut the tag

Only after gates 1–6 pass:

```bash
# Ensure the version in Cargo.toml ([workspace.package].version) is correct,
# the tree is clean, and you are on main.
git tag -s v0.1.0 -m "coven-github v0.1.0"
git push origin v0.1.0
```

> Tags are signed (`-s`). Do not push an unsigned release tag.

## Status of automatable gates (this branch)

| Gate | Result |
|---|---|
| 1. `cargo fmt --all -- --check` | ✅ clean |
| 2. `cargo check --workspace` | ✅ clean |
| 3. `cargo clippy … -D warnings` | ✅ clean |
| 4. `cargo test --workspace` | ✅ 18 passing |
| 5. docs smoke (`doctor` + `smoke-webhook.sh`) | ✅ verified locally |
| 6. disposable-repo E2E | ⏳ requires live App creds + `coven-code` |
