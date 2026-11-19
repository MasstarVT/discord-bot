# The Rust bot

`rust/` is a port of the JavaScript bot (`index.js`, `src/`) to Rust. It is a drop-in replacement: same slash
commands, same text, same PostgreSQL schema, same Redis keys, same `.env`. The JS code stays the specification;
`rust/PORTING.md` is the contract the port follows.

| Path | What |
|---|---|
| `bins/bot` | the bot binary: `index.js` + `src/bot.js` (start, `--deploy`, shutdown) |
| `bins/dashboard` | the web dashboard binary (`dashboard.js`) |
| `crates/bot-*` | config, database, utilities, image cards, the framework (`src/handlers`, `src/events`, core services) |
| `crates/feat-*` | the feature modules, one group each (commands, plugins, services) |
| `crates/dash-*` | the dashboard pieces |

## Build

You need a stable Rust toolchain (cargo 1.85 or newer, edition 2024). From `rust/`:

```sh
cargo build --release -p bot -p dashboard
```

The binaries are `rust/target/release/bot` and `rust/target/release/dashboard` (`.exe` on Windows). The Prisma
migrations are compiled into the bot.

## Run

Run the binary **from the repository root**, like `npm start`. It reads `.env` from the current directory, and keeps
`state/` (the restart-brake history) and `.deploy-hash` there, unless `STATE_DIR` / `DEPLOY_HASH_FILE` say otherwise.

```sh
./rust/target/release/bot            # npm start
./rust/target/release/bot deploy     # npm run deploy (also: bot --deploy)
```

What a start does, in order:

1. Loads `.env` the way `dotenv` does (variables already set in the environment win).
2. When `DATABASE_URL` is set, applies the pending Prisma migrations from `prisma/migrations/` and exits with code 1
   if that fails. This is what the JS Docker image does with `prisma migrate deploy` before `node index.js`. The runner
   writes Prisma's own `_prisma_migrations` table, so `prisma migrate deploy` and the Rust bot agree on what has been
   applied. An applied migration whose file changed since is only a warning
   (`The migration `<name>` was modified after it was applied.`), as with Prisma.
3. Starts the health endpoints on `HEALTH_PORT` (8081): `GET /healthz` and `GET /readyz`.
4. Parks (stays up, `/readyz` returns `parked:missing-env`) when `DISCORD_TOKEN`, `CLIENT_ID`, `DATABASE_URL` or
   `REDIS_URL` is missing, and parks on the fatal gateway codes 4004, 4013 and 4014 instead of retrying.
5. Applies the restart brake (more than 5 starts in an hour wait before connecting).
6. Deploys the slash commands only when their hash differs from `.deploy-hash` (the same file and format as the JS).
7. Connects to PostgreSQL, Redis and Discord (all shards in this one process), then starts the job scheduler when
   the first shard is ready.

`SIGTERM` or `SIGINT` (Ctrl+C) shut it down like the JS bot: the scheduler stops (running jobs get 5 s), the
achievement counts and the audit-log queue are flushed, the shards disconnect, then the database and Redis close.
Past 25 s the process exits anyway; a second signal exits at once.

## Deploy the slash commands

```sh
./rust/target/release/bot deploy
```

This PUTs every command (to `DEPLOY_GUILD_ID` when set, otherwise globally) and writes the hash to
`DEPLOY_HASH_FILE`, exactly like `npm run deploy`. A normal start does the same automatically when the commands have
changed. The command payloads are byte-identical to the JS ones, so the hash the JS bot wrote is the hash the Rust
bot computes: switching bots does not redeploy.

## Test

Tests are offline by default. The database and Redis tests need the mock servers in `rust/mockdb/` (a real
PostgreSQL 17 and a Redis-protocol mock). From the repository root:

```sh
node rust/mockdb/mockdb.mjs start                       # if the servers are not running
eval "$(node rust/mockdb/mockdb.mjs db bot)"            # a private database and Redis db for the bot crate
cd rust && cargo test -p bot
```

`bash rust/run-all-tests.sh` runs every crate the same way, each with a fresh database. Without
`TEST_DATABASE_URL` / `TEST_REDIS_URL` the database tests print `skipped:` and return. Several tests run Node
scripts from `rust/fixtures/gen/` to compare with the JS bot, so `node` and the repository's `node_modules` must be
installed.

The bot crate's tests check the whole registry against the JS bot (all 79 commands, all 43 plugins, every customId
prefix and job type), the deploy hash against the JS hash, `.env` parsing against `dotenv`, and a full start against
the mock database: boot, READY, a job the JS bot scheduled, shutdown.

## The web dashboard

`bins/dashboard` is `dashboard.js` (`npm run dashboard`): a separate process from the bot that shares its database
and Redis. Build it with the bot (above) and run it **from the repository root**, where it reads `.env`:

```sh
./rust/target/release/dashboard      # npm run dashboard
```

It reads the same keys as the JS dashboard (the "Web dashboard" section of `.env.example`): `CLIENT_ID`,
`DISCORD_TOKEN`, `DISCORD_CLIENT_SECRET`, `DASHBOARD_SESSION_SECRET`, `DATABASE_URL`, `REDIS_URL`, and optionally
`DASHBOARD_URL`, `DASHBOARD_PORT` (8080), `DASHBOARD_HOST`, `DASHBOARD_TRUSTED_PROXIES`, the contact and support keys and
`BOT_OWNER_ID`. When a required key is missing or invalid it does not exit: it logs which keys to set and parks
(`/healthz` answers 200, every other path 503), like the JS. Configured, it logs
`Dashboard listening on <host>:<port> (public URL <url>).`; `SIGTERM` / `SIGINT` close the server and every open
connection, then the database and Redis, with a 10 s limit (a second signal exits at once).

The dashboard never runs scheduled jobs and never migrates the database. What needs the bot's gateway connection
(moderation actions, giveaways, polls, panels, XP changes) is queued as a `dashboard.task` job and run by the bot
within a few seconds; the protocol is the JS one, so the Rust dashboard works with the JS bot and the JS dashboard
with the Rust bot. Session cookies are sealed the same way, so logged-in users stay logged in across a switch.

Its tests (`cargo test -p dashboard`, mock database `dashboard`) compare the settings and records bridges with the JS
ones on the same data, run dashboard tasks in both directions between the two languages, and drive the real server
end to end with a stubbed Discord API.

### Switching between the JS and the Rust dashboard

Stop the running dashboard (its pm2 process, Ctrl+C, ...) and start the other one from the repository root:
`./rust/target/release/dashboard` or `npm run dashboard`. They use the same port, the same `.env` and the same
`DASHBOARD_SESSION_SECRET`, so nothing else changes. Either dashboard works with either bot.

## Switching between the JS and the Rust bot

Both bots use the same `.env`, the same database and the same Redis, and each reads what the other wrote.

1. Stop the running bot (`pm2 stop discord-bot`, `docker compose stop`, or Ctrl+C). Never run both at once: they
   would log in with the same token and both claim the scheduled jobs.
2. Start the other one from the repository root:
   - Rust: `./rust/target/release/bot`
   - JS: `npm start` (or your pm2 / Docker setup)

Nothing needs converting. The schema is the Prisma schema, unchanged; settings and other cache entries use the JS
key names and JSON shapes; scheduled jobs, buttons and select menus on messages the other bot posted keep working.
Going back to the JS bot after the Rust one applied a migration is fine too: it applied the repository's own
migrations and recorded them the way Prisma does.

## License

MIT, by MasstarVT.
