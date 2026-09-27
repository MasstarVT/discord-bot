FROM node:22-bookworm-slim

# Prisma 5.22's query and schema engines need libssl, which slim images don't ship.
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app

ENV NODE_ENV=production \
    STATE_DIR=/app/state \
    DEPLOY_HASH_FILE=/app/state/.deploy-hash \
    HEALTH_PORT=8081 \
    CHECKPOINT_DISABLE=1

# Dependencies first so code-only changes reuse this layer.
COPY package.json package-lock.json ./
COPY prisma ./prisma
RUN npm ci --omit=dev \
 && node_modules/.bin/prisma generate \
 && npm cache clean --force

COPY . .

# /app/state holds the restart-brake history and the slash-command hash. Mount
# a named volume here; Docker copies this ownership into a new, empty volume.
RUN mkdir -p /app/state && chown node:node /app/state

USER node

LABEL org.opencontainers.image.source=https://github.com/MasstarVT/discord-bot

EXPOSE 8081

# Liveness only: 200 while the main thread's event loop runs (also during the
# restart brake and while parked). Readiness is /readyz, polled by Uptime Kuma.
# The slim image has no curl or wget, so use node's fetch.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:${HEALTH_PORT:-8081}/healthz',{signal:AbortSignal.timeout(4000)}).then(r=>process.exit(r.ok?0:1),()=>process.exit(1))"

# Apply pending migrations, then replace the shell with node so it gets signals.
CMD ["sh", "-c", "node_modules/.bin/prisma migrate deploy && exec node index.js"]
