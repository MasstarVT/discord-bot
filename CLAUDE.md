# CLAUDE.md

Self-hosted MEE6-style Discord bot: discord.js v14 (ESM, Node 22+), Prisma 5.22 on PostgreSQL, Redis cache only, PM2/Docker. Each feature is a module admins toggle with `/module` (`README.md` lists features and commands).

Detail lives in two docs, read only the part you need (grep first):
- `docs/claude/architecture.md`: env keys, entry flow, extension points, hooks, customIds, scheduler, rewards, settings, Redis keys, schema protocol, web dashboard, utilities, conventions.
- `docs/claude/modules.md`: one section per module (commands, code, hooks, gotchas).

## Commands
`npm run dev|start`, `npm test` (offline, `node --test "test/*.test.js"`; one file: `node --test test/<f>.test.js`), `npm run deploy` (register slash commands), `npx prisma generate` after every schema edit, `npm run db:migrate` (new additive migration), `npm run pm2:start|reload|logs`, `npm run dashboard|pm2:dashboard` (web dashboard, own process). Required `.env`: `DISCORD_TOKEN`, `CLIENT_ID`, `DATABASE_URL`, `REDIS_URL` (copy `.env.example`; a new `process.env` read must be added there).

## Rules that bite
- Features are auto-discovered from `src/commands/<category>/`, `src/events/`, `src/plugins/<id>.js`. Never edit `commandLoader.js`, `eventLoader.js` or `pluginLoader.js` to add one. Events are thin dispatchers (`runHooks`); feature logic goes in plugins/services.
- Every command exports `module` explicitly (a `MODULES` key or `null`). New hook id goes in `src/config/hookPoints.js` first; new component prefix in `src/config/customIds.js`; new job type in `src/config/jobTypes.js`. Only automod and custom-commands may call `ctx.stop()`.
- `GuildSettings` only through `src/services/settingsService.js`. Call `ensureGuildSettings` before creating any per-guild row. Arrays/JSON change via `updateSettingsWith`.
- Schema changes are additive only (never drop/rename/retype); never edit an applied migration. Raw SQL uses `SQL_NOW`/`sqlUtc`, `::bigint` maths, Int columns clamped with `XP_MAX`/`COIN_MAX`.
- Redis is a cache, never the source of truth; always pass a TTL; `cache` never throws. No `setInterval` (use a recurring `ScheduledJob`). `logger` only, never `console`. Signal handlers only in `index.js`.
- Acknowledge interactions within 3 s (`ackBudgetMs`, `peekSettings`). Wrap timers and collectors so nothing rejects unhandled (a dead shard restarts the bot).
- Any user-influenced URL goes through `safeFetch`; user regex through `safeRegex`; admin text through `placeholderParser`/`customEmbed`. Never parse `@everyone` mentions. Per-guild caps are `LIMITS` in `src/config/constants.js`, never hard-coded.
- Adding a module: settings flag + migration, `DEFAULT_MODULE_SETTINGS`, `MODULES` entry, commands, plugin, README rows, a section in `docs/claude/modules.md`, an entry below, `.env.example` keys, a `CATEGORIES` + `DESCRIPTIONS` entry in `src/dashboard/catalog.js`. `test/integration.test.js` fails until all agree (it checks this file against the code).

## Registry (checked by tests; module key, `GuildSettings` flag, plugin)
`moderation` `modEnabled` `src/plugins/moderation.js` · `automod` `automodEnabled` `src/plugins/automod.js` · `logging` `loggingEnabled` `src/plugins/logging.js` · `welcome` `welcomeEnabled` `src/plugins/welcome.js` · `captcha` `captchaEnabled` `src/plugins/captcha.js` · `reactionroles` `reactionRolesEnabled` `src/plugins/reaction-roles.js` · `leveling` `xpEnabled` `src/plugins/levels.js` · `customcommands` `customCommandsEnabled` `src/plugins/custom-commands.js` · `automations` `automationsEnabled` `src/plugins/automations.js` · `translation` `translationEnabled` `src/plugins/translation-fixes.js` · `invites` `invitesEnabled` `src/plugins/invites.js` · `embeds` `embedsEnabled` `src/plugins/embeds.js` · `reminders` `remindersEnabled` `src/plugins/reminders.js` · `giveaways` `giveawaysEnabled` `src/plugins/giveaways.js` · `polls` `pollsEnabled` `src/plugins/polls.js` · `starboard` `starboardEnabled` `src/plugins/starboard.js` · `tickets` `ticketsEnabled` `src/plugins/tickets.js` · `tempvoice` `tempVoiceEnabled` `src/plugins/temp-voice.js` · `stats` `statsEnabled` `src/plugins/stat-channels.js` · `birthdays` `birthdaysEnabled` `src/plugins/birthdays.js` · `achievements` `achievementsEnabled` `src/plugins/achievements.js` · `economy` `economyEnabled` `src/plugins/economy.js` · `games` `gamesEnabled` `src/plugins/games-fixes.js` · `feeds` `feedsEnabled` `src/plugins/feeds.js` · `search` `searchEnabled` (no plugin) · `liverole` `liveRoleEnabled` `src/plugins/liverole.js` · `counting` `countingEnabled` `src/plugins/counting.js`

Job types: `mod.unban` `mod.expire` `welcome.autorole` `captcha.timeout` `reminder.fire` `giveaway.end` `poll.end` `tempvoice.cleanup` `stats.refresh` `birthday.tick` `feed.poll` `levels.voiceSweep` `dashboard.task`

Core plugins (no module): `src/plugins/dashboard-tasks.js` `src/plugins/bot-joins.js`
