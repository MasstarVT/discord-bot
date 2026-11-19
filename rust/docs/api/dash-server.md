# dash-server: public API (from its author)

I ported src/dashboard/server.js, discordApi.js and landing.js in full, and the static front end is served unchanged.

Ported files, JS to Rust (all under rust/crates/dash-server/src/):
- server.js to server.rs. One axum fallback handler matches the raw request path the way the JS does, so every check runs in the JS order: method, CSRF, session, ids, rate limits, body, permissions. It has every route, header, status code, error shape and log line.
- server.js static files to static_files.rs: the 23-path whitelist, ETag and gzip, If-None-Match, and the files embedded from src/dashboard/public with a directory override.
- server.js listen and shutdown, and createParkedServer, to serve.rs: a hyper accept loop that hands each request its ConnectInfo, with a 10 s header timeout, and closes every connection at shutdown.
- discordApi.js to discord_api.rs, over reqwest. It ports the caches with TTLs and size caps, shared in-flight loads, the 429 retries with an injectable pause, the error mapping, mapGuildResources, sortChannels, safeMentions and the invite and CDN URLs. The API base, the OAuth endpoints, the clock and the pause are injectable.
- landing.js to landing.rs.
- Intl.Collator('en', {sensitivity:'base', numeric:true}) to collate.rs.
- logger.rs holds the injectable logger.

Goldens come from rust/fixtures/gen/dash-server-golden.mjs, which writes rust/fixtures/golden/dash-server/: http.json, discord-api.json, pages.json, collator.json and the public-test/ directory.
- http.json: the script runs the real JS server in-process with fakes ported one to one from test/dashboard.test.js, sec-dashboard.test.js and review-dashboard-server.test.js. It has 16 sessions and 1648 answers.
- tests/golden.rs replays every step on the Router, in order. Status, headers, bodies, log lines and the calls the fakes saw all match.
- Every Discord client call matches the JS when replayed against a local stub serving the recorded answers.
- The pages are byte-identical, and the collator matches on a 4624-pair table.

tests/behaviour.rs ports the cases that need time, concurrency or a socket:
- the client's caching, sharing and retry cases;
- the concurrent login-callback case;
- on a real socket: HEAD, 204 and 304 framing, 413 closing the connection, absolute-form targets, traversal paths, the peer address reaching the login limit, and shutdown;
- trusted proxies, the "logs never contain tokens" case, and the front-end check that pages link only to served paths.

The crate has no SQL or cache code, so there are no database tests. The whole suite was run with TEST_DATABASE_URL and TEST_REDIS_URL set (mock database dash_server) and nothing was skipped. cargo check, cargo test and cargo clippy --all-targets are clean.

## API

Crate dash_server. Main items are re-exported at the root.

Building the server:
- `create_dashboard_router(ServerOptions) -> Result<axum::Router, String>` is createDashboardServer. It fails only when the session key cannot be derived (empty secret).
- `ServerOptions::new(config: dash_auth::DashboardConfig, deps: DashboardDeps)` has public fields:
  - `logger: Option<Arc<dyn DashLogger>>`, default BotLogger;
  - `public_dir: PublicDir::{Embedded (default) | Path(PathBuf)}`;
  - `changelog_path: Option<PathBuf>`, default the embedded CHANGELOG.md.
- `DashboardDeps` fields:
  - `discord: Arc<dyn ServerDiscord>`;
  - `settings: Arc<dyn dash_collections::SettingsBridge>`;
  - `records: Option<Arc<dyn RecordsBridge>>`: None makes the record routes answer 503;
  - `bot_joins: Option<Arc<dyn BotJoins>>`;
  - `collections: Option<dash_collections::Collections>`: default builtin;
  - `now: Option<Clock>`.

Discord client:
- `ServerDiscord: DashboardDiscord` adds:
  - `get_current_user`, `get_user_guilds(uid, token, allow_stale)`, `forget_user`, `get_bot_guilds`, `is_bot_in_guild(id, verify)`, `invite_url(Option<&str>)`;
  - `exchange_code -> anyhow::Result<TokenGrant>`, `revoke_token`.
- `DiscordApi` (discordApi.js createDiscordApi) implements both traits.
  - Build it with `DiscordApi::from_config(&config, Some(logger))`, or with `DiscordApi::new(DiscordApiOptions{config: DiscordApiConfig, api_base, token_url, revoke_url, now, logger, sleep})`.
  - It also has `get_bot_guild_ids()` and `search_members_limit(g, q, limit)`.
  - The pure helpers are public: `user_avatar_url`, `guild_icon_url`, `invite_url`, `sort_channels`, `map_guild_resources`, `safe_mentions`, `member_profile`, `normalize_guild`.
  - So are the constants `INVITE_PERMISSIONS`, `DANGEROUS_PERMISSIONS`, `USER_AGENT` and the TTLs.

Bot joins:
- `BotJoins::list_bot_joins(&[String]) -> anyhow::Result<HashMap<String, BotJoinRow{inviter_id, inviter_name, source, joined_at}>>` is botJoinService.listBotJoins.
- bins/dashboard implements it over feat-general's botJoinService. `joined_at` must be the `toISOString()` form (bot_util::js::to_iso_string).

Running it:
- `serve(router, host, port, shutdown_future) -> io::Result<()>` is server.listen(port, host) plus close() and closeAllConnections(). It fails when the address cannot be bound; dashboard.js logs that and exits 1.
- `serve_listener(listener, router, headers_timeout_ms, shutdown)` takes an already bound listener.
- The router needs `ConnectInfo<SocketAddr>` on every request. serve() supplies it. Plain axum::serve without connect info makes every client 'unknown', which is one login-limit source for everyone.
- serve() logs nothing. dashboard.js's 'Dashboard listening on …' and 'parked' lines, its signal handling and its 10 s forced exit belong to bins/dashboard.
- `create_parked_router(&missing, &invalid_keys) -> Router` is createParkedServer; serve it with serve() too.

Other modules:
- `landing::{render_landing, render_docs, render_legal, render_changelog}(template, &PageOptions) -> Result<String, String>`, plus `landing::COMMANDS`.
- `server::contact_html`, `SECURITY_HEADERS`, `BODY_LIMIT_BYTES`, `RECORD_BODY_LIMIT_BYTES`, `PROPAGATION_SECONDS`.
- `static_files::{STATIC_FILES, static_file, accepts_gzip, etag_matches, EMBEDDED, EMBEDDED_CHANGELOG}`.
- `collate::name_compare`.
- `RecordingLogger` for tests.

## Known gaps

- collate.rs name_compare (server.js and discordApi.js nameCollator, Intl.Collator en/base/numeric): an approximation of ICU. It covers case and accent folding, æ/ß/œ/ø/đ/ł/þ expansions, digit runs compared by value, ICU's ASCII punctuation order, other symbols before '$', then Latin letters, then other scripts by code point. It matches a 4624-pair Node table but is not full ICU for other scripts or rarer punctuation.
- static_files.rs make_entry (server.js loadStaticEntry): the gzip copy is made by flate2/miniz_oxide at level 9, not zlib. The compressed bytes differ, so the 'gzip only when smaller' decision could differ for a file at the edge. ETags are unchanged because they are hashed from the identity bytes.
- serve.rs serve_connection (server.js headersTimeout/keepAliveTimeout): hyper's header_read_timeout of 10 s also bounds idle keep-alive connections. Node closes idle connections after 5 s (keepAliveTimeout) and sends 'Keep-Alive: timeout=5'; Rust sends no Keep-Alive header.
- server.rs read_body (server.js requestTimeout 15 s): the 15 s limit starts when the body is read, not when the request starts. The 408 answer carries the security headers and Content-Length 0; Node writes a bare 408.
- Header names go out in lower case on HTTP/1 (hyper), and hyper's request header size limit is not Node's 16 KiB maxHeaderSize, so a 431 for oversized headers can differ.
- server.rs handle (server.js handle catch): a dependency failure that is not a typed error (the JS fake's getUserGuilds returning null, giving a TypeError) has no typed form in Rust. It is a panic caught with catch_unwind, which answers the same 500 internal_error and the same log line. Unwinding is required; no profile sets panic=abort.
- static_files.rs PublicDir::Embedded (server.js PUBLIC_DIR): by default the front end and CHANGELOG.md are embedded at build time, so editing public/ needs a rebuild. PublicDir::Path and changelog_path read from disk, as the JS does.
- logger.rs BotLogger::error (logger.error(msg, err)): logs the error's message without a stack.
- discord_api.rs load_guild_resources and the server's guild list (Promise.all): futures::try_join returns the first error it polls, which may not be the first to reject in time.
- discord_api.rs DiscordApi: reqwest sends 'User-Agent: node' (what Node's fetch sends) and 'Accept-Encoding: gzip'. Node also sends 'accept-language: *', 'sec-fetch-mode: cors' and 'accept-encoding: gzip, deflate'.
- Not ported here, because they test record stores owned by other crates: the channel-access describe of test/sec-dashboard.test.js, its saved-embed role-ping describe, and the automation role-ping describe of test/review-dashboard-server.test.js. test/review-dashboard-frontend.test.js tests browser JS that stays JavaScript.
