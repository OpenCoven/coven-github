#!/usr/bin/env bash
# Provision a fresh Ubuntu 24.04 host (e.g. Hetzner CPX41) to run hosted
# coven-github. Idempotent; run as root ON THE SERVER:
#
#   curl -fsSL https://raw.githubusercontent.com/OpenCoven/coven-github/main/deploy/hosted/provision.sh | bash
#
# Then follow docs/hosted-deploy.md to place config, keys, and .env.
set -euo pipefail

DEPLOY_DIR=/opt/coven-github
RAW=https://raw.githubusercontent.com/OpenCoven/coven-github/main/deploy/hosted

echo "==> Installing Docker Engine + compose plugin"
if ! command -v docker >/dev/null 2>&1; then
  curl -fsSL https://get.docker.com | sh
fi

echo "==> Hardening: unattended upgrades + firewall (22/80/443 only)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq unattended-upgrades ufw curl
ufw default deny incoming
ufw default allow outgoing
ufw allow 22/tcp
ufw allow 80/tcp
ufw allow 443/tcp
ufw --force enable

echo "==> Laying out ${DEPLOY_DIR}"
mkdir -p "${DEPLOY_DIR}"/{config,keys,data,workspaces}
chmod 700 "${DEPLOY_DIR}/keys"
# The image runs as uid 10001 (user `coven`); it owns its state dirs.
chown 10001 "${DEPLOY_DIR}/data" "${DEPLOY_DIR}/workspaces"
cd "${DEPLOY_DIR}"
for f in compose.yaml Caddyfile litestream.yml .env.example; do
  curl -fsSL "${RAW}/${f}" -o "${f}"
done
[ -f .env ] || { cp .env.example .env && chmod 600 .env; }

echo "==> Recording the docker group id for the compose worker"
DOCKER_GID="$(getent group docker | cut -d: -f3)"
grep -q '^DOCKER_GID=' .env \
  && sed -i "s/^DOCKER_GID=.*/DOCKER_GID=${DOCKER_GID}/" .env \
  || echo "DOCKER_GID=${DOCKER_GID}" >> .env

cat <<'NEXT'

Provisioned. Next (docs/hosted-deploy.md):
  1. Fill /opt/coven-github/.env            (domain, image tag, R2 credentials)
  2. Place /opt/coven-github/config/production.toml and keys/app.pem (0600)
  3. Point DNS A/AAAA for your webhook domain at this host
  4. docker compose run --rm coven-github doctor --config /config/production.toml
  5. docker compose up -d
  6. curl -fsS https://<domain>/healthz
NEXT
