# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

```bash
npm run dev          # Start bot with nodemon (auto-restart on file changes)
npm start            # Start bot with node directly
npm run deploy       # Force-register slash commands via Discord REST API, then exit
                     #   Set DEPLOY_GUILD_ID in .env for instant guild-scoped deployment
                     #   Omit it for global deployment (up to 1 hour propagation)
                     #   Note: commands are also auto-deployed on every normal startup
                     #   when the command set has changed (hash-checked, no-op if unchanged)

npm run db:push      # Push schema changes to Postgres without creating a migration file (dev only)
npm run db:migrate   # Create and apply a named migration (use for production-bound changes)
npm run db:generate  # Regenerate Prisma client after schema edits
npm run db:studio    # Open Prisma Studio GUI to inspect/edit data
npm run db:seed      # Insert a minimal GuildSettings row for local testing

npm run pm2:start    # Launch under PM2 (production)
npm run pm2:reload   # Zero-downtime reload
npm run pm2:logs     # Tail PM2 logs
```

Required `.env` keys before the bot will start: `DISCORD_TOKEN`, `CLIENT_ID`, `DATABASE_URL`, `REDIS_URL`. Copy `.env.example`.

## Architecture

### Entry Flow

`index.js` → calls `autoDeployCommands()` (hash-checks and deploys slash commands if changed) → `ShardingManager` spawns `src/bot.js` as a worker thread. `bot.js` creates the `discord.js` Client, then calls `loadCommands` and `loadEvents` before `client.login`. The ShardingManager uses `totalShards: 'auto'` so Discord controls shard count — no code change needed as the bot scales past 2500 guilds.

`index.js --deploy` short-circuits the boot sequence, calls `deployCommands()` from `commandLoader.js`, and exits.

**Auto-deploy:** `autoDeployCommands()` in `src/handlers/commandLoader.js` hashes all command payloads and compares against `.deploy-hash` in the project root. It only calls the Discord REST API when the hash changes — safe to run on every restart. Adding a new command file and restarting is all that's needed; no manual deploy step required.

### File-System-as-Registry Pattern

The core handlers discover modules by walking the filesystem — **never edit `commandLoader.js` or `eventLoader.js` when adding features**:

- **Commands:** Any `.js` file under `src/commands/**/` that exports `{ data, execute }` is auto-loaded into `client.commands`. Optionally export `permissionLevel` (integer from `PERMISSION_LEVELS`) and `autocomplete`.
- **Events:** Any `.js` file under `src/events/` that exports `{ name, once, execute }` is auto-registered via `client.on/once`. The `client` instance is injected as the last argument to avoid circular imports.

### Interaction Routing

All Discord interactions funnel through `src/handlers/interactionRouter.js`. It routes by interaction type and enforces permission level checks before calling `command.execute()`.

**Custom ID namespace:** Button and select menu custom IDs must follow `action:param1:param2` (colon-delimited). The router splits on `:` and looks up `action` in `client.buttonHandlers` / `client.selectHandlers`. Register handlers at startup: `client.buttonHandlers.set('my-action', handlerFn)`.

### Permission Levels

Defined in `src/config/constants.js` as `PERMISSION_LEVELS`: `MEMBER=0`, `MODERATOR=1`, `ADMIN=2`, `OWNER=3`. Resolved by `src/utils/permissionChecker.js` → checks `BOT_OWNER_ID` env var, then `PermissionFlagsBits.Administrator`, then presence in `GuildSettings.modRoleIds`.

### Data Layer

- **Prisma** (`src/database/client.js`) is the authoritative store. One singleton exported as default.
- **Redis** (`src/services/redis.js`) is a write-through cache only — never the source of truth. Use `cache.get/set/del/invalidate`. Never use `KEYS`; `invalidate(pattern)` uses `SCAN`.
- Guild settings cache key: `guild:{guildId}:settings`, TTL 5 minutes. **Always call `cache.del(cacheKey)` after mutating GuildSettings.**
- XP cooldown key: `xp:cooldown:{guildId}:{userId}`, TTL = `GuildSettings.xpCooldown` seconds.

### Adding a New Module

1. Create commands in `src/commands/<category>/command-name.js`
2. Create event hooks in `src/events/` (or add call sites to existing stubs in `messageCreate.js`, `guildMemberAdd.js`, etc.)
3. Add any new DB fields to `prisma/schema.prisma` and run `npm run db:migrate`
4. Add the module toggle boolean to `GuildSettings` and its default to `DEFAULT_MODULE_SETTINGS` in `src/config/constants.js`
5. Register button/select/modal handlers via `client.*Handlers.set(...)` in the command or a dedicated service file

### Schema Notes

- `Infraction.caseId` is per-guild (not global) — enforced by `@@unique([guildId, caseId])`. To generate the next case ID for a guild, query `MAX(caseId) WHERE guildId = ?` and increment.
- `ReactionRoleItem.customId` maps directly to the button's `custom_id`; format it as `rr:{groupId}:{roleId}`.
- `GuildSettings` uses `String[]` (Postgres array) for `modRoleIds`, `autoroleIds`, `noXpRoleIds`, `noXpChannelIds`.
- `LevelRole` maps `(guildId, level)` → `roleId` with `@@unique([guildId, level])` — one role reward per level per guild.
- `Infraction` has a foreign key to `GuildSettings` via `guildId`. Always ensure a `GuildSettings` row exists before creating infractions. `createInfraction()` handles this automatically with an upsert, and `ready.js` upserts settings for all guilds on startup.

### XP / Leveling (Module E)

XP formula: `xpForLevel(level) = 5 × level² + 50 × level + 100` (defined in `constants.js`). This is the XP required to advance *from* a given level.

Helper functions in `src/services/xpService.js`:
- `levelFromXp(totalXp)` → current level for a given accumulated XP total
- `xpIntoLevel(totalXp)` → XP accumulated within the current level (0 to `xpForLevel(level) - 1`)
- `grantXp(message, client)` → called from `messageCreate`; handles cooldown, exemptions, variance, upsert, level-up notification, role rewards
- `handleVoiceXp(oldState, newState, client)` → called from `voiceStateUpdate`; uses Redis key `voice:start:{guildId}:{userId}` to track join time

Redis keys:
- `xp:cooldown:{guildId}:{userId}` — TTL = `GuildSettings.xpCooldown` (default 60s)
- `voice:start:{guildId}:{userId}` — set on voice join, deleted on leave; 24h TTL as safety fallback

### Custom Commands & Triggers (Module F)

**Tags** (`CustomTag` model, `src/commands/utility/tag.js`): server-scoped named text/embed responses invoked by any member via `/tag use <name>`. Tag names are normalised to lowercase + hyphens on creation. The `useEmbed` field controls whether content renders as an embed description or plain text.

**Auto-triggers** (`AutoTrigger` model, `src/services/tagService.js`): run against every non-bot message when `customCommandsEnabled` is `true`. Only the first matching trigger fires per message. Match types: CONTAINS, EXACT, STARTS_WITH, ENDS_WITH, REGEX. Active triggers are cached per guild at `guild:{guildId}:triggers` (TTL 5min); always call `invalidateTriggerCache(guildId)` after create/update/delete.

Redis keys added:
- `guild:{guildId}:triggers` — cached `AutoTrigger[]`, TTL 5min
- `trigger:cd:{guildId}:{triggerId}:{userId}` — per-user cooldown, TTL = `AutoTrigger.cooldown`

### Moderation Service (`src/services/moderationService.js`)

Key exports:
- `createInfraction(guild, target, moderator, type, opts)` — creates a DB record atomically (sequential caseId), sends a DM to the target, posts to the mod log channel, and runs escalation checks on warns. Always upserts `GuildSettings` first to avoid FK violations.
- `resolveInfractions(guildId, userId, types)` — marks matching active infractions inactive. Used by `/unban` and `/unmute`.
- `syncGuildBans(guild, botClientId)` — fetches Discord's ban list and imports any bans that have no active `BAN` infraction in the DB as `[Imported]` records. Called at startup (`ready.js`) and on `guildCreate` so pre-bot bans are visible in history.
- `sendModLog(guild, infraction, target, moderator, client)` — posts a mod log embed to `GuildSettings.modLogChannelId`.
- `assertHierarchy(guild, targetMember, executorMember)` — throws if role hierarchy would block the action.

### Moderation Command Conventions

- **Ephemeral pattern:** Moderation action commands (`warn`, `kick`, `ban`, `unban`, `softban`, `timeout`, `unmute`) use `deferReply({ ephemeral: true })` so the "thinking" state is private. On success, call `interaction.deleteReply()` then `interaction.followUp({ embeds: [...] })` to post publicly. Error paths use `editReply` which stays ephemeral.
- **Lookup commands** (`case`, `history`) also post publicly using the same `deleteReply + followUp` pattern.
- **Pre-ban check:** `/ban` fetches the existing ban before creating any DB records to avoid orphaned infractions when a user is already banned.
- **`/purge` timeframe mode:** When `timeframe` is provided, `fetchInTimeframe()` paginates through the channel in batches of 100 until the cutoff timestamp is reached, then deletes in chunks via `bulkDeleteAll()`. Discord's 14-day bulk-delete limit is enforced. `amount` can be combined with `timeframe` as a cap.
- **`/timeout` duration:** Free-form text field (e.g. `5m`, `2h`, `7d`) parsed by `parseDuration()` — not a dropdown. Max 28 days enforced in code.

### Admin Command Conventions

All admin commands (`levelconfig`, `welcome`, `trigger`, `reactionrole`) use `deferReply()` (no ephemeral flag) so all responses post publicly in the channel.

### Welcome Card (`src/utils/welcomeCard.js`)

- Requires `assets/fonts/Nunito-Bold.ttf` to be present — without it `@napi-rs/canvas` renders blank text silently. The file is **not** auto-downloaded; ensure it exists after cloning.
- Card displays: `member.displayName` (server nickname > username) in white, `@member.user.username` in grey, member count in green.
- Canvas size: 700×250px. Falls back gracefully on avatar load failure (grey circle).

### Embed Conventions

All user-facing embeds must use the factory functions from `src/utils/embedBuilder.js` (`successEmbed`, `errorEmbed`, `infoEmbed`, `warnEmbed`). These enforce the color palette and timestamp. They return an `EmbedBuilder` — pass it to `interaction.reply({ embeds: [...] })`.
