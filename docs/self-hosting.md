# Self-Hosting coven-github

This guide walks you through registering a GitHub App and running `coven-github` on your own infrastructure.

## Prerequisites

- Rust toolchain (`rustup`)
- `coven-code` binary installed and in PATH (or set `coven_code_bin` in config)
- A public HTTPS endpoint for the webhook receiver (ngrok works for local dev)
- A GitHub account with permission to create and install GitHub Apps
- Model provider credentials available to the `coven-code` runtime you plan to use

This guide targets macOS and Linux operators. Windows should work through WSL2 or a containerized worker, but the production isolation path is still being hardened.

## 1. Register a GitHub App

You can register the App from the prefilled manifest (fastest) or by hand.

### Option A — manifest flow (recommended)

`docs/app-manifest.json` describes the exact permissions and event subscriptions
`coven-github` needs. Use it to create the App in one round trip:

1. Open `docs/app-manifest.json` and replace the two `https://your-host`
   placeholders (`hook_attributes.url` → `https://your-host/webhook`,
   `redirect_url`) with your public endpoint.
2. Visit `https://github.com/settings/apps/new` (personal) or
   `https://github.com/organizations/<org>/settings/apps/new` (org) and use the
   "Create from manifest" flow, or POST the manifest via the
   [App manifest API](https://docs.github.com/apps/sharing-github-apps/registering-a-github-app-from-a-manifest).
3. GitHub returns the **App ID**, generated **webhook secret**, and a downloadable
   **private key** — save all three for the config step.

The manifest already requests the correct permissions and subscribes to every
event below, so you can skip the manual checklist.

### Option B — manual registration

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
   - Pull request review
   - Pull request review comment
   - Check suite / Check run
5. Click **Create GitHub App**
6. Generate and download a **private key** (PEM file)
7. Note your **App ID**

> **Webhook triggers.** `coven-github` reacts to: issue **assignment** to a bot
> familiar, configured **trigger labels**, **@mentions** in issue comments, PR
> conversation comments, submitted **PR reviews**, and inline **review comments**.
> GitHub's `ping` delivery is acknowledged with a `pong` so you can confirm the
> endpoint is wired up from the App's **Advanced → Recent Deliveries** page.

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

Important config fields:

| Field | Purpose |
|---|---|
| `server.bind` | Local address the webhook server listens on. |
| `server.cave_base_url` | Optional CovenCave URL used in Check Runs and comments. |
| `github.app_id` | Numeric GitHub App ID from the App settings page. |
| `github.private_key_path` | Path to the downloaded PEM private key. Do not commit it. |
| `github.webhook_secret` | Secret GitHub uses to sign webhook deliveries. |
| `worker.coven_code_bin` | Path to the `coven-code` binary with headless mode support. |
| `worker.workspace_root` | Temporary task workspace root. Keep it outside the repo. |
| `worker.timeout_secs` | Wall-clock limit for each familiar run. |
| `familiars[].bot_username` | GitHub App bot username that assignment and mentions match. |
| `familiars[].trigger_labels` | Labels such as `coven:fix` that create familiar tasks. |

## 5. Run

```bash
./target/release/coven-github serve --config config/local.toml
```

The server starts on the configured bind address. Point your GitHub App webhook at `https://your-host/webhook`.

Expected startup log:

```txt
coven-github listening on 0.0.0.0:3000
```

## 6. Local smoke test

Before connecting a real GitHub App delivery, verify the server rejects unsigned payloads:

```bash
curl -i \
  -H 'X-GitHub-Event: issues' \
  -d '{"action":"labeled"}' \
  http://localhost:3000/webhook
```

Expected result: `401 Unauthorized` with `{"error":"missing signature"}`. That confirms the webhook route is reachable and signature enforcement is active.

## 7. End-to-end test

On a repo where the App is installed:
1. Create an issue
2. Assign it to your bot user (`@coven-cody`)
3. Watch the Check Run appear and the familiar start working

You can also apply a configured label such as `coven:fix` to route the issue through `familiars[].trigger_labels`.

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

See [Security Model](security.md) and [Container Isolation](container-isolation.md) for the production security target.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `401 missing signature` | Local curl or GitHub did not send `X-Hub-Signature-256`. | Expected for unsigned smoke tests; check GitHub App webhook secret for real deliveries. |
| `401 invalid signature` | `github.webhook_secret` does not match the GitHub App secret. | Rotate/copy the secret into `config/local.toml` and restart. |
| No task appears after assignment | Bot username or installed repository does not match config. | Confirm `familiars[].bot_username` equals the App bot login and the App is installed on the repo. |
| Label does nothing | Label is not in `familiars[].trigger_labels`. | Add the exact label name and restart. |
| Check Run fails immediately | GitHub App permissions are incomplete or the head SHA/base branch path needs hardening. | Confirm Contents, Issues, Pull requests, Checks, and Metadata permissions. |
| Familiar never exits | Runtime process hung. | Lower `worker.timeout_secs`; current workers enforce this timeout. |
| No PR opens | `coven-code` did not write commits or a successful `result.json`. | Inspect worker logs and the task workspace before cleanup in a development run. |
