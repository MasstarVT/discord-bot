# syntax=docker/dockerfile:1

# ── Build: the Rust bot and dashboard (rust/). ───────────────────────────────
FROM rust:1-bookworm AS build

WORKDIR /src

# The Rust crates embed files from outside rust/ at compile time: prisma/migrations
# (the migration runner), src/dashboard/public and src/dashboard/commands.json (the
# dashboard front end) and CHANGELOG.md. .dockerignore keeps the context small.
COPY rust ./rust
COPY prisma ./prisma
COPY src/dashboard/public ./src/dashboard/public
COPY src/dashboard/commands.json ./src/dashboard/commands.json
COPY CHANGELOG.md ./CHANGELOG.md

# Cache mounts keep the crate downloads and the compiled dependencies between
# builds; the binaries are copied out because a cache mount is not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/rust/target \
    cd rust \
 && cargo build --release --locked -p bot -p dashboard \
 && mkdir -p /out \
 && cp target/release/bot target/release/dashboard /out/

# ── Runtime ──────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim

# ca-certificates for outgoing HTTPS (Discord, feeds, translation); curl for the
# health check. The binaries use rustls, so no libssl is needed.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# Same name and ids as the user of the node image this replaced, so an existing
# state volume keeps working.
RUN groupadd --gid 1000 node \
 && useradd --uid 1000 --gid node --shell /bin/sh --create-home node

WORKDIR /app

ENV NODE_ENV=production \
    STATE_DIR=/app/state \
    DEPLOY_HASH_FILE=/app/state/.deploy-hash \
    HEALTH_PORT=8081

COPY --from=build /out/bot /out/dashboard /usr/local/bin/

# Optional runtime files, read relative to /app: the Scrabble word list
# (assets/dictionaries/words.txt). Fonts, migrations and the dashboard front end
# are compiled into the binaries.
COPY assets ./assets

# /app/state holds the restart-brake history and the slash-command hash. Mount
# a named volume here; Docker copies this ownership into a new, empty volume.
RUN mkdir -p /app/state && chown node:node /app/state

USER node

LABEL org.opencontainers.image.source=https://github.com/MasstarVT/discord-bot

EXPOSE 8081 8080

# Liveness only: 200 while the process runs (also during the restart brake and
# while parked). Readiness is /readyz, polled by Uptime Kuma.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD curl -fsS --max-time 4 "http://127.0.0.1:${HEALTH_PORT:-8081}/healthz" -o /dev/null || exit 1

# The bot applies pending migrations itself when DATABASE_URL is set and parks
# (instead of crash-looping) when it is missing. Exec form, so it gets signals.
# Other entry points in this image: `bot --deploy` (push slash commands) and
# `dashboard` (the web dashboard, port 8080).
CMD ["bot"]
