# CompleteTech hosted dogfood adapter

This directory tracks the Python adapter currently deployed for the
CompleteTech-hosted dogfood GitHub App at `webhook.complete.tech`.

The adapter is intentionally deployment-specific. It is not the canonical Rust
worker implementation; it exists so hosted dogfood behavior can be reviewed,
reproduced, and changed through PRs instead of invisible server edits.

## Files

- `coven_github_adapter.py` - webhook handler, task runner, PR evidence capture,
  Codex-backed headless runtime invocation, and dogfood comment publisher.

## Runtime inputs

The deployment expects secrets and mutable state to be supplied outside git:

- `GITHUB_APP_PRIVATE_KEY_PATH` or `.coven-github-private-key.pem`
- `GITHUB_APP_ID`
- `COVEN_GITHUB_STATE_DIR`
- `COVEN_GITHUB_POLICY_PATH`
- `COVEN_CODE_BIN`
- Codex OAuth tokens under the deployed account's `.coven-code` directory

Do not commit private keys, webhook secrets, OAuth tokens, generated task state,
workspaces, or attempt artifacts.

## Current dogfood behavior

- Emits headless contract v2 session briefs.
- Captures PR checkout metadata and changed-file patches before invoking
  `coven-code`.
- Publishes visible structured review evidence, including `reviewed_files`,
  `supporting_files`, findings, test evidence, no-findings rationale, and
  limitations.
