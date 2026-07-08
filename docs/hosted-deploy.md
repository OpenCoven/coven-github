# Hosted deployment runbook

How the hosted OpenCoven GitHub adapter runs in production: one VM, Docker
Compose, Caddy TLS ingress, and continuous SQLite replication to Cloudflare
R2. This is the operational half of the [HOSTED.md](../HOSTED.md) beta gates;
the code half (durable queue, isolation, metering, entitlements) is shipped.

**Architecture choice:** the store is single-writer SQLite and the container
worker backend drives the host Docker daemon, so the natural unit is one
well-sized VM (Hetzner CPX41-class: 8 vCPU / 16 GB). Tier task caps keep beta
load well inside that. Hosted Dedicated later = this same stack on a
customer-dedicated VM.

Everything lives in [`deploy/hosted/`](../deploy/hosted/):

| File | Role |
|---|---|
| `provision.sh` | One-shot Ubuntu 24.04 host setup (Docker, firewall, layout) |
| `compose.yaml` | Adapter + Caddy + Litestream services |
| `Caddyfile` | TLS ingress for the webhook domain |
| `litestream.yml` | SQLite → R2 continuous replication |
| `.env.example` | Secrets template (domain, image tag, R2 credentials) |

## First deploy

1. **VM**: create an Ubuntu 24.04 host (Hetzner CPX41 or similar). Point DNS
   `A`/`AAAA` for the webhook domain (e.g. `gh.opencoven.ai`) at it.
2. **Provision** (as root on the VM):
   ```sh
   curl -fsSL https://raw.githubusercontent.com/OpenCoven/coven-github/main/deploy/hosted/provision.sh | bash
   ```
   Installs Docker, enables unattended upgrades, opens only 22/80/443, and
   lays out `/opt/coven-github`.
3. **Secrets** (never in git):
   - Fill `/opt/coven-github/.env` — domain, image tag, R2 token (create an
     R2 bucket + API token scoped to it first).
   - Place the GitHub App PEM at `keys/app.pem`, mode `0600`.
   - Write `config/production.toml` from [`config/example.toml`](../config/example.toml):
     `storage.path = "/data/coven-github.db"`, `worker.backend = "container"`,
     `api.mode = "token"` with tenant tokens, `[billing] require_plan = true`
     at Marketplace go-live (see
     [marketplace-listing.md](marketplace-listing.md)), and `[[installations]]`
     entries for grandfathered beta tenants.
4. **Preflight + start**:
   ```sh
   cd /opt/coven-github
   docker compose run --rm coven-github doctor --config /config/production.toml
   docker compose up -d
   curl -fsS https://gh.opencoven.ai/healthz
   ```
5. **Wire GitHub**: set the App's webhook URL to
   `https://gh.opencoven.ai/webhook` and send a ping delivery — the adapter
   answers `pong` and records the delivery.

The image is published by [`.github/workflows/publish.yml`](../.github/workflows/publish.yml)
to `ghcr.io/opencoven/coven-github` (`latest` + SHA on main, semver on tags).

## Monitoring

- **Liveness/readiness**: `GET /healthz` — 200 when the store answers, 503
  otherwise. Point an external uptime monitor at it (the compose healthcheck
  restarts the container; the external check catches host/DNS/TLS failure).
- **Queue and task health**: the tenant API is the dashboard —
  `/api/github/tasks` (states), `/api/github/usage` (per-installation load),
  `/api/github/audit` (ignored deliveries, attempt outcomes). A growing
  `queued` count with idle workers, or repeated `attempt:failed` audit rows,
  is the primary alert condition.
- **Logs**: `docker compose logs -f coven-github` (`RUST_LOG` in
  compose.yaml). Secrets are redacted by the adapter.
- **Replication**: `docker compose exec litestream litestream dbs -config
  /etc/litestream.yml` should list the db; alert if the R2 bucket's latest
  WAL segment is older than a few minutes.

## Backup and restore

Litestream streams every WAL segment to R2 (10 s sync, 30-day retention).
**Run a restore drill before onboarding paying tenants**, and quarterly:

```sh
docker compose stop coven-github
docker compose run --rm litestream restore -if-replica-exists \
  -config /etc/litestream.yml -o /data/restored.db /data/coven-github.db
# inspect, then swap in and restart
```

Losing this database loses delivery idempotency, task history, usage
metering, and **billing entitlements** — treat replication health as a
page-worthy alert.

## Upgrades

```sh
cd /opt/coven-github
docker compose pull coven-github
docker compose up -d coven-github
curl -fsS https://gh.opencoven.ai/healthz
```

Schema migrations run forward-only at boot. GitHub redelivers webhooks that
5xx during the restart window, and the delivery-id dedup makes redelivery
safe. Pin `COVEN_GITHUB_TAG` to a SHA or semver tag for controlled rollouts;
roll back by re-pinning the previous tag (schema rollbacks are not supported
— restore from R2 if a migration must be undone).

## Incident basics

- **Webhook 5xx / down**: GitHub retries; fix the host, deliveries catch up.
  Check `docker compose ps`, `/healthz`, disk space (`df -h /opt`).
- **Stuck task**: find it via `/api/github/tasks`, inspect
  `/api/github/audit`; the worker enforces timeouts and cleans workspaces —
  a task that outlives its timeout is a bug, capture logs before restarting.
- **Runaway load from one tenant**: intake caps and claim-time concurrency
  already gate per installation; drop the tenant's tier or set explicit
  `[installations.limits]` and restart.
- **Compromise suspected**: rotate the webhook secret and App PEM in GitHub,
  replace `keys/app.pem` and the secret in `production.toml`, restart, and
  audit `/api/github/audit` for the exposure window.
