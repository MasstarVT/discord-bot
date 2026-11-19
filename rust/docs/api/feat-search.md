# feat-search: public API (from its author)

I ported all seven JS files. All 97 tests pass with the mock database and Redis variables set; none were skipped. Clippy shows no warnings in this crate.

| JS | Rust |
|---|---|
| src/services/searchService.js | src/search_service.rs |
| src/services/afkService.js | src/afk_service.rs |
| src/plugins/liverole.js | src/plugins/liverole.rs |
| src/plugins/afk.js | src/plugins/afk.rs |
| src/commands/search/search.js | src/commands/search.rs |
| src/commands/admin/liverole.js | src/commands/liverole.rs |
| src/commands/community/afk.js | src/commands/afk.rs |

The AFK and live-role code reads the cached guild and makes its REST calls through one replaceable object in src/ops.rs (`DiscordOps`, production `SerenityOps`); tests install a fake. Search requests go through a replaceable fetcher, `SearchFetch` (the JS `__setDeps`). Tests use canned responses and never the network.

Tests:
- Offline (tests/feat_search.rs, 73): commands and plugins match the golden files. The pure functions are compared with JS output from rust/fixtures/gen/feat-search-golden.mjs, which writes rust/fixtures/golden/feat-search/{search,afk,liverole}.json. That covers the text cleaners, projections, embeds, cache keys, the AniList request body, escapeMarkdown and the live-role helpers. The behaviour tests ported from test/search.test.js, test/sec-misc.test.js (the budget tests), test/review-feeds.test.js (the long-body test) and test/liverole.test.js also run here.
- Database and Redis (tests/db.rs, 24): the test/afk.test.js flows against the real AfkStatus table, the raw SQL run against the schema, and both directions JS to Rust with rust/fixtures/gen/feat-search-js.mjs. That covers AfkStatus rows, the search cache envelopes (AniList, Urban, Pokémon), the Pokémon index, the shared AniList pause, and the cooldown and budget keys.

## API

Entry point: `feat_search::register(&mut Registry)` registers /search, /liverole and /afk and the plugins `liverole` and `afk`. All modules are `pub`.

search_service:
- Lookups:
  - `search_anilist(&App, MediaType, query, allow_adult, user_id: Option<&str>, guild_id: Option<&str>) -> Result<Lookup<AniListResult>, SearchError>`
  - `get_pokemon(&App, &PokemonQuery, user, guild) -> Result<Lookup<PokemonInfo>, SearchError>`
  - `define_urban(&App, term, user, guild) -> Result<Lookup<UrbanResult>, SearchError>`
  - `get_pokemon_index(&App) -> Option<Vec<(String, i64)>>`
- Pure helpers:
  - `clean_query`, `key_form`, `cache_key(Api, source)`, `read_envelope`, `pokemon_slug`, `title_case`, `https_url`, `defang_mentions`
  - `strip_anilist_html`, `strip_urban_brackets`, `clean_urban_text`, `clean_flavor_text`, `parse_retry_after`, `interpret_anilist_response`
  - `project_anilist_results`, `project_pokemon`, `project_urban`, `match_pokemon_index`
- Embeds: `build_anilist_embeds`, `build_pokemon_embed`, `build_urban_embeds`, `no_results_embed`, `search_error_embed`.
- Command flows: `execute_anilist`, `execute_pokemon`, `execute_urban`, `autocomplete_pokemon`.
- Constants: `ANILIST_URL`, `ANILIST_QUERY`, `ANILIST_PAUSE_KEY`, `POKEAPI_INDEX_KEY`, `cache_ttl::*`, `max_bytes::*`, `Api::budget_per_minute`.
- Test seam: `set_fetch(&App, Arc<dyn SearchFetch>)`.

afk_service:
- Text and nickname rules: `clean_reason`, `plain_text`, `name_without_prefix`, `afk_length`, `read_pings`, `jump_link`, `is_ignored_channel(settings, &ChannelPlace)`, `prefixed_nickname`, `restored_nickname`, `manageable`, `can_rename`.
- Nickname changes: `add_nickname_prefix(&App, GuildId, Option<&Member>, settings) -> NickChange`, `remove_nickname_prefix`.
- Id memo: `peek_afk_ids`, `load_afk_ids`, `forget_afk_member`, `forget_afk_ids`.
- Rows: `load_status(&PgPool, g, u)`, `load_statuses`, `set_afk(&App, g, u, reason, nick_changed, previous_nick)`, `clear_afk`, `record_ping(&App, g, u, channel, message, author)` (a `'static` future), `list_statuses -> (rows, total)`.
- Also: `may_notify`, the `afk::*` constants, `GONE_CODES`, `RAW_SQL`.

plugins::afk:
- `handle_message(&App, settings, &AfkMessage, welcome_delete_ms)` with `AfkMessage::of(&MessageCreate)`
- `may_concern_afk`, `welcome_text`, `notice_text`, `shown_name`, `can_reply`, `reply`

plugins::liverole:
- Pure helpers: `is_streaming`, `streaming_changed`, `should_handle_presence`, `stream_label`, `safe_stream_url`, `stream_vars`, `meets_requirement`, `plan_member`, `plan_sync`, `classify_role_error`, `format_sync_result`, `live_role_problems`.
- Engine:
  - `reconcile_member(&App, GuildId, UserId, Mode, Settings, Source) -> Outcome`
  - `count_losing_holders(&Guild, RoleId, settings)`
  - `load_members`
  - `sync_guild(&App, GuildId, Mode, reason) -> SyncResult`
  - `announce_live(&App, GuildId, UserId, settings) -> AnnounceOutcome`
- Also: `startup_syncs_idle`, `DEFAULT_LIVE_ANNOUNCE_MESSAGE`, `limits::*` (the JS `LIVEROLE`), `reasons::*`.

commands::liverole: `bounded_sync`, `describe_rule`, `announce_value`, `status_embed`.

ops: `DiscordOps` trait, `SerenityOps`, `set_ops` (tests), `presences_enabled(&App)` / `set_presences_enabled` (a per-app override of DISABLE_PRESENCES).

## Known gaps

- src/services/searchService.js isAgeRestrictedChannel: serenity's PartialChannel has no nsfw field, so search_service::is_age_restricted_channel reads the channel (and a thread's parent) from the guild cache. A channel that is not cached counts as not age-restricted, so it fails closed: no adult titles, and Urban Dictionary is refused while urbanNsfwOnly is on.
- src/plugins/liverole.js reconcileLocked / syncGuild / countLosingHolders / announceLive and commands/admin/liverole.js runStatus: these read guild.presences from the serenity cache, like discord.js's guild.presences.cache. bot-core caches presences (client::cache_settings) and its gateway runs the presenceUpdate hooks after the cache update (gateway.rs intake). The hook's new_presence only supplies the user id; the newest presence comes from the cache inside the member lock, as in the JS. If the cache ever lags the event, the queued reconcile uses the cached presence.
- src/plugins/liverole.js syncGuild and commands/admin/liverole.js runStatus: holders and streamers are processed and listed in user-id order. discord.js uses cache insertion order. This only changes which members fall within the 50-changes-per-sync cap and the first 15 shown by /liverole status.
- src/plugins/liverole.js reconcileMember: the JS catches any thrown error and returns 'failed'. The Rust engine has no failing paths left (every Discord error is classified), but a panic inside the member lock would propagate (KeyedMutex re-raises it) instead of becoming 'failed'.
- src/plugins/liverole.js reconcileLocked: discord.js 'partial' members do not exist in the serenity cache. A member missing from the cache is fetched over REST (DiscordOps::fetch_member); a cached member is used as is.
- src/services/searchService.js getPokemonIndex: a cached index whose entries are not [string, integer] pairs is treated as a miss, which starts a background refresh. The JS returns such entries and its autocomplete then answers [].
- src/commands/community/afk.js runSet (the undo after a failed save): discord.js patches interaction.member on setNickname. The serenity Member is a snapshot, so the undo rebuilds the prefixed nickname before calling remove_nickname_prefix. The result is the same.
- src/commands/community/afk.js runRemove: when the interaction carries no resolved user (only possible in tests), the target's User comes from the cached or fetched member.
- src/services/searchService.js escapeBlock: bot_core::markdown::escape_markdown has no maskedLink/heading options and is quadratic on long text (about 60 s on 14 KB in a debug build). search_service::escape_markdown_default therefore re-runs the same discord.js patterns, each step only when its marker character is present. The masked-link escape, the spoiler split and the space-before-newline replacement are hand-written single passes. All of them are checked against discord.js output on a sweep of inputs.
- src/services/searchService.js cachedLookup: single-flight uses a shared future. If the first caller is cancelled, a follower finishes the request instead of failing with it; the in-flight entry is removed by whoever completes it (the JS finally).
