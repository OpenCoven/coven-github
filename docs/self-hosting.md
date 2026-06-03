# Self-Hosting coven-github

This guide walks you through registering a GitHub App and running `coven-github` on your own infrastructure.

## Prerequisites

- Rust toolchain (`rustup`)
- `coven-code` binary installed and in PATH (or set `coven_code_bin` in config)
- A public HTTPS endpoint for the webhook receiver (ngrok works for local dev)

## 1. Register a GitHub App

1. Go to **GitHub → Settings → Developer settings → GitHub Apps → New GitHub App**
2. Set:
   - **App name:** `coven-cody` (or your org's name)
   - **Homepage URL:** `https://opencoven.ai`
   - **Webhook URL:** `https://your-host/webhook`
   - **Webhook secret:** generate a random string (save it for config)
3. **Permissions:**
   - Repository → Contents: Read & Write
   - Repository → Issues: Read & Write
   - Repository → Pull requests: Read & Write
   - Repository → Checks: Write
   - Repository → Metadata: Read
4. **Subscribe to events:**
   - Issues
   - Issue comment
   - Pull request review comment
   - Check suite / Check run
5. Click **Create GitHub App**
6. Generate and download a **private key** (PEM file)
7. Note your **App ID**

## 2. Install the App on a repo

From your GitHub App's page, click **Install App** and select the target repository.

## 3. Build coven-github

```bash
git clone https://github.com/OpenCoven/coven-github
cd coven-github
cargo build --release
```

## 4. Configure

```bash
cp config/example.toml config/local.toml
```

Edit `config/local.toml`:
- Set `github.app_id` to your App ID
- Set `github.private_key_path` to the downloaded PEM
- Set `github.webhook_secret` to the secret from step 1
- Set `worker.coven_code_bin` to your `coven-code` binary path
- Configure `[[familiars]]` with your bot username and model

## 5. Run

```bash
./target/release/coven-github serve --config config/local.toml
```

The server starts on the configured bind address. Point your GitHub App webhook at `https://your-host/webhook`.

## 6. Test it

On a repo where the App is installed:
1. Create an issue
2. Assign it to your bot user (`@coven-cody`)
3. Watch the Check Run appear and the familiar start working

## Local development with ngrok

```bash
ngrok http 3000
# Copy the https URL → set as webhook URL in GitHub App settings
```

## Docker

```dockerfile
FROM rust:1.82 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates git && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/coven-github /usr/local/bin/
COPY config/example.toml /config/local.toml
CMD ["coven-github", "serve", "--config", "/config/local.toml"]
```

## Security notes

- The webhook secret is critical — validate it on every request (coven-github does this automatically)
- Installation tokens expire every hour — coven-github refreshes them automatically
- Never commit your private key PEM to the repository
- Run workers in isolated containers per task in production (see `docs/container-isolation.md`)
