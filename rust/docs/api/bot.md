# bot: public API (from its author)

I ported the bot binary: index.js and src/bot.js, plus dotenv/config, which no other crate had covered. All 16 tests pass with the mock database and Redis variables set, nothing was skipped, and clippy shows no warnings in this crate.

What went where:
- index.js -> rust/bins/bot/src/index.rs. It covers the `--deploy` / `deploy` mode, the health endpoints on HEALTH_PORT, the SIGTERM/SIGINT shutdown, and the missing-env and fatal-code parks with the 10-minute reminder. It also covers the restart brake and its brake-delay phase, the auto-deploy (same hash, same DEPLOY_HASH_FILE format, 120 s bound), the shard start, the 60 s ready wait, `/readyz`, and the exits on a dead client. The shutdown is bounded at 20 s for the shards and 25 s overall; a second signal exits at once.
- src/bot.js -> src/bot.rs: building the app on PostgreSQL and Redis, a BotHandle that stays usable while the client task runs, and the shutdown in the bot.js order (scheduler 5 s, then the flushes, then the shards, then the pool and Redis).
- dotenv 16.6.1 -> src/dotenv.rs: dotenv's own LINE regex run through regress, CRLF handling, quote stripping, `\n`/`\r` expansion only inside double quotes, no `$VAR` expansion, existing variables win, plus the DOTENV_CONFIG_PATH/OVERRIDE options and their `dotenv_config_*=` arguments.
- The JS loaders' file discovery -> src/registry.rs: the 27 feature `register` functions.
- Plain `fn main` in src/main.rs: loads `.env` before any thread starts, then builds the tokio runtime.
- Migrations: on start, when DATABASE_URL is set, the binary runs bot_db's `migrate_deploy` and exits 1 if it fails. This copies the Docker image's `prisma migrate deploy`.
- I checked src/ for runtime files nobody owns: every src/{utils,services,database,handlers,events} file is covered by groups.json, bot-core, bot-util, bot-db or bot-cards, so nothing else needed porting.
- Also written: rust/README.md (build, run, deploy, tests, switching between the JS and Rust bots), rust/fixtures/gen/bot-bin-js.mjs (the JS deploy hash and dotenv.parse), and rust/fixtures/golden/bot/dotenv-sample.env.

Tests:
- tests/registry.rs (15 tests, offline):
  - The full registry matches the golden files: 79 commands byte-identical with their routing metadata, 43 plugins, and `persistent_handler_problems` is empty.
  - Plugins come out in plugins.json order whatever order the crates register in.
  - Ported from test/integration.test.js: A1–A9 (including the EXPECTED_COMMANDS table), B1–B3, C1, C6, D1, E1, E3, E4, F1, F3, H3 (checked against architecture.md and config.json), K8, and Z1–Z5 run on one real install.
  - The deploy hash equals the JS computeDeployHash, with and without a guild id. With the JS-written hash file, `auto_deploy_commands` returns Unchanged.
  - dotenv parsing matches JS dotenv on a sample with awkward cases.
  - The index.js helpers: port parsing, env paths, readiness and parking, and the intents.
- tests/startup.rs (1 test, mock database and Redis):
  - `migrate_deploy` applies nothing, so Prisma's recorded checksums match this build.
  - The JS scheduleJob (through bot-core-js.mjs) writes a due `mod.unban` job.
  - The app is built with production wiring and Discord offline (FakeRest). All 27 crates install, every module is turned on, and a READY event goes through `gateway::process`.
  - Checked: the scheduler started, the "Ready as … | 1 guild(s) | Shard 0" and "guildReady completed for 1/1 guild(s)." lines, the settings row, and the levels.voiceSweep and birthday.tick jobs seeded by the gated levels and the birthdays startup hooks.
  - The scheduler claims the JS job and the Rust handler runs it: one DELETE /guilds/G/bans/U with reason "Temporary ban expired", an UNBAN infraction row, and the job row deleted.
  - After shutdown the scheduler is stopped, nothing is in flight, the pool is closed, and the hook runner and scheduler logged no failure lines.
  - No plugin declares an init (hasInit is false for all 43 in plugins.json), so there was no plugin init to run. The boot steps did run (feat-games' numguess hook is installed).

Also checked by running the binary: `bot deploy` without credentials exits 1 with the JS message. A start with no env parks: `/readyz` returns 503 `{"status":"not-ready","reason":"parked:missing-env"}` and `/healthz` returns 200.

## API

Package `bot` (lib `bot` plus bin `bot`; `bot` starts the bot, `bot deploy` or `bot --deploy` pushes the commands).

- `bot::registry`: `FEATURES: [(&str, RegisterFn); 27]`, `register_fns()`, `build_registry() -> Registry` (every command and plugin of the JS bot).
- `bot::bot`: `build_app(pool, redis_url, AppConfig) -> Result<App, CacheError>` (production wiring); `BotHandle::of(&Bot)` with `ready_states()`, `shard_is_ready(id)`, `shutdown()`; `shutdown(&App, Option<(&Arc<ShardManager>, &Arc<GatewayState>)>)` (the bot.js shutdown order); `sync_levels_ready_at(App)` (no longer called: feat-levels reads `App::ready_at_ms`). The binary uses `Bot::split()`.
- `bot::index`: `Env::from_process()` (with `missing()`, `state_dir()`, `deploy_config()`), `run(Env) -> i32`, `boot(Arc<Lifecycle>, Env)`, `deploy_mode(&DeployConfig) -> i32`, `migrate(url)` / `migrate_logged(url)`, `Lifecycle` (with `readiness()`, `park()`, `set_phase()`), `readiness_of(&[bool])`, `parse_health_port` / `health_port_number`, `park_text`, `restart_brake`, `start_health`, `install_signal_handlers`, `shutdown(lc, signal)`, and the index.js constants.
- `bot::dotenv`: `parse(&str) -> IndexMap`, `options(env, argv)`, `plan(cwd, &Options, is_set)`, `load_into_process_env()`.

Files are in rust/bins/bot/src/ and rust/bins/bot/tests/; the fixture script is rust/fixtures/gen/bot-bin-js.mjs and the dotenv sample is rust/fixtures/golden/bot/dotenv-sample.env.

## Known gaps

- index.js ShardingManager (totalShards 'auto', mode 'worker', respawn false) -> every shard runs in this one process on serenity's shard manager; the shard count still comes from Discord's recommendation. Shard events are emulated: 'Spawning Shard N...' is logged after the login has started, and 'Shard N is ready.' when bot-core's gateway records ClientReady. There is no 'Shard N reconnecting...' line. A client exit is reported as 'Shard 0 died/disconnected' whichever shard failed, and the whole client stops together.
- src/utils/logger.js shardPrefix: lines logged on the shard side have no '[Shard N] ' prefix, because index.js and bot.js share one process (bot_util::logger::set_shard_id is never set).
- index.js process.on('uncaughtException') -> exit(1) and process.on('unhandledRejection'): not ported. Rust has no rejections; panics in tokio tasks are caught and bot-core logs handler panics. A panic on the main thread uses Rust's default handler (exit code 101).
- index.js signals: SIGTERM and SIGINT on Unix; on Windows only Ctrl+C (handled as SIGINT).
- index.js STATE_DIR and commandLoader.js HASH_FILE: the JS resolves 'state' and '.deploy-hash' against the repository root (__dirname). Rust resolves them against the working directory, so the binary must run from the repository root, as rust/README.md says. A relative DEPLOY_HASH_FILE is relative to the working directory in both.
- Migrations: the Rust start applies pending Prisma migrations when DATABASE_URL is set (the JS Docker CMD behaviour). `npm start` outside Docker applies none. (fixed) a modified applied migration is now a warning, as in Prisma (`The migration `<name>` was modified after it was applied.`).
- dotenv/config: .env.vault / DOTENV_KEY decryption is not supported. DOTENV_CONFIG_ENCODING is ignored: the file is read as UTF-8 and invalid bytes are replaced. DOTENV_CONFIG_DEBUG/QUIET log nothing. Keys the OS cannot hold (containing '=' or NUL) are skipped instead of set.
- index.js HEALTH_PORT: an invalid value goes to the error handler like Node's listen error, but the message text is my own ('options.port should be >= 0 and < 65536. Received NaN.').
- src/bot.js partials (Channel, Message, Reaction, GuildMember, User, GuildScheduledEvent): serenity has no partials. bot-core's gateway builds the same payloads from raw events and its pre-update snapshots.
- src/bot.js makeCache limits: GuildStickerManager 0, GuildScheduledEventManager 0, StageInstanceManager 0 and ThreadMemberManager 0 have no serenity equivalent; serenity keeps stickers, scheduled events and stage instances on the cached guild. MessageManager 200 per channel is matched (max_messages = 200). Members, presences, channels and users are unlimited in both.
- src/bot.js sweepers: the 3 h message lifetime is emulated by bot-core (a message older than 3 h counts as uncached) but nothing is evicted beyond the 200-per-channel cap. discord.js's default archived-thread sweeper (DefaultSweeperSettings.threads) is not ported.
- src/bot.js allowedMentions and src/events/ready.js setActivity('your server', Watching) match (bot-core sets both); intents match, with GuildPresences dropped when DISABLE_PRESENCES is set.
- (fixed) src/handlers/commandLoader.js loadCommands order: `build_registry` sorts the commands into the JS walkDir order (`registry::JS_COMMAND_ORDER`, tested against commands.json).
- src/bot.js reportFatal / 'Discord login failed with <code>': logged only when the login itself fails (the gateway lookup returns 401). A fatal close code during IDENTIFY parks with the default PARKED text only.
- During a shutdown with no live client (still starting, parked, or dead), the binary also closes the database pool and Redis quietly. The JS main thread has no such connections to close.
