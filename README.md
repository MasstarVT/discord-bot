# Discord Bot

A self-hosted, MEE6-style multi-purpose Discord bot built on discord.js, with Postgres (via Prisma) for persistence, Redis for caching, and PM2 or Docker for process management. Moderation, auto-moderation, audit logs, welcome and goodbye messages, levels, custom commands, automations, reaction roles, economy, games, social feeds and more are separate modules, and every module can be switched on or off per server with `/module`. There is no premium tier and no API key to buy: every built-in feature runs on Discord data or public, keyless endpoints.

## Features

Each bullet except the web dashboard is a module you can turn on or off with `/module`; the MEE6 plugin it mirrors is in brackets. Modules marked *off by default* are switched on with `/module enable <name>`; their setup commands work while the module is off, so you can configure first.

- 🔨 **Moderation** (MEE6: Moderator) — `/ban` (also temporary), `/kick`, `/softban`, `/timeout` (over 28 days uses a mute role), `/unmute`, `/unban`, `/warn` with automatic escalation, `/history view|clear`, `/case`, `/purge` with filters, `/slowmode`; immune and moderator roles, a case log channel and public or private replies via `/modconfig`. Temporary bans expire through a durable scheduler.
- 🤖 **Auto-Moderation** (MEE6: Moderator › Auto-moderation) — *off by default.* Ten rules (bad words and regexes, repeated text, invites, links, caps, emojis, spoilers, mentions, zalgo, spam rate), each with its own action (delete, warn, timeout, kick, ban) and role/channel scope; staff are exempt.
- 📜 **Audit Logs** (MEE6: Moderator › Audit logging) — *off by default.* Per-type log channels and individually toggled events for messages, members, voice, channels, threads, roles, emojis, invites and the server, batched to respect rate limits.
- 👋 **Welcome & Goodbye** (MEE6: Welcome & Goodbye) — *off by default.* Join message, optional welcome card with a custom background, welcome DM, goodbye message, delayed autoroles that respect Rules Screening, built from one shared placeholder set.
- 🛡️ **Verification** (MEE6: Welcome › Captcha verification) — *off by default.* Image-code or one-button captcha for new members, with a timeout that kicks members who never verify.
- 🎭 **Reaction Roles** (MEE6: Reaction Roles) — role panels as buttons, a dropdown or plain emoji reactions, on bot posts or on an existing message.
- ⭐ **Levels** (MEE6: Levels) — *off by default.* Message and voice XP (15 to 25 per message by default), level-up announcements, reward roles, XP boost roles, `/rank` card, `/leaderboard`, `/xp` management.
- 🏷️ **Custom Commands** (MEE6: Custom Commands) — *off by default.* `/tag` commands (also run as `!name`), multi-action responses, random replies, argument placeholders, plus `/trigger` auto-responders.
- ⚙️ **Automations** (MEE6: Automations) — *off by default.* When → If → Do rules over messages, reactions, voice, joins and threads, with presets and JSON export and import.
- 🌐 **Auto-Translate** (beyond MEE6) — *off by default; beyond MEE6.* Translate a channel's messages into up to five languages; `/translate`, `/detectlanguage`, `/languages` work everywhere. The dashboard shows per channel which languages members write in and get translated into.
- 📨 **Invite Tracker** (MEE6: Invite Tracker) — who invited whom, `/invites` leaderboard and personal invite links.
- 🧱 **Embed Messages** (MEE6: Embed Messages) — build, save, publish and edit embeds with fields and link buttons; presets for welcome channels.
- ⏰ **Reminders** (MEE6: Reminder (Reminders/Timers)) — scheduled channel messages: time of day, interval or one-shot, in the server's time zone.
- 🎉 **Giveaways** (MEE6: Giveaways) — button-entry giveaways with role requirements, weighted odds, reroll and XP or coin prizes.
- 📊 **Polls** (MEE6: Polls) — single or multiple choice polls with live results and an end time.
- 🌟 **Starboards** (MEE6: Starboards) — repost popular messages to a channel; several boards per server.
- 🎫 **Ticketing** (MEE6: Ticketing) — ticket panels with buttons or a dropdown, categories, staff claim, HTML transcripts.
- 🔊 **Temporary Channels** (MEE6: Temporary Channels) — join-to-create voice hubs with owner controls (`/voice lock|limit|rename|...`).
- 📈 **Statistics Channels** (MEE6: Statistics Channels) — counter channels for members, bots, online, peak online, boosts, channels, roles and more.
- 🎂 **Birthdays** (MEE6: Birthdays) — `/birthday`, announcements at a chosen hour in the server's time zone and an optional birthday role.
- 🏆 **Achievements** (MEE6: Achievements) — *off by default.* Tiered achievements for messages, reactions, levels, boosts and purchases, with role, XP or coin rewards.
- 🪙 **Economy** (MEE6: Economy) — a separate wallet per server: `/daily` streaks, `/work`, `/pay`, `/shop`, `/inventory`, `/use`, `/richest`, boosts, plus the casino games `/blackjack`, `/roulette` and `/slots` with escrowed bets.
- 🎮 **Games** (MEE6: Economy › games (+ ours)) — Rock-Paper-Scissors, number guess, Tic-Tac-Toe and a full two-player Scrabble (beyond MEE6).
- 📰 **Social Feeds** (MEE6: RSS / YouTube / Reddit / Bluesky / Podcast alerts) — post new RSS, YouTube channel, Reddit, Bluesky and podcast items to a channel, keyless.
- 🔎 **Search Anything** (MEE6: Search Anything) — `/anime`, `/manga` (AniList), `/pokemon`, `/urban`, keyless.
- 🔴 **Live Role** (MEE6: Twitch Alerts › live-streaming role) — *off by default.* A role for members while they stream, from Discord presence (no Twitch API).
- 🔢 **Counting** (beyond MEE6) — *off by default.* A channel where members count up from 1 in turns: the bot reacts ✅ to each right number, and a wrong number or counting twice in a row starts the count again from 1. `/counting set` picks the channel.
- 🖥️ **Web Dashboard** — log in with Discord and change each server's settings in a browser: every module's switch and options, the time zone, embed colour and command prefix, and lists such as saved embeds, custom commands, reminders and feeds; a separate process, see [Web dashboard](#web-dashboard).

**Core (always on):** `/help` (shows only what the member can use and what is enabled), `/module`, `/settings` (time zone, embed colour, custom-command prefix, `/urban` age gate), `/info`, `/emoji`, `/translate`, `/detectlanguage`, `/languages`.

**Beyond MEE6:** Auto-Translate, Counting, the Scrabble and other free games, and a per-server economy without premium limits. **Research only:** Twitch, YouTube Data API, Reddit API, X, Instagram, TikTok and Kick alerts, social counters, music and AI features are written up in [docs/external-api-research.md](docs/external-api-research.md) but not built.

## Commands

77 slash commands, registered automatically on startup when the command set changes (hash-checked). Commands of a module that is switched off are refused with a pointer to `/module`; an admin's configuration command for that module still works so it can be set up first. Admin commands are hidden from regular members by Discord permissions (Manage Server) and re-checked by the bot.

| Command | Module | Who | Description |
|---|---|---|---|
| `/ban` | 🔨 Moderation | Moderators | Ban a user, optionally for a set time (MEE6 /ban, /tempban). |
| `/case` | 🔨 Moderation | Moderators | View or edit a moderation case. |
| `/history` | 🔨 Moderation | Moderators (clear: Admins) | View or clear a member's infractions (MEE6 /infractions, /clear-all-infractions). |
| `/kick` | 🔨 Moderation | Moderators | Kick a member from the server (MEE6 /kick). |
| `/modconfig` | 🔨 Moderation | Admins | Moderator settings: escalation, immune and mod roles, mute role, case log, visibility. |
| `/purge` | 🔨 Moderation | Moderators | Bulk-delete recent messages in this channel (MEE6 /clear) |
| `/slowmode` | 🔨 Moderation | Moderators | Set or remove a channel's slowmode (MEE6 /slowmode) |
| `/softban` | 🔨 Moderation | Moderators | Ban and instantly unban a user to delete their recent messages. |
| `/timeout` | 🔨 Moderation | Moderators | Time out a member; over 28 days uses the mute role (MEE6 /mute, /tempmute). |
| `/unban` | 🔨 Moderation | Moderators | Lift a ban by user ID (MEE6 /unban). |
| `/unmute` | 🔨 Moderation | Moderators | Remove a member's timeout or mute role (MEE6 /unmute). |
| `/warn` | 🔨 Moderation | Moderators | Warn a member; warnings can trigger automatic actions (MEE6 /warn). |
| `/automod` | 🤖 Auto-Moderation | Admins | Auto-moderation rules, actions and exemptions (MEE6 Moderator › Auto-moderation). |
| `/logging` | 📜 Audit Logs | Admins | Audit logs: turn on/off, pick log channels, toggle events (MEE6 Moderator › Audit logs). |
| `/goodbye` | 👋 Welcome & Goodbye | Admins | Goodbye message when a member leaves (MEE6 Goodbye message). |
| `/welcome` | 👋 Welcome & Goodbye | Admins | Welcome & Goodbye: join message, card, DM and autoroles (MEE6 Welcome & Goodbye). |
| `/captcha` | 🛡️ Verification | Admins | Captcha verification for new members (MEE6 Welcome › Captcha verification). |
| `/reactionrole` | 🎭 Reaction Roles | Admins | Role panels with buttons, a dropdown or emoji reactions (MEE6 Reaction Roles) |
| `/leaderboard` | ⭐ Levels | Everyone | Show the server XP leaderboard (MEE6 /levels). |
| `/levelconfig` | ⭐ Levels | Admins | Levels settings: XP, level-up messages, role rewards, rank card (MEE6 Levels). |
| `/rank` | ⭐ Levels | Everyone | Show your rank card or a member's (MEE6 /rank). |
| `/xp` | ⭐ Levels | Admins | Manage member XP (MEE6 /give-xp, /remove-xp). |
| `/tag` | 🏷️ Custom Commands | Everyone (some subcommands: Admins) | Custom commands (MEE6 !name): run, create and manage this server's commands. |
| `/trigger` | 🏷️ Custom Commands | Admins | Auto-responders for matching messages (custom commands). |
| `/automation` | ⚙️ Automations | Admins | Automations: When → If → Do rules (MEE6 Automations) |
| `/autotranslate` | 🌐 Auto-Translate | Admins | Set up auto-translation for a channel (up to 5 target languages). |
| `/invites` | 📨 Invite Tracker | Everyone | Invite Tracker: /invites, /inviter, /invites-leaderboard and personal links |
| `/embed` | 🧱 Embed Messages | Admins | Embed Messages: build, publish and edit saved embeds (fields, link buttons) |
| `/reminder` | ⏰ Reminders | Admins | Scheduled channel messages (MEE6 Reminders/Timers) |
| `/giveaway` | 🎉 Giveaways | Moderators | Run giveaways with a Participate button (MEE6 Giveaways, /reroll) |
| `/poll` | 📊 Polls | Moderators | Create and manage polls (MEE6 /poll, /poll-end). |
| `/starboard` | 🌟 Starboards | Admins | Starboards: repost popular messages to a channel (MEE6 Starboards). |
| `/ticket` | 🎫 Ticketing | Everyone | Manage the ticket in this channel (MEE6 /ticket-claim, -close, -reopen, -delete). |
| `/ticket-panel` | 🎫 Ticketing | Admins | Set up ticket panels: options, managers, categories, transcripts (MEE6 Ticketing). |
| `/voice` | 🔊 Temporary Channels | Everyone | Control your temporary voice channel (MEE6 /voice-* commands). |
| `/voice-hub` | 🔊 Temporary Channels | Admins | Join-to-create voice hubs (MEE6 Temporary Channels). |
| `/stats` | 📈 Statistics Channels | Admins | Counter channels with live server stats (MEE6 Statistics Channels). |
| `/birthday` | 🎂 Birthdays | Everyone | Share and see birthdays (MEE6 /remember-birthdays, /birthday, /next-birthdays). |
| `/birthday-config` | 🎂 Birthdays | Admins | Configure birthday announcements and the birthday role (MEE6 Birthdays). |
| `/achievement-config` | 🏆 Achievements | Admins | Set up achievements (MEE6 Achievements, /achievements-reset) |
| `/achievements` | 🏆 Achievements | Everyone | Show a member's achievements, badges and progress (MEE6 /achievements) |
| `/balance` | 🪙 Economy | Everyone | Show your or a member's coins on this server (MEE6 /coins). |
| `/blackjack` | 🪙 Economy | Everyone | Play blackjack against the dealer for coins. |
| `/daily` | 🪙 Economy | Everyone | Claim your daily coins; claim on consecutive days for a streak bonus (MEE6 /daily). |
| `/economy-admin` | 🪙 Economy | Admins | Manage the economy: coins, shop items, settings and reset (MEE6 economy admin). |
| `/economy-boost` | 🪙 Economy | Admins | Manage coin boosts for /daily and /work (MEE6 boosts, up to 10). |
| `/inventory` | 🪙 Economy | Everyone | Show your or a member's items (MEE6 /items). |
| `/pay` | 🪙 Economy | Everyone | Give some of your coins to another member (MEE6 /give-coins). |
| `/pity` | 🪙 Economy | Everyone | Get 500 coins when your balance is below 100 (12-hour cooldown). |
| `/richest` | 🪙 Economy | Everyone | Show the richest members of this server (MEE6 /richest). |
| `/roulette` | 🪙 Economy | Everyone | Bet coins on the European roulette wheel (MEE6 /roulette). |
| `/shop` | 🪙 Economy | Everyone | Browse and buy items from this server's shop (MEE6 /shop /buy). |
| `/slots` | 🪙 Economy | Everyone | Spin the 3×3 slot machine for coins. |
| `/use` | 🪙 Economy | Everyone | Use an item from your inventory (MEE6 /use). |
| `/work` | 🪙 Economy | Everyone | Work a shift to earn coins (MEE6 /work). |
| `/games` | 🎮 Games | Everyone | Browse all games available to play. |
| `/numguess` | 🎮 Games | Everyone | Guess the bot's number (1–100) in 7 tries! |
| `/rps` | 🎮 Games | Everyone | Play Rock, Paper, Scissors against the bot! |
| `/scrabble` | 🎮 Games | Everyone | Challenge another player to a game of Scrabble! |
| `/scrabble-help` | 🎮 Games | Everyone | Learn how to play Scrabble and use its commands. |
| `/scrabble-play` | 🎮 Games | Everyone | Scrabble move: word, pass, exchange or resign |
| `/tictactoe` | 🎮 Games | Everyone | Challenge another player to Tic-Tac-Toe! |
| `/feed` | 📰 Social Feeds | Admins | RSS, YouTube, Reddit, Bluesky and podcast alerts (MEE6 social alerts) |
| `/anime` | 🔎 Search Anything | Everyone | Search AniList for an anime: score, episodes, status, genres. |
| `/manga` | 🔎 Search Anything | Everyone | Search AniList for a manga: score, chapters, status, genres. |
| `/pokemon` | 🔎 Search Anything | Everyone | Look up a Pokémon on PokeAPI: types, abilities, stats. |
| `/urban` | 🔎 Search Anything | Everyone | Look up a word or phrase on Urban Dictionary. |
| `/liverole` | 🔴 Live Role | Admins | Give a role while members stream (MEE6 Twitch Alerts live-streaming role). |
| `/counting` | 🔢 Counting | Admins | A channel where members count up from 1, one number each. |
| `/detectlanguage` | Core | Everyone | Detect the language of a piece of text. |
| `/emoji` | Core | Admins | Add, list or delete emojis (MEE6 /add_emoji) |
| `/help` | Core | Everyone | Show the commands you can use here. |
| `/info` | Core | Everyone | User, server and role info (MEE6 /user-info /server-info /role-info) |
| `/languages` | Core | Everyone | List all supported translation language codes. |
| `/module` | Core | Admins | Turn bot modules on or off for this server. |
| `/settings` | Core | Admins | Server settings: timezone, embed colour, custom-command prefix, /urban (MEE6 Settings). |
| `/translate` | Core | Everyone | Translate text into another language. |

## Setup

Requires Node.js 22 or newer, PostgreSQL 12 or newer and Redis.

```bash
npm install
cp .env.example .env   # fill in DISCORD_TOKEN, CLIENT_ID, DATABASE_URL, REDIS_URL
npx prisma migrate deploy   # apply the committed migrations (new or existing database)
npm run dev                 # start with auto-restart
```

- **Migrations:** `npx prisma migrate deploy` applies everything in `prisma/migrations/` and is what production (and the Docker image) runs. Developers who change `prisma/schema.prisma` run `npm run db:migrate` to create a new migration; CI fails when the schema and the migrations disagree. The `20261010120000_mee6_parity` migration is additive and turns on the modules that already worked before module toggles existed (moderation, reaction roles, and custom commands for servers that already had tags). Existing global economy balances are copied once, lazily, into each server's own balance the first time a member uses the economy there.
- **Font:** `assets/fonts/Nunito-Bold.ttf` must exist (it is committed). Without it the welcome and rank cards render blank text.
- **Privileged gateway intents:** enable **Server Members**, **Presence** and **Message Content** under Bot → Privileged Gateway Intents in the Developer Portal; without them Discord closes the connection with code 4014. Server Members powers welcome messages, autoroles, invite tracking and member counters. Presence powers the online and peak statistics counters and the Live Role. Message Content powers auto-moderation, XP, custom commands and triggers, auto-translate and counting.
- **Permissions:** the bot checks what it needs before each action and tells you which permission is missing. Giving it a role with the permissions of the modules you use (Manage Roles, Manage Channels, Ban/Kick/Moderate Members, Manage Messages, Manage Server for invite tracking) is enough; role-based features need the bot's top role above the roles it manages.
- **Creating roles and channels from a command:** next to each role or channel picker in the setup commands is a matching `new_…` option (for example `/liverole set new_role:Live` or `/logging enable new_channel:mod-logs`). Pick an existing role or channel, or type a name there instead and the bot creates it, reusing one that already has exactly that name (but never a role with powerful permissions, or a public channel where a private log channel is wanted). New roles get no permissions. Log and transcript channels are created hidden from @everyone. Creating needs **Manage Roles** or **Manage Channels**, both for you and for the bot. After updating, run `npm run deploy` so Discord shows the new options.
- **Production:** `npm run pm2:start`, `pm2:reload`, `pm2:logs`.
- **Scrabble dictionary (optional):** put a word list at `assets/dictionaries/words.txt` (one word per line, for example the public-domain ENABLE list; not shipped; about 10-15 MB of RAM per shard) to have `/scrabble-play` validate words. Without it moves are not checked against a dictionary. `/scrabble-play` takes `word`, `pass`, `exchange` or `resign`; `?X` plays a blank as the letter X, and a missed turn (3 minutes) forfeits. Scrabble games live in memory, so a restart ends them.

### Configuration

Required: `DISCORD_TOKEN`, `CLIENT_ID`, `DATABASE_URL`, `REDIS_URL`. Everything else is optional and commented out in `.env.example`.

| Key | Default | Purpose |
|---|---|---|
| `DEPLOY_GUILD_ID` | global | register commands to one guild instantly (global registration takes up to an hour) |
| `BOT_OWNER_ID` | none | your user id: OWNER permission level everywhere; on the dashboard, every server the bot is in |
| `HEALTH_PORT` | 8081 | health endpoints |
| `STATE_DIR` | `./state` | restart-brake history |
| `DEPLOY_HASH_FILE` | `./.deploy-hash` | command hash |
| `PRISMA_LOG_QUERIES` | off | `1` logs every Prisma query |
| `TRANSLATE_PROVIDER`, `LIBRETRANSLATE_URL`, `LIBRETRANSLATE_API_KEY` | `google` | translation backend (below) |
| `HTTP_CONTACT` | `n/a` | contact added to the User-Agent of feed and search requests |
| `REDDIT_USER_AGENT` | standard User-Agent | User-Agent for Reddit feeds |
| `BSKY_APPVIEW_URL` | `https://public.api.bsky.app` | Bluesky AppView |
| `TEST_DATABASE_URL` | none | opt-in database tests |
| `DISCORD_CLIENT_SECRET`, `DASHBOARD_SESSION_SECRET` | none | web dashboard only, required there (below): OAuth2 client secret; session cookie key of at least 32 characters |
| `DASHBOARD_URL`, `DASHBOARD_PORT`, `DASHBOARD_HOST` | `http://localhost:8080`, 8080, `0.0.0.0` | web dashboard only (below): public URL (https makes cookies Secure), listen port and interface |

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
- **Migrations:** the container runs `prisma migrate deploy` before it starts the bot (PostgreSQL 12 or newer). Every schema change needs an additive migration (`npm run db:migrate`), and CI fails when `schema.prisma` and `prisma/migrations/` disagree. Rolling back the image does not roll back the database.
- **Privileged intents:** enable Server Members, Presence and Message Content under Bot → Privileged Gateway Intents in the Developer Portal. Without them Discord closes the connection with code 4014. The Presence intent is used by the statistics counters (online and peak) and the Live Role.
- **Health endpoints** on `HEALTH_PORT` (default 8081):
  - `GET /healthz` returns 200 while the process runs. The image's `HEALTHCHECK` uses it.
  - `GET /readyz` returns 200 only when every shard is connected to Discord. Otherwise it returns 503 with a `reason`: `starting`, `brake-delay`, `shard-not-ready` or `parked:<code>`.
- **Restart brake:** every start is recorded in `STATE_DIR` (`/app/state` in the image; mount a volume there). From the 6th start within an hour, the bot waits 2, 4, 8, 16 and then 30 minutes before connecting, so a crash loop can't use up Discord's 1000 logins per day. A shard that dies, or that Discord disconnects for good with a non-fatal code, ends the process, and Docker's restart policy plus the brake decide when to try again.
- **Parked:** close codes 4004 (bad token), 4013 and 4014 (invalid or disallowed intents), and missing required environment variables, are never retried. The process stays up, logs what to fix, and `/readyz` returns 503 `parked:<code>`. Fix the cause, then restart the container.
- **Slash commands:** the command hash lives at `DEPLOY_HASH_FILE` (`/app/state/.deploy-hash` in the image), so restarts don't re-register unchanged commands. It also covers `CLIENT_ID` and `DEPLOY_GUILD_ID`, so switching between a guild and global registration re-registers on the next start.
- **Shutdown:** `SIGTERM` or `SIGINT` stops the job scheduler, flushes queued audit-log messages, disconnects every shard from Discord and exits within 25 s.

## Web dashboard

Server admins can log in with Discord and change the settings of every server they manage: each module's on/off switch and the options inside it, the server's general settings, and its lists. They can also carry out moderation actions and manage giveaways, polls, panels, member XP and member coins. The dashboard is a separate process, `dashboard.js`, that runs next to the bot. It uses the bot's PostgreSQL database and Redis, and adds no dependencies.

**Setup**

1. In the [Discord Developer Portal](https://discord.com/developers/applications), open your application, go to **OAuth2**, copy the **Client Secret**, and add `<DASHBOARD_URL>/auth/callback` under **Redirects** (for example `http://localhost:8080/auth/callback`). The redirect must match `DASHBOARD_URL` exactly.
2. Add these keys to the bot's env file (see [Configuration](#configuration)):
   - `DISCORD_CLIENT_SECRET`: the client secret from step 1.
   - `DASHBOARD_SESSION_SECRET`: at least 32 characters. Generate one with `node -e "console.log(require('crypto').randomBytes(32).toString('hex'))"`. Changing it logs everyone out.
   - `DASHBOARD_URL`: the public address people open in the browser, origin only with no path. Default `http://localhost:8080`.
   - `DASHBOARD_PORT` (default `8080`) and `DASHBOARD_HOST` (default `0.0.0.0`) are optional.

   The dashboard also uses the bot's `CLIENT_ID`, `DISCORD_TOKEN`, `DATABASE_URL`, `REDIS_URL` and `BOT_OWNER_ID`. Its database must already be migrated (`npx prisma migrate deploy`; the Docker image does this when the bot starts). If a required key is missing or invalid, the dashboard does not start: `/healthz` still answers 200, and every other page answers 503 and names the keys to fix (names only).
3. Start it:
   - **npm:** `npm run dashboard`, or `npm run dashboard:dev` to restart on file changes.
   - **PM2:** `npm run pm2:dashboard` starts the `discord-bot-dashboard` app; `npm run pm2:start` still starts only the bot. Logs go to `./logs/pm2-dashboard-out.log` and `./logs/pm2-dashboard-error.log`. Reload with `pm2 reload discord-bot-dashboard`.
   - **Docker:** the same image runs the dashboard with `node dashboard.js`. Add this service under `services:`, next to `discord-bot`, and point it at the same env file as the bot (with the dashboard keys added to it):

     ```yaml
       dashboard:
         image: ghcr.io/masstarvt/discord-bot:latest
         command: ["node", "dashboard.js"]
         env_file: discord-bot.env
         environment:
           HEALTH_PORT: "8080"
         ports:
           - "8080:8080"
         depends_on:
           - discord-bot
         restart: unless-stopped
     ```

     Setting `HEALTH_PORT` to `8080` makes the image's `HEALTHCHECK` probe the dashboard's `/healthz`, and the port is published on 8080. The dashboard container runs no migrations; `depends_on` starts it after the bot container, which applies them.

- **Who can log in:** any Discord user can sign in, but a server is shown only if they are in it and either own it or have Administrator or Manage Server there (the bot's Admin level). The `BOT_OWNER_ID` user also gets a separate "All servers the bot is in" section and can manage any of those servers, member or not, with the server owner's rights; moderation actions still need them to be a member, as with the slash commands. For a server they are not in, a **Join** button has the bot create a single-use invite that expires after 30 minutes (it needs Create Invite there, and the server's audit log shows the invite as requested by the bot owner). Only the bot owner also sees who added the bot to each server, and when, under the server's name (the bot reads it from the audit log or the server's integrations when it joins, so it needs View Audit Log or Manage Server there; servers it joined earlier, or where it lacked those permissions, are checked again at each restart). Discord's server list is checked on every request and cached for up to a minute. A login lasts at most 7 days, then the user signs in again. If the bot is not in a server, the dashboard offers an invite link instead of its settings.
- **What you can change:** every module's switch and its options (channels, roles, messages and embeds, amounts and limits, automod rules, warning escalation, card styles and so on), and the general settings: time zone, embed colour and custom-command prefix. Channel and role choices are checked against the server before a save.
- **Lists:** saved embeds (create and edit with a live preview; post, move, update or take down the posted message), custom commands, auto-responders, reminders (edit, pause, send a test), social feeds (check now), level rewards, shop items, coin boosts, achievements, starboards, auto-translate channels, translation activity per channel (read-only: messages translated, the languages members write in and the languages they were translated into), voice hubs, counter channels, moderation cases, member levels, member balances, giveaways, polls, reaction-role panels, ticket panels and automations (with a "Check for problems" action). Moderation cases, member levels, member balances, giveaways and polls have a search box, and a "Load more" button for long lists. A new level reward is given to members who reach that level later; `/levelconfig role sync` in Discord gives it to members already past it. Deleting a saved embed does not delete its posted message, so take that down first.
- **Actions:** moderation cases (warn, time out, remove a timeout or mute, kick, softban, ban, temporary ban or unban; edit a case's reason or proof link; clear a member's warnings, for administrators), member XP (give, remove, set or reset), giveaways (start, end now, reroll, restart a draft, cancel, change the bonus odds), polls (start, end now, delete) and panels (reaction roles and tickets: create, edit, post, update and delete). Member coins (give, remove, set or reset) are changed directly. Moderation actions follow the same rules as the slash commands: your highest role must be above the member's, and immune members are protected. Cases, XP, giveaways, polls and panels are carried out by the bot, which must be online. The dashboard waits up to 12 seconds for it, which is usually a few seconds. If the bot does not pick the action up, nothing is done and the dashboard says so. If it is still working when the dashboard stops waiting, the list says so, and a refresh shows the result.
- **Not in the dashboard:** posting the verification panel (the dashboard edits one that is already posted); attaching a reaction-role panel to a message that already exists (`/reactionrole attach`); giving a reaction-role panel a role with moderator or admin permissions (`/reactionrole add` asks you to confirm); the staff actions inside a ticket, such as claim, close, add a member or transcript (`/ticket`); `/purge` and `/slowmode`; the list of who entered a giveaway (`/giveaway entrants`); giving XP to every member of a role (`/xp give target:@Role`); giving or taking shop items (`/economy-admin item give` and `remove`); exporting and importing automations as JSON (`/automation export` and `import`); resetting a member's achievement progress (`/achievement-config reset`); and emoji management (`/emoji`). The dashboard never creates or deletes Discord channels or roles: voice hubs and counter channels use a channel that already exists, and deleting one leaves its channel in place. Everything not listed here is still set with the slash commands (see [Commands](#commands)).
- **Propagation:** the bot keeps each server's settings in memory for 30 seconds, so a change reaches it within about 30 seconds. Custom commands and auto-responders take up to a minute, starboards a minute, achievements and auto-translate channels 30 seconds, voice hubs about 5 minutes, and automations about a minute. Counter channels are renamed about every 10 minutes, and Discord limits how often a channel can be renamed; "Update the counters now" asks for an update within about a minute. Feeds are checked every 5 to 30 minutes, depending on the source. Reminders, feeds, saved embeds, shop items, coin boosts and level rewards apply as soon as they are saved. No restart is needed.
- **HTTPS:** run the dashboard behind an HTTPS reverse proxy in production, and set `DASHBOARD_URL` to the public `https://` address. That makes the login cookie `Secure` and turns on HSTS. If the proxy runs on the same machine (not in Docker), set `DASHBOARD_HOST=127.0.0.1` so the port is reachable only through the proxy.
- **Secrets:** keep `DASHBOARD_SESSION_SECRET` and `DISCORD_CLIENT_SECRET` out of version control. Pages are served with a strict Content-Security-Policy, and cross-site writes are refused.

## Translation backend

Auto-translate and `/translate` use Google's free endpoint by default. It has no API key, and it rate-limits (HTTP 429) quickly under chat traffic. For steady use, point the bot at a self-hosted [LibreTranslate](https://github.com/LibreTranslate/LibreTranslate) server:

```env
TRANSLATE_PROVIDER=libretranslate
LIBRETRANSLATE_URL=http://<host>:5000
# LIBRETRANSLATE_API_KEY=   # only if the server requires keys
```

`TRANSLATE_PROVIDER` is `google` or `libretranslate`; any other value logs a warning and uses Google. On an HTTP 429 the bot pauses translation (30 minutes for Google, 1 minute for LibreTranslate, or the server's `Retry-After` when that is longer, capped at 6 hours) instead of retrying on every message, and logs once. The pause is shared between shards through Redis. Auto-translate translates the first 1,000 characters of a message and shows up to 1,000 characters per language. Traditional Chinese is `zh-tw`; the aliases `iw`, `nb`, `zh-cn`, `zh-hant` and `fil` are accepted.

## External services

No built-in feature needs an API key. Outbound requests go through an SSRF-safe fetcher with size and time limits and a User-Agent that carries `HTTP_CONTACT`.

- **Built, keyless:** RSS and Atom feeds, YouTube channel feeds (the public Atom feed; YouTube sometimes answers 404 for it, and the bot backs off and eventually disables a feed that keeps failing), Reddit subreddit `.rss` feeds (set `REDDIT_USER_AGENT`, Reddit throttles generic agents), Bluesky accounts through the public AppView, podcasts (iTunes Search plus the RSS feed), `/anime` and `/manga` (AniList), `/pokemon` (PokeAPI), `/urban` (Urban Dictionary; answers only in age-restricted channels unless an admin allows it with `/settings urban`), and the Live Role, which uses Discord's streaming presence rather than the Twitch API. Details and limits: `docs/external-api-research.md`.
- **Research only, not integrated:** Twitch alerts (Helix/EventSub), YouTube Data API, the Reddit Data API, X/Twitter, Instagram, TikTok, Kick, social counters, music and AI features. Each needs a key, an OAuth app, a paid plan or unstable scraping; the trade-offs are written up in [docs/external-api-research.md](docs/external-api-research.md).

## Upgrading from earlier versions

- `/history` is now `/history view` and `/history clear`; `/scrabble-play` is now `word`, `pass`, `exchange` and `resign` subcommands.
- Custom commands answer to `<prefix><name>` (default `!`) as well as `/tag use`; `/settings prefix off` disables the prefix. Servers that already had tags are switched to the Custom Commands module by the migration, so their tags start answering to the prefix.
- Economy balances are per server. Level-up messages use the MEE6 placeholder set (`{user}` is a mention, `{user.name}` the name; old messages are rewritten by the migration). The single `xpRate` became an `xpMin`/`xpMax` range (15 to 25 by default).
- Slash commands are registered for servers only, except `/translate`, `/detectlanguage` and `/languages`, which also work in DMs.

## Development and tests

```bash
npm test                      # every test/*.test.js, fully offline
node --test test/economy.test.js   # one file
```

The suite needs no Discord, Postgres, Redis or network. `test/integration.test.js` also fails when a command, module, job type, plugin or environment variable is added without updating this README, `CLAUDE.md` and `.env.example`. To run the database tests too, point `TEST_DATABASE_URL` at a migrated throw-away Postgres and run `node --test --test-concurrency=1 "test/*.test.js"`. CI (`.github/workflows/ci-publish.yml`) syntax-checks every file, validates the Prisma schema, runs the offline suite, applies the migrations to an empty PostgreSQL 17, runs the suite again with the database tests, and fails on schema/migration drift. `CLAUDE.md` documents the architecture for contributors.

## License

MIT — see [LICENSE](LICENSE).
