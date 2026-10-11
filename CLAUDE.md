# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

A self-hosted, MEE6-style multi-purpose Discord bot: discord.js v14 (ESM, Node 22+), Prisma 5.22 on PostgreSQL 12 or newer, Redis as a cache only, PM2 or Docker. Every feature is a module admins switch per server with `/module` (`README.md` has the feature and command list).

## Development Commands

```bash
npm run dev          # Start bot with nodemon (auto-restart on file changes)
npm start            # Start bot with node directly
npm test             # node --test "test/*.test.js": offline (no Discord, DB, Redis, network)
npm run deploy       # Force-register slash commands via Discord REST API, then exit
                     #   DEPLOY_GUILD_ID set: instant guild-scoped; omitted: global (up to 1 hour)
                     #   Commands are also auto-deployed on startup when the set has changed

npx prisma generate  # after every schema edit (tests import @prisma/client)
npm run db:migrate   # Create and apply a NEW additive migration
npm run db:push      # Scratch databases only (production: `prisma migrate deploy`)
npm run db:studio    # Open Prisma Studio GUI to inspect/edit data
npm run db:seed      # Insert a minimal GuildSettings row for local testing (TEST_GUILD_ID)

npm run pm2:start    # Launch under PM2 (production)
npm run pm2:reload   # Zero-downtime reload
npm run pm2:logs     # Tail PM2 logs
```

## Environment

Required `.env` keys before the bot will start: `DISCORD_TOKEN`, `CLIENT_ID`, `DATABASE_URL`, `REDIS_URL`. Copy `.env.example`. If any is missing the process parks (stays up, `/readyz` returns 503 `parked:missing-env`) instead of exiting. Every other key is optional and commented out in `.env.example` (an empty value in a Docker env file overrides the image's defaults); a new `process.env` read must be added there (a test checks).

| Key | Default | Purpose |
|---|---|---|
| `HEALTH_PORT`, `STATE_DIR`, `DEPLOY_HASH_FILE` | 8081, `./state`, `./.deploy-hash` | health endpoints; restart-brake history; command hash |
| `DEPLOY_GUILD_ID`, `BOT_OWNER_ID` | global, none | instant guild-scoped deployment; user with the OWNER level |
| `NODE_ENV`, `PRISMA_LOG_QUERIES` | none, off | `production` hides debug logs; `1` logs every Prisma query |
| `TRANSLATE_PROVIDER`, `LIBRETRANSLATE_URL`, `LIBRETRANSLATE_API_KEY` | `google`, `http://localhost:5000`, none | `google` or `libretranslate` (anything else warns, uses google) |
| `HTTP_CONTACT` | `n/a` | contact in the User-Agent of every `safeFetch` request |
| `REDDIT_USER_AGENT`, `BSKY_APPVIEW_URL` | `DEFAULT_USER_AGENT`, `https://public.api.bsky.app` | Reddit feed User-Agent; Bluesky AppView |
| `TEST_GUILD_ID`, `TEST_DATABASE_URL` | 0, none | `db:seed` guild; opt-in DB tests |

## Architecture

### Entry Flow

`index.js` runs `autoDeployCommands()`, then the `ShardingManager` spawns `src/bot.js` as a worker thread (`totalShards: 'auto'`, so Discord controls the shard count). `bot.js` creates the `Client` and boots in this order: `collectPlugins` → `installPlugins` → `registerCoreComponents` → `loadCommands` → `loadEvents` → `client.login`. On `ready` the scheduler starts, then `runGuildReady` runs the `guildReady` hooks for every available guild (concurrency 4; it ensures the `GuildSettings` row first); `guildCreate` and `guildAvailable` run it too. `guildDelete` clears the guild's Redis keys and cancels its recurring jobs; rows are kept so a re-invite restores everything.

`index.js --deploy` short-circuits the boot sequence, calls `deployCommands()`, and exits.

**Process supervision (main thread, `index.js`):**
- A `node:http` health server (`src/utils/healthServer.js`) starts first: `/healthz` = event loop alive, `/readyz` = every shard `isReady()`.
- The restart brake (`src/utils/restartBrake.js`) records each start in `STATE_DIR`; more than 5 starts per hour delays the next login (2^(n-5) min, max 30).
- The ShardingManager uses `respawn: false`. A dead shard, or one disconnected for good (non-fatal unrecoverable close such as 4011), exits the process; Docker's restart policy and the brake handle retries.
- Close codes 4004/4013/4014 are fatal (`src/utils/fatalGateway.js`): the shard reports them via `client.shard.send()` and the main thread parks instead of retrying.
- Because a dead shard restarts the whole bot, per-event failures must not kill the worker: `eventLoader.js` catches and logs errors from every event handler, and `bot.js` logs client `error` events and unhandled rejections in the shard (e.g. from collector callbacks). Uncaught synchronous exceptions still end the shard.
- `SIGTERM`/`SIGINT` are handled only in `index.js`: each shard runs `client.shutdown()` (defined in `bot.js`: stops the scheduler, flushes the audit-log queue, `client.destroy()`, closes Prisma and Redis), then the process exits within 25 s. Don't add signal handlers elsewhere.

**Auto-deploy:** `autoDeployCommands()` in `src/handlers/commandLoader.js` hashes `CLIENT_ID`, `DEPLOY_GUILD_ID` and all command payloads and compares against `DEPLOY_HASH_FILE`. It only calls Discord when the hash changes; adding a command file and restarting is all that is needed. `buildCommandPayload` registers commands for servers only (`contexts: [0]`, `integration_types: [0]`) unless the command exports `allowInDMs = true`.

### Extension points: the file-system registry

Three directories are auto-discovered; **never edit `commandLoader.js`, `eventLoader.js` or `pluginLoader.js` when adding a feature.**

**Commands** (`src/commands/<category>/*.js`; `_`-prefixed files are skipped) export:

- `data`, `execute(interaction, client)`: required.
- `module`: **required, explicit**: a `MODULES` key or `null` (core command).
- `gated` (default `true`): refused while the module is off. ADMIN config commands set `false` (admins can set a disabled module up); only `/embed` is gated.
- `permissionLevel` (default MEMBER) and `subcommandPermissions` `{ 'sub': level, 'group sub': level }` (a key that is not a real subcommand is a bug; tests check).
- `allowInDMs`: `true` only for DM-safe commands; all others fail closed outside a cached guild. `autocomplete`: gated exactly like `execute`; a refusal answers an empty list.

ADMIN commands call `setDefaultMemberPermissions(ManageGuild)` (Discord hides them; our level check is separate). At most 100 global commands (76 today), each under 4000 payload characters (tests enforce it): use subcommands, and autocomplete instead of `choices` for lists over 25.

**Events** (`src/events/*.js`, `{ name, once, execute }`; the client is injected last) are thin dispatchers: pre-filter, then `await runHooks('<event>', { client, guild, ...fields })`. **Never put feature logic in `src/events/`.** They may import only `discord.js`, `hookRegistry`, `settingsService`, `scheduler`, `redis`, `config/jobTypes`, `utils/async`, `utils/logger` (tested). Besides gateway events there are the synthetic `guildReady` and the internal `levelUp` and `economyPurchase` points. `messageCreate` passes only real user messages (no bots, webhooks or system messages).

**Plugins** (`src/plugins/<id>.js`): `export const plugin = { id, module, hooks, components, jobs, provides, init }`. `id` equals the file name; `module` is a `MODULES` key or `null`; importing has no side effects (`init(client)` is synchronous, run at install). `hooks`: `{ event, id, run(ctx), filter?, module?, always?, timeoutMs? }`; `components`: `{ type: 'button'|'select'|'modal', prefix, handler(interaction, client), module? }`; `jobs`: `{ type, handler(job, ctx) }`; `provides`: `{ xp?, coins? }` each `{ grant, take }`. `collectPlugins()` validates (ids and prefixes owned by this plugin, ids in `HOOK_ORDER`, no duplicates; an offending entry is dropped); `installPlugins()` registers hooks, components (module-gated), job handlers, reward providers, then `init`. Each plugin is named in its module's section; search has none.

### Hook points

`src/config/hookPoints.js` (`HOOK_ORDER`: event → ordered hook ids `<pluginId>` or `<pluginId>:<name>`) is the order of truth; add a hook's id there first. `runHooks(event, fields)` (never throws) runs each hook: sync `filter` (false or throwing skips) → module gate (skipped for `always: true` and `module: null`; no settings counts as disabled) → `run(ctx)` with a soft timeout (10 s default, or `timeoutMs`). A failing or timed-out hook is logged; the next still runs.
- `ctx`: `event`, `client`, `guild`, `guildId`, `state` (shared scratch; keys in use: `invite`, `captchaPending`, `customCommand`, `automodAction`), `stop(reason)`, `stopped`, `getSettings()` (memoised, never throws), plus the event's fields. **Only automod and custom-commands may call `ctx.stop()`** (a test enforces it).
- Internal points: `levelUp` (from `xpService.js` and `/xp give`: `member` may be null, `userId`, `oldLevel`, `newLevel`, `channel`, `source`) and `economyPurchase` (`/shop buy` only: `member`, `userId`, `item`, `quantity`, `totalPrice`, `channel`).

### Interaction routing and the customId registry

For a slash command or autocomplete the router applies one gate (`checkCommandAccess`): guild guard (fails closed unless `allowInDMs`) → module toggle (gated commands of disabled modules refused; unreadable settings answer "Temporarily Unavailable") → permission level (`subcommandPermissions['group sub']` beats `['sub']` beats `permissionLevel`) → `execute`. Settings come from the memo or a bounded load (3 s ack window).
- Components route by prefix to `client.buttonHandlers`/`selectHandlers`/`modalHandlers` (`HandlerMap`: first registration wins). A `collector` prefix with no live collector gets "expired" after 2.5 s.
- **`src/config/customIds.js`** (`CUSTOM_ID_PREFIXES`) lists every prefix with `owner`, `types` and `kind`: `persistent` (registered by a plugin or `registerCoreComponents`, survives restarts), `collector` (only a live collector answers) or `field` (modal input id, no params). `buildCustomId(prefix, ...params)`/`parseCustomId` use `prefix:param1:param2` (params never contain `:`, max 100 characters). **To add a prefix, edit `customIds.js`**; tests reject unknown prefixes and persistent prefixes without exactly one handler. Register persistent components only through a plugin's `components`, never `client.*Handlers.set` in a command.
- Persistent components re-check permission levels themselves (the router does not); collector handlers acknowledge first.

### Scheduler (`src/services/scheduler.js`)

Durable timed work in the `ScheduledJob` table. `JOB_TYPES` (`src/config/jobTypes.js`) is the registry; an unknown type throws.
- `scheduleJob({ guildId, type, runAt, payload, dedupeKey, maxAttempts = 5, onConflict })` calls `ensureGuildSettings`; `onConflict` `replace` (default), `keep` or `earliest` applies when the dedupe key has a live row. `cancelJobs({ dedupeKey | guildId | type | types | ids })`, `getJob(dedupeKey)`.
- Handlers `async (job, ctx) => void | { nextRunAt, payload? }` (`ctx = { client, guild, lateByMs, attempt }`) must be idempotent (a claim can repeat after a crash). Return `{ nextRunAt }` to reschedule; throw `PermanentJobError` to fail without retry; other errors retry with back-off 30 s × 2^n (max 1 h) up to `maxAttempts`.
- Every ~5 s each shard claims up to 25 due jobs (`FOR UPDATE SKIP LOCKED`) for its own available guilds: no shard-0 poller, no Redis lock. A job with no handler waits.
- **Never use `setInterval`** (tested); use a recurring job. `guildDelete` cancels `RECURRING_JOB_TYPES`; `guildReady` hooks re-seed them.

**Job types** (`src/config/jobTypes.js`):

| Type | Owner plugin | Recurring | Dedupe key |
|---|---|---|---|
| `mod.unban` | moderation | no | `mod.unban:{guildId}:{userId}` |
| `mod.expire` | moderation | no | `mod.expire:{infractionId}` |
| `welcome.autorole` | welcome | no | `welcome.autorole:{guildId}:{userId}` |
| `captcha.timeout` | captcha | no | `captcha.timeout:{guildId}:{userId}` |
| `reminder.fire` | reminders | yes | `reminder:{reminderId}` |
| `giveaway.end` | giveaways | no | `giveaway.end:{giveawayId}` |
| `poll.end` | polls | no | `poll.end:{pollId}` |
| `tempvoice.cleanup` | temp-voice | no | `tempvoice.cleanup:{channelId}` |
| `stats.refresh` | stat-channels | yes | `stats.refresh:{guildId}` |
| `birthday.tick` | birthdays | yes | `birthday.tick:{guildId}` |
| `feed.poll` | feeds | yes | `feed.poll:{feedId}` |
| `levels.voiceSweep` | levels | yes | `levels.voiceSweep:{guildId}` |

### Rewards bus (`src/services/rewards.js`)

XP and coins are granted through providers, so giveaways, achievements and automations never import levels or economy (`xp`: levels plugin, `coins`: economy plugin). `grant(kind, { guild, userId, amount, reason, member, channel })` / `take(...)` never throw and return `{ ok, total?, reason? }` (`reason`: `invalid-amount` unless an integer 1 to 1e9, `unavailable`, `module-disabled`, `error`); `isRewardAvailable(kind, settings)` is sync.

### Modules and `/module`

`MODULES` in `src/config/modules.js` is the single registry (`MODULE_FLAGS` is the legacy alias). The router, hook runner, `/module` and `/help` read it. `isModuleEnabled(settings, key)`: `null` key → on; unknown key or no settings → off; else the flag must be exactly `true`. `/module list|enable|disable` (ADMIN, core) toggles flags.

| Key | `GuildSettings` flag | Default | Label | Config command |
|---|---|---|---|---|
| `moderation` | `modEnabled` | on | 🔨 Moderation | `/modconfig` |
| `automod` | `automodEnabled` | off | 🤖 Auto-Moderation | `/automod` |
| `logging` | `loggingEnabled` | off | 📜 Audit Logs | `/logging` |
| `welcome` | `welcomeEnabled` | off | 👋 Welcome & Goodbye | `/welcome` |
| `captcha` | `captchaEnabled` | off | 🛡️ Verification | `/captcha` |
| `reactionroles` | `reactionRolesEnabled` | on | 🎭 Reaction Roles | `/reactionrole` |
| `leveling` | `xpEnabled` | off | ⭐ Levels | `/levelconfig` |
| `customcommands` | `customCommandsEnabled` | off | 🏷️ Custom Commands | `/tag` |
| `automations` | `automationsEnabled` | off | ⚙️ Automations | `/automation` |
| `translation` | `translationEnabled` | off | 🌐 Auto-Translate | `/autotranslate` |
| `invites` | `invitesEnabled` | on | 📨 Invite Tracker | `/invites` |
| `embeds` | `embedsEnabled` | on | 🧱 Embed Messages | `/embed` |
| `reminders` | `remindersEnabled` | on | ⏰ Reminders | `/reminder` |
| `giveaways` | `giveawaysEnabled` | on | 🎉 Giveaways | `/giveaway` |
| `polls` | `pollsEnabled` | on | 📊 Polls | `/poll` |
| `starboard` | `starboardEnabled` | on | 🌟 Starboards | `/starboard` |
| `tickets` | `ticketsEnabled` | on | 🎫 Ticketing | `/ticket-panel` |
| `tempvoice` | `tempVoiceEnabled` | on | 🔊 Temporary Channels | `/voice-hub` |
| `stats` | `statsEnabled` | on | 📈 Statistics Channels | `/stats` |
| `birthdays` | `birthdaysEnabled` | on | 🎂 Birthdays | `/birthday-config` |
| `achievements` | `achievementsEnabled` | off | 🏆 Achievements | `/achievement-config` |
| `economy` | `economyEnabled` | on | 🪙 Economy | `/economy-admin` |
| `games` | `gamesEnabled` | on | 🎮 Games | `/games` |
| `feeds` | `feedsEnabled` | on | 📰 Social Feeds | `/feed` |
| `search` | `searchEnabled` | on | 🔎 Search Anything | `/settings` |
| `liverole` | `liveRoleEnabled` | off | 🔴 Live Role | `/liverole` |

`levelingEnabled` is deprecated and never read (`xpEnabled` is used).

**Adding a module:** (1) boolean flag on `GuildSettings` + migration; (2) `DEFAULT_MODULE_SETTINGS` entry in `src/config/constants.js`; (3) `MODULES` entry; (4) commands with an explicit `module`; (5) `src/plugins/<id>.js`, its hook ids in `HOOK_ORDER`, prefixes in `customIds.js`, job types in `jobTypes.js`; (6) README command-table rows, a section here, `.env.example` keys. `npm test` fails until all agree.

### Permission levels
`PERMISSION_LEVELS` (`src/config/constants.js`): `MEMBER=0`, `MODERATOR=1`, `ADMIN=2`, `OWNER=3`. `resolvePermissionLevel(member, settings)` (`src/utils/permissionChecker.js`): `BOT_OWNER_ID` → OWNER; guild owner, Administrator or Manage Server → ADMIN; a `modRoleIds` role or Kick/Ban/Moderate Members → MODERATOR.

### Settings (`src/services/settingsService.js`)

**The only way to read or write `GuildSettings`** (never `prisma.guildSettings.*` elsewhere; never `cache.del('guild:…:settings')`; tests enforce both). Rows are cached in Redis (300 s, versioned by `_cacheV`), memoised per shard for 30 s and deep-frozen (copy arrays before editing).
- `getSettings(guildId, { fresh })` memo → Redis → Postgres (row created on first use; throws only on Postgres failure); `getSettingsSafe` returns `null` instead; `peekSettings` is memo-only and synchronous.
- `ensureGuildSettings(guildId)`: **call before creating any per-guild row** (every per-guild model has an FK to it).
- `updateSettings(guildId, data)` upserts scalars with write-through; `updateSettingsWith(guildId, mutator)` is a locked (`FOR UPDATE`) read-modify-write for arrays and JSON, the mutator being pure; `invalidateSettings` is for guild join/leave only.
- A cache miss is never a deletion: if Redis or a lookup fails, keep the data and degrade.

### Data layer

- **Prisma** (`src/database/client.js`): one singleton; query logging only with `PRISMA_LOG_QUERIES=1`. Int columns are 32-bit: clamp with `XP_MAX`/`COIN_MAX`/`economyMaxBalance`; SQL maths uses `::bigint`. Raw SQL uses `SQL_NOW`/`sqlUtc(date)` (UTC, like Prisma `DateTime`; never bare `now()`), and a service exports its statements as `RAW_SQL = [{ name, build }]` so `test/db.test.js` can run them.
- **Redis** is a cache only, never the source of truth.

**`cache` API** (`src/services/redis.js`): never throws; errors are logged and a fallback returned (`get` -> `null`, `set`/`del` -> `false`, `incr`/`slidingWindow` -> `null`, `invalidate` -> `0`). `setNX` returns `true`/`false`/`null` (Redis down): fail-open callers test `!== false`, fail-closed `!== true`. Always pass a TTL; commands time out after 750 ms; `slidingWindow(key, windowMs, member?)` counts hits in a ZSET window. `{g}` = guild id; owners without a path are in `src/services/`. `guildDelete` purges `guild:{g}:*`, `xp:cooldown:{g}:*`, `voice:start:{g}:*`. Dotted names such as `feed.poll:` are `ScheduledJob` dedupe keys, not Redis.

| Key pattern | Owner | TTL | Purpose |
|---|---|---|---|
| `guild:{g}:settings` | settingsService | 300 s | row + `_cacheV` marker (`SETTINGS_CACHE_VERSION` 2; other versions are misses); 30 s memo |
| `guild:{g}:embed:{name}` | embedService | 300 s (miss 60 s) | saved embed |
| `guild:{g}:achievements`, `:automations`, `:rrpanels`, `:starboards`, `:triggers`, `:translateChannels` | achievement, automation, reactionRole, starboard, tag, translation services | 300 s | per-guild config caches |
| `guild:{g}:invites` | inviteTracker | 7 d | invite use-count snapshot |
| `xp:cooldown:{g}:{userId}` | xpService | `xpCooldown` s | fail-closed |
| `voice:start:{g}:{userId}` | xpService | 26 h | open voice session; fail-closed |
| `tag:cd:{g}:{tagId}[:{userId}]`, `trigger:cd:{g}:{triggerId}:{userId}` | tagService | cooldown | fail-closed |
| `automation:cd:{g}:{automationId}:{userId}`, `automation:repost:{automationId}:{messageId}`, `automation:problem:{automationId}` | automationService | cooldown / 30 d / 7 d | cooldown (in-process fallback), repost once, last error |
| `automod:dup:{g}:{userId}:{sha16}`, `automod:rate:{g}:{userId}` | automodService | 30 s / 5 s | fail-open |
| `captcha:img:{g}[:{userId}]`, `captcha:dm:{g}` | captchaService | 60 s / 600 s | image and kick-DM caps; fail-open |
| `welcome:rate:{g}:{kind}` | welcomeService | 60 s | flood caps; fail-open |
| `rr:rate:{g}:{userId}` | reactionRoleService | 10 s | click limit |
| `feed:fetch:{sha1(url)}`, `feed:rl:reddit:{slot}`, `feed:itunes:rl`, `feed:block:reddit` | feedService | interval - 30 s / 60 s / 60 s / 10-60 min | parsed feed, Reddit slot budget, podcast search cap, Reddit 429 block |
| `search:{api}:{sha1}`, `search:pokeapi:index`, `search:{api}:pause`, `search:budget:{api}:{minute}`, `search:cd:{userId}` | searchService | 6 h (PokeAPI 7 d, empty 1 h) / pause / 120 s / 3 s | cache, 429 pause, budget, cooldown; fail-open |
| `tr:{provider}:{target}:{sha1}`, `translate:{provider}:pausedUntil` | translationService | 1 h / pause length | result cache, 429 pause |
| `log:timeout:{g}:{userId}:{untilSec}` | `src/plugins/logging.js` | to timeout end | de-dupes timeout logs; fail-open |
| `mod:bansync:{g}` | `src/plugins/moderation.js` | 6 h | ban import throttle |
| `invites:link:cd:{g}:{userId}` | `src/commands/utility/invites.js` | 60 s | `/invites link` cooldown |
| `ach:react:{g}:{userId}:{messageId}:{emojiKey}`, `ach:boost:{g}:{userId}:{premiumSince}` | achievementService | 24 h | count once |
| `birthday:announced:{g}:{userId}` | birthdayService | to year end + 1 d | announced-year tombstone |
| `tempvoice:rename:{channelId}` | tempVoiceService | 10 min | rename stamps |

### Schema notes

Every model with a `guildId` has an FK to `GuildSettings(guildId)` with `onDelete: Cascade` (leaving a guild keeps its rows); children without `guildId` cascade from their parent.

- `GuildSettings`: one row per guild, parent of all; use `settingsService` only. One boolean flag per `MODULES` entry (`autoTriggersEnabled` is a `/trigger` sub-switch; `levelingEnabled` and `xpRate` are legacy), plus `timezone`, `embedColor`, `customPrefix`, and JSON such as `automodConfig` and `infractionEscalation`. `String[]` fields change via `updateSettingsWith` (row lock), not `updateSettings`.
- `ScheduledJob`: `dedupeKey` unique globally (`scheduleJob` upserts), `version` bumped on reschedule, lease `lockedBy`/`lockedUntil`.
- `Infraction`: `caseId` is per guild, `@@unique([guildId, caseId])`, allocated under `pg_advisory_xact_lock`, never MAX+1.
- `UserLevel` `[guildId, userId]` (`xp` clamped to `XP_MAX`); `LevelRole` `[guildId, level]`: one reward role per level.
- `UserEconomy`: legacy global wallet by `userId`, never written, only read by `getAccount`/`peekAccount`. `EconomyAccount` is per server: `getAccount` creates it lazily and copies the legacy balance once (`importedGlobal`) unless `economyImportGlobal` is false.

**Schema-change protocol** (after this branch merges): edit `prisma/schema.prisma` and `npx prisma generate`; `npm run db:migrate -- --name <change>` on a scratch database and commit the migration with the schema; **additive only** (never drop or rename columns/tables or change types: rolling back the image does not roll back the database); an enum `ADD VALUE` is not usable as a default or in data statements of the same migration; data fixes are hand-written `UPDATE`s below the generated DDL; CI applies every migration to an empty PostgreSQL 17 and fails on drift; never edit an applied migration. Until `20261010120000_mee6_parity` is deployed, schema changes on branch `mee6-parity` regenerate that single migration instead of adding a second one.

### Safe utilities (`src/utils/`)

| Module | Exports worth knowing | Use when |
|---|---|---|
| `placeholderParser.js` | `renderPlaceholders(tpl, ctx)` (`ctx` = `{member, user, guild, channel, vars, args}`): MEE6 tokens, args `{1}` `{1:default}`, extra tokens via `ctx.vars`, unknown tokens stay literal | any admin-written text |
| `customEmbed.js` | MessageTemplate `{content, embed, buttons}`: `validateTemplate`, `renderTemplate(tpl, ctx, opts)` (payload or `null`), `buildTemplateModal`/`parseTemplateModal` (`tpl-*` inputs); its `LIMITS` are message sizes, not the per-guild table | configurable messages |
| `safeRegex.js` | `validateRegex`, `safeRegexTest` (vm timeout; a timeout quarantines the regex), `containsWord` (Unicode whole word) | any user-supplied pattern |
| `safeFetch.js` | `safeFetch`, `safeFetchJson/Text/Image`, `SafeFetchError`, `DEFAULT_USER_AGENT` (includes `HTTP_CONTACT`) | any request to a user-influenced URL (SSRF-safe, limits) |
| `interactionHelpers.js` | `safeDefer`, `safeReply`, `replyError`, `requireBotPermissions`, `checkChannelUsable`, `checkAssignableRole`, `pingableRoleIds`, `respondAutocomplete`, `awaitConfirm` | replies, role/channel validation, autocomplete, confirms |
| `async.js` | `withTimeout`, `ackBudgetMs`, `mapLimit`, `createKeyedMutex`, `createThrottle` | lookups before an ack, bounded fan-out, per-key serialisation |
| `embedBuilder.js`, `paginator.js` | `successEmbed`, `errorEmbed`, `infoEmbed`, `warnEmbed`, `neutralEmbed`, `applyGuildColor`, `guildEmbed`; `paginate(target, pages, opts)` (ids `paginator:prev|next|stop`) | every embed (guild colour on info/neutral only) |
| `time.js`, `parseDuration.js`, `text.js` | `zonedToUtc`, `nextTimeOfDay`, `nextIntervalRun`, `parseDateInput`, `discordTimestamp`; `parseDuration` (ms or `null`), `formatDuration`; `clampText`, `chunkLines`, `formatNumber`, `progressBar` | guild-`timezone` maths, durations, text clamping |
| `deletionTracker.js`, `guildMembers.js`, `imageSize.js` | `markBotDeletion`/`wasDeletedByBot` (before the bot deletes, so delete logs skip it); `fetchAllMembers` -> `{ok, members}` or `{ok:false, reason}`; `assertImageDimensions` (throws) | bot deletions, full member fetches, before `loadImage` of user bytes |
| `welcomeCard.js`, `profileCard.js` | `generateWelcomeCard`, `generateProfileCard` | canvas PNGs; need the bundled `assets/fonts/Nunito-Bold.ttf`: the Docker image has no system fonts, so a missing file means blank text |

### Global conventions

- **Mentions:** the client default is `allowedMentions: { parse: ['users'], repliedUser: false }`; never parse `@everyone`; roles only when an admin configured a ping role, filtered by `pingableRoleIds`.
- **Embeds:** bot text uses the `embedBuilder` factories; admin-authored templates use `customEmbed`, clamped after placeholder expansion. Keep `{ ephemeral: true }` (not `MessageFlags.Ephemeral`).
- **Acknowledge within 3 s:** bound lookups before `deferReply`/`showModal` with `ackBudgetMs(interaction)` and `peekSettings`; collectors acknowledge first.
- **Canvas:** welcome card 700×250 (`welcomeCardStyle`), rank card 900×280 (`rankCardStyle`); both need the committed `assets/fonts/Nunito-Bold.ttf` (the image has no system fonts, a missing file renders blank text).
- **Limits:** per-guild caps are `LIMITS` in `src/config/constants.js`; never hard-code (`XP_MAX`, `COIN_MAX` clamp XP and coins). **Roles:** `checkAssignableRole`, then add/remove one at a time (never `roles.add([...])` or `roles.set`). User images: `assertImageDimensions`; full member lists: `fetchAllMembers`; autocomplete: filter in the database, return 25 via `respondAutocomplete`.
- **Moderation commands** `deferReply({ ephemeral: true })`, then on success `deleteReply()` + public `followUp()` (subject to `modVisibility`); errors `editReply` (`/purge` always replies privately). **Admin config commands** use a public `deferReply()`; exceptions: `/settings` validation errors and `/automod words|patterns` replies are ephemeral.
- **Timers and collectors catch everything** (a stray rejection must not kill the shard). `logger` only, never `console`. Check bot permissions before Discord writes and answer friendly. REST codes 10003, 10008, 10011, 50001, 50013 (unknown channel/message/role, missing access/permission) mean "gone": degrade and clean up, do not retry.

## Feature modules

One subsection per module in `MODULE_NAMES` order, then the core commands. Hooks are written `event:hookId`; Redis keys are in the table above.

### 🔨 Moderation — `moderation`
- **Commands:** `/ban /kick /softban /timeout /unmute /unban /warn /case /history /purge /slowmode` MODERATOR; `/history clear` ADMIN; `/modconfig` ADMIN.
- **Code:** `src/plugins/moderation.js`, `src/services/moderationService.js`. `executeModAction` is the only punishment path (`source` in `MOD_SOURCES`): never throws, Discord action first, a failure records nothing. `createInfraction` only records (still DMs, logs, escalates WARN). `isImmune`; `sendModLog` ignores `loggingEnabled`.
- **Rules:** `/timeout` max 28 d (longer = mute role, else refused); TEMPBAN = BAN + duration. `/purge` always replies privately and never auto-deletes; `/purge` and `/slowmode` default to ModerateMembers.
- **Jobs:** `mod.unban` (`mod.unban:{guildId}:{userId}`), `mod.expire` (`mod.expire:{infractionId}`); run with the module off.
- **Hooks:** `guildReady:moderation` runs `syncGuildBans` (also guildCreate; Redis `mod:bansync:{guildId}`, 6 h).

### 🤖 Auto-Moderation — `automod`
- **Commands:** `/automod` ADMIN; `words`/`patterns` replies ephemeral.
- **Code:** `src/plugins/automod.js`, `src/services/automodService.js`. Rules: badWords repeatedText invites links caps emojis spoilers mentions zalgo spam. Actions: off delete warn delete_warn timeout kick ban (punishments via `executeModAction`). `automodConfig` v2 top level: `version rules badWords badWordPatterns blockedDomains allowedDomains exemptRoleIds exemptChannelIds`. Staff bypass: `isImmune`, MODERATOR, Manage Messages.
- **Hooks:** `messageCreate:automod` (first), `messageUpdate:automod`; after a delete: `ctx.stop('automod')`, `ctx.state.automodAction = { rule, deleted }`. A failed delete never punishes.

### 📜 Audit Logs — `logging`
- **Commands:** `/logging` ADMIN.
- **Code:** `src/plugins/logging.js` (26 hooks, 32 `LOG_EVENTS`), `src/services/loggingService.js`, `src/services/logEvents.js`, `src/utils/deletionTracker.js`.
- **Events:** message, member (incl. `userUpdate`), voice, channel, thread, role, emoji, invite, server.
- **Sending:** `sendLog(guild, logType, embed, opts)`, `opts { event?, settings?, files? }`; `LOG_TYPES` MOD ignores `loggingEnabled`. Batched ~1 s; `flushLogQueue()` at shutdown.
- **Gotchas:** call `markBotDeletion(id, reason)` before bot deletions (read via `wasDeletedByBot`); `logIgnoredChannelIds` mutes message events only.

### 👋 Welcome & Goodbye — `welcome`
- **Commands:** `/welcome`, `/goodbye`; ADMIN.
- **Code:** `src/plugins/welcome.js`, `src/services/welcomeService.js`, `src/utils/welcomeCard.js`. Modals `welcome-setup`, `welcome-dm-setup`, `goodbye-setup` (`tpl-*` fields). `welcomeCardStyle` = `{ v: 1, enabled, background, backgroundUrl, textColor, accentColor, overlayOpacity }`.
- **Autorole:** `assignAutorole(member, { force })`: `force` skips only the captcha gate, never Rules Screening. Job `welcome.autorole` (`welcome.autorole:{guildId}:{userId}`), payload `{ userId, watch?: true }`; runs with the module off.

### 🛡️ Verification — `captcha`
- **Commands:** `/captcha` ADMIN.
- **Code:** `src/plugins/captcha.js`, `src/services/captchaService.js`. Prefixes `captcha`, `captcha-modal:answer`, `captcha-modal:panel`.
- **Behaviour:** only a sha256 answer hash is stored (`timingSafeEqual`, 3 tries). Job `captcha.timeout` (`captcha.timeout:{guildId}:{userId}`) kicks via `executeModAction` (source `captcha`). Verifying then calls `assignAutorole(member, { force: true })`.

### 🎭 Reaction Roles — `reactionroles`
- **Code:** `/reactionrole`, `src/plugins/reaction-roles.js`. `RRLayout` BUTTONS, SELECT, REACTIONS (emoji reactions; `attach` binds an existing message).
- **customIds:** `rr:{groupId}:{roleId}`, `rr-select:{groupId}`; module-gated, no level check.
- **Hooks (id `reaction-roles`):** the four messageReaction* events; `always`: `messageDelete`, `roleDelete`.
- **Gotchas:** `safeReply` after `deferUpdate` follows up, so the personal select uses `editReply`.

### ⭐ Levels — `leveling`
- **Commands:** `/levelconfig`, `/xp` ADMIN; `/rank`, `/leaderboard`. `/xp give target:@Role` caps at 5,000 members.
- **Code:** `src/plugins/levels.js`, `src/services/xpService.js`, `src/utils/profileCard.js`. `xpForLevel(l) = 5l^2 + 50l + 100`; `levelFromXp`, `xpIntoLevel`; `RAW_SQL`.
- **XP:** `xpMin`/`xpMax` replace `xpRate`; cooldown `xp:cooldown:{guildId}:{userId}` fails closed. No `grantXp`/`handleVoiceXp` call sites remain.
- **Voice:** `voice:start:{guildId}:{userId}` = `{ start, channelId }`; recurring job `levels.voiceSweep` (`levels.voiceSweep:{guildId}`).
- **Hooks:** `messageCreate:levels`, `voiceStateUpdate:levels`, `roleDelete:levels` (always), `guildReady:levels`.
- **Level-up:** announce modes (`AnnounceMode`) OFF|CURRENT|DM|CHANNEL; message tokens `{player}` `{level}` `{rank}` `{xp}`; emits `levelUp` (`ctx.member` may be null); `provides.xp` reward provider (grant/take).

### 🏷️ Custom Commands — `customcommands`
- **Commands:** `/tag` (gated; writes ADMIN), `/trigger` (ADMIN).
- **Code:** `src/plugins/custom-commands.js`, `src/services/tagService.js`.
- **Prefix:** `customPrefix` (default `!`); `/settings prefix off` stores `''` (`/tag use` still works). Upgraded guilds that have tags start answering `!name`.
- **Triggers:** `autoTriggersEnabled` (`/trigger disable` only clears it); cache `guild:{guildId}:triggers` (5 min, `invalidateTriggerCache`); cooldowns `tag:cd:{guildId}:{tagId}`, `trigger:cd:{guildId}:{triggerId}:{userId}` fail closed.
- **Hooks:** `messageCreate:custom-commands` (may `ctx.stop()`), `messageCreate:custom-commands:triggers`.

### ⚙️ Automations — `automations`
- **Commands:** `/automation` ADMIN.
- **Code:** `src/plugins/automations.js`, `src/services/automationService.js`; button prefix `automation` (`automation:{automationId}:{buttonIndex}`). 8 hooks, id `automations`.
- **Loop guard:** bots never trigger message events, chain depth <=3, 30 runs/10 s per guild, cooldown `automation:cd:{guildId}:{automationId}:{userId}`.

### 🌐 Auto-Translate — `translation`
- **Commands:** `/autotranslate` ADMIN.
- **Code:** `src/plugins/translation-fixes.js` (hook last in `messageCreate`), `src/services/translationService.js`.
- **Providers:** `TRANSLATE_PROVIDER` `google` (default) or `libretranslate` (`LIBRETRANSLATE_URL`, `LIBRETRANSLATE_API_KEY`); else warns, uses google. Traditional Chinese is `zh-tw`; aliases `iw nb zh-cn zh-hant fil`; the first 1,000 characters are translated, up to 1,000 shown per language.
- **Redis:** `tr:{provider}:{target}:{sha1}` (1 h), `translate:{provider}:pausedUntil`; a 429 pauses (30 min Google, 1 min LibreTranslate).

### 📨 Invite Tracker — `invites`
- **Commands:** `/invites` (gated).
- **Code:** `src/plugins/invites.js`, `src/services/inviteTracker.js`. `guildMemberAdd:invites` is `always` and sets `ctx.state.invite`; Redis `guild:{guildId}:invites` (7 d).
- **Rule:** valid = still here or stayed >=24 h; leaving sooner (or self-invite) = fake. `GuildMemberCache` rows are written for every non-bot join.

### 🧱 Embed Messages — `embeds`
- **Code:** `/embed` (`gated = true`: refused while off), `src/plugins/embeds.js`, `src/services/embedService.js`: `getSavedEmbedTemplate(guildId, name)` (template or null, ignores the toggle, used by automations), `searchSavedEmbeds` (autocomplete).
- **customIds/Hooks:** modal `embed-modal:new:{name}` / `embed-modal:{savedEmbedId}`; `messageDelete:embeds` (`always`) drafts a deleted post.

### ⏰ Reminders — `reminders`
- **Code:** `/reminder`, `src/plugins/reminders.js`.
- **Job:** `reminder.fire`, dedupe `reminder:{reminderId}`, recurring. **Hooks (`always`):** `channelDelete:reminders`, `guildReady:reminders`. No Redis.
- **customIds:** modal `reminder-modal:{reminderId}`, `module: null`; the handler re-checks ADMIN.
- **Gotchas:** ignores `job.runAt`; sends only a due occurrence within the late tolerance (TIME_OF_DAY 15 min, INTERVAL min(15 min, interval/2), ONE_SHOT 60 min). The first occurrence after a `GuildSettings.timezone` change may skip once; `/reminder resume` re-syncs.

### 🎉 Giveaways — `giveaways`
- **Code:** `/giveaway` (MODERATOR), `src/plugins/giveaways.js`. Entry: roles and weights, no level requirement; `xp`/`coins` prizes via the rewards bus.
- **customIds/Job:** `giveaway:join:{giveawayId}`; `giveaway.end`, dedupe `giveaway.end:{giveawayId}`. **Hooks (`always`):** `messageDelete:giveaways`, `messageDeleteBulk:giveaways`.
- **Gotchas:** per-giveaway mutex plus conditional `updateMany`.

### 📊 Polls — `polls`
- **Code:** `/poll` (MODERATOR), `src/plugins/polls.js`.
- **customIds/Job:** `poll:vote:{pollId}:{i}`, `poll-select:{pollId}`, `poll-modal:{nonce}`; `poll.end`, dedupe `poll.end:{pollId}`.
- **Hooks (`always`, id `polls`):** `messageDelete`, `messageDeleteBulk`, `channelDelete`.
- **Gotchas:** `pg_advisory_xact_lock` serialises votes and creates.

### 🌟 Starboards — `starboard`
- **Code:** `/starboard`, `src/plugins/starboard.js`.
- **Hooks (id `starboard`):** the four messageReaction* events, `messageUpdate`; `always`: `messageDelete`, `messageDeleteBulk`, `channelDelete`.
- **Gotchas:** staff-deleted reposts stay `blocked`; NSFW needs `allow_nsfw` and an age-restricted board; board-channel messages never count.

### 🎫 Ticketing — `tickets`
- **Code:** `/ticket` (staff = MODERATOR+ or panel manager role), `/ticket-panel`, `src/plugins/tickets.js`.
- **customIds:** `ticket:open:{panelId}:{optionId}`, `ticket:{action}:{ticketId}`, `ticket-select:{panelId}`, `ticket-modal:{panelId}:{optionId}` / `ticket-modal:tpl:{panelId}:{kind}`; the modal has `module: null`: the editor re-checks ADMIN, the reason form the module.
- **Hook:** `channelDelete:tickets` (`always`).
- **Gotchas:** delete saves an HTML transcript; a cache miss is never a deleted channel.

### 🔊 Temporary Channels — `tempvoice`
- **Code:** `/voice`, `/voice-hub`, `src/plugins/temp-voice.js`.
- **customIds/Job:** `voice:{action}:{channelId}`, `voice-select:{act}:{channelId}`, `voice-modal:{action}:{channelId}`; `tempvoice.cleanup`, dedupe `tempvoice.cleanup:{channelId}`.
- **Hooks (`always`, id `temp-voice`):** `voiceStateUpdate`, `channelDelete`, `guildReady`. **Redis:** `tempvoice:rename:{channelId}` 600 s.
- **Gotchas:** `confirmChannel` (REST) decides before a row is purged.

### 📈 Statistics Channels — `stats`
- **Code:** `/stats`, `src/plugins/stat-channels.js`. Only `ONLINE`/`PEAK_ONLINE` (`StatKind`) need the Presence intent (`src/bot.js`).
- **Job:** `stats.refresh` (recurring, 10 min), dedupe `stats.refresh:{guildId}`.
- **Hooks (id `stat-channels`):** `guildMemberAdd`, `guildMemberRemove`; `always`: `guildReady`, `channelDelete`.
- **Gotchas:** `StatChannel.lastRenamedAt` limits renames to one per 5 min per channel.

### 🎂 Birthdays — `birthdays`
- **Code:** `/birthday` (public output omits the year), `/birthday-config`, `src/plugins/birthdays.js`.
- **Job:** `birthday.tick` (recurring), dedupe `birthday.tick:{guildId}`: hourly in `GuildSettings.timezone`, announces at `birthdayHour`, removes the role next local day, runs while off. **Hook:** `guildReady:birthdays` (`always`).
- **Gotchas:** raw SQL claims each celebration once per local year.

### 🏆 Achievements — `achievements`
- **Code:** `/achievements`, `/achievement-config`, `src/plugins/achievements.js`.
- **Hooks (id `achievements`):** `messageCreate`, `messageReactionAdd`, `guildMemberUpdate`, `levelUp`, `economyPurchase`.
- **Gotchas:** unlock = compare-and-set on `AchievementProgress.tier` before payout. `ctx.member` is null in `levelUp`. Embed mode: `<@id>` in content, `allowedMentions` users only.

### 🪙 Economy — `economy`
- **Commands:** `/balance` `/daily` `/pity` `/work` `/pay` `/richest` `/shop` `/inventory` `/use`; admin: `/economy-admin`, `/economy-boost` (up to `LIMITS.ECONOMY_BOOSTS`; multiplies one `/daily` or `/work` payout only).
- **Models:** PER-SERVER `EconomyAccount`; legacy global `UserEconomy` is read-only. `getAccount(guildId, userId, settings)` creates an account lazily, copying the global balance ONCE (`importedGlobal`, unless `economyImportGlobal` is false), else `economyStartingBalance`. `economyConfig(settings)` reads the rest (`economyGameChannelIds` + `economyGameChannelMode` ALLOW|DENY, `economyMaxPayout` = net-winnings cap).
- **Money safety:** every balance write is ONE SQL statement (`::bigint` maths, `LEAST` cap or conditional `WHERE`), clamped to `COIN_MAX`; no read-modify-write in JS.
- **Casino API (`src/services/economyService.js`):** `tryLockCasino`/`unlockCasino` (sync, in memory, one game per member per server); `reserveBet` debits first; `settleBet` credits the GROSS payout once and REQUIRES the `stake` option; `gameChannelCheck`.
- **Plugin:** `src/plugins/economy.js`: only the `coins` provider, no hooks. `economyPurchase` fires from `/shop buy` ONLY.

### 🎮 Games — `games`
- **Commands:** `/blackjack` `/roulette` `/slots` are `module = 'economy'` (escrowed bets); the rest (`/numguess` `/rps` `/tictactoe` `/scrabble*` `/games`) are `games`-gated. `/games` lists `client.commandCategories` entries `games`; a new game needs its emoji in `GAME_EMOJI` (`src/commands/games/games.js`).
- **customIds:** collector prefixes `ttt` `ttt-ch` `rps` `scrabble-ch` `bj`; persistent `scrabble-rack:{gameId}` (registered by `src/plugins/games-fixes.js`).
- **Scrabble:** games live in memory (`activeGames` in `src/services/scrabbleService.js`), so a restart ends them; turn timeout = forfeit. `/scrabble-play word|pass|exchange|resign`; `?X` plays a blank as X. Optional dictionary `assets/dictionaries/words.txt` (operator-supplied, e.g. ENABLE list, not shipped, ~10-15 MB RAM per shard); without it any A-Z word passes.

### 📰 Social Feeds — `feeds`
- **Commands:** `/feed` (ADMIN); `LIMITS.FEEDS` 30.
- **Code:** `src/services/feedService.js`, `src/services/feedParsers.js` (`fast-xml-parser`), `src/plugins/feeds.js`; model `Feed`, `FeedType` RSS|YOUTUBE|REDDIT|BLUESKY|PODCAST.
- **Job/hooks:** `feed.poll` (recurring, dedupe `feed.poll:{feedId}`) is claimed per owning shard, no shard-0 poller. `guildReady:feeds` and `channelDelete:feeds`, both `always`.
- **Behaviour:** the first fetch only seeds (`initialized`); `seenIds` keeps the newest 100, recorded before sending; max 5 posts per poll; 20 consecutive failures disable the feed (`disabledReason`). Fetching uses `safeFetch`; env `HTTP_CONTACT`, `REDDIT_USER_AGENT`, `BSKY_APPVIEW_URL`. Mentions: ping role only.

### 🔎 Search Anything — `search`
- **Commands:** `/anime` `/manga` `/pokemon` `/urban`; MEMBER, no API keys; config is core `/settings urban`.
- **Code:** `src/services/searchService.js` (no plugin, no database). `/urban` needs an age-restricted channel unless `urbanNsfwOnly === false` (fails closed).

### 🔴 Live Role — `liverole`
- **Commands:** `/liverole set|require|enable|disable|status` (ADMIN); default off.
- **Code:** engine and hooks in `src/plugins/liverole.js`: `presenceUpdate:liverole` (needs the privileged `GuildPresences` intent) and `guildReady:liverole`, neither `always`.
- **Behaviour:** `ActivityType.Streaming` earns the role; optional `liveRoleRequiredRoleId`.

### Core commands (no module)
- `/help`, `/module` (ADMIN): `/help` hides commands above the caller's level or of disabled modules; custom commands show as `!name` (`/tag use name:x` when `customPrefix === ''`).
- `/settings show|timezone|color|prefix|urban` (ADMIN): timezone IANA or `UTC`; `embedColor` only for info/neutral embeds (`applyGuildColor`/`guildEmbed`), never 0; `customPrefix` 1-5 code points, custom commands only, `off` stores `''`.
- `/info user|server|role` (MEMBER); `/emoji add|list|delete` (ADMIN + Manage Expressions).
- `/translate` `/detectlanguage` `/languages`: MEMBER, also work in DMs.

## Testing

`npm test` runs every `test/*.test.js` with `node --test`, offline (fake `DATABASE_URL`/`REDIS_URL`, outbound network blocked); run one with `node --test test/<file>.test.js`. `test/smoke.test.js`, `test/registry.test.js`, `test/utils.test.js` and `test/foundation.test.js` cover loading, payload limits, registries, router, plugin loader and shared services; `test/<feature>.test.js` one feature each.
- `test/integration.test.js`: the 76-command table (module, gating, level), hook/customId/job registries against the real plugins, repo rules (no `console`/`setInterval`, thin events, single settings gateway, env vars documented) and a runtime install of all plugins. **It fails when a command, module, job, plugin or env var is added without updating `README.md`, this file and `.env.example`**; backticked `src/...`/`test/...` paths in the two docs must exist.
- **Database tests (opt-in):** set `TEST_DATABASE_URL` to a migrated throw-away Postgres (CI: its service container after `prisma migrate deploy`) and run `node --test --test-concurrency=1 "test/*.test.js"`; per-feature DB cases are otherwise skipped. `test/db.test.js` runs every exported `RAW_SQL` statement in a rolled-back transaction; its samples use guild `'0'`, so upsert that `GuildSettings` row before INSERT builders (foreign key). Use guild ids unique to your file and delete what you create.
