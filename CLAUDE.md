# CLAUDE.md

MEE6-style Discord bot: discord.js v14 (ESM, Node 22+), Prisma 5.22 on PostgreSQL, Redis cache only, PM2/Docker. Features are modules toggled with `/module` (see `README.md`). Details (grep): `docs/claude/architecture.md` (core, hooks, scheduler, settings, schema, dashboard) and `docs/claude/modules.md` (per module).

## Commands
`npm run dev|start`, `npm test` (offline; one file: `node --test test/<f>.test.js`), `npm run deploy`, `npx prisma generate` after schema edits, `npm run db:migrate`, `npm run pm2:start|reload|logs`, `npm run dashboard|pm2:dashboard`. New `process.env` reads go in `.env.example`.

## Rules that bite
- Features auto-load from `src/commands/<category>/`, `src/events/`, `src/plugins/<id>.js`; never edit the loaders. Events only `runHooks`; logic goes in plugins/services.
- Commands export `module` (a `MODULES` key or `null`). New hook ids, customId prefixes and job types go in `src/config/` (`hookPoints.js`, `customIds.js`, `jobTypes.js`) first. Only automod and custom-commands call `ctx.stop()`.
- `GuildSettings` only via `src/services/settingsService.js`; `ensureGuildSettings` before any per-guild row; arrays/JSON via `updateSettingsWith`.
- Schema changes additive only; never edit an applied migration. Raw SQL: `SQL_NOW`/`sqlUtc`, `::bigint` maths, clamp Int columns with `XP_MAX`/`COIN_MAX`.
- Redis: cache only, always a TTL. No `setInterval` (recurring `ScheduledJob`). `logger`, never `console`. Signal handlers only in `index.js`.
- Ack interactions within 3 s (`ackBudgetMs`, `peekSettings`). Nothing may reject unhandled.
- User URLs via `safeFetch`, user regex via `safeRegex`, admin text via `placeholderParser`/`customEmbed`. Never parse `@everyone`. Caps come from `LIMITS` in `src/config/constants.js`.
- New module: flag + migration, `DEFAULT_MODULE_SETTINGS`, `MODULES` entry, commands, plugin, README rows, a `docs/claude/modules.md` section, a registry row, `.env.example` keys, `src/dashboard/catalog.js` entries (category, description, two-letter code) and `fields.js` groups. `test/integration.test.js` checks they agree.

## Registry
Module keys, `GuildSettings` flags, plugin files, job types and core plugins: the module table under "Modules and `/module`" in `docs/claude/architecture.md` (test-checked; keep it complete).
