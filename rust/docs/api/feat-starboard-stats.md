# feat-starboard-stats: public API (from its author)

The crate was still the empty skeleton after the crash, so this is a complete port from scratch.
- src/services/starboardService.js -> src/starboard_service.rs. Discord calls go through src/starboard_io.rs: the StarboardIo trait, a serenity implementation, and set_starboard_io for tests. The shared DiscordError type and a capped insertion-ordered map are in src/discord_io.rs.
- src/services/statsService.js -> src/stats_service.rs, with Discord through src/stats_io.rs (StatsIo trait, serenity implementation, set_stats_io).
- src/plugins/starboard.js -> src/plugins/starboard.rs. All 8 hooks, filters as in the JS; messageDelete, messageDeleteBulk and channelDelete are always-on. The reaction, edit and delete hooks spawn their work and return at once, like the JS.
- src/plugins/stat-channels.js -> src/plugins/stat_channels.rs: guildReady, guildMemberAdd/Remove and channelDelete hooks, plus the stats.refresh job (Done or Reschedule).
- src/commands/admin/starboard.js -> src/commands/starboard.rs: create, edit, delete (with the confirm prompt), list and autocomplete.
- src/commands/admin/stats.js -> src/commands/stats.rs: create, edit, delete, list, refresh, category and autocomplete.
- The JS module-level state (board memo with generations, reposted map, coalescer, keyed mutex, semaphore, throttles, rows memo, pending pulls, busy guilds, fetch back-off, name aliases, config mutex) lives in per-app structs via app.ext.
- Redis key guild:{g}:starboards is written in the JS key order with a 300 s TTL, and read element by element with the JS defaults.
- Every hand-written SQL statement is in a RAW_SQL list, uses SQL_NOW and sets updatedAt.
- JS side of the tests: rust/fixtures/gen/feat-starboard-stats-js.mjs.

Tests: tests/starboard.rs (37) and tests/stats.rs (26).
- Golden checks for both commands and both plugins.
- The JS tests ported: the pure functions, coalescer, reaction-user paging, early exits, self-star removal, the full sync and refresh engines against a fake Discord layer, CRUD, rule-20 cases, job and hooks, both commands through the router (including the delete confirm collector), and the plugins dispatched through the hook runner.
- Checked against the JS: board-name, emoji-list and unicode-emoji sweeps, the repost payload, the settings summary, and stat templates and names.
- Cross-language, both directions: the cached board list; starboard entries (JS sync, then a Rust sync, then a JS sync of the same entry); boards and updates; StatChannel rows and overwrites; the stats.refresh job, its dedupe key and its {} payload; and JS refresh respecting the Rust rename reservation.

## API

The only entry point is register(reg). Every module is public.

feat_starboard_stats::starboard_service (sb):
- Pure helpers:
  - names and emoji: normalize_board_name, normalize_emoji_key, emoji_key_of, board_matches_emoji, is_unicode_emoji, parse_emoji_list(input, &[SbEmoji]) -> Result<Vec<String>, String>, format_emoji_key, format_emoji_list, format_emoji_list_app.
  - eligibility: snowflake_time, channel_chain, is_nsfw_channel, source_skip_reason, message_skip_reason.
  - counting and decisions: compute_count, decide_action -> SyncAction, pick_display_emoji, build_count_line.
  - repost payload: select_image, build_repost_payload(guild_id, &Message, count, emoji_text) -> API JSON.
  - settings_summary(&BoardLite, custom_emoji_fn, reposts).
- Coalescer: Coalescer::new / request / waiting_size, sync_spacing_for, note_reposted.
- Board cache:
  - BoardLite (camelCase serde, the JS key order), to_board_lite, boards_key.
  - peek_boards, get_boards(app, g, fresh) -> Option<Arc<Vec<BoardLite>>>, invalidate_boards, set_boards_memo.
  - Hook filters: guild_may_have_boards, reaction_filter.
- Sync engine:
  - fetch_reaction_users, apply_action, sync_message(app, SyncRequest).
  - Hook bodies: handle_reaction(app, ReactionInput) -> &str, on_reaction_event, on_message_update, on_message_delete, on_message_delete_bulk, on_channel_delete.
  - set_sync_impl is the test seam.
- CRUD:
  - create_board(app, g, BoardPatch) -> Result<Starboard, BoardError>, get_board_by_name, update_board(app, g, id, BoardPatch) -> (before, after), delete_board, list_boards_with_counts -> Vec<BoardWithCount>, search_boards -> Vec<BoardSearchRow>.
  - BoardError: Limit / NotFound / IgnoreLimit / SettingsConflict / Db, and is_unique_violation.
- RAW_SQL holds every hand-written statement.

feat_starboard_stats::stats_service (stats):
- Kinds: STAT_KINDS, kind_info(kind) -> (label, template), is_list_kind / is_online_kind / is_member_event_kind, PROBLEMS, describe_problem, StatsError { code, channel_id }.
- Pure helpers: clamp_int, refresh_job_key, normalize_template, render_stat_name, validate_stat_template -> Result<String, String>, is_member_cache_complete, count_channels, build_snapshot, compute_value, decide_rename, compute_next_run_at, parse_channel_ref, build_overwrites, claim_pull, run_exclusive.
- Discord and database reads:
  - confirm_channel -> ChannelCheck.
  - load_rows / list_rows, list_rows_safe -> Vec<RowRef>, get_guild_rows, find_row_by_ref(app, g, input), inspect_row.
- Refresh engine: collect_snapshot, refresh_guild -> RefreshResult (all times in epoch ms).
- Scheduling: ensure_refresh_job(app, g, Option<ms>), pull_refresh, cancel_refresh_job, refresh_job_handler(app, g) -> Option<next_ms> (None deletes the job).
- Hook bodies and filters: on_guild_ready, member_event_filter, on_member_change, channel_delete_filter, on_channel_delete.
- /stats service functions: create_stat_channel(app, g, kind, template, Option<(role_id, role_name)>, Option<category_id>, actor_tag) -> Created, edit_template, delete_stat_channel -> channel_deleted: bool, refresh_now -> RefreshNow { Empty | Busy | Done }.
- Test seams: reset_state, set_rows_memo, pending_pull_of, fetch_blocked_until_of. RAW_SQL lists the hand-written statements.

Discord layers: starboard_io::{StarboardIo, SerenityStarboardIo, set_starboard_io, starboard_io} and stats_io::{StatsIo, SerenityStatsIo, set_stats_io, stats_io}. These are the seam for the dashboard or tests to replace Discord.

Also public:
- commands::starboard: data, command, MODULE_OFF_NOTE, describe_changes, list_fields, autocomplete_choices.
- commands::stats: data, command, MODULE_OFF_NOTE, row_line, list_view, refresh_lines, label_of, shown_template, autocomplete_choices.
- plugins::stat_channels::refresh_job.

## Known gaps

- src/services/starboardService.js handleReaction: the shortcut that skips the sync when the author reacts to their own message only fires on messageReactionAdd. Serenity sends message_author_id on add events only. On remove, the sync runs and finds the count unchanged; nothing visible changes.
- src/services/statsService.js refreshGuild: the RateLimitError branch is in place but effectively unreachable, because serenity queues rate-limited requests instead of throwing. A long queue hits the 10 s rename timeout and the counter is reported as waiting, as in the JS.
- src/services/starboardService.js mirrorEdit (onMessageUpdate): the reactions come from the MESSAGE_UPDATE payload, not the discord.js message cache. On boards that count any emoji, the emoji shown on the edited repost can differ when the payload has no reactions.
- src/commands/admin/starboard.js resolveChannel: a channel option missing from the guild cache falls back to serenity's PartialChannel, which has no nsfw flag. For such a channel the allow_nsfw check treats it as not age-restricted.
- src/services/statsService.js confirmChannel: a timed-out REST fetch of an uncached channel is dropped rather than left running in the background as withTimeout does. The result is the same: 'unknown', and the row is kept.
