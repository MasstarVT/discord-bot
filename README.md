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
npm run db:push        # or npm run db:migrate for production
npm run dev             # start with auto-restart
```

Production process management is handled via PM2 (`npm run pm2:start`, `pm2:reload`, `pm2:logs`).

## License

MIT — see [LICENSE](LICENSE).
