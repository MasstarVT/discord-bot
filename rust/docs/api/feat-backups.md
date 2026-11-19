# feat-backups: public API (from its author)

All five JS files are ported in full into rust/crates/feat-backups. Every test passes with the mock database variables set, and clippy reports no warnings in the crate.
- src/utils/guildSnapshot.js -> src/guild_snapshot.rs: snapshot v1 capture, parse, bits and masking, restore state, decisions, preview, adoption, phases and run_restore_batch. The engine works on a plain GuildView and makes Discord calls only through the RestoreApi trait. Each result is applied to the engine's own view, as the discord.js cache does.
- The Discord side of the engine is new in src/live_guild.rs: the guild view from the serenity cache, LiveGuild (the REST calls with the discord.js bodies) and the summary post and DM.
- src/services/backupService.js -> src/backup_service.rs: every export, the backup.auto and backup.restore jobs, the embeds and the persistent backup button. RestoreHost (RestoreApi plus the summary post) lets the restore job run against a fake.
- src/plugins/backups.js -> src/plugin.rs.
- src/commands/admin/backup.js -> src/commands/backup.rs.
- src/commands/admin/emoji.js -> src/commands/emoji.rs: pure helpers, the upload guard in app.ext, and test seams (EmojiTestHooks).

Tests are in two files:
- tests/backups.rs (30 tests):
  - Golden sweeps against rust/fixtures/golden/feat-backups/golden.json, made by rust/fixtures/gen/feat-backups-golden.mjs. Ten whole restores of the JS fake guild match call for call: state, create/edit/put logs, role and channel order, the guild afterwards, previews and summary and preview embeds.
  - Ported engine tests: crash adoption, idempotency, timeouts and stalls, a private channel created private, the sec-misc adoption rules, and a check that the restore never deletes.
  - Database tests of the service and both jobs.
  - Router tests of /backup and of the Confirm button.
  - Cross-language tests in both directions through rust/fixtures/gen/feat-backups-js.mjs: JS createBackup -> Rust parse and restore, Rust create -> JS read and restore, JS startRestore -> Rust runs the job, Rust start_restore -> JS reads the payload, and list/search/count/newest compared with JS.
- tests/emoji.rs (7 tests): parser and text sweeps against JS, the upload guard, and add/delete/autocomplete through the router.
- The fake guild shared by the fixture scripts is rust/fixtures/gen/feat-backups-fake.mjs.

## API

Entry point: feat_backups::register(reg). Public modules:

guild_snapshot (pure engine):
- capture_guild(&GuildView, now) -> Snapshot; Snapshot::to_json() is the stored data and to_parsed_json() adds idMap.
- parse_snapshot(&Value) -> Result<Snapshot, SnapshotError>.
- exportable(&Value), snapshot_bytes(&Value), snapshot_stats(&Snapshot).
- bits / bits_str, mask_bits, bot_bits, bit_names, all_permission_bits.
- parse_id_map, new_restore_state(backup_id, mode, started_by, channel, &id_map, now_ms), normalise_restore_state(&Value, now_ms) -> Option<RestoreState>; RestoreState serialises to the JS payload.
- preview_restore(&GuildView, &Snapshot, mode, &id_map) -> Preview.
- make_env(view, &snapshot, state, reason, Some(&dyn RestoreApi)) + run_restore_batch(&mut env) -> BatchResult.
- decide_role / decide_channel / decide_overwrite.
- Trait RestoreApi (view() plus the REST calls).

live_guild:
- LiveGuild::new(&app, guild_id) implements RestoreApi from the serenity cache and REST; it also offers cached(), channel_usable, send_channel, send_dm and fetch_me.
- guild_view(&Guild, bot_id).

backup_service (all take &App or &PgPool plus plain ids):
- Constants and keys: mod backup (BACKUP constants), auto_job_key, restore_job_key, is_live.
- Rows: clean_name(Option<&str>), stamp_of, backup_label(&BackupInfo). BackupInfo is LIST_SELECT and serialises to the Prisma shape.
- Queries:
  - list_backups(db, g); get_backup(db, g, id); get_backup_with_data(db, g, id) -> ServerBackup; delete_backup(db, g, id) -> u64.
  - count_backups -> BackupCounts{manual, automatic}; newest_automatic -> Option<(id, createdAt)>; search_backups(db, g, q).
  - prune_automatic(db, g, keep: Option<i64>, spare: &[String]).
- Backups and jobs:
  - create_backup(&app, &GuildView, created_by, name, automatic) -> CreateResult::{Ok(BackupInfo), Refused{reason, error}}.
  - ensure_auto_job(&app, g, Option<run_at>) -> bool; on_guild_ready (hook).
  - handle_auto_job / handle_auto_job_with(&app, g, Option<&GuildView>) -> JobOutcome.
  - handle_restore_job / handle_restore_job_with(&app, &Job, late_by_ms, attempt, &dyn RestoreHost).
- Starting a restore:
  - seed_id_map(&GuildView, &Snapshot); carried_from(&GuildView, Option<&ScheduledJob>, backup_id, now_ms) -> Carried.
  - restore_preview(view, snapshot, mode, prev_job, backup_id, now_ms): note the extra now_ms argument here and on carried_from.
  - start_restore(&app, view, backup_id, &snapshot, mode, started_by, channel) -> Result<Result<(), StartRefused::{Running, Mode}>>.
  - missing_restore_permissions(Option<&MeView>).
- Embeds and texts: missing_permissions_embed, build_summary_embed(&RestoreState, stopped), post_summary, owner_only_embed, build_info_embed, build_list_embed, build_preview_embed, restore_buttons(id, mode), restore_status_text(job, now_ms), build_status_embed(StatusInput).
- Button: handle_backup_button(ComponentCtx).
- RAW_SQL: the module's raw statements.

commands::backup: data(), command(), run_delete_with(ctx, confirm) as the test seam, export_file(&ServerBackup) -> (name, bytes).

commands::emoji:
- data(), command().
- Helpers: parse_emoji_source -> EmojiSource, sanitize_emoji_name, name_from_url, resolve_emoji_name -> NameResult, taken_names, pick_free_name, emoji_slot_limit, count_emoji_slots -> Slots, format_slots, sniff_image, describe_fetch_error, describe_emoji_error, format_emoji_line, build_list_pages, resolve_guild_emoji, emoji_choices, emoji_cdn_url.
- State: UploadGuard (app.ext) and EmojiTestHooks (app.ext test seam).

## Known gaps

- guild_snapshot.rs api(): a Discord call that times out is cancelled (tokio::time::timeout drops it), where the JS withTimeout lets it keep running. The next batch's audit-log adoption covers a create that still went through.
- backup_service.rs get_restore_job / create_backup / start_restore: a database error while reading the restore job becomes None (bot-core get_job logs it), so a backup or restore goes ahead where the JS would throw.
- backup_service.rs post_summary: the start channel is resolved from the serenity cache, which can lag behind a channel the same batch just recreated. The summary then goes by DM where the JS would post in the channel.
- guild_snapshot.rs RoleData::to_body: role colours are sent as colors.primary_color only. Gradient secondary and tertiary colours are not sent, as in the JS, which keeps only the primary colour.
- live_guild.rs guild_view: roles and channels are ordered by id. discord.js keeps gateway insertion order, which only matters when two bot-made roles or channels have the same name during adoption.
- live_guild.rs LiveGuild::create_channel / channel_view: available_tags is sent with emoji_id null, so a custom-emoji tag loses its emoji (the JS keeps none either). manageable comes from serenity's permission computation.
- backup_service.rs on_guild_ready: the 0-5 min startup jitter comes from the system clock's sub-second nanoseconds, not Math.random.
- commands/emoji.rs run_list: falls back to the cache when GET /emojis fails or takes over 10 s (same as the JS). The list fetch is cancelled on timeout, which is harmless.
- commands/emoji.rs invoker_has: the invoker's guild permissions come from the cached member's roles (owner = all). When the guild is not cached, the interaction's own member.permissions are used instead, as in the JS fallback.
- backup_service.rs search_backups: % and _ in the query are LIKE wildcards, matching Prisma's contains (shown by the cross-language test); this is not a behaviour difference, just noted.
