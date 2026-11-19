# feat-levels: public API (from its author)

Ported in full:
- src/services/xpService.js -> crates/feat-levels/src/xp_service.rs. Covers the XP maths, rolls, boosts, no-XP rules, voice eligibility, sessions and segments, role plans, announce routing and level-up payloads. All 8 RAW_SQL statements are ported statement for statement and exported as raw_sql(). Also: the level-change pipeline, announceLevelUp, syncLevelRoles, message and voice XP, the voice sweep job, the roleDelete cleanup, the bulk lock and tasks, the xp RewardProvider, and the deprecated no-ops.
- src/plugins/levels.js -> src/plugin.rs: 4 hooks with the JS filters, always and timeoutMs; the levels.voiceSweep job; provides xp.
- src/commands/admin/levelconfig.js -> src/commands/levelconfig.rs: every subcommand, group, option and the autocomplete. The xp group answers privately. The role give uses the confirm prompt and the background follow-up. The card preview is drawn by bot-cards in spawn_blocking.
- src/commands/leveling/rank.js -> src/commands/rank.rs: the card, with the fallback embed.
- src/commands/leveling/leaderboard.js -> src/commands/leaderboard.rs: up to 200 rows, paginated.

Tests are in tests/levels.rs (one binary). They cover:
- the ported JS unit and DB tests;
- golden checks for commands.json and plugins.json;
- a JS-generated fixture (rust/fixtures/gen/feat-levels-js.mjs golden -> rust/fixtures/golden/feat-levels/pure.json) covering the maths, rolls, boosts, role plans, the announce matrix, voice sessions, segments and rates, level-up payloads (the noop:levelup customId included), expiring links, and the cache keys, values and TTLs the JS writes;
- DB tests with the router;
- cross-language tests in both directions for UserLevel rows, getRank, LevelRole rows, the xpBoostRoles, noXpRoleIds and settings fields, the roleDelete cleanup, voice:start session values, and the levels.voiceSweep job row (type, dedupeKey, payload {}, maxAttempts 5).

## API

The only entry point is feat_levels::register(&mut Registry). The services are public in feat_levels::xp_service; the functions that touch the database or Discord take &App and plain ids.

Pure functions:
- level_from_xp(f64) -> i64, xp_into_level(f64) -> f64, xp_to_next_level(i64), total_xp_for_level(f64) -> f64
- normalize_xp_range(Option<f64>, Option<f64>) -> (i64, i64), roll_xp(RollXp, rng), math_random()
- normalize_boost_roles(Option<&Value>) -> Vec<BoostRole>, boost_roles_json(&[BoostRole]), best_boost_multiplier, has_no_xp_role, is_no_xp_channel(&[String], Option<&ChannelRef>), channel_ref
- voice_eligibility(&VoiceInput), count_other_listeners, parse_voice_session(Option<&Value>, now) -> Option<VoiceSession>, voice_session_json, compute_voice_segment, voice_xp_for_minutes
- plan_level_roles(&[(level, roleId)], level, stack, sync_down, holds) -> RolePlan
- resolve_announce_target -> AnnounceTarget, template_mentions_player, level_up_vars, build_level_up_payload(LevelUpInput) -> Option<LevelUpPayload> (with to_payload()/to_json()), rank_where
- keys and constants: cooldown_key, cooldown_ttl, voice_key, sweep_dedupe_key, DEFAULT_LEVELUP_MESSAGE, ANNOUNCE_MODES, ANNOUNCE_MODE_LABELS, VOICE_* and BULK_* constants

Database:
- apply_xp_delta(&App, XpDelta) -> XpChange
- subtract_xp(&App, g, u, amount) -> Option<XpChange> (old_xp set), set_xp(&App, g, u, xp) -> Option<XpChange>
- reset_xp(&PgPool, g, u) -> Option<old level>, set_stored_level(&PgPool, g, u, xp, level) -> bool
- give_xp_to_members(&App, g, &[String], amount) -> BulkGive { updated, level_ups }
- get_rank(&PgPool, g, u, xp), find_user_level, level_roles, reward_pairs, ranked_rows, ranked_count
- raw_sql() -> Vec<(name, Query)>

Pipeline:
- handle_level_change(&App, LevelChange) -> LevelChangeOutcome (builder: LevelChange::of(..).member().channel().announce())
- announce_level_up(&App, Announce) -> bool, level_up_payload_for
- sync_level_roles(&App, GuildId, &Member, level, Option<&GuildSettings>, SyncOptions) -> RoleSync { added, removed, failed }
- fetch_member, fetch_members, cached_member, display_name

Hooks and voice:
- message_xp(...) -> MessageXp, handle_message_xp
- reconcile_voice_member(&App, GuildId, Member, VoiceSource, Settings, now, fresh_join) (queues on the member's lock when called), voice_view, voice_change_matters, voice_state_xp
- seed_voice_sessions_for, seed_voice_sweep, run_voice_sweep(&App, guild_id) -> JobOutcome
- role_deleted(&App, g, roleId, settings) -> u64

Bulk, rewards and the rest:
- try_start_bulk / end_bulk / is_bulk_running(&App, g), run_bulk_role_sync, report_bulk_result(&App, &BulkReport, text, ephemeral)
- XpRewardProvider (the xp kind on app.rewards()); set_ready_at / ready_at; grant_xp / handle_voice_xp (deprecated no-ops)

Commands:
- levelconfig: data(), command(), upsert_reward, level_role_count, status_fields, noxp_patch, boost_add_patch, merged_style, xp_change_lines, is_expiring_discord_link, run_bulk_in_background, xp_sync_role_level_ups, ConfigError
- rank: data(), command(), fetch_avatar, draw_card(CardData) (spawn_blocking), fallback_embed
- leaderboard: data(), command(), line, footer, start_page, guild_icon_url

## Known gaps

- xpService.js reconcileVoiceMember: bot-core does not expose client.readyTimestamp. ready_at(app, now) records the time of the first voice evaluation in this process instead, or the boot can set it exactly with xp_service::set_ready_at(app, ms). Because that time is at or after the real ready time, at most the time between ready and the first voice evaluation goes uncredited; no time from before the process started is ever credited.
- xpService.js handleVoiceStateXp: the levels hook is second in HOOK_ORDER.voiceStateUpdate (after temp-voice), so it cannot take its per-member mutex place synchronously and keep gateway order. The JS cannot either: it awaits getSettings before voiceMutex.run. As in the JS, the listener count and voice facts are read inside the mutex at run time (VoiceSource::State).
- leaderboard.js fetchMissingNames and xpService.js runBulkRoleSync: guild.members.fetch({ user: ids }), a gateway member-chunk request, is replaced by cached members plus REST get_member for the missing ones (5 at a time, bounded by a timeout: 5 s per chunk of 100 for names, 10 s per chunk for the bulk sync).
- xpService.js announceLevelUp: there is no guild.channels.fetch(id) fallback; the target channel is read from the serenity cache only.
- levelconfig.js subNoxpRole/subNoxpChannel: guild.available is treated as 'the guild is in the serenity cache'. The existing-entries prune uses cached roles, or cached channels plus threads.
- xpService.js syncLevelRoles: when Discord is not connected (no http), each planned role op is reported in failed with the reason 'Discord is not connected'. The JS always has a client.
- xpService.js handleMessageXp: when the hook payload has no channel and the cache misses it, the no-XP channel check is skipped. The JS always has message.channel.
- levelconfig.js subRoleAdd: the reply is sent with safe_reply. If it is not delivered, the bulk lock is released and no backfill runs. The JS does the same when editReply throws, except that the JS rethrows and the Rust returns Ok.
