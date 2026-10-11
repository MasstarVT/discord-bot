# External API research

**Covers:** every MEE6-parity feature that depends on a third-party service: what it would take, what it costs, and which ones were built in the MEE6-parity effort.
**Prepared:** 2026-10-09. Some live checks ran on 2026-10-10 UTC.
**Status reviewed:** 2026-10-11 against the code on branch `mee6-parity`.

## How to read this document

**Status labels**

| Label | Meaning |
|---|---|
| **Built (keyless)** | Implemented in the MEE6-parity build. Uses public endpoints only: no API key, OAuth app or payment. |
| **Built (Discord-only)** | Implemented in the MEE6-parity build. Uses only Discord gateway data. |
| **Already supported** | Shipped in the bot before the MEE6-parity effort. |
| **Research only — not integrated** | Needs keys, OAuth, provider approval or payment, or is outside this effort's scope. Nothing is wired up; the section records what integrating it would take. |

**Evidence labels**

- **Dates:** every price, limit and policy was checked against the provider's own page on 2026-10-09 unless noted. Sources show the page's "last updated" date when it has one, otherwise "accessed 2026-10-09".
- **Unverified:** the provider's own page could not be read at research time, so the claim comes from secondary sources. reddit.com, redditinc.com, apidocs.imgur.com, support.deepl.com and the Discord developer-policy page all block automated fetches.
- **Observed:** taken from a live API response (headers, status codes, language lists) on the date given.
- **As built:** a fact about the shipped code, checked against the source on the "Status reviewed" date. It describes what the bot does, not what a provider documents.
- **Effort:** **S** is about a day, **M** a few days, **L** a week or more, or ongoing operational risk. "—" means the research gave no estimate.

---

## 1. At a glance

### 1a. Built in this effort

| Feature | Source | Auth | Price | Binding limit / main caveat | Delivery | Effort | Command | Status |
|---|---|---|---|---|---|---|---|---|
| RSS / Atom feeds (any site) | The feed URL itself | None | Free | Limits and terms are per site; not researched | Poll | — | `/feed add rss` | **Built (keyless)** |
| YouTube new uploads | Channel feed `youtube.com/feeds/videos.xml?channel_id=…` | None | Free | Undocumented. **Returned HTTP 404 for every channel tried on 2026-10-10** (observed). No live flag, about 15 latest entries, ToS grey area. | Poll | — | `/feed add youtube` | **Built (keyless)** |
| Reddit new posts | `reddit.com/r/{sub}/new/.rss` | None | Free | About 25 latest items. Some datacenter clients get 403. Liable to disappear without notice (unverified). | Poll | S (fragile) | `/feed add reddit` | **Built (keyless)** |
| Bluesky new posts | `public.api.bsky.app` `getAuthorFeed` (as built; the profile RSS is not used) | None | Free | Rate limits not published (unverified) | Poll | S | `/feed add bluesky` | **Built (keyless)** |
| Podcast new episodes | iTunes Search to find the show, then the show's own feed | None | Free | Limits and terms not researched | Poll | — | `/feed add podcast` | **Built (keyless)** |
| `/anime`, `/manga` | AniList GraphQL (`graphql.anilist.co`) | None | Free | AniList: 30 requests/min (observed). As built, the bot caches results for 6 h (1 h when empty) and makes at most 24 AniList lookups a minute across all shards. | Request/response | S | `/anime`, `/manga` | **Built (keyless)** |
| `/pokemon` | PokeAPI v2 | None | Free | No rate limit, but fair use requires local caching. As built, responses are cached for 7 days. | Request/response | S | `/pokemon` | **Built (keyless)** |
| `/urban` | Urban Dictionary | None | Free | Undocumented API, no published terms (unverified). Content is often NSFW. As built, `/urban` runs only in age-restricted channels unless an admin turns that off (`/settings urban`). | Request/response | S | `/urban` | **Built (keyless)** |
| Twitch "live" auto-role | Discord presence (`STREAMING` activity) | None; the Presences intent is already enabled | Free | Uses Discord presence data only; no Twitch API | Gateway events | — | `/liverole` | **Built (Discord-only)** |

### 1b. Research only — not integrated

| Feature | Recommended source | Auth | Free tier / price | Binding limit | Delivery | Effort | Research verdict |
|---|---|---|---|---|---|---|---|
| Twitch live alerts | Helix `GET /streams` polling now; EventSub through a **conduit with a WebSocket shard** later | App access token (client credentials) | Free | 100 `user_id`s per call; token-bucket rate limit | Poll (no public URL), or EventSub | S (poll) / M (conduit) | Do it |
| YouTube uploads / live (API) | Data API v3: `playlistItems.list` on the uploads playlist, then `videos.list` for live state. WebSub optional. | API key | Free: 10,000 units/day; `search.list` has its own 100 calls/day | Quota: about 100 channels at a 15-minute poll | Poll; WebSub needs public HTTPS | M | Do it (the RSS feed currently returns 404) |
| Reddit Data API | OAuth API, after Reddit approves the app | OAuth (needs approval) | 100 QPM free, approval-gated (unverified) | Approval queue; unauthenticated `.json` blocked since May 2026 | Poll | M | Low priority |
| X / Twitter | X API v2, pay-per-use | Bearer token (app-only) | **No free tier.** $0.005 per post read, $0.010 per user read | 3M post reads per month, then Enterprise | Poll, or paid webhooks | M plus ongoing spend | Defer |
| Instagram | Graph API Business Discovery | Facebook Login plus your own IG professional account | Free, behind Meta's permission process | Professional (Business/Creator) accounts only | Poll | L | Not feasible for arbitrary accounts |
| TikTok | Display API, for creators who link their own account | Per-user OAuth | Free | Only users who authorize the app | Poll / webhook | L | Not feasible for arbitrary accounts |
| Kick live alerts | Public API `GET /public/v1/channels`, or the `livestream.status.updated` webhook | App access token (client credentials) | Free | 50 channels per call; 10,000 subscriptions per event type | Poll, or webhook (public HTTPS) | S (poll) / M (webhook) | Do it |
| Music playback | Lavalink 4.2.x plus SoundCloud / direct HTTP; YouTube only through unofficial plugins | Varies | Software is free; the cost is legal and ToS risk | YouTube ToS; Spotify development-mode caps; DAVE voice E2EE required | Voice | L | Defer |
| Other search commands | `/wiki` (Wikipedia), `/weather` (Open-Meteo), `/youtube search`, `/imgur`; Jikan as an anime alternative; Kitsu (MEE6's anime source; not used — AniList was chosen) | None for Wikipedia, Open-Meteo, Jikan; key for YouTube, Imgur, OpenWeatherMap | Free (Open-Meteo is non-commercial only) | Per API (§4.9) | Request/response | S each | Pick the keyless ones |
| AI chat / persona | Claude API with `claude-haiku-5-5` (OpenAI as the alternative) | API key | Pay-as-you-go: $0.10 / $0.50 per MTok in/out | Org rate limits and your own spend cap | Request/response | M | Optional, with a budget cap |
| Translation (paid providers) | **Azure Translator F0** or **Google Cloud Translation v2**; DeepL and Amazon also compared | API key (Amazon: AWS SigV4) | Azure: 2M chars/month free. Google: 500K. DeepL Free: 500K. | Characters | Request/response | S per provider, after an M refactor | Do it next |

**Translation today:** the keyless Google `gtx` endpoint (the default) and a self-hosted LibreTranslate server are **already supported**, selected with `TRANSLATE_PROVIDER`.

---

## 2. Shared plumbing for feeds and alerts

Every feed and alert integration, keyless or keyed, has the same shape, so the plumbing is built once.

**As built (keyless feeds):** one recurring scheduler job `feed.poll` per feed (dedupe key `feed.poll:{feedId}`). Jobs are claimed only by the shard that owns the guild (`FOR UPDATE SKIP LOCKED`), so there is no shard-0 poller and no Redis lock, and alerts are never duplicated across shards. Each poll posts with `channel.send` from that shard, so the cross-shard REST post described in §2.1 is not used. The `/feed` command (admin level; module `feeds`) adds a feed with `/feed add rss|youtube|reddit|bluesky|podcast` and manages feeds with `/feed list`, `remove`, `test`, `edit`, `enable` and `disable`.

### 2.1 Where the poller runs (sharding)

Every shard runs `src/bot.js`. A `setInterval` poller placed in `bot.js` or in a `ready` event fires **once per shard** and posts duplicate alerts. The ShardingManager uses `totalShards: 'auto'`, so this only shows up once the bot crosses Discord's shard threshold.

| Pattern | How | Trade-off |
|---|---|---|
| 1. Shard 0 only | `if (client.shard?.ids.includes(0) ?? true) startPollers(client)` | Simplest. Alerts stop while shard 0 is down (the process exits anyway when a shard dies). |
| 2. Redis lock | `SET alerts:lock:{platform} <shardId> NX PX 90000`, renewed every tick; the lock holder polls | More robust, still cheap on Redis. |
| 3. Main thread (`index.js`) | Run pollers next to the health server; post through `@discordjs/rest` | Cleanest home for **webhook receivers**: the main thread already owns an HTTP server and outlives shard restarts. Costs a second REST client. |

**Research recommendation (not what was built; see the note at the top of this section):** pollers on shard 0 or behind a Redis lock; any future webhook receiver in the main thread.

**Delivery gotcha:** in discord.js 14.27, `client.channels.fetch(id)` returns nothing for a channel whose guild isn't cached on that shard unless you pass `{ allowUnknownGuild: true }`. Post alerts with `client.rest.post(Routes.channelMessages(channelId), { body })` instead; that works from any shard and from the main thread.

### 2.2 Polling vs. webhook receivers

- Twitch EventSub webhooks, YouTube WebSub, Kick webhooks and X Activity webhooks all need a **publicly reachable HTTPS callback**; Twitch also requires **port 443**. Self-hosted, that means a reverse proxy (Caddy, Traefik) or a tunnel (Cloudflare Tunnel), plus signature verification.
- `src/utils/healthServer.js` serves only `/healthz` and `/readyz`. Don't expose `HEALTH_PORT` publicly for webhooks; add a separate listener on `WEBHOOK_PORT` with a `PUBLIC_BASE_URL`.
- **Polling needs none of that**, so it is the first step wherever a platform allows it. Twitch also has a middle path, conduits with WebSocket shards (§4.1): push delivery with no public URL.

### 2.3 Data model sketch

**As built:** keyless feeds use the `Feed` model in `prisma/schema.prisma` (`FeedType`: `RSS`, `YOUTUBE`, `REDDIT`, `BLUESKY`, `PODCAST`), with first-poll seeding (`initialized`), `seenIds` (the newest 100), ETag and Last-Modified, failure counting and auto-disable (`disabledReason`), gated by the `feeds` module (`feedsEnabled`, on by default). The `SocialAlert` sketch below is kept as the proposal for keyed live alerts (Twitch, Kick); it is not built, and neither are the `alertsEnabled` toggle or the `alerts:*` Redis keys in the table below. The numbers in the code:

- **Failure threshold:** 20 consecutive failed checks switch the feed off (`disabledReason` reads like "20 failed checks in a row: HTTP 404") and post a notice. `/feed enable` resets the count and re-seeds the feed.
- **Back-off:** each consecutive failure doubles the wait before the next check, up to 32 times the base interval and never more than 12 h. A `Retry-After` is honoured up to 6 h, a Reddit 403 waits at least 1 h (a Reddit 429 starts a bot-wide block instead, §3.3), and every delay gets ±10 % jitter and is never under a minute.
- **Poll interval:** set per source and not configurable: Bluesky 5 min, Reddit 10 min, RSS and YouTube 15 min, podcasts 30 min. At most 5 new items are posted per check (the newest five, oldest first), and a server can follow 30 feeds.
- **Redis keys in use** (all optional; with Redis down the poll goes straight to the network): `feed:fetch:{sha1(url)}` (fetched feed shared between guilds, TTL = poll interval minus 30 s), `feed:block:reddit`, `feed:rl:reddit:{slot}` and `feed:itunes:rl` (see §3.3 and §3.5).

```prisma
model SocialAlert {
  id               Int      @id @default(autoincrement())
  guildId          String
  platform         String   // 'twitch' | 'youtube' | 'kick' | 'bluesky' | 'reddit' | ...
  externalId       String   // Twitch user_id, YouTube channel ID, Kick broadcaster_user_id, Bluesky DID, subreddit
  displayName      String
  discordChannelId String
  mentionRoleId    String?
  message          String?  // template for placeholderParser: {streamer} {title} {url} {game}
  lastSeenId       String?  // last video/post/stream id posted
  isLive           Boolean  @default(false)
  createdAt        DateTime @default(now())
  guild            GuildSettings @relation(fields: [guildId], references: [guildId])

  @@unique([guildId, platform, externalId, discordChannelId])
  @@index([platform, externalId])
}
```

- **Module toggle:** `alertsEnabled` on `GuildSettings` and in `DEFAULT_MODULE_SETTINGS`, plus `alerts: 'alertsEnabled'` in `MODULE_FLAGS`, following the repo's "Adding a New Module" steps.
- **Poll by platform and fan out:** query the distinct `(platform, externalId)` pairs, call the source once per batch, then post to every subscribed guild channel.

| Redis key | Holds |
|---|---|
| `alerts:lock:{platform}` | The poller lock (pattern 2) |
| `alerts:state:{platform}:{externalId}` | Last-known live or stream state, 24 h TTL |
| `alerts:seen:{platform}:{externalId}` | A small set of recently posted IDs for dedupe. EventSub, Kick and TikTok webhooks are at-least-once, so the same event can arrive twice. |

### 2.4 Caveats that apply to every poller

- **First-poll seeding:** when a subscription is added, the first poll should record the current items as seen (`lastSeenId`) without posting them. Otherwise the channel receives the feed's whole backlog (about 15 entries for a YouTube channel feed, about 25 for a subreddit feed).
- **Rate limits and backoff:** poll politely, back off hard on 403/429 (reuse the circuit-breaker pattern in `translationService.js`), and send a descriptive `User-Agent` with contact details where the provider asks for one (Reddit, Wikipedia).
- **Terms of service:** each provider's terms are in its section. Some keyless feeds (YouTube's channel feed, Reddit's `.rss`) are undocumented or discouraged, so treat them as best-effort.
- **Outgoing message safety:** send with `allowedMentions: { roles: [mentionRoleId] }`. A streamer's or poster's title can contain `@everyone`.

**As built:** the code implements these caveats. *First-poll seeding:* adding a feed probes it once and records what it already shows (up to 100 ids) without posting; a feed that could not be fetched at add time is seeded by its first successful check instead. *Dedupe:* item ids are kept in `seenIds` and recorded before posting, so a crash can lose an item but never repeat one, and items that could not be sent because of a permission or network error are retried on the next check. *Back-off:* the per-feed rules in §2.3, plus a bot-wide Reddit pause and budget (§3.3); the circuit breaker in `translationService.js` is not reused. *User-Agent:* every request goes through `safeFetch`, whose default is `DiscordBot/1.0 (+self-hosted; contact: <HTTP_CONTACT, or n/a>)`, and Reddit feeds use `REDDIT_USER_AGENT` instead when it is set. *Outgoing messages:* `allowedMentions` is `{ parse: [], roles }`, where `roles` holds only the feed's ping role, and only when the bot is allowed to ping it in that channel.

---

## 3. Built (keyless)

Sections 3.1 to 3.6 are **Built (keyless)**; section 3.7 is **Built (Discord-only)**.

### 3.1 RSS / Atom feeds

**What it powers:** posts for new items from any feed URL (MEE6 "RSS Feeds", Premium).

| Item | Detail |
|---|---|
| Source | The feed URL itself (outbound HTTP to a public, keyless endpoint) |
| Parsing | `fetch` plus `fast-xml-parser` (5.11.2, checked 2026-10-09), the same parser suggested for YouTube Atom and Reddit RSS |
| Caveats | Limits and terms are per site and were not researched. Apply first-poll seeding, dedupe and backoff (§2.4). |

**As built:** `/feed add rss url:<feed URL or a site that links to one> channel:#… [ping_role] [message]`. The URL must be http or https, carry no credentials and be at most 500 characters; if it returns a web page, the bot follows one autodiscovered feed link. Fetching uses `safeFetch` (SSRF guard, 10 s timeout, 2 MB cap, up to 5 redirects), not plain `fetch`, and parsing uses `fast-xml-parser` 5.11.2 for RSS 2.0, Atom and RSS 1.0 (RDF). Checks are conditional (ETag, Last-Modified) and run every 15 min; the fetched feed is shared between guilds through `feed:fetch:{sha1(url)}` with a TTL of the poll interval minus 30 s (870 s for RSS and YouTube, 570 s for Reddit, 270 s for Bluesky, 1,770 s for podcasts). Items are deduped by their `guid` or Atom `<id>`, falling back to the link. Posts are an embed card with the default text `📰 New from **{feed}**`. Placeholders: `{title}`, `{url}`, `{link}`, `{author}` and `{feed}`, plus the server and channel placeholders (such as `{server}` and `{channel}`) and `{random:x:y}`; there are no `{user…}` placeholders in feeds, and the `message` option takes `\n` for a new line (at most 1000 characters).

### 3.2 YouTube new uploads via the channel feed

**What it powers:** "New video from X" posts (MEE6 "YouTube Alerts", Premium, placeholder `{youtube_channel}`).

| Item | Detail |
|---|---|
| Endpoint | `https://www.youtube.com/feeds/videos.xml?channel_id=…` |
| Auth / price | None / free |
| Contents | About 15 latest entries. **No live flag**, so "is live now" can't be detected from the feed. |
| Reliability | **Undocumented and currently broken.** Returned **HTTP 404 for every channel tried on 2026-10-10 UTC** (observed). Developers have reported repeated outages since Dec 2025, the latest from 2026-10-04. |
| ToS | The YouTube API Services Developer Policies (2026-09-14) prohibit scraping and non-API access (§III.E.6, §III.I.14). Relying on this endpoint is a grey area as well as unreliable. |
| Channel identification | The feed is keyed by channel ID (`UC…`). Resolving an `@handle` needs the Data API key or scraping. |
| Fallback | Data API polling (§4.2) is the documented route: research only, needs an API key. Whether WebSub keeps delivering while the feed 404s is **unverified**. |

**As built:** `/feed add youtube youtube_channel:<UC… channel ID or youtube.com/channel/UC… link> channel:#… [ping_role] [message]`. `@handle`, `/c/` and `/user/` forms are refused, because resolving them needs the Data API key the bot does not use. The bot polls `https://www.youtube.com/feeds/videos.xml?channel_id=…` (Atom, parsed with `fast-xml-parser` as in §3.1) every 15 min. A feed that fails when it is added, a 404 included, is still accepted with a warning; the bot keeps trying, `/feed list` shows the error, and the feed switches itself off after 20 failed checks in a row. The Data API and WebSub are not used. Placeholder: `{youtube_channel}` (the entry's author, else the feed's name). The default message is `📺 **{youtube_channel}** just posted a new video!` followed by `{link}`, with no embed card, so Discord shows the video preview.

**Sources:**
- https://developers.google.com/youtube/terms/developer-policies (last updated 2026-09-14)
- RSS outage reports: https://github.com/joaoccaldas/konam/issues/102 (Oct 2026), https://github.com/ocaml/ocaml.org/issues/3512 (Feb 2026), https://discuss.ai.google.dev/t/youtube-rss-feed-endpoint-returns-404-errors/113379 (Dec 2025)

### 3.3 Reddit new posts via `.rss`

**What it powers:** "New post in r/X" feeds (MEE6 "Reddit Alerts", Premium).

**Every Reddit claim here is unverified:** no Reddit primary source could be read, so it all comes from secondary reporting.

| Item | Detail |
|---|---|
| Endpoint | `https://www.reddit.com/r/{sub}/new/.rss` (Atom) |
| Auth / price | None / free. Reported to still work without auth. |
| Contents | Roughly 25 latest items: title, link, author, excerpt. No scores. |
| Reliability | Some automated or datacenter clients get 403. Reddit has hinted that RSS is a scraping surface, so treat it as **liable to disappear without notice**. Present it to users as best-effort. |
| Related | Unauthenticated `.json` is reported **blocked with 403 since late May 2026**, including from residential IPs; spoofing a browser User-Agent doesn't help. The OAuth Data API is approval-gated (§4.3). |
| Library | `snoowrap` is **deprecated on npm** (last release 2021). Use `fetch` plus `fast-xml-parser`. |

**Implementation notes:**
- Poll each subscribed subreddit every 5–10 minutes from a single poller, with a descriptive User-Agent that includes contact info.
- Dedupe by the Atom `<id>` (`t3_…`).
- Back off hard on 403/429, reusing the circuit-breaker pattern in `translationService.js`.
- Store the subreddit name and the last entry ID.

**Effort:** S, but fragile.

**As built:** `/feed add reddit subreddit:<name, r/name or reddit.com link> channel:#… [ping_role] [message]` (2 to 21 letters, digits or underscores; user profiles are refused). The bot polls `https://www.reddit.com/r/{sub}/new/.rss` (Atom, parsed with `fast-xml-parser` as in §3.1) every 10 min, deduping by the entry `<id>`, with `REDDIT_USER_AGENT` as the User-Agent when it is set. The protection is bot-wide and lives in Redis: at most 8 network fetches a minute across all Reddit feeds (`feed:rl:reddit:{slot}`, with an in-process count while Redis is down), and a 429 starts a block (`feed:block:reddit`, the `Retry-After` clamped to 10 to 60 min, 15 min when absent) during which no Reddit feed is fetched; that 429 is not counted as the feed's failure. A 403 does count as a failure and waits at least 1 h before the next try. When a subreddit is added, a 404 refuses it ("Subreddit Not Found") and any other failure is accepted with a warning. The code does not reuse the circuit breaker in `translationService.js` and does not run a single poller; each feed has its own `feed.poll` job (§2). Placeholder: `{subreddit}`. The default message is `👽 New post in **r/{subreddit}**` with an embed card.

**Sources (secondary, unverified against Reddit):**
- https://prowlo.com/blog/reddit-data-api (vendor blog; "last updated Aug 12, 2026")
- r/redditdev announcement of 2025-11-11 via mirror: https://r.datuan.dev/r/redditdev/comments/1oug31u/introducing_the_responsible_builder_policy_new/noeamby
- https://dev.to/tonywangca/why-reddit-blocked-unauthenticated-json-in-2026-and-how-to-still-get-reddit-data-58b9 (accessed 2026-10-09)
- https://blog.betterbird.eu/2025/12/reddit-feeds-in-error-whats-going-on (Dec 2025)

**Canonical Reddit pages to check in a browser** (not readable by automated fetch):
- https://www.reddit.com/r/redditdev/comments/1oug31u/ (Responsible Builder Policy announcement, 2025-11-11)
- https://support.reddithelp.com/hc/en-us/articles/42728983564564-Responsible-Builder-Policy
- https://support.reddithelp.com/hc/en-us/articles/16160319875092-Reddit-Data-API-Wiki
- https://redditinc.com/policies/data-api-terms

### 3.4 Bluesky (AT Protocol)

**What it powers:** "New post from @handle" feeds (MEE6 "Bluesky Alerts", beta, Premium; placeholders `{bluesky_name}` `{bluesky_handle}`). It is the only social alert with a documented, keyless, public read API.

| Item | Detail |
|---|---|
| Public AppView | `GET https://public.api.bsky.app/xrpc/app.bsky.feed.getAuthorFeed?actor={handle\|did}&filter=posts_no_replies&limit=10`. **No auth.** `limit` max is 100. |
| Observed | 200 OK on 2026-10-10, with no rate-limit headers. |
| Rate limits | Published limits for the public AppView couldn't be retrieved (the docs site moved to `endpoints.bsky.app`). Treat them as **unverified** and poll politely: one batch per minute or slower. |
| RSS alternative | `https://bsky.app/profile/{handle}/rss` redirects to `/profile/{did}/rss` and returns `application/xml` (observed 302 then 200, 2026-10-10). **Public posts only**; inline links are stripped. |
| Real time | Jetstream or the firehose can deliver posts as they happen; overkill for a few dozen accounts. |
| Library | `@atproto/api` 0.24.3 (2026-10-09), or plain `fetch` |

**Gotchas:**
- Handles can change: resolve to and store the **DID**.
- The author feed includes reposts (a `reason` field). Filter them unless the guild wants them.
- Users who hide their posts from logged-out viewers won't appear.

**Effort:** S. The research recommended doing it first: it proves the shared framework at zero cost.

**As built:** `/feed add bluesky handle:<handle, bsky.app profile link or DID> channel:#… [ping_role] [message] [include_reposts]`. The handle is resolved once with `GET {AppView}/xrpc/app.bsky.actor.getProfile?actor=…` and the DID is stored; every 5 min the bot calls `GET {AppView}/xrpc/app.bsky.feed.getAuthorFeed?actor={did}&filter=posts_no_replies&limit=30` and parses the JSON (no XML). `{AppView}` is `https://public.api.bsky.app`, or the value of `BSKY_APPVIEW_URL` when it is set. Pinned posts are dropped, and reposts are skipped unless `include_reposts` is on. The feed's name follows the account's current `@handle`. The profile RSS and `@atproto/api` are not used (plain `safeFetch`). Placeholders: `{bluesky_name}` and `{bluesky_handle}`. The default message is `🦋 **{bluesky_name}** (@{bluesky_handle}) posted on Bluesky` followed by `{link}`, with no embed card, so Discord shows the post preview.

**Sources:**
- Live probes of `public.api.bsky.app` and `bsky.app/profile/…/rss` (2026-10-10 UTC)
- https://openrss.org/blog/bluesky-has-launched-rss-feeds (Jan 2024)
- API reference now at https://endpoints.bsky.app/ (accessed 2026-10-09)

### 3.5 Podcasts

**What it powers:** new-episode posts (MEE6 "Podcast Alerts", Premium; placeholders `{podcast_title}` `{episode_title}`).

| Item | Detail |
|---|---|
| Source | iTunes Search (keyless) to find the show, then polling the show's own feed |
| Caveats | iTunes Search's rate limits and terms were **not researched**. Its keyless status comes from the parity research's general knowledge, not from a provider page. Feed polling follows §2.4 and §3.1. |

**As built:** `/feed add podcast podcast:<name or RSS feed URL> channel:#… [ping_role] [message]`. A name is searched with `GET https://itunes.apple.com/search?media=podcast&entity=podcast&limit=5&term=…`, capped at 20 searches a minute bot-wide (`feed:itunes:rl`), and the first result that exposes a feed URL is used; the reply names the match so an admin can correct it. The show's own feed is then polled like §3.1 (conditional requests, `fast-xml-parser`) every 30 min, with a 10 MB size cap instead of 2 MB. Placeholders: `{podcast_title}` and `{episode_title}`. The default message is `🎙️ New episode of **{podcast_title}**: {episode_title}` with an embed card (the feed's own image as the thumbnail when the episode has none).

### 3.6 Search commands: `/anime`, `/manga`, `/pokemon`, `/urban`

Request/response slash commands, with no alert framework needed. As built, results are cached in Redis at `search:{api}:{sha1(source)}`, where `api` is `anilist`, `pokeapi` or `urban` and `source` is the cleaned, lower-cased query (for AniList it also holds the media type and whether adult titles are allowed). TTLs: 6 h for AniList and Urban Dictionary, 7 days for PokeAPI, and 1 h for an empty result.

| Command | API | Auth | Limits / terms | Notes |
|---|---|---|---|---|
| `/anime`, `/manga` | **Kitsu** (MEE6's source) | None (per the parity research's general knowledge) | Not researched | **Not used (AniList chosen).** Check Kitsu's terms and limits before shipping. |
| `/anime`, `/manga` | **AniList GraphQL** `https://graphql.anilist.co` | None for public queries | **Observed `X-RateLimit-Limit: 30`** (per minute) on 2026-10-10. The docs page returned 403. | Honor `X-RateLimit-Remaining` and `Retry-After`. |
| `/pokemon` | **PokeAPI v2** | None | No rate limit since 2018, but **fair use requires local caching**; abusers' IPs get banned | Cache responses long-term; Pokémon data rarely changes. |
| `/urban` | **Urban Dictionary** `api.urbandictionary.com/v0/define?term=` | None | **Undocumented**; no published API terms found (unverified) | Content is often NSFW. Restrict to age-restricted channels or filter. |

MEE6's "Search Anything" plugin (free tier) uses Kitsu, PokeAPI and Urban Dictionary for these commands.

**Effort:** S each.

**As built:** the module is `search` (Search Anything, on by default) and all four commands are open to every member.

- `/anime query:` and `/manga query:`: `POST https://graphql.anilist.co` with a GraphQL `Page` query (JSON) for the 5 best matches. Adult titles are filtered out (`isAdult: false`, and checked again in the bot) unless the channel is age-restricted, so those channels cache separately.
- `/pokemon name:`: `GET https://pokeapi.co/api/v2/pokemon-species/{name}` plus `/pokemon/{name}` (forms and numbers work too). The name autocomplete reads an in-memory index built from `/pokemon-species?limit=2000` (kept 24 h and stored in Redis as `search:pokeapi:index` for 7 days), so autocomplete never waits for the network.
- `/urban term:`: `GET https://api.urbandictionary.com/v0/define?term=…`, the first 5 definitions. **NSFW gate:** it refuses to run outside age-restricted channels (a thread uses its parent channel's flag) while the server's `urbanNsfwOnly` setting is on, which is the default. `/settings urban nsfw-only:False` opens it to every channel, and an unreadable setting counts as on.
- **Limits:** 8 s timeout per request, a 3 s cooldown per user (`search:cd:{userId}`), and a bot-wide budget per minute and source of 24 AniList, 30 Urban Dictionary and 30 PokeAPI lookups (`search:budget:{api}:{minute}`). A 429, or AniList's `X-RateLimit-Remaining` reaching 0, pauses that source for up to 5 min (`search:{api}:pause`; `Retry-After` and `X-RateLimit-Reset` are honoured). Cache hits use none of this, identical lookups in flight share one request, and with Redis down the counters fall back to in-process copies. Requests go through `safeFetch`, so they carry the default User-Agent with `HTTP_CONTACT`.

**Sources:**
- AniList live header probe (2026-10-10 UTC)
- https://pokeapi.co/docs/v2 (accessed 2026-10-09)
- https://wiki.mee6.xyz/en/plugins/search-anything

### 3.7 Twitch "live" auto-role (Discord presence)

**What it powers:** a role given to members while they are streaming, with an optional role restriction. This matches the Live-Streaming auto-role in MEE6's Twitch Alerts plugin.

| Item | Detail |
|---|---|
| Source | Discord presence: the `STREAMING` activity type |
| Auth | None. The privileged `GuildPresences` intent is already enabled in `src/bot.js`. |
| Twitch API | **Not used.** No keys, polling or webhooks. |
| Scope | Assigns and removes a role only. Go-live **alerts** for arbitrary streamers need Twitch Helix (§4.1), which is research only. |

**As built:** module `liverole` (`liveRoleEnabled`, **off by default**), with the role and the optional requirement stored as `liveRoleId` and `liveRoleRequiredRoleId`.

- **`/liverole` (admin level):** `set role:` picks the role, `require [role]` limits it to members who hold another role (no role means everyone), `enable` turns it on and syncs, `disable [remove_roles]` turns it off and by default removes the role from its holders, and `status` shows the setup and who is live. `set` refuses @everyone, managed roles, roles the bot or the admin cannot assign and roles with dangerous permissions, asks for confirmation when the role would be taken from members who are not live, and refuses outright when that would be more than 50 members.
- **`presenceUpdate` hook:** acts only when a member's Streaming state changes. A member with a Streaming activity who meets the requirement gets the role; when the activity ends, the role is removed whatever the requirement says. Bots are ignored.
- **`guildReady` hook:** reconciles the whole guild at startup and when the bot joins one (at most 50 role changes per sync, one member at a time; guilds over 25,000 members use cached members only). It is skipped while the module is off.
- Any Discord Streaming activity counts; the code does not check which platform it comes from. Every role change is sent with a short reason string, such as "Live role: started streaming".

**Source:** https://wiki.mee6.xyz/en/plugins/twitch-alerts

---

## 4. Research only — not integrated

### 4.1 Twitch live alerts (Helix + EventSub)

**What it would power:** "X is live playing Y" posts with title, category, thumbnail and a role ping, plus optional "stream ended" edits.

**API surface:**
- **`GET https://api.twitch.tv/helix/streams`** accepts an app or user access token and up to **100 `user_id`** (or `user_login`) values per request. It returns only users who are live. Fields include `id`, `user_login`, `game_name`, `type: "live"`, `title`, `viewer_count`, `started_at` and `thumbnail_url` (with `{width}x{height}` placeholders).
- **EventSub `stream.online` / `stream.offline` (v1)** needs no broadcaster authorization. A subscription naming a broadcaster who hasn't authorized your app **costs 1** against `max_total_cost`; it's free if they have. Only 3 subscriptions may share the same type and condition.

**Transports and token types** (the key constraint for a self-hosted bot):

| Transport | Token | Public URL? | Notes |
|---|---|---|---|
| Webhook | **App** token only | Yes: HTTPS on port 443 | Twitch verifies the callback; HMAC-signed; at-least-once delivery; subscriptions are revoked after repeated failures. Sample responses show `max_total_cost: 10000`, but the page doesn't state it as a rule (unverified). |
| WebSocket | **User** token only | No | Max 3 connections, 300 subscriptions per connection, and **`max_total_cost` 10** per client and user pair. Fits watching your own channel, not other people's. |
| **Conduit** (shards = webhooks or WebSockets) | **App** token | **No**, if the shards are WebSockets | Up to 5 conduits per client, 20,000 shards per conduit. After the WebSocket welcome message you must call `PATCH /helix/eventsub/conduits/shards` within 10 s. A disconnected shard is disabled and must be reassigned. The conduit is deleted after 72 h with no enabled shard. Same cost model as other subscriptions. |

So an **app token plus a conduit with one WebSocket shard** gives push alerts for arbitrary streamers with **no public endpoint**. **Helix polling** is simpler still: 1,000 tracked streamers is 10 calls per poll.

| Item | Detail |
|---|---|
| Auth | Register an app in the Twitch developer console and use the client-credentials grant. The app access token expires and must be re-minted. |
| Price | Free |
| Rate limits | Token bucket of points per minute, with separate buckets for app and user tokens; default cost 1 point. Read the budget from `Ratelimit-Limit` / `Ratelimit-Remaining` / `Ratelimit-Reset`. The docs show `800` only as an example. |
| ToS | The Twitch Developer Agreement was **not reviewed**; read it before shipping. At minimum, link each alert back to the channel. |
| Libraries (npm, 2026-10-09) | `@twurple/auth` / `@twurple/api` 8.2.0 (2026-09-13); `@twurple/api` includes conduit endpoints (`createConduit`, `updateConduitShards`, `getConduitShards`). `@twurple/eventsub-ws` 8.2.0 has **no conduit support** in its typings (it targets user-token WebSockets), so a conduit shard means a small hand-written client on `ws` (already a discord.js dependency). `@twurple/eventsub-http` 8.2.0 handles webhooks. |

**Implementation sketch:**
1. `/alerts twitch add <login> #channel [@role]`: resolve the login to a `user_id` with `GET /helix/users` (up to 100 per call) and store a `SocialAlert`.
2. Poller (shard 0 or Redis lock), every 60–120 s: batch tracked IDs by 100 into `GET /streams?user_id=…`. When a user appears and `alerts:state` doesn't hold the same stream `id`, post and store the state. When a user is missing from the response, mark them offline.
3. Later, conduit upgrade: create the conduit once (persist its ID), open `wss://eventsub.wss.twitch.tv/ws`, PATCH the shard with the session ID, subscribe `stream.online` and `stream.offline` with `transport: { method: 'conduit', conduit_id }`, and re-PATCH on reconnect.

**Data to store:** Twitch `user_id` (stable; logins can change), login and display name, last stream `id`.

**Effort:** S for polling; M for the conduit (reconnect handling, shard reassignment).

**Sources:**
- https://dev.twitch.tv/docs/api/reference/#get-streams (accessed 2026-10-09)
- https://dev.twitch.tv/docs/eventsub/manage-subscriptions/ (accessed 2026-10-09)
- https://dev.twitch.tv/docs/eventsub/handling-conduit-events/ (accessed 2026-10-09)
- https://dev.twitch.tv/docs/eventsub/eventsub-subscription-types/ (accessed 2026-10-09)
- https://dev.twitch.tv/docs/api/guide (rate limits; accessed 2026-10-09)
- https://dev.twitch.tv/docs/rss/change-log.xml (latest entry 2026-10-01; app-token support for more endpoints went GA 2026-05-29)

### 4.2 YouTube Data API v3 and WebSub

**What it would power:** "New video from X" and "X is live now" posts, without depending on the channel feed (§3.2).

| Option | Cost | Live detection | Public URL | Status |
|---|---|---|---|---|
| **Data API v3 polling**: `playlistItems.list` on the channel's uploads playlist, then `videos.list` | **1 unit** per call against **10,000 units/day** | Yes: `snippet.liveBroadcastContent` (`live` / `upcoming` / `none`) and `liveStreamingDetails.actualStartTime` | No | Documented, stable |
| **WebSub (PubSubHubbub)**: hub `https://pubsubhubbub.appspot.com/subscribe`, topic `https://www.youtube.com/xml/feeds/videos.xml?channel_id=…` | Free; then 1 unit per `videos.list` lookup | Not in the notification. It reports **uploads, title changes and description changes**; follow up with `videos.list`. | **Yes** | Documented; leases must be renewed before `hub.lease_seconds` runs out |
| Channel RSS (§3.2) | Free | No | No | Undocumented; 404 on 2026-10-10 (observed) |

**Quota:** the default is **10,000 units/day for most endpoints**, but **`search.list` and `videos.insert` each have their own bucket of 100 calls/day** (1 unit per call). `videos.list`, `channels.list`, `playlistItems.list` and `activities.list` cost 1 unit each. A search-based design is capped at 100 searches/day; use the uploads playlist instead.

**Budget:** `channels × (1440 / poll_minutes)` units/day, plus a few `videos.list` batches.

| Poll interval | Units per channel per day | Channels that fit in 10,000 |
|---|---|---|
| 15 min | about 96 | about 100 |
| 30 min | about 48 | about 200 |
| WebSub trigger, 6 h safety poll | about 1 per new video | Far more |

**ToS (YouTube API Services Developer Policies, last updated 2026-09-14):**
- Show YouTube as the source with its brand features and link the YouTube Terms of Service.
- Publish a privacy policy that discloses use of YouTube API Services.
- Refresh or delete stored API data (titles, thumbnails, statistics) within **30 days**.
- One API project per client; don't share credentials.
- Scraping and non-API access are prohibited (§III.E.6, §III.I.14).

**Auth:** an API key from a Google Cloud project with the YouTube Data API enabled. No OAuth for public data.

**Libraries:** `@googleapis/youtube` 40.0.0 (2026-10-03) or plain `fetch`; `fast-xml-parser` 5.11.2 for WebSub Atom.

**Implementation sketch:**
1. `/alerts youtube add <@handle | channel URL | UC…>`: call `channels.list` (1 unit); store `channelId` and `contentDetails.relatedPlaylists.uploads`.
2. Poller: `playlistItems.list(playlistId=uploads, maxResults=5)` per channel, diffed against `lastSeenId`. Batch new video IDs into one `videos.list(part=snippet,liveStreamingDetails)`. `live` posts "is live"; `upcoming` goes to a recheck set, re-polled with batched `videos.list` until `actualStartTime` appears; `none` posts as an upload.
3. Optional WebSub: subscribe each topic with `hub.callback=${PUBLIC_BASE_URL}/webhooks/youtube` and a `hub.secret`. Answer the `hub.challenge` GET, verify the HMAC on POSTs, renew leases from a daily job, and dedupe by `videoId` (title and description edits notify too). Whether WebSub keeps delivering while the RSS endpoint 404s is **unverified**.

**Effort:** M.

**Sources:**
- https://developers.google.com/youtube/v3/determine_quota_cost (last updated 2026-10-08)
- https://developers.google.com/youtube/v3/guides/push_notifications (last updated 2026-10-07)
- https://developers.google.com/youtube/v3/docs/videos (last updated 2026-10-08)
- https://developers.google.com/youtube/terms/developer-policies (last updated 2026-09-14)

### 4.3 Reddit Data API (OAuth)

All unverified (see §3.3 for the sources and canonical pages).

- **Approval-gated.** Reddit's Responsible Builder Policy was announced in r/redditdev on **2025-11-11**, with a policy update reported for 2026-06-05. It ends self-service OAuth tokens: developers must request access and get explicit approval. Reported review times are 2–4 weeks. Commercial use needs separate written approval.
- **Free-tier limit:** 100 queries per minute per OAuth client ID, averaged over 10 minutes (Data API Wiki, reportedly updated 2026-05-11). Unauthenticated API requests get no allowance.
- **Implementation (after approval):** application-only OAuth (client credentials), then `GET /r/{sub}/new?limit=25` with `before=` the last fullname, using plain `fetch` against `oauth.reddit.com`.

**Effort:** M, gated on approval. **Verdict:** low priority.

### 4.4 X / Twitter

**What it would power:** "New post from @X" feeds.

**Pricing (docs.x.com, accessed 2026-10-09; page shows no date):**

| Item | Price |
|---|---|
| Post read | **$0.005** per post returned |
| User read | **$0.010** per user returned |
| Owned reads (your own posts, followers, etc.) | $0.001 |
| Post create / with URL / summoned | $0.015 / $0.20 / $0.010 |
| X Activity API webhooks | Billed per event delivered, e.g. $0.005 per `post.create` |

- **Cap:** pay-per-usage plans are capped at **3 million post reads per billing month**; above that you need Enterprise.
- **Deduplication:** resources are deduplicated within a **24-hour UTC day**, so re-reading the same post that day costs once. The page calls this a soft guarantee.
- **No free tier.** New accounts get $20 in credits after saving a card, and X matches the first auto-recharge up to $50.
- **Legacy plans (secondary, unverified):** pay-per-use became the default in early February 2026; Basic ($200/month) and Pro ($5,000/month) closed to new sign-ups; free-plan users moved to pay-per-use with a one-time voucher; "Public Utility Apps" kept free access.
- **Cost example:** 20 accounts posting 10 times a day is about 6,000 post reads a month, roughly **$30/month** plus user lookups. An empty poll should cost nothing, since billing is per resource returned, but the page doesn't say so explicitly (unverified).
- **Auth / endpoint:** app-only bearer token; `GET /2/users/:id/tweets?since_id=…`; plain `fetch` (no official Node SDK evaluated).
- **Spend control:** set a hard spending limit in the console; the pricing page says requests are blocked once it's reached.
- **ToS:** X's developer agreement applies; displaying posts generally requires following X's display requirements (not re-verified).

**Effort:** M plus ongoing spend. **Verdict:** defer. It is the only alert feature with no free path, and the owner would carry the cost for every guild using it.

**Sources:**
- https://docs.x.com/x-api/getting-started/pricing (accessed 2026-10-09)
- Secondary: https://roboin.io/article/en/2026/02/08/x-transitions-api-to-pay-per-use-model-ending-free-plan/ (2026-02-08), https://www.postproxy.dev/blog/x-api-pricing-2026/ (accessed 2026-10-09)

### 4.5 Instagram

- **Basic Display API** (consumer) was shut down on **2024-12-04**. There is no replacement for personal accounts.
- **Business Discovery** (Instagram API with Facebook Login) returns basic metadata and metrics, including media IDs and counts, about **other professional (Business/Creator) accounts**. Limits:
  - Queries go through *your own* IG professional account and its user token.
  - Age-gated accounts return no data.
  - Returned media objects can't be fetched directly ("insufficient permissions").
  - Permission requirements are deferred to the endpoint reference; expect Meta's permission and app-review process for production (unverified).
- **What a bot could do:** poll Business Discovery for public professional accounts' media lists and post new permalinks. Needs a Meta app, a linked Facebook Page or Instagram Login, and your own professional IG account. It cannot follow personal accounts.

**Effort:** L. **Verdict:** not feasible for "follow any Instagram account".

**Sources:**
- https://developers.facebook.com/docs/instagram-platform/instagram-api-with-facebook-login/business-discovery (examples use Graph API v25.0; accessed 2026-10-09)
- https://developers.facebook.com/blog/post/2024/09/04/update-on-instagram-basic-display-api (2024-09-04)

### 4.6 TikTok

- **Display API:** `/v2/user/info/`, `/v2/video/list/`, `/v2/video/query/` with scopes `user.info.basic` and `video.list`. Meant for showing **a creator's own** profile and videos after that creator authorizes your app.
- **Arbitrary accounts:** not explicitly forbidden, but every flow is user-authorized.
- **Webhooks:** HTTPS POST, at-least-once, retries for up to 72 h. The event list is on a separate page and wasn't verified.
- **Research API:** limited to **academic and not-for-profit researchers in eligible regions**; commercial use is **not permitted**.
- **What a bot could do:** a "creator self-link" flow. The creator OAuths their own TikTok (needs a public redirect URI), then the bot polls `video.list`. It cannot follow arbitrary creators.

**Effort:** L. **Verdict:** not feasible for arbitrary accounts.

**Sources:**
- https://developers.tiktok.com/doc/display-api-overview (last updated 2026-08-04)
- https://developers.tiktok.com/doc/webhooks-overview (last updated 2026-08-04)
- https://developers.tiktok.com/products/research-api/ (accessed 2026-10-09)

### 4.7 Kick live alerts

| Item | Detail |
|---|---|
| Auth | OAuth 2.1 at `https://id.kick.com/oauth/token`. App access tokens come from the **client-credentials** grant (`client_id` plus `client_secret`). API host: `api.kick.com`. |
| Polling | `GET https://api.kick.com/public/v1/channels` with an app token. Filter by `broadcaster_user_id` (≤50 per call) **or** `slug` (≤50), not both. The response carries `stream.is_live`, `stream.start_time`, `stream.viewer_count` and a top-level `stream_title`. `/public/v1/livestreams` with a broadcaster filter is **deprecated**; `/public/v2/livestreams` has no broadcaster filter. |
| Webhooks | Create with `POST /public/v1/events/subscriptions` and `method: "webhook"`; **app tokens are accepted, with `broadcaster_user_id` required.** Event `livestream.status.updated` v1, payload `broadcaster`, `is_live`, `title`, `started_at`, `ended_at`. Limit: 10,000 subscriptions per event type per app. |
| Webhook security | Verify `Kick-Event-Signature` (RSA PKCS#1 v1.5 over SHA-256 of `messageId.timestamp.body`) against the key from `GET /public/v1/public-key`. Don't hardcode the key; it rotates. Kick **auto-unsubscribes** an app whose webhook keeps failing for more than a day. Where the webhook URL is configured (app settings) wasn't confirmed. |
| Rate limits | Not documented on the pages read (unverified) |
| Price / library | Free / plain `fetch` (no official SDK evaluated) |

**Implementation sketch:** same as Twitch polling, using the channels endpoint in batches of 50. Store `broadcaster_user_id` (stable) and the slug.

**Effort:** S (polling) / M (webhooks plus a public endpoint).

**Sources (accessed 2026-10-09):**
- https://docs.kick.com/apis/channels.md
- https://docs.kick.com/apis/livestreams.md
- https://docs.kick.com/events/subscribe-to-events.md
- https://docs.kick.com/events/event-types.md
- https://docs.kick.com/events/webhook-security.md
- https://docs.kick.com/getting-started/generating-tokens-oauth2-flow.md
- https://docs.kick.com/changelog.md

### 4.8 Music playback

**What it would power:** `/play`, queues and filters in voice channels. MEE6 no longer has a music player plugin (§7).

**Players:**

| Option | Version (2026-10-09) | Notes |
|---|---|---|
| **discord-player** with extractors | `discord-player` 7.2.0 (2026-03-01), `@discord-player/extractor` 7.2.0, `discord-player-youtubei` 3.1.0 (2026-09-06) | In-process (FFmpeg plus Opus inside the shard). A DAVE-related bug is open (#2013, 2026-07-01). |
| **Lavalink** (separate JVM server) plus a Node client | Lavalink 4.2.2 (2026-03-06); `youtube-source` plugin 1.18.2 (2026-07-27); clients `shoukaku` 4.3.0, `lavalink-client` 2.11.0 | Moves audio work out of the shards, which suits this repo's "a dead shard restarts the bot" model. **4.2.0 (2026-02-28) was the first release with DAVE support**; client libraries must support it too. |

**Discord voice E2EE (DAVE):** Discord completed the end-to-end encryption migration in early March 2026. Clients and apps without DAVE can no longer join calls; stage channels are excluded. `@discordjs/voice` 0.19.2 depends on `@snazzah/davey`. Any music stack must be DAVE-capable.

**Sources of audio:**

| Source | Status |
|---|---|
| **YouTube** | The Developer Policies (2026-09-14) prohibit downloading, caching or storing audiovisual content (§III.E.1), **separating or isolating the audio** (§III.I.7), playback from a player not displayed to the user (§III.I.9), and access by any means other than the API Services (§III.I.14). Every YouTube music-bot approach (InnerTube clients, yt-dlp 2026.08.19, Lavalink's youtube-source) conflicts with these terms, and YouTube changes keep breaking the extractors. |
| **Spotify** (metadata only; the Web API never streams audio) | Development mode allows **5 allow-listed users** and the owner needs **Premium**. Quota is shared per developer account; up to 25 apps per account since July 2026. **Extended quota is limited to registered organizations with ≥250k MAU** (since 2025-05-15). The Developer Terms (v10, effective 2025-05-15) prohibit facilitating stream-ripping, transferring Spotify Content to third parties, and facilitating unauthorized audio sharing, so the "Spotify link, then find it on YouTube" pattern conflicts with them. Whether client-credentials calls are limited by the 5-user allow-list is **unverified**. |
| **SoundCloud** | Register apps at soundcloud.com/you/apps or with the API CLI (Artist Pro recommended for the CLI path). The Client Credentials grant covers public search, playback and URL resolution: 50 tokens per 12 h per app, 30 per hour per IP. Streaming requires crediting the uploader and SoundCloud and linking back; tracks with `access: blocked` can't be streamed off-platform. A daily play-request limit is on a separate page; secondary sources say 15,000 per 24 h per client_id (unverified). |
| **Clean sources** | Direct HTTP streams (internet radio), local files, and SoundCloud within its terms |

**Implementation sketch (if pursued):** Lavalink 4.2.x as a sidecar container, `LAVALINK_NODES` pointing at it, a DAVE-capable client; per-guild queues in memory, persisted in Redis for restarts; `/play` resolving only SoundCloud and HTTP sources; a `musicEnabled` module toggle.

**Effort:** L (voice infrastructure, a new container, ToS exposure). **Verdict:** defer; if wanted, ship SoundCloud and radio only.

**Sources:**
- https://developers.google.com/youtube/terms/developer-policies (2026-09-14)
- https://developer.spotify.com/documentation/web-api/concepts/quota-modes (accessed 2026-10-09)
- https://developer.spotify.com/documentation/web-api/references/changes/july-2026 (July 2026)
- https://developer.spotify.com/terms (v10, effective 2025-05-15)
- https://techcrunch.com/2026/02/06/spotify-changes-developer-mode-api-to-require-premium-accounts-limits-test-users/ (2026-02-06)
- https://developers.soundcloud.com/docs/api/introduction (accessed 2026-10-09)
- https://github.com/lavalink-devs/Lavalink/releases (4.2.0 notes, 2026-02-28)
- https://discord.com/blog/every-voice-and-video-call-on-discord-is-now-end-to-end-encrypted (accessed 2026-10-09)
- https://github.com/Androz2091/discord-player/issues/2013 (2026-07-01)
- npm registry for versions (2026-10-09)

### 4.9 Other search commands

Not in this effort. Wikipedia, Open-Meteo and Jikan are keyless but out of scope; the rest need keys.

| Command | API | Auth | Limits / terms | Notes | Effort |
|---|---|---|---|---|---|
| `/youtube search` | Data API `search.list` | API key (shared with §4.2) | **100 calls/day**, a separate bucket at 1 unit each | Cache aggressively; tell users when the daily budget is gone. | S |
| (alternative for `/anime`) | Jikan v4 (unofficial MyAnimeList API) | None | 3 req/s and 60 req/min per secondary sources (unverified) | It scrapes MAL, so availability varies. | S |
| `/wiki` | Wikipedia REST / Action API | None (OAuth optional) | Unauthenticated: **200 req/min with a compliant User-Agent** (name/version plus contact). IP-only clients get **10 req/min**. | Set `User-Agent: DiscordBot/1.0 (contact@…)`. | S |
| `/weather` | **Open-Meteo** | None | **Non-commercial only**: 600/min, 5,000/h, 10,000/day, 300,000/month | Geocode with Open-Meteo's geocoding API. | S |
| (alternative) | OpenWeatherMap | API key | Free plan: 60/min, 1M/month. One Call 4.0: 1,000/day free, then $0.0015 per call. | Use if the bot is ever commercial. | S |
| `/imgur` | Imgur API | Client-ID | **Unverified** (apidocs.imgur.com blocks automated fetches). Older mirrors describe a credit system of about 12,500 requests/day per app. | Skip unless re-verified. | S |
| `/twitch` | Twitch | Key (per the parity research) | Not researched | MEE6 Search Anything command | — |

**Sources:**
- https://developers.google.com/youtube/v3/determine_quota_cost (2026-10-08)
- https://www.mediawiki.org/wiki/Wikimedia_APIs/Rate_limits (last edited 2026-06-03)
- https://open-meteo.com/en/pricing (accessed 2026-10-09)
- https://openweathermap.org/price (accessed 2026-10-09)
- Jikan secondary sources: https://www.moritzjung.dev/obsidian-media-db-plugin-docs/reference/jikan, https://rubydoc.info/gems/jikanrb (accessed 2026-10-09)

### 4.10 AI chat / persona

**What it would power:** an opt-in persona that replies when mentioned or in a configured channel, plus optional `/ask` and "summarize this channel". (MEE6's equivalents, MEE6 AI and AI Characters, are separate subscriptions.)

**Claude API models and pricing (accessed 2026-10-09):**

| Model | API ID | Input / output per MTok | Cache hit | Batch (50% off) | Default effort | Context / max output |
|---|---|---|---|---|---|---|
| Claude Haiku 5.5 | `claude-haiku-5-5` | **$0.10 / $0.50** (prompts ≤100K tokens); $0.50 / $2.50 above that | $0.01 | $0.05 / $0.25 | `medium` | 1M / 128K |
| Claude Sonnet 5.5 | `claude-sonnet-5-5` | $2 / $10 | $0.10 (another Anthropic reference lists $0.20; re-check the pricing page) | $1 / $5 | `high` | 1M / 128K |
| Claude Opus 5.5 | `claude-opus-5-5` | $4 / $20 | $0.20 | $2 / $10 | `medium` | 1M / 128K |

**Per-turn cost estimate** (about 2,000 input tokens of persona plus recent history, 300 output tokens, before thinking tokens):

| Model | Cost per turn | Turns per $1 |
|---|---|---|
| Haiku 5.5 | about $0.00035 | about 2,800 |
| Sonnet 5.5 | about $0.007 | about 140 |
| Opus 5.5 | about $0.014 | about 70 |

- Thinking is adaptive on all three and billed as output. For chat, set `output_config: { effort: 'low' }` and measure.
- Cache the persona system prompt with `cache_control`. Prefixes below a model-dependent minimum aren't cached; check `usage.cache_read_input_tokens`.
- **Recommended default:** `claude-haiku-5-5`, with Sonnet 5.5 as an opt-in per guild.

**Alternative: OpenAI** (pricing page accessed 2026-10-09): `gpt-6-luna` $0.10 / $0.50, `gpt-5.4-nano` $0.20 / $1.25, `gpt-5.4-mini` $0.75 / $4.50 per MTok. OpenAI's **Moderation endpoint (`omni-moderation-latest`) is free**; it classifies text and images across 13 categories (harassment, hate, illicit, self-harm, sexual, sexual/minors, violence and subcategories). It can pre-screen input or output for whichever LLM is used, but that sends the content to a second processor.

**Policy and safety:**
- **AI disclosure:** Anthropic's Usage Policy (the version on the site shows an effective date of **2026-11-12**) requires consumer-facing bots to **disclose that users are talking to an AI**, at least at the start of a session or in the interface. The persona's name and footer should say "AI".
- **Minors:** Discord servers can include users under 18. The policy defines minors as under 18 and applies additional guidelines to products serving them. Keep the persona non-romantic, refuse sexual content, and make it opt-in per guild and admin-only to enable.
- **High-risk advice:** the policy requires human review before personalized legal, medical or financial recommendations reach a person; the system prompt should decline that kind of advice.
- **Refusals:** check `stop_reason === 'refusal'` before reading `content`. Server-side refusal fallbacks don't exist for Haiku 5.5.
- **Discord Developer Policy** (effective 2024-07-08; quoted from a search snippet because the page returned 403): API data may be used only as necessary for the bot's stated functionality. Send only the triggering message and a short window of context, never whole-channel history by default, and say in the bot's privacy policy that message content goes to Anthropic (or OpenAI) when the feature is used.
- **Abuse controls:** per-user cooldowns (`ai:cd:{guildId}:{userId}`); a per-guild daily token budget (`ai:budget:{guildId}:{yyyymmdd}`, incremented by `usage.input_tokens + usage.output_tokens`); a global monthly spend cap; `allowedMentions: { parse: [] }` on every reply; user text treated as untrusted (prompt injection), with no model tools that can act on the server.

**Library:** `@anthropic-ai/sdk` 0.133.0 (2026-10-09); OpenAI alternative `openai` 7.32.0.

**Implementation sketch:**

```js
import Anthropic from '@anthropic-ai/sdk';
const anthropic = new Anthropic(); // reads ANTHROPIC_API_KEY

const res = await anthropic.messages.create({
  model: process.env.AI_MODEL || 'claude-haiku-5-5',
  max_tokens: 1024,
  system: [{ type: 'text', text: persona.systemPrompt, cache_control: { type: 'ephemeral' } }],
  output_config: { effort: 'low' },
  messages: history, // last N turns from Redis ai:ctx:{channelId}, ending with the user's message
});
if (res.stop_reason === 'refusal') return reply('I can’t help with that.');
const text = res.content.filter((b) => b.type === 'text').map((b) => b.text).join('');
```

**Data to store:** an `AiPersona` row per guild (`systemPrompt`, `model`, `channelIds[]`, `dailyTokenBudget`); an `aiEnabled` toggle in `MODULE_FLAGS`; a short-lived Redis context list per channel (TTL about 30 min, capped at N messages).

**Effort:** M.

**Sources:**
- https://platform.claude.com/docs/en/about-claude/pricing (accessed 2026-10-09)
- https://platform.claude.com/docs/en/about-claude/models/overview (accessed 2026-10-09)
- https://www.anthropic.com/legal/aup (version shown effective 2026-11-12)
- https://developers.openai.com/api/docs/pricing (accessed 2026-10-09)
- https://developers.openai.com/api/docs/guides/moderation (accessed 2026-10-09)
- https://support-dev.discord.com/hc/en-us/articles/8563934450327-Discord-Developer-Policy (effective 2024-07-08; read via search snippet)

### 4.11 Translation providers

#### Where the bot is today (`src/services/translationService.js`)

| Area | Current behaviour |
|---|---|
| Providers | `TRANSLATE_PROVIDER` selects `google` (the keyless `translate.googleapis.com/translate_a/single?client=gtx` endpoint, the default) or `libretranslate` (self-hosted, at `LIBRETRANSLATE_URL`). Both are **already supported**. |
| Selection | `TRANSLATE_PROVIDER` is read lazily: `google` (default) or `libretranslate`, case-insensitive. Any other non-empty value logs a warning and uses `google`. |
| 429 handling | A 429 pauses translation: `PAUSE_MS` is 30 min for `google`, 1 min for `libretranslate`, or the server's `Retry-After` when longer (capped at 6 h). The pause is shared across shards through Redis (`translate:{provider}:pausedUntil`). The Google adapter only uses the `gtx` client, so there is no client rotation. |
| Detection cost | Auto-translate takes the detected source language from the first translate response and drops a target equal to the source, so a message costs about N × its length (N = target languages). `detectLanguage` (for `/detectlanguage`) is a translate-to-English on the Google path and `/detect` on LibreTranslate. |
| Language codes | `LANGUAGES` holds the supported codes in Google's style; `CODE_ALIASES` normalises legacy and regional codes (`iw` to `he`, `zh-hant` to `zh-tw`, `nb` to `no`, ...); LibreTranslate has its own code mapping (`toLtCode`) and loads the server's language list. |

**Hebrew:** Google reports Hebrew as `iw`; `CODE_ALIASES` maps it to `he` in one place, so every provider and the same-language skip see `he`. (An earlier version showed "iw" and translated Hebrew into Hebrew.)

**As built:** two parts of the plan below are already in the code: the warning for an unknown `TRANSLATE_PROVIDER` (from item 1) and item 3 (detection taken from the first response). The rest (a provider registry, per-provider adapters, quota-aware breakers and code maps for paid providers) is not built (research only).

#### Provider comparison

| Provider | Status | Free tier | Paid price | Auth | Detect | Multiple targets per request | Coverage of the bot's 73 codes | Quality (reputation; not benchmarked) |
|---|---|---|---|---|---|---|---|---|
| **Google Cloud Translation v2 "Basic"** (NMT) | Research only | **First 500,000 chars/month free** (applied as a $10 monthly credit, shared with v3) | **$20/M chars** (lower at volume) | API key | Returned with the translation (`detectedSourceLanguage`), no extra charge when the source is omitted. `/detect` is billed per character. | No (one `target` per call) | **All**; the bot already uses Google codes | Same engine family as `gtx` today |
| **Google Cloud Translation v3 "Advanced"** | Research only | Same pool | NMT $20/M; Translation LLM $10/M input plus $10/M output | Service account / OAuth (no API key) | Yes | No | All | Adds glossaries, batch, LLM model |
| **DeepL API Free / Pro** | Research only | **500,000 chars/month** (Free; from a search snippet of DeepL support) | Pro: base fee plus usage. Secondary sources say about $5.49/month + $25/M, or €4.99 + €20/M (**unverified**) | `Authorization: DeepL-Auth-Key …`; Free uses `api-free.deepl.com`, Pro `api.deepl.com` | **No detect endpoint**; `/v2/translate` returns `detected_source_language` | No | 126 targets. **Missing `km` and `si`.** Needs mapping: `no`→`NB`, `zh`→`ZH-HANS`, `en`→`EN-US`/`EN-GB`, `pt`→`PT-BR`/`PT-PT`. Variants work as **targets only**. | Often rated highest for European languages |
| **Azure AI Translator** (Microsoft) | Research only | **F0: 2,000,000 chars/month** | **S1: $10/M chars** (US regions, from the pricing page's data) | `Ocp-Apim-Subscription-Key` plus `Ocp-Apim-Subscription-Region` | Returns `detectedLanguage` when `from` is omitted. A `/detect` endpoint exists, and per the FAQ, **Detect calls don't count toward character consumption**. | **Yes**: repeat `to=`. Each target counts as a separate translation (3,000 chars to 3 languages is 9,000 chars), so this saves round-trips, not characters. Limit 50,000 chars per request across all targets. | 138 codes (observed). **Missing `eo`, `yi`.** Needs mapping: `zh`→`zh-Hans`, `no`→`nb`, `tl`→`fil`, `sr`→`sr-Cyrl`/`sr-Latn`, `mn`→`mn-Cyrl`. | Comparable to Google |
| **Amazon Translate** | Research only | The pricing page lists **2M chars/month for 12 months** from the first request. AWS's current Free Tier for new accounts is **credit-based** ($100 at sign-up plus up to $100 more; the Free plan lasts ≤6 months). Whether the 12-month Translate offer still applies to new accounts is **unverified**. | **$15/M chars** | AWS SigV4 (IAM key pair) | `SourceLanguageCode: auto` | No | Not checked (use `ListLanguages`) | Comparable |
| **LibreTranslate** (self-hosted, Argos models) | **Already supported** | Free (AGPL-3.0); CPU and RAM are the cost | Hosted libretranslate.com plans not checked | Optional `api_key` | `/detect` plus `detectedLanguage` on translate | No | The public instance serves **51 languages** (observed). **26 of the bot's 73 are missing:** af, be, bs, cy, gu, hr, hy, is, ka, kk, km, mk, ml, mn, mr, mt, my, ne, pa, si, ta, te, uz, xh, yi, zu. | Noticeably below the commercial engines |

**Free-tier capacity** (2 target languages, 80-character average messages):

| Provider | Today's flow (N + 1): 240 chars/message | Restructured flow (N): 160 chars/message |
|---|---|---|
| Google Cloud v2 (500K) | about 2,080 messages/month | about 3,125 |
| DeepL Free (500K) | — | about 3,125 |
| **Azure F0 (2M)** | about 8,300 | about **12,500** |

- A busy server exceeds every free tier, so a metered provider must **fail closed**: stop translating and log rather than bill unexpectedly. A Redis cache keyed `tr:{target}:{sha1(text)}` (TTL 24 h) absorbs repeated short messages ("gg", "lol").
- **Quality** was not benchmarked; the column is reputation. A blind A/B on a sample of the guild's real messages is cheap and is the only reliable way to choose.
- **Versions** (npm, 2026-10-09): `@google-cloud/translate` 10.2.0, `deepl-node` 1.28.1, `@aws-sdk/client-translate` 3.1149.0. Azure and Google v2 need only `fetch`.

#### How a keyed provider would slot into `translationService.js`

1. **Replace the ternary with a provider table.**

   ```js
   const PROVIDERS = { google: gtx, libretranslate: lt, 'google-cloud': gcloud, deepl, azure, aws };
   const requested = (process.env.TRANSLATE_PROVIDER || 'google').toLowerCase();
   const PROVIDER = requested in PROVIDERS ? requested : 'google'; // warn once if requested !== PROVIDER
   ```

   Each entry implements `translate(text, targets[], source)` returning `[{ target, text }]` plus `detectedLang`; an optional `detect(text)`; `toCode` / `fromCode` maps following the `TO_LT` pattern; an `unsupported` set; and `pauseMs` per status. Log a warning for an unknown `TRANSLATE_PROVIDER` instead of silently falling back.
2. **Per-provider, per-status circuit breaker.** Keep 429 handling. Add quota-exhausted signals: Azure's docs say **403** "often indicates that you used all the free translations" on a trial subscription; DeepL and Google have their own quota errors. Each should pause until the next billing period or for a fixed long pause, not 1 minute. Replace `PAUSE_ON_429_MS`'s google-versus-rest switch with `PROVIDERS[p].pauseMs`.
3. **Drop the separate detection call in `runAutoTranslate`.** Translate to the first target with `source = auto` and take `detectedLang` from that response. If the first target equals the detected source, **drop that result from the embed too**, then skip the remaining same-language targets. **Azure exception:** `/detect` is free, so call it first and send only the differing targets in one request, for exactly N × length. Keep `fetchDetection` for `/detectlanguage`; with DeepL that becomes a translate-to-English whose output is discarded.
4. **Code maps and gaps.** Add per-provider maps (table above). Hide unsupported targets from `/autotranslate` and `/languages` while that provider is active, or fall back per message to `gtx` with a warning. DeepL's upper-case codes (`EN`, `PT-BR`) are already handled by the existing `.split('-')[0].toLowerCase()` normalizer.
5. **Chunking:** cap translated input (for example 1,000 chars). Output is already sliced to 300 characters per target in the embed, so paying to translate a 4,000-character Nitro message is wasted spend.

**Research recommendation:** Azure Translator F0 as the default keyed provider (largest permanent free tier, unbilled detection, multiple targets per call, 71 of the 73 codes after mapping); Google Cloud v2 where exact parity with today's output and codes matters; DeepL for European-language communities; keep `gtx` as the zero-config fallback and LibreTranslate for fully offline setups.

**Effort:** M for the refactor (provider table, breaker, detection restructure), then S per provider.

**Sources:**
- https://cloud.google.com/translate/pricing (accessed 2026-10-09)
- https://docs.cloud.google.com/translate/docs/editions (last updated 2026-10-07)
- https://developers.deepl.com/docs/getting-started/intro (accessed 2026-10-09)
- https://developers.deepl.com/docs/getting-started/supported-languages (accessed 2026-10-09)
- https://support.deepl.com/hc/en-us/articles/360021200939-DeepL-API-Free (500K figure via search snippet; page returns 403 to fetch)
- DeepL Pro price, secondary and unverified: https://www.g2.com/products/deepl-api/pricing, https://learn.microsoft.com/connectors/deepl
- https://azure.microsoft.com/en-us/pricing/details/translator/ (accessed 2026-10-09)
- https://learn.microsoft.com/en-us/azure/ai-services/translator/text-translation/reference/v3/translate (updated 2026-07-23)
- https://learn.microsoft.com/en-us/azure/ai-services/translator/translator-faq (per-target counting, free Detect calls; accessed 2026-10-09)
- https://learn.microsoft.com/en-us/azure/ai-services/Translator/service-limits (50,000 chars per request; accessed 2026-10-09)
- Azure `/languages?api-version=3.0&scope=translation` probe (2026-10-10 UTC)
- https://aws.amazon.com/translate/pricing/ (accessed 2026-10-09)
- https://aws.amazon.com/free/ and https://aws.amazon.com/free/free-tier-faqs/ (accessed 2026-10-09)
- https://github.com/LibreTranslate/LibreTranslate (latest release v1.9.6, 2026-05-26)
- `https://libretranslate.com/languages` probe (2026-10-10 UTC)

---

## 5. Recommended order

The research's recommended integration order, with the current status of each step.

| Step | Feature | Why this position (from the research) | Effort | Status |
|---|---|---|---|---|
| 1 | **Translation provider refactor**, then Azure F0 (or Google Cloud v2) | Fixes a live pain point: `gtx` rate-limits hard and trips the 30-minute breaker. Low risk, keyless providers stay as fallbacks, and the (N+1)→N detection fix pays off on every provider. | M, then S | Research only — not integrated |
| 2 | **Alert framework plus Bluesky** | Builds the shared poller, sharding guard, `SocialAlert` table and module toggle against a keyless API. | M (framework) + S | **Built (keyless)** (as the `Feed` model and per-feed `feed.poll` jobs, §2, not a `SocialAlert` table) |
| 3 | **Twitch** via Helix polling | The highest-demand MEE6 alert. Free, app token, no public URL. | S | Research only — not integrated |
| 4 | **Kick** via channels polling | Same shape as Twitch; free. | S | Research only — not integrated |
| 5 | **YouTube** via Data API polling | The RSS shortcut is down; quota allows about 100 channels at 15 minutes. Add WebSub only if the bot gets a public URL. | M | Research only — not integrated (the channel feed is built instead; §3.2) |
| 6 | **Search commands**: Wikipedia, PokeAPI, AniList, weather (Open-Meteo), then Urban Dictionary with an NSFW guard | Keyless, independent, quick wins. | S each | PokeAPI, AniList, Urban Dictionary: **built (keyless)**. Wikipedia, Open-Meteo: research only. |
| 7 | **AI persona** (Haiku 5.5, budget-capped, opt-in) | Useful but carries spend and moderation duties; do it after the free features. | M | Research only — not integrated |
| 8 | **Twitch conduit / webhooks**, **YouTube WebSub** | Only once there is a public HTTPS endpoint, or the poll intervals stop being acceptable. | M | Research only — not integrated |
| 9 | **Reddit** (RSS best-effort; API only if Reddit approves the app) | The access path is unstable and approval-gated. | S / M | RSS: **built (keyless)**. API: research only. |
| 10 | **Music** (Lavalink, SoundCloud and radio only) | Heavy infrastructure plus ToS exposure. | L | Research only — not integrated |
| — | **X** | No free tier; revisit only if the owner accepts the spend. | M plus spend | Research only — not integrated |
| — | **Instagram, TikTok** | No compliant way to follow arbitrary accounts. | Not recommended | Research only — not integrated |

**Added by owner decision, outside the research's ordering:** generic RSS/Atom feeds, the YouTube channel feed (despite the observed 404s), podcasts via iTunes Search, AniList for `/anime`/`/manga` (Kitsu considered), and the presence-based Twitch "live" auto-role.

---

## 6. Environment variables

None of these is required at startup. Each module should **disable itself with a single warning** when its keys are missing, rather than parking the bot the way the four core keys (`DISCORD_TOKEN`, `CLIENT_ID`, `DATABASE_URL`, `REDIS_URL`) do.

The names below are the research's proposals in the style of `.env.example` (optional keys commented out). `.env.example` is authoritative.

### 6.1 Keyless features (built)

No API keys are needed. These are the optional settings the code reads (`src/utils/safeFetch.js` and `src/services/feedService.js`):

```dotenv
# ── Outbound requests (feeds and search commands) ─────────────────────────────
# HTTP_CONTACT=you@example.com                 # put in the default User-Agent that safeFetch sends (feeds, search commands); default "n/a"

# ── Reddit (RSS) ──────────────────────────────────────────────────────────────
# REDDIT_USER_AGENT=discord-bot/1.0 (by /u/yourname)   # User-Agent for Reddit feeds only; optional, falls back to the default one

# ── Bluesky ───────────────────────────────────────────────────────────────────
# BSKY_APPVIEW_URL=https://public.api.bsky.app # no key needed; this is the default
```

**Proposed by the research, not implemented:** `ALERT_POLL_INTERVAL_SEC=120`, a default poll interval for feed and alert pollers. The code reads no such variable; poll intervals are fixed per source (§2.3).

The Twitch "live" auto-role needs no environment variables; it relies on the `GuildPresences` intent the bot already requests.

### 6.2 Research-only integrations (not integrated)

```dotenv
# ── Shared (only needed for webhook/WebSub receivers) ─────────────────────────
# PUBLIC_BASE_URL=https://bot.example.com      # public HTTPS origin behind your reverse proxy/tunnel
# WEBHOOK_PORT=8082                            # separate from HEALTH_PORT; never expose /healthz publicly

# ── Twitch ────────────────────────────────────────────────────────────────────
# TWITCH_CLIENT_ID=
# TWITCH_CLIENT_SECRET=                        # client-credentials → app access token
# TWITCH_EVENTSUB_SECRET=                      # webhook transport only (HMAC signing secret)

# ── YouTube Data API ──────────────────────────────────────────────────────────
# YOUTUBE_API_KEY=                             # also used by /youtube search (100 searches/day)
# YOUTUBE_POLL_INTERVAL_SEC=900
# YOUTUBE_WEBSUB_SECRET=                       # WebSub only (hub.secret for HMAC)

# ── Kick ──────────────────────────────────────────────────────────────────────
# KICK_CLIENT_ID=
# KICK_CLIENT_SECRET=                          # client-credentials at id.kick.com

# ── Reddit Data API ───────────────────────────────────────────────────────────
# REDDIT_CLIENT_ID=                            # only after Reddit approves the app
# REDDIT_CLIENT_SECRET=

# ── X ─────────────────────────────────────────────────────────────────────────
# X_BEARER_TOKEN=
# X_MONTHLY_READ_BUDGET=5000                   # bot-side cap in post reads; also set a spend limit in console.x.com

# ── Music ─────────────────────────────────────────────────────────────────────
# LAVALINK_NODES=[{"name":"main","url":"lavalink:2333","auth":"youshallnotpass"}]
# SOUNDCLOUD_CLIENT_ID=
# SOUNDCLOUD_CLIENT_SECRET=
# SPOTIFY_CLIENT_ID=                           # metadata only; see ToS notes before enabling
# SPOTIFY_CLIENT_SECRET=

# ── Other search commands ─────────────────────────────────────────────────────
# OPENWEATHER_API_KEY=                         # only if using OpenWeatherMap instead of Open-Meteo

# ── AI chat ───────────────────────────────────────────────────────────────────
# AI_PROVIDER=anthropic                        # anthropic | openai
# ANTHROPIC_API_KEY=
# AI_MODEL=claude-haiku-5-5                    # or claude-sonnet-5-5 / claude-opus-5-5
# OPENAI_API_KEY=                              # alternative provider, or for the free moderation endpoint
# AI_MODERATION=none                           # none | openai
# AI_DAILY_TOKEN_BUDGET=200000                 # per guild; enforced via Redis counter
# AI_MONTHLY_USD_CAP=10                        # global stop-loss

# ── Translation backend (extends the existing TRANSLATE_PROVIDER switch) ──────
# TRANSLATE_PROVIDER=google                    # today: google | libretranslate. Proposed: google-cloud | deepl | azure | aws
# GOOGLE_TRANSLATE_API_KEY=                    # google-cloud (v2 Basic, API key)
# DEEPL_API_KEY=
# DEEPL_API_URL=https://api-free.deepl.com     # or https://api.deepl.com for Pro
# AZURE_TRANSLATOR_KEY=
# AZURE_TRANSLATOR_REGION=                     # e.g. eastus (required for regional resources)
# AWS_REGION=
# AWS_ACCESS_KEY_ID=
# AWS_SECRET_ACCESS_KEY=
# TRANSLATE_MAX_INPUT_CHARS=1000               # cap characters sent per message
```

### 6.3 Already supported

```dotenv
# TRANSLATE_PROVIDER=libretranslate
# LIBRETRANSLATE_URL=http://localhost:5000
# LIBRETRANSLATE_API_KEY=
```

---

## 7. MEE6 reference: external-service plugins

From the MEE6 parity research. Tiers follow MEE6's plan comparison table (dated June 16, 2026). The dependency notes in this table (keyless vs. key needed) are general knowledge, not MEE6 documentation.

| MEE6 plugin | What it does | MEE6 tier | This bot |
|---|---|---|---|
| Twitch Alerts | Go-live alerts (`{streamer}` `{link}`) plus a Live-Streaming auto-role (optional role restriction) | Premium | Auto-role: **built (Discord-only)** (§3.7). Alerts: research only (§4.1). |
| YouTube Alerts | New upload alerts (`{youtube_channel}`) | Premium | Channel feed: **built (keyless)** (§3.2). Data API: research only (§4.2). |
| Reddit Alerts | New subreddit posts; compact or wide embeds | Premium | `.rss`: **built (keyless)** (§3.3). OAuth API: research only (§4.3). |
| X (Twitter) Alerts | New posts, retweets, replies (`{twitter_user}`) | Premium | Research only (§4.4) |
| Instagram Alerts | New posts (`{instagram_account}`) | Premium | Research only (§4.5) |
| TikTok Alerts (beta) | New videos (`{tiktok_user}`), post-latest option | Premium | Research only (§4.6) |
| Bluesky Alerts (beta) | New posts, optional reposts (`{bluesky_name}` `{bluesky_handle}`) | Premium | **Built (keyless)** (§3.4) |
| Kick Alerts (beta) | Go-live alerts (`{streamer}` `{link}`) | Premium | Research only (§4.7) |
| RSS Feeds | New feed items | Premium | **Built (keyless)** (§3.1) |
| Podcast Alerts | New episodes (`{podcast_title}` `{episode_title}`) | Premium | **Built (keyless)** (§3.5) |
| Search Anything | `/anime` `/manga` (Kitsu), `/pokemon` (PokéAPI), `/urban` (Urban Dictionary), `/imgur`, `/twitch`, `/youtube` | **Free** | `/anime` `/manga` (AniList) `/pokemon` `/urban`: **built (keyless)** (§3.6). `/imgur` `/twitch` `/youtube`: research only (§4.9). |
| Social stat counters | Follower counts for Instagram, Reddit, Twitch, YouTube, X | Premium | Not researched; would need keys |
| MEE6 AI | `/write` (OpenAI), `/imagine`, replies to mentions using a backstory | Separate AI subscription | Research only (§4.10) |
| AI Characters | Persona characters that chat in channels; `/characters` | Separate subscription | Research only (§4.10) |
| Bot Maker (beta) | AI-generated custom modules | Premium | Out of scope |
| Monetize | Paid role subscriptions through Stripe | — | Out of scope |
| Web3 | NFT/crypto commands, counters, wallet gating | Web3 subscription | Out of scope |
| Record, Music Quiz | Voice recording; music quiz | — | **Deprecated** by MEE6 |
| Music player | — | — | **Not a current MEE6 plugin**; third-party reports say it was shut down. Research only here (§4.8). |
| Twitch Bot | Twitch-chat XP | — | **Removed** by MEE6 |

**Sources:**
- https://help.mee6.xyz/en/articles/710936-mee6-free-vs-premium-plans-comparison
- https://help.mee6.xyz/en/articles/611780-what-permissions-does-mee6-need
- https://wiki.mee6.xyz/en/plugins
- https://wiki.mee6.xyz/en/plugins/record
- https://wiki.mee6.xyz/en/plugins/music-quiz
- https://wiki.mee6.xyz/en/features/twitch-bot
- https://wiki.mee6.xyz/en/plugins/search-anything
- Social alert pages: https://wiki.mee6.xyz/en/plugins/twitch-alerts (twitter-, youtube-, reddit-, instagram-, tiktok-, bluesky-, kick-, podcast-alerts and rss-feeds follow the same pattern)
- https://wiki.mee6.xyz/en/features/mee6-ai
- https://wiki.mee6.xyz/en/features/ai-characters
- https://help.mee6.xyz/en/articles/847011-how-to-use-mee6-bot-maker
- https://wiki.mee6.xyz/en/plugins/monetize
- https://wiki.mee6.xyz/en/getting-started and https://wiki.mee6.xyz/en/features/settings (MEE6 is slash-only now, except custom commands)
