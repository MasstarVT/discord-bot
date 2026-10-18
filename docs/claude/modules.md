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
- **Commands:** `/automod` ADMIN (groups `words` `patterns` `domains` `exempt` `honeypot`); `words`/`patterns` replies ephemeral.
- **Code:** `src/plugins/automod.js`, `src/services/automodService.js`. Rules: badWords repeatedText invites links caps emojis spoilers mentions zalgo spam. Actions: off delete warn delete_warn timeout kick ban (punishments via `executeModAction`). `automodConfig` v2 top level: `version rules badWords badWordPatterns blockedDomains allowedDomains exemptRoleIds exemptChannelIds`. Staff bypass: `isImmune`, MODERATOR, Manage Messages.
- **Bad words:** whole-word, but a run of `_` at the edge of a word (`_bad_`, `__bad__`, `bad_`) is Discord italics/underline and counts as a boundary (`textForWords`); `snake_bad_word` and an escaped `\_` stay word characters. A listed word written only in Han/Hiragana/Katakana/Thai/Lao/Khmer/Myanmar (no word spaces; Hangul is spaced) matches as a substring.
- **Hooks:** `messageCreate:automod` (first), `messageUpdate:automod`; after a delete: `ctx.stop('automod')`, `ctx.state.automodAction = { rule, deleted }`. A failed delete never punishes (except the honeypot).
- **Honeypot:** `automodConfig.honeypot = { channelIds (≤ `LIMITS.AUTOMOD_HONEYPOTS`, text/announcement only, a thread counts as its parent, a category never), action softban|ban|kick|timeout (default softban), timeoutSeconds }`. Not a `RULE_KEYS` rule. Checked first in `runAutomod`: works with every rule off, ignores `exemptChannelIds`, skips edits; owner, staff and `exemptRoleIds` bypass. Outcome via `honeypotOutcome`: always deletes, punishes even if the delete failed, ban/softban clear 1 day (`HONEYPOT_DELETE_MESSAGE_SECONDS`), no in-channel notice. `/automod honeypot add [channel] [action] [timeout] [warning]` (no channel: creates `#do-not-post` at position 0, needs Manage Channels) and `remove channel`; both forget deleted channels. Dashboard: virtual field `automodHoneypot`.

### 📜 Audit Logs — `logging`
- **Commands:** `/logging` ADMIN.
- **Code:** `src/plugins/logging.js` (26 hooks, 32 `LOG_EVENTS`), `src/services/loggingService.js`, `src/services/logEvents.js`, `src/utils/deletionTracker.js`.
- **Events:** message, member (incl. `userUpdate`), voice, channel, thread, role, emoji, invite, server.
- **Sending:** `sendLog(guild, logType, embed, opts)`, `opts { event?, settings?, files? }`; `LOG_TYPES` MOD ignores `loggingEnabled`. Batched ~1 s; `flushLogQueue()` at shutdown.
- **Gotchas:** call `markBotDeletion(id, reason)` before bot deletions (read via `wasDeletedByBot`); `logIgnoredChannelIds` mutes message events only. `bot.js` sweeps cached messages after 3 h, so edits and deletes of older messages log without their old content.

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
- **Role templates:** `template:pronouns|colors|age|regions|platforms|dms|pings|timezones-us|timezones-world` (`/reactionrole create`, and the dashboard's create-only `template` field) use `ROLE_TEMPLATES` (`src/utils/roleTemplates.js`; the timezone sets come from `TIMEZONE_ROLE_TEMPLATES` in `src/utils/timezones.js` and have `linksZones`). Each template has its own `panel` (mode, layout, texts), roles (name, label, emoji, optional color) and an optional `note` for the admin. `setupRoleTemplate` (`src/services/roleTemplateSetup.js`) checks everything first (Manage Roles, `checkAssignableRole` on reused roles, Discord's 250-role cap, and `LIMITS.TIMEZONE_ROLES` for timezones), reuses roles by name or creates them with no permissions (and the template color), and links timezone roles in `GuildSettings.timezoneRoles`. Template panels are buttons or a dropdown, never emoji reactions; a timezone panel is always Unique. New color roles land at the bottom, so the colors note tells admins to drag them up. The dashboard saves the panel empty and runs `rr.roleTemplate`, which fills and posts it; a refusal deletes the empty panel.

### ⭐ Levels — `leveling`
- **Commands:** `/levelconfig`, `/xp` ADMIN; `/rank`, `/leaderboard`. `/xp give target:@Role` caps at 5,000 members.
- **Code:** `src/plugins/levels.js`, `src/services/xpService.js`, `src/utils/profileCard.js`. `xpForLevel(l) = 5l^2 + 50l + 100`; `levelFromXp`, `xpIntoLevel`; `RAW_SQL`.
- **XP:** `xpMin`/`xpMax` replace `xpRate`; cooldown `xp:cooldown:{guildId}:{userId}` fails closed. No `grantXp`/`handleVoiceXp` call sites remain.
- **Voice:** `voice:start:{guildId}:{userId}` = `{ start, channelId }`; recurring job `levels.voiceSweep` (`levels.voiceSweep:{guildId}`). Turning voice XP, `/levelconfig` or `/module disable leveling` off drops open sessions without credit.
- **Hooks:** `messageCreate:levels`, `voiceStateUpdate:levels` (sync filter `voiceChangeMatters` skips 'state' changes that leave deafen alone; other members are re-checked detached, only when a channel has 1-2 listeners), `roleDelete:levels` (always), `guildReady:levels`.
- **Level-up:** announce modes (`AnnounceMode`) OFF|CURRENT|DM|CHANNEL; message tokens `{player}` `{level}` `{rank}` `{xp}`; emits `levelUp` (`ctx.member` may be null); `provides.xp` reward provider (grant/take).

### 🏷️ Custom Commands — `customcommands`
- **Commands:** `/tag` (gated; writes ADMIN), `/trigger` (ADMIN).
- **Code:** `src/plugins/custom-commands.js`, `src/services/tagService.js`.
- **Prefix:** `customPrefix` (default `!`); `/settings prefix off` stores `''` (`/tag use` still works). Upgraded guilds that have tags start answering `!name`.
- **Triggers:** `autoTriggersEnabled` (`/trigger disable` only clears it); cache `guild:{guildId}:triggers` (5 min, `invalidateTriggerCache`); cooldowns `tag:cd:{guildId}:{tagId}`, `trigger:cd:{guildId}:{triggerId}:{userId}` fail closed.
- **Hooks:** `messageCreate:custom-commands` (may `ctx.stop()`), `messageCreate:custom-commands:triggers`.
- **Disabled tags:** hidden from members everywhere (`list`, autocomplete, `info`, `raw`, `action list`); admins still see them.
- **Admin rules (`/tag`):** a `send` action needs View Channel + Send Messages for the admin in the target (owner/Administrator exempt); a tag giving or taking a role at or above the admin's highest role cannot be edited, toggled or have its cooldown, permissions, actions or replies changed by them (owner exempt; dashboard: `checkStoredRoles` on the `tags` collection).

### ⚙️ Automations — `automations`
- **Commands:** `/automation` ADMIN.
- **Code:** `src/plugins/automations.js`, `src/services/automationService.js`; button prefix `automation` (`automation:{automationId}:{buttonIndex}`). 8 hooks, id `automations`.
- **Loop guard:** bots never trigger message events, chain depth <=3, 30 runs/10 s per guild, cooldown `automation:cd:{guildId}:{automationId}:{userId}` (given back when the budget or the 25-run queue refuses the run).
- **Import:** `/automation import` refuses GIVE/TAKE roles at or above the importer's highest role (owner exempt), like `action add`; it also needs the channel access and role-ping rights below, and replacing an automation that gives such a role is refused.
- **Admin rules:** a send/repost/thread target needs View Channel + Send Messages (Create Public Threads for threads) for the admin (owner/Administrator exempt; presets and import too); a role mention the admin could not ping is refused at config time (`pingableRoleIds`; the dashboard applies it with `pingRoleProblem` to message texts, the Welcomer text and saved embeds it adds: owner, Administrator, Mention Everyone and the bot owner may ping any role, a role the automation already pinged is kept); an automation giving or taking a role at or above the admin's highest role cannot be edited, enabled, disabled or changed by them (owner exempt; dashboard: `checkStoredRoles` on the `automations` collection).
- **Privacy at run time:** REPOST, and SEND_MESSAGE/SEND_EMBED/SEND_IN_THREAD/CREATE_THREAD texts with `{message.content}` or `{message.old_content}`, skip (problem logged) when the message's channel is hidden from @everyone and the target is not (`src/utils/privacyGuards.js`).

### 🌐 Auto-Translate — `translation`
- **Commands:** `/autotranslate` ADMIN.
- **Code:** `src/plugins/translation-fixes.js` (hook last in `messageCreate`), `src/services/translationService.js`.
- **Providers:** `TRANSLATE_PROVIDER` `google` (default) or `libretranslate` (`LIBRETRANSLATE_URL`, `LIBRETRANSLATE_API_KEY`); else warns, uses google. Traditional Chinese is `zh-tw`; aliases `iw nb zh-cn zh-hant fil`; the first 1,000 characters are translated, up to 1,000 shown per language.
- **Channel list:** `saveTranslateChannel` (translationService) does the cap check and the insert under a per-guild advisory lock (`translate-channels:{guildId}`), shared by `/autotranslate` and the dashboard store; `refreshTranslateCache` bumps a per-guild generation so a read that began earlier never re-memoises a stale list.
- **Redis:** `tr:{provider}:{target}:{sha1}` (1 h), `translate:{provider}:pausedUntil`; a 429 pauses (30 min Google, 1 min LibreTranslate).
- **Activity:** `src/services/translateStatsService.js` `recordTranslation` counts each auto-translate reply that was sent and each `/translate` in a server (never DMs) in `TranslateStat`: per channel, `via` `auto|command`, `direction` `from` (detected language, `und` when none) or `to` (each target shown), `count` clamped to int4. One `INSERT … ON CONFLICT` per message, unawaited, never throws; counts only, never text. The dashboard's read-only `translateStats` list shows it per channel.

### 📨 Invite Tracker — `invites`
- **Commands:** `/invites` (gated).
- **Code:** `src/plugins/invites.js`, `src/services/inviteTracker.js`. `guildMemberAdd:invites` is `always`, does not await attribution and sets `ctx.state.invitePromise` (never rejects, null after `JOIN_HOOK_WAIT_MS`; only the join log awaits it, detached) and `ctx.state.invite` when it settles; Redis `guild:{guildId}:invites` (7 d), warmed on guildReady and on `/module enable invites` (not on a dashboard toggle).
- **Rule:** valid = still here or stayed >=24 h; leaving sooner (or self-invite) = fake. `GuildMemberCache` rows are written for every non-bot join.

### 🧱 Embed Messages — `embeds`
- **Code:** `/embed` (`gated = true`: refused while off), `src/plugins/embeds.js`, `src/services/embedService.js`: `getSavedEmbedTemplate(guildId, name)` (template or null, ignores the toggle, used by automations), `searchSavedEmbeds` (autocomplete).
- **customIds/Hooks:** modal `embed-modal:new:{name}` / `embed-modal:{savedEmbedId}`; `messageDelete:embeds` (`always`) drafts a deleted post.
- **Admin access:** `publish` needs View Channel + Send Messages for the member in the channel; `update` and `delete` of a published embed need View Channel in its channel (owner/Administrator exempt).

### ⏰ Reminders — `reminders`
- **Code:** `/reminder`, `src/plugins/reminders.js`.
- **Job:** `reminder.fire`, dedupe `reminder:{reminderId}`, recurring. **Hooks (`always`):** `channelDelete:reminders`, `guildReady:reminders`. No Redis.
- **customIds:** modal `reminder-modal:{reminderId}`, `module: null`; the handler re-checks ADMIN.
- **Gotchas:** ignores `job.runAt`; sends only a due occurrence within the late tolerance (TIME_OF_DAY 15 min, INTERVAL min(15 min, interval/2), ONE_SHOT 60 min). The first occurrence after a `GuildSettings.timezone` change may skip once; `/reminder resume` re-syncs.
- **Send failures:** a transient one (Discord 5xx, 429, network error, timeout) hands back the occurrence's claim and throws, so the scheduler retries with its backoff (`maxAttempts` 10) while the retry still fits the late tolerance; the final attempt never throws (recurring reschedules, ONE_SHOT finishes). Permanent codes (50001/50013/10003/10013/50007/50035) are never retried. Every send carries a per-occurrence `nonce` + `enforceNonce`, so a retry cannot post twice; a timed-out send is retried only on the first two (<= 60 s) backoff steps.

### 🎉 Giveaways — `giveaways`
- **Code:** `/giveaway` (MODERATOR), `src/plugins/giveaways.js`. Entry: roles and weights, no level requirement; `xp`/`coins` prizes via the rewards bus.
- **customIds/Job:** `giveaway:join:{giveawayId}`; `giveaway.end`, dedupe `giveaway.end:{giveawayId}`. **Hooks (`always`):** `messageDelete:giveaways`, `messageDeleteBulk:giveaways`.
- **Gotchas:** per-giveaway mutex plus conditional `updateMany`. `endedAt` is taken just before the entries are read, and draws and rerolls only count entries with `createdAt <= endedAt` (a join that raced the end is never drawn).

### 📊 Polls — `polls`
- **Code:** `/poll` (MODERATOR), `src/plugins/polls.js`.
- **customIds/Job:** `poll:vote:{pollId}:{i}`, `poll-select:{pollId}`, `poll-modal:{nonce}`; `poll.end`, dedupe `poll.end:{pollId}`.
- **Hooks (`always`, id `polls`):** `messageDelete`, `messageDeleteBulk`, `channelDelete`.
- **Gotchas:** `pg_advisory_xact_lock` serialises votes and creates. Shown percentages come from `percentages()` (largest remainder over the sum of the counts): they add up to exactly 100 once anyone voted, and the bar is drawn from the same number (also in the dashboard `results`).
- **Dashboard:** `postPoll` (create, post, record the message id, schedule `poll.end`) is shared by `/poll create` and the `poll.start` task; `poll.end` and `poll.delete` (`src/services/dashboardTasks/polls.js`) use `endPoll`. Delete ends an active poll silently (no results post) before it removes the message and the record.

### 🌟 Starboards — `starboard`
- **Code:** `/starboard`, `src/plugins/starboard.js`.
- **Hooks (id `starboard`):** the four messageReaction* events, `messageUpdate`; `always`: `messageDelete`, `messageDeleteBulk`, `channelDelete`.
- **Gotchas:** staff-deleted reposts stay `blocked`; NSFW needs `allow_nsfw` and an age-restricted board; board-channel messages never count. Syncs of one message are spaced `SYNC_SPACING_MS` (1.5 s), or `REPOST_SYNC_SPACING_MS` (10 s) once it has a live repost (each sync re-reads every reactor over REST); the last sync of a burst always runs.
- **Privacy:** a message in a channel or thread @everyone cannot view (any private thread) is never reposted to a board @everyone can view; a private board may take private posts. `sourceSkipReason` returns `private`; `mirrorEdit` re-checks eligibility (NSFW and privacy) and leaves the repost unedited when the source no longer qualifies.

### 🎫 Ticketing — `tickets`
- **Code:** `/ticket` (staff = MODERATOR+ or panel manager role), `/ticket-panel`, `src/plugins/tickets.js`.
- **customIds:** `ticket:open:{panelId}:{optionId}`, `ticket:{action}:{ticketId}`, `ticket-select:{panelId}`, `ticket-modal:{panelId}:{optionId}` / `ticket-modal:tpl:{panelId}:{kind}`; the modal has `module: null`: the editor re-checks ADMIN, the reason form the module.
- **Hook:** `channelDelete:tickets` (`always`).
- **Gotchas:** delete saves an HTML transcript; a cache miss is never a deleted channel.

### 🔊 Temporary Channels — `tempvoice`
- **Code:** `/voice`, `/voice-hub`, `src/plugins/temp-voice.js`.
- **customIds/Job:** `voice:{action}:{channelId}`, `voice-select:{act}:{channelId}`, `voice-modal:{action}:{channelId}`; `tempvoice.cleanup`, dedupe `tempvoice.cleanup:{channelId}`.
- **Hooks (`always`, id `temp-voice`):** `voiceStateUpdate`, `channelDelete`, `guildReady`. **Redis:** `tempvoice:rename:{channelId}` 600 s.
- **Gotchas:** `confirmChannel` (REST) decides before a row is purged; `purgeRow` also deletes a still-cached companion text channel. A channel whose (non-sync) hub was deleted refuses overwrite actions (`MSG.HUB_GONE`): lock, hide, ban, transfer, claim. Lock/hide also clear the Connect/View allows the template gives non-exempt roles and members (else a category's Verified-role allow beats the @everyone deny).

### 📈 Statistics Channels — `stats`
- **Code:** `/stats`, `src/plugins/stat-channels.js`. Only `ONLINE`/`PEAK_ONLINE` (`StatKind`) need the Presence intent (`src/bot.js`); with `DISABLE_PRESENCES=1` (or above 75,000 members) they show Discord's `approximatePresenceCount` from `guild.fetch()`.
- **Job:** `stats.refresh` (recurring, 10 min), dedupe `stats.refresh:{guildId}`.
- **Hooks (id `stat-channels`):** `guildMemberAdd`, `guildMemberRemove`; `always`: `guildReady`, `channelDelete`.
- **Gotchas:** `StatChannel.lastRenamedAt` limits renames to one per 5 min per channel.

### 🎂 Birthdays — `birthdays`
- **Code:** `/birthday` (public output omits the year), `/birthday-config`, `src/plugins/birthdays.js`.
- **Job:** `birthday.tick` (recurring), dedupe `birthday.tick:{guildId}`: hourly in `GuildSettings.timezone`, announces at `birthdayHour`, removes the role next local day, runs while off. **Hook:** `guildReady:birthdays` (`always`).
- **Gotchas:** raw SQL claims each celebration once per local year. `tickGuild` pages today's candidates by `userId` and skips departed members and bots without claiming them (later ticks re-check them), so 50+ of them cannot starve the members behind them. `/birthday-config set-user` with a year says the age will be shown in the announcement.

### 🏆 Achievements — `achievements`
- **Code:** `/achievements`, `/achievement-config`, `src/plugins/achievements.js`.
- **Hooks (id `achievements`):** `messageCreate`, `messageReactionAdd`, `guildMemberUpdate`, `levelUp`, `economyPurchase`.
- **Gotchas:** unlock = compare-and-set on `AchievementProgress.tier` before payout. `ctx.member` is null in `levelUp`. Embed mode: `<@id>` in content, `allowedMentions` users only. MESSAGES progress is buffered per member (`MESSAGE_FLUSH`: written as one +n UPDATE 5 s after the first waiting message, at once at 50, or when the active set changes), so unlocks come up to 5 s late; `bot.js` calls `flushMessageCounts()` at shutdown and a crash loses at most that window. Tests that need each message written at once call `__setMessageFlush({ delayMs: 0 })`.

### 🪙 Economy — `economy`
- **Commands:** `/balance` `/daily` `/pity` `/work` `/pay` `/richest` `/shop` `/inventory` `/use`; admin: `/economy-admin`, `/economy-boost` (up to `LIMITS.ECONOMY_BOOSTS`; multiplies one `/daily` or `/work` payout only).
- **Models:** PER-SERVER `EconomyAccount`; legacy global `UserEconomy` is read-only. `getAccount(guildId, userId, settings)` creates an account lazily, copying the global balance ONCE (`importedGlobal`, unless `economyImportGlobal` is false), else `economyStartingBalance`. `economyConfig(settings)` reads the rest (`economyGameChannelIds` + `economyGameChannelMode` ALLOW|DENY, `economyMaxPayout` = net-winnings cap).
- **Money safety:** every balance write is ONE SQL statement (`::bigint` maths, `LEAST` cap or conditional `WHERE`), clamped to `COIN_MAX`; no read-modify-write in JS.
- **Casino API (`src/services/economyService.js`):** `tryLockCasino`/`unlockCasino` (sync, in memory, one game per member per server); `reserveBet` debits first; `settleBet` credits the GROSS payout at most once and REQUIRES the `stake` option (it retries only on `failedBeforeSend` errors: a lost response is never retried); `gameChannelCheck`.
- **Refunds:** `refundPurchase` returns the full price, clamped to `COIN_MAX` only (not the server's `economyMaxBalance`), so a buyer already at the cap loses nothing.
- **Plugin:** `src/plugins/economy.js`: only the `coins` provider, no hooks. `economyPurchase` fires from `/shop buy` ONLY. Use-on-buy ROLE buys are serialised per member and re-check the role on a forced member fetch (two buys sent together used to both pass).

### 🎮 Games — `games`
- **Commands:** `/blackjack` `/roulette` `/slots` are `module = 'economy'` (escrowed bets); the rest (`/numguess` `/rps` `/tictactoe` `/scrabble*` `/games`) are `games`-gated. `/games` lists `client.commandCategories` entries `games`; a new game needs its emoji in `GAME_EMOJI` (`src/commands/games/games.js`). `/slots` pays 15/11/7/5/4/3x per line (💎 7️⃣ 🍇 🍒 🍋 🍎), RTP 95.02 % (derivation in `slots.js`; `test/review-economy.test.js` keeps it in 94-96 %).
- **customIds:** collector prefixes `ttt` `ttt-ch` `rps` `scrabble-ch` `bj`; persistent `scrabble-rack:{gameId}` (registered by `src/plugins/games-fixes.js`).
- **Scrabble:** games live in memory (`activeGames` in `src/services/scrabbleService.js`), so a restart ends them; turn timeout = forfeit. `/scrabble-play word|pass|exchange|resign`; `?X` plays a blank as X. Optional dictionary `assets/dictionaries/words.txt` (operator-supplied, e.g. ENABLE list, not shipped, ~10-15 MB RAM per shard); without it any A-Z word passes.

### 📰 Social Feeds — `feeds`
- **Commands:** `/feed` (ADMIN); `LIMITS.FEEDS` 30.
- **Code:** `src/services/feedService.js`, `src/services/feedParsers.js` (`fast-xml-parser`), `src/plugins/feeds.js`; model `Feed`, `FeedType` RSS|YOUTUBE|REDDIT|BLUESKY|PODCAST.
- **Job/hooks:** `feed.poll` (recurring, dedupe `feed.poll:{feedId}`) is claimed per owning shard, no shard-0 poller. `guildReady:feeds` and `channelDelete:feeds`, both `always`.
- **Behaviour:** the first fetch only seeds (`initialized`); `seenIds` keeps the newest 100, recorded before sending; max 5 posts per poll; 20 consecutive failures disable the feed (`disabledReason`). Fetching uses `safeFetch`; env `HTTP_CONTACT`, `REDDIT_USER_AGENT`, `BSKY_APPVIEW_URL`. Mentions: ping role only.
- **304:** `fetchAndParse` treats HTTP 304 as "not modified" only when it sent `If-None-Match` or `If-Modified-Since`; an unprompted 304 is the error `UNEXPECTED_304` (so `/feed add` shows a clear message).

### 🔎 Search Anything — `search`
- **Commands:** `/anime` `/manga` `/pokemon` `/urban`; MEMBER, no API keys; config is core `/settings urban`.
- **Code:** `src/services/searchService.js` (no plugin, no database). `/urban` needs an age-restricted channel unless `urbanNsfwOnly === false` (fails closed).

### 🔴 Live Role — `liverole`
- **Commands:** `/liverole set|require|enable|disable|status` (ADMIN); default off.
- **Code:** engine and hooks in `src/plugins/liverole.js`: `presenceUpdate:liverole` (needs the privileged `GuildPresences` intent), `guildReady:liverole` and `guildMemberUpdate:liverole` (re-checks a member who gained or lost `liveRoleRequiredRoleId`), none `always`.
- **Behaviour:** `ActivityType.Streaming` earns the role; optional `liveRoleRequiredRoleId`.
- **Gotchas:** the `guildReady` sync is queued (one guild at a time, a guild at most once) and the hook returns at once; command syncs do not use that queue. With `DISABLE_PRESENCES=1` the presence cache is empty, so `syncGuild` returns `presence-unavailable` and `reconcileMember` does nothing except in clear mode (no role is stripped); `/liverole` answers "Live Role Unavailable" except `disable`.

### 🔢 Counting — `counting`
- **Commands:** `/counting set|reset|status` (ADMIN); default off, `set` picks `countingChannelId` and turns the module on. `reset number:N` carries on from N (never raises `best`).
- **Code:** rules and the `CountingState` row (one per guild: `count`, `lastUserId`, `lastMessageId`, `best`) in `src/services/countingService.js`; hooks `messageCreate:counting` (right after automod) and `messageDelete:counting` in `src/plugins/counting.js`. Both filter on `inCountingChannel` (`peekSettings`); `judgeMessage` runs under the guild lock and returns the reaction/note, sent after the lock is released.
- **Rules:** only a message that is just a number (up to 9 digits) counts; anything else is chat. The next number from someone other than the last counter gets ✅; a wrong number or a second count in a row gets ❌ and resets to 0 with a note. At 0 a wrong number only gets ❌ plus a "starts at 1" hint at most once a minute. Deleting the message with the current count posts the next number.
- **Gotchas:** every count and `/counting set|reset` runs under `runLocked(guildId)`, entered before any await in the hook, so counts are judged in the order they reach the hook; hooks ahead of it (automod) can still swap two near-simultaneous messages. A row whose `channelId` is not `countingChannelId` (changed on the dashboard) starts from 0 and keeps `best`. Edited numbers are not re-judged.

### Core commands (no module)
- `/help`, `/module` (ADMIN): `/help` hides commands above the caller's level or of disabled modules; custom commands show as `!name` (`/tag use name:x` when `customPrefix === ''`).
- `/settings show|timezone|color|prefix|urban|timezone-role add|remove|list` (ADMIN): timezone IANA or `UTC`; `timezone-role` links roles to zones in `GuildSettings.timezoneRoles` (`[{ roleId, zone }]`, `LIMITS.TIMEZONE_ROLES`, via `updateSettingsWith`; deleted roles are dropped on the next write and ignored on read); with no `zone` it reads the role name (`zoneFromRoleName`: CST, EST/EDT, UTC+1); also editable on the dashboard General page (`timezoneRoles` objectList); `embedColor` only for info/neutral embeds (`applyGuildColor`/`guildEmbed`), never 0; `customPrefix` 1-5 code points, custom commands only, `off` stores `''`.
- `/info user|server|role` (MEMBER); `/emoji add|list|delete` (ADMIN + Manage Expressions).
- `/translate` `/detectlanguage` `/languages`: MEMBER, also work in DMs.
- `/timestamp when [style] [public]` (MEMBER): `parseWhen` (`src/utils/time.js`) reads the text in the caller's zone, resolved by `resolveTimezone` (`src/utils/timezones.js`): own `UserTimezone` → highest held timezone role → server `timezone` → UTC; a trailing abbreviation, `utc+2` or Region/City in the text wins. Private by default (every style with its code); `public:true` posts the stamp. Autocomplete previews the reading (memo/Redis only, no Postgres).
- `/timezone set|show|clear` (MEMBER, ephemeral): personal zone in `UserTimezone` (global, no guild FK) via `src/services/userTimezoneService.js` (memo 30 s → Redis `usertz:{userId}` 1 h → Postgres). Abbreviations (`ZONE_ABBREVIATIONS`: CST → America/Chicago, DST included) are accepted; ambiguous ones (IST, AST) and fixed offsets are refused.
- `/owner` (MEMBER): replies in plain content pinging `BOT_OWNER_ID` (only that user is in `allowedMentions`); ephemeral error when unset.
