# Feature module reference

Read the one section you need. One subsection per module, then core commands. Hooks are written `event:hookId`.

Setup pickers that a feature writes to or hands out (destinations, reward and panel roles, categories) have a `new_<picker>` companion that creates the role or channel instead (`src/utils/newTargets.js`, see architecture.md › Safe utilities). Filters (exempt/ignore/allowed/denied/required/bonus), ping roles, lookups, one-off send channels and remove subcommands deliberately have none. A new setup picker should get one.

### 🔨 Moderation — `moderation`
- **Commands:** `/ban /kick /softban /timeout /unmute /unban /warn /case /history /purge /slowmode` MODERATOR; `/history clear` ADMIN; `/modconfig` ADMIN.
- **Code:** `src/plugins/moderation.js`, `src/services/moderationService.js`. `executeModAction` is the only punishment path (`source` in `MOD_SOURCES`): never throws, Discord action first, a failure records nothing. `createInfraction` only records (still DMs, logs, escalates WARN). `isImmune`; `sendModLog` ignores `loggingEnabled`.
- **Rules:** `/timeout` max 28 d (longer = mute role, else refused); TEMPBAN = BAN + duration. `/purge` always replies privately and never auto-deletes; `/purge` and `/slowmode` default to ModerateMembers.
- **Jobs:** `mod.unban` (`mod.unban:{guildId}:{userId}`), `mod.expire` (`mod.expire:{infractionId}`); run with the module off.
- **Dashboard:** source `dashboard` (in `MOD_SOURCES`, checked like `command`: the acting user's roles, immunity and self-moderation; the DM may name them). Tasks `mod.action`, `mod.editCase`, `mod.clearWarnings` (`src/services/dashboardTasks/moderation.js`) do what `/warn` … `/history clear` do and re-check the module and the user's rights.
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
- **Activity:** `src/services/translateStatsService.js` `recordTranslation` counts each auto-translate reply that was sent and each `/translate` in a server (never DMs) in `TranslateStat`: per channel, `via` `auto|command`, `direction` `from` (detected language, `und` when none) or `to` (each target shown), `count` clamped to int4. One `INSERT … ON CONFLICT` per message, unawaited, never throws; counts only, never text. The dashboard's read-only `translateStats` list shows it per channel.

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
- **Dashboard:** `postPoll` (create, post, record the message id, schedule `poll.end`) is shared by `/poll create` and the `poll.start` task; `poll.end` and `poll.delete` (`src/services/dashboardTasks/polls.js`) use `endPoll`. Delete ends an active poll silently (no results post) before it removes the message and the record.

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
