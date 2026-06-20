# Familiar GitHub Starter

This example shows a minimal familiar setup for routing GitHub issues to Cody through `coven-github`.

Use it as a starting point for a demo repository:

1. Copy `config.toml` to your own `config/local.toml`.
2. Replace the GitHub App ID, private key path, and webhook secret.
3. Set `bot_username` to your GitHub App bot login.
4. Confirm `coven_code_bin` points at a `coven-code` binary with headless mode support.
5. Add labels such as `coven:fix` and `coven:docs` to issues you want Cody to draft PRs for.

The example keeps autonomy conservative: the familiar should draft PRs and expose Cave oversight rather than merging changes directly.
