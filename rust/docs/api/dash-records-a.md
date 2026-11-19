# dash-records-a: public API (from its author)

All 11 store files are ported in full, each as a dash_collections::RecordStore (files under rust/crates/dash-records-a/src/):
- achievements.store.js -> achievements_store.rs
- automations.store.js -> automations_store.rs
- balances.store.js -> balances_store.rs
- cases.store.js -> cases_store.rs
- economyBoosts.store.js -> economy_boosts_store.rs
- embeds.store.js -> embeds_store.rs
- feeds.store.js -> feeds_store.rs
- giveaways.store.js -> giveaways_store.rs
- levelRoles.store.js -> level_roles_store.rs
- levels.store.js -> levels_store.rs
- polls.store.js -> polls_store.rs
- Shared code is in common.rs: lockGuildSettingsSql, lockSql, TX_OPTS, the transaction error type, and requireTasks/taskOutcome.
- lib.rs has register_stores, store_for, STORE_KEYS and raw_sql (the JS RAW_SQL lists).

How the stores reach data:
- They use the feature crates where the JS calls a service: feat-economy, feat-achievements, feat-automations, feat-embeds, feat-feeds and feat-tags (effective_prefix).
- Where the JS uses Prisma directly they use bot-db/sqlx, with the same locks and transactions. Prisma's unescaped ILIKE is kept, and Prisma.join is kept for votersSql.
- Cache writes match the JS: the embed cache 300 s, the {__none:true} marker 60 s, the achievements index rebuild, and the automations cache drop.
- feed.poll jobs are scheduled or cancelled with the same dedupe key and payload, and the dashboard task kinds and payloads are the same.

Tests: one binary, tests/stores.rs, plus a harness module. The JS side is rust/fixtures/gen/dash-records-a-js.mjs. There are 12 cross-language scenarios, one or two per store, about 400 ops in all:
- The same op list runs through the JS stores on guild A and the Rust stores on guild B.
- After normalising guild ids, cuids and dates, the comparison covers every answer, error (code, message, path, saved), task call and Discord call, with key order checked too.
- Then each side reads back (list plus get) what the other wrote, and those are compared exactly. Seeds use JS services (setXp/applyXpDelta, setBalance, recordInfraction, createPollRow/applyVote, Prisma).
- The scenarios port the store cases of dashboard-collections, dashboard-collections-more, dashboard-moderation, dashboard-giveaways-polls, dashboard-automations, review-dashboard-server (role pings) and sec-dashboard (channel access, publish allowlist).
- Other tests:
  - cap-under-concurrency for levelRoles, economyBoosts and achievements, and the embeds cap;
  - the editor round-trip test;
  - embed cache TTLs, read from both sides;
  - RAW_SQL text checked against a JS golden (rust/fixtures/gen/dash-records-a-golden.mjs -> rust/fixtures/golden/dash-records-a/raw-sql.json) and run in rolled-back transactions;
  - registry registration and action ids;
  - pure helpers;
  - render_for_discord offline.

The ordered key comparison caught one real bug: giveaway and poll task payloads came out in a different key order (serde_json Map::remove swaps). It is fixed with shift_remove.

## API

Crate dash_records_a (rust/crates/dash-records-a/src/lib.rs):

Registration:
- `register_stores(registry: &mut dash_collections::StoreRegistry, app: &bot_core::App) -> Result<(), String>` registers the 11 stores of STORE_KEYS: embeds, feeds, levelRoles, achievements, economyBoosts, cases, levels, balances, giveaways, polls, automations. Err names a key the registry's collections do not have.
- bins/dashboard calls it once at startup, next to dash-records-b's equivalent, then asserts `registry.missing()` is empty. Keep that registry for the life of the process. The feeds 15 s "test" throttle lives in the FeedsStore instance (like the JS module-level throttle), so building stores per request would reset it.

What the App needs: a Postgres pool, the Redis cache (embed cache, achievements index, automations cache drop, automation problem keys, feed fetch cache), settings and the scheduler (feed.poll jobs). No Discord gateway is needed. The Discord REST client and the task runner come in through StoreCtx/ReadCtx (`discord`, `tasks`), as recordsBridge.js passes them.

Other public items:
- `store_for(key, &App) -> Option<Arc<dyn RecordStore>>` builds one store.
- `STORE_KEYS: [&str; 11]`.
- `raw_sql() -> Vec<(name, sql, sample params)>` is the JS RAW_SQL lists: dashboard.lockGuildSettings, dashboard.lockAchievement, dashboard.embeds.lockGuild, dashboard.feeds.lockGuild, dashboard.polls.voters.
- Each store type is public: LevelRolesStore, AchievementsStore, EconomyBoostsStore, CasesStore, LevelsStore, BalancesStore, GiveawaysStore, PollsStore, FeedsStore, EmbedsStore, AutomationsStore, each `::new(App)`.

Helpers the JS exports:
- `embeds_store::render_for_discord(template, &DiscordResources, channel_id, &StoreCtx, Option<&GuildSettings>, RenderMode::{Publish, Edit, Plain}) -> Result<(body, refused_role_ids), StoreError>`
- `automations_store::check_against_server(&App, guild, &StoreCtx, Option<&[Value]>, Option<&[Value]>, &Before)`
- `giveaways_store::HISTORY_PAGE`, `polls_store::HISTORY_PAGE`
- `polls_store::voters_sql(n)`
- `achievements_store::{tier_to_wire, tier_from_wire, ACHIEVEMENT_LOCK_SQL}`
- `cases_store::case_id_of`
- `giveaways_store::{is_item_id, read_history_cursor, names_for, start_payload}`, `polls_store::start_payload`
- `common::{LOCK_GUILD_SETTINGS_SQL, ADVISORY_LOCK_SQL}`

Create and update answer `Saved::item(..)`. The JS stores never use withMessage, so `message` is always None. Actions answer `ActionAnswer { item, message }`.

## Known gaps

- (fixed) feeds_store.rs update/remove now use bot_core's fallible `Scheduler::try_cancel_jobs`, so a database error is a 500 as in the JS.
- embeds/feeds/automations/balances stores (requireDiscord, loadResources, needDiscord, checkHolder: typeof ctx.discord[method] === 'function'): DashboardDiscord cannot say that a single method is missing. 'unavailable' is answered only when there is no client at all.
- embeds/feeds/automations/balances/giveaways/polls stores (isDiscordApiError and catch-all try blocks): the DashboardDiscord trait only returns DiscordApiError, so the JS branches that rethrow other error types (for example a TypeError from the client) cannot happen in Rust.
- All stores, ctx.logger?.info/error: replaced by tracing::info!/error!.
- All stores, Prisma client calls: written out as SQL with the same filters, order, limits and locks. For example `userId IN (...)` becomes `= ANY($n)`, and updateMany becomes UPDATE ... with @updatedAt set by UpdateBuilder or SQL_NOW. Only the RAW_SQL statements are byte-identical to the JS.
- achievements_store.rs date_value (achievements.store.js create/update, new Date(data.deadline)): a deadline is parsed with bot_util date_parse, which reads ISO and HTTP dates only. A V8-legacy date string becomes an internal error where JS would store it. This is the same as the inherited dash-schema datetime gap; validated input is always ISO.
- economy_boosts_store.rs update (economyBoosts.store.js update, toItem(await withUses(...)) on a row deleted mid-request): JS throws a TypeError, which becomes a 500. Rust answers StoreError::Internal with that text.
- Tests: the bot side of the dashboard tasks (xp.adjust, mod.*, giveaway.*, poll.*, access.channels) is scripted with fixed answers in both languages, not the real feat-dashboard-tasks handlers. The JS tests in dashboard-moderation and dashboard-giveaways-polls that drive the real tasks are ported only as far as the store's side goes.
