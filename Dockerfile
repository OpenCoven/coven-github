# syntax=docker/dockerfile:1
#
# Multi-stage build for the coven-github webhook server.
#
#   docker build -t coven-github .
#   docker run --rm -p 3000:3000 \
#     -v "$PWD/config:/config:ro" -v "$PWD/keys:/keys:ro" \
#     coven-github serve --config /config/local.toml
#
# See docs/self-hosting.md for the full walkthrough and compose.yaml for a
# ready-to-edit Compose service.

# ── Builder ─────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS builder
WORKDIR /app

# Copy the whole workspace and build the release binary. (A cold build pulls the
# full dependency tree; subsequent builds reuse Docker's layer cache.)
COPY . .
RUN cargo build --release --locked -p coven-github

# ── Runtime ─────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates: TLS to api.github.com. git: clone target repos.
# curl: container healthchecks against /healthz (deploy/hosted/compose.yaml).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git curl \
    && rm -rf /var/lib/apt/lists/*

# Run as an unprivileged user. Secrets are mounted read-only at runtime, never
# baked into the image (see the .dockerignore — keys/ and config/local.toml are
# excluded from the build context).
RUN useradd --system --create-home --uid 10001 coven
USER coven
WORKDIR /home/coven

COPY --from=builder /app/target/release/coven-github /usr/local/bin/coven-github

EXPOSE 3000

# `doctor` exits non-zero on a broken config, so it works as a preflight check.
ENTRYPOINT ["coven-github"]
CMD ["serve", "--config", "/config/local.toml"]
