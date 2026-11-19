# dash-records-b: public API (from its author)

I ported all 11 record stores in full, with no stubs. Each is a RecordStore implementation, in rust/crates/dash-records-b/src/:
- reactionRoles.store.js -> reaction_roles_store.rs: rr.publish, rr.repost, rr.delete and rr.roleTemplate tasks; the rrpanel and rritems advisory locks; RAW_SQL.
- reminders.store.js -> reminders_store.rs: validate_reminder_input, sync_reminder_job / reminder.fire jobs, the 'test' action, RAW_SQL.
- shopItems.store.js -> shop_items_store.rs (through feat-economy).
- starboards.store.js -> starboards_store.rs (through feat-starboard-stats).
- statChannels.store.js -> stat_channels_store.rs: the stats.refresh job and the 'refresh' action.
- tags.store.js -> tags_store.rs (through feat-tags): role, ping and channel-access rules; cooldown clearing.
- tempVoiceHubs.store.js -> temp_voice_hubs_store.rs (through feat-tempvoice).
- ticketPanels.store.js -> ticket_panels_store.rs: the ticket.check, ticket.publish, ticket.refresh and ticket.delete tasks; option diffing under the ticket-option lock; the 'publish' and 'refresh' actions; RAW_SQL.
- translateChannels.store.js -> translate_channels_store.rs (through feat-translation).
- translateStats.store.js -> translate_stats_store.rs.
- triggers.store.js -> triggers_store.rs (through feat-tags).

Shared helpers are in common.rs: lockSql, load_resources, run_after_save / taskOutcome, and the collection check re-run on update. lib.rs holds register, stores and KEYS.

Tests, run with TEST_DATABASE_URL and TEST_REDIS_URL set (mock DB dash_records_b, Redis db 36), nothing skipped:
- tests/db.rs (13 tests). rust/fixtures/gen/dash-records-b-js.mjs runs the JS stores. Each scenario runs the same steps through JS on one guild and through Rust on another. Every step is compared after normalizing ids, guild ids and run timestamps (createdAt, updatedAt, lastRenamedAt, messageMissingAt). Reminder nextRunAt and job runAt are compared unmasked. The comparison covers:
  - answers and error code/message/path/saved;
  - task kinds and payloads, and Discord calls;
  - DB rows, ScheduledJob rows and Redis cache values.
- Both directions are checked: JS lists and reads the cache of what Rust wrote, and Rust lists what JS wrote.
- Ported from test/dashboard-collections, dashboard-collections-more, dashboard-panels and sec-dashboard:
  - the custom command, auto-responder and reminder DB cases;
  - the panel cases (publish, sync, repost, delete, templates, unposted, failed and pending tasks);
  - channel-access refusals;
  - caps, including caps under concurrent creates;
  - 'what a store returns is accepted back as an update' for 10 stores;
  - RAW_SQL statements run in rolled-back transactions.
- tests/offline.rs (9 tests): registration, action ids per collection, the refusals a store gives before touching the DB, and helpers.

## API

Crate dash_records_b, for bins/dashboard. Call it once at startup.

Registration:
- `register(registry: &mut dash_collections::StoreRegistry, app: &bot_core::App) -> Result<(), String>` registers the stores of reactionRoles, reminders, shopItems, starboards, statChannels, tags, tempVoiceHubs, ticketPanels, translateChannels, translateStats and triggers. `Err` names a key the registry's collections do not know.
- Register dash-records-a too, then assert that `registry.missing()` is empty.
- `stores(&App) -> Vec<(&'static str, Arc<dyn RecordStore>)>` gives the same pairs; `KEYS: [&str; 11]` lists the keys.

The App:
- It must be built on the same PostgreSQL and Redis as the bot, e.g. `App::production(pool, REDIS_URL)`.
- Through it the stores write the cache invalidations the bot reacts to: guild:{g}:triggers, guild:{g}:translateChannels, guild:{g}:starboards, guild:{g}:rrpanels, and the tag:cd:{g}:{id}* keys.
- They also write ScheduledJob rows (reminder.fire with dedupe reminder:{id}; stats.refresh with dedupe stats.refresh:{g}) and GuildSettings changes (translationEnabled).

Each store is a public struct built with `::new(App)`: ReactionRolesStore, RemindersStore, ShopItemsStore, StarboardsStore, StatChannelsStore, TagsStore, TempVoiceHubsStore, TicketPanelsStore, TranslateChannelsStore, TranslateStatsStore, TriggersStore. Action ids: statChannels ['refresh'], reminders ['test'], reactionRoles ['repost'], ticketPanels ['publish', 'refresh'].

Dashboard tasks go through StoreCtx.tasks (TaskRunner) with the JS kinds and payloads:
- rr.publish {groupId, mode:'post'|'sync', removedEmojis?}
- rr.repost {groupId, channelId}
- rr.delete {groupId}
- rr.roleTemplate {groupId, template}
- ticket.check {categoryIds, transcriptChannelId}
- ticket.publish {panelId, channelId}
- ticket.refresh {panelId, message?, managersAdded?, managersRemoved?}
- ticket.delete {panelId}
- access.channels {checks}

Raw statements: `reminders_store::RAW_SQL`, `reaction_roles_store::RAW_SQL` and `ticket_panels_store::RAW_SQL` (name, sql, sample bind).

Helpers in `common`: `load_resources`, `run_task`, `run_after_save`, `recheck`, `custom_emoji_id`, `locale_int`. `reminders_store::schedule_error` is public for tests.

## Known gaps

- src/dashboard/collections/starboards.store.js store.create -> starboards_store.rs create_board: starboardService.createBoard is replicated in this crate (ensureGuildSettings, the GuildSettings row lock, the cap count, the insert, invalidate_boards), statement for statement. The reason is that feat-starboard-stats BoardPatch has no ignoredChannelIds field, so the service's create_board cannot store the ignore list the form sends.
- src/dashboard/collections/reminders.store.js actions.test -> reminders_store.rs preview_payload: reminderService.buildSendPayload(row, { guild, channel, settings, ping: false }) is replicated: normalize_template, render_template with the default-template fallback, the ping-role content prefix, embedColor. The reason is that feat-feeds build_send_payload needs serenity Guild/GuildChannel, which the dashboard does not have. It renders with dash-collections render_context_for(resources), as the JS does.
- All stores, ctx.logger?.info?.(...): logged with bot_util::log_info! (the process logger), because StoreCtx carries no logger.
- src/dashboard/collections/triggers.store.js update: if the row is deleted between the optimistic write and the re-read, feat-tags write_trigger answers CONFLICT (the JS writeTrigger returns null). The store then answers the same not_found, but it does not call invalidateTriggerCache first as the JS does. The deleting write has already dropped the cache.
- triggers/tags/reminders.store.js String(ctx?.userId ?? '0') -> common.rs actor_id: an empty StoreCtx.user_id is stored as '0'. The JS keeps '' when userId is ''.
- src/dashboard/collections/translateChannels.store.js cleanLanguages String(invalid[0]).slice(0, 40) -> common.rs js_slice: cut at 40 UTF-16 units like the JS, but a surrogate pair split at the cut becomes U+FFFD. The JS keeps the lone surrogate.
- src/dashboard/collections/statChannels.store.js create: tx.$queryRaw(lockGuildSettingsSql(guildId)) is imported in the JS from levelRoles.store.js, which belongs to dash-records-a. This crate uses bot_db::tx::LOCK_GUILD_SETTINGS_SQL, which is the same SQL text.
- src/dashboard/collections/reactionRoles.store.js list (include items orderBy position) and ticketPanels.store.js list (include options): Prisma's second query `WHERE groupId IN (...)` is written as `= ANY($1)` with the same ORDER BY. The rows returned are the same.
