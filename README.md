# Discord Bot

A production-grade, multi-purpose Discord bot built on discord.js, with Postgres (via Prisma) for persistence and Redis for caching, and PM2 for process management.

## Features

- **Moderation, Admin & Utility** commands alongside **Economy**, **Leveling**, **Fun**, and **Games** modules
- Slash commands, auto-deployed on startup when the command set changes (hash-checked)
- Sharded via `discord.js`'s `ShardingManager` (`totalShards: 'auto'`)
- Prisma ORM for database access and migrations

## Setup

```bash
npm install
cp .env.example .env   # fill in DISCORD_TOKEN, CLIENT_ID, DATABASE_URL, REDIS_URL
npm run db:migrate     # apply prisma/migrations (and create one after editing the schema)
npm run dev             # start with auto-restart
```

Production process management is handled via PM2 (`npm run pm2:start`, `pm2:reload`, `pm2:logs`).

## Running in Docker / homelab

GitHub Actions tests every push and pull request. Pushes to `main` publish `ghcr.io/masstarvt/discord-bot` as an immutable `:sha-<7 hex>` tag and move `:latest` to it, but only while that commit is still the head of `main` (a run that finishes after a newer push only adds its `sha-` tag), so `:latest` never goes backwards by accident. The **rollback** workflow points `:latest` back at an older `sha-` tag. If a rollback run shows as cancelled (a publish queued up behind it), run it again.

```yaml
services:
  discord-bot:
    image: ghcr.io/masstarvt/discord-bot:latest
    restart: unless-stopped
    init: true
    stop_grace_period: 30s
    env_file: discord-bot.env   # DISCORD_TOKEN, CLIENT_ID, DATABASE_URL, REDIS_URL, ...
    volumes:
      - bot_state:/app/state
volumes:
  bot_state:
```

- **Env file:** set only the keys you need. Don't copy empty keys or `NODE_ENV=development` from `.env.example`: Compose passes empty values through, and they override what the image sets (`NODE_ENV=production`, `HEALTH_PORT`, `STATE_DIR`, `DEPLOY_HASH_FILE`).
- **Migrations:** the container runs `prisma migrate deploy` before it starts the bot. Every schema change needs a migration (`npm run db:migrate`), and CI fails when `schema.prisma` and `prisma/migrations/` disagree. Keep migrations additive, because rolling back the image does not roll back the database.
- **Privileged intents:** enable Server Members, Presence and Message Content under Bot → Privileged Gateway Intents in the Developer Portal. Without them Discord closes the connection with code 4014.
- **Health endpoints** on `HEALTH_PORT` (default 8081):
  - `GET /healthz` returns 200 while the process runs. The image's `HEALTHCHECK` uses it.
  - `GET /readyz` returns 200 only when every shard is connected to Discord. Otherwise it returns 503 with a `reason`: `starting`, `brake-delay`, `shard-not-ready` or `parked:<code>`.
- **Restart brake:** every start is recorded in `STATE_DIR` (`/app/state` in the image; mount a volume there). From the 6th start within an hour, the bot waits 2, 4, 8, 16 and then 30 minutes before connecting, so a crash loop can't use up Discord's 1000 logins per day. A shard that dies, or that Discord disconnects for good with a non-fatal code, ends the process, and Docker's restart policy plus the brake decide when to try again.
- **Parked:** close codes 4004 (bad token), 4013 and 4014 (invalid or disallowed intents), and missing required environment variables, are never retried. The process stays up, logs what to fix, and `/readyz` returns 503 `parked:<code>`. Fix the cause, then restart the container.
- **Slash commands:** the command hash lives at `DEPLOY_HASH_FILE` (`/app/state/.deploy-hash` in the image), so restarts don't re-register unchanged commands. Delete it after switching `DEPLOY_GUILD_ID` between one guild and global.
- **Shutdown:** `SIGTERM` or `SIGINT` disconnects every shard from Discord and exits within 25 s.

## License

MIT — see [LICENSE](LICENSE).
