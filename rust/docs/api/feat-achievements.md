# feat-achievements: public API (from its author)

Ported in full, JS file to Rust file (all under rust/crates/feat-achievements/src/):
- src/services/achievementService.js -> achievement_service.rs: every constant and helper, the index memo plus the Redis copy {v:1,guildId,items} (load uses set-NX, refresh a plain SET), the three-phase recordProgress SQL, catch-up seeding, the compare-and-set tier claim, payout through app.rewards(), announcements (plan_announcement decides, send_announcement sends), almost-there DMs, the write-behind message buffer, flushMessageCounts, the 5 hooks and the command data functions. Payout and announcements run as spawned tasks.
- src/services/birthdayService.js -> birthday_service.rs: date helpers, parseBirthdayInput, chunking, templates, save/remove/upcoming, the claim SQL, revertClaims, clearMarker, transferBirthdayRole, ensureTickJob, onGuildReady, runBirthdayTick and tickGuild. Discord access goes through a BirthdayDiscord trait: production uses SerenityBirthdays, the tests a fake.
- src/plugins/achievements.js -> plugins/achievements.rs; src/plugins/birthdays.js -> plugins/birthdays.rs (birthday.tick job).
- src/commands/admin/achievement-config.js -> commands/achievement_config.rs (all 15 subcommands and autocomplete); src/commands/community/achievements.js -> commands/achievements.rs; src/commands/community/birthday.js -> commands/birthday.rs (member and config subcommands).
- The shutdown flush is registered with bot_core::client::on_shutdown at order 10, 3000 ms, label "achievement count flush", warning text as in bot.js.
Tests: tests/achievements.rs (43) and tests/birthdays.rs (31). They cover the ported JS tests (achievements.test.js, birthdays.test.js, the birthday cases of review-community2.test.js), the commands.json and plugins.json goldens, and a sweep of about 900 pure-helper calls compared with Node. They also cover the DB cases (raw SQL, concurrent first events, the claim race, the buffer and its shutdown flush, boost dedupe, almost-there, a refused reward, reset, tier changes, the tick with departed-member paging, the 50 cap, failed grants, pending members, the running guard, Feb 29 and time zones) and the commands through the router, delete/reset confirm collectors included. Cross-language tests run in both directions for rows, the Redis index copy, the reaction and boost keys, tombstones, claims and the birthday.tick job (type, dedupe key, payload {}). The JS side is rust/fixtures/gen/feat-achievements-js.mjs.

## API

Entry point: feat_achievements::register(reg). It registers /achievement-config, /achievements and /birthday and the 'achievements' and 'birthdays' plugins. Modules are pub.

feat_achievements::achievement_service (svc):
- Write paths take &App or &PgPool plus string ids: create_achievement(&App, g, &NewAchievement) -> Result<Result<Achievement, WriteRefusal>>; update_achievement(&PgPool, g, id, &AchievementPatch); delete_achievement(&PgPool, g, id). Run them inside with_config_lock(&App, g, || async {..}), then call refresh_achievement_cache(&App, g).
- Reads: list_achievements, resolve_achievement (an id, or a name compared with ILIKE like Prisma insensitive), name_taken, search_achievements, load_member_view(&App, g, user, boosting), get_tier_stats.
- Progress: reset_member_progress(&App, g, user, &ids); apply_tier_change(&PgPool, id, &[Tier], TierChange).
- Recording: record_progress(&App, &Origin, trigger, &[Lite], inc, floor); hook entry points on_message_create, on_reaction_add, on_member_update, on_level_up, on_purchase; get_achievement_index; flush_message_counts(&App, final_flush); settle_background(&App).
- Pure helpers: parse_goals, normalize_tiers, validate_tiers, normalize_rewards, merge_rewards, evaluate_progress, format_member_entry, format_rewards, parse_deadline, normalize_badge_emoji, plan_announcement.
- Keys: achievements_key, reaction_key, boost_key. Types: Lite and CachedIndex (the Redis JSON), Reward, Tier.

feat_achievements::birthday_service:
- Pure helpers: parse_birthday_input, next_occurrence, next_tick_at, tombstone_ttl_seconds, chunk_announcements, render_birthday_line, format_birthday, code_block, resolve_time_zone, clamp_hour, format_hour, pick_template, set_id (an empty string counts as unset).
- DB: get_birthday, count_birthdays, has_markers, save_birthday(&App, g, user, BirthdayDate, Option<&GuildSettings>, &now), claim_birthdays, revert_claims, clear_marker.
- Discord-touching functions take &dyn BirthdayDiscord or Arc<dyn BirthdayDiscord>: remove_birthday, get_upcoming_birthdays, transfer_birthday_role, tick_guild. Callers pass SerenityBirthdays::new(&app, guild_id). Offline the member fetch fails, so remove_birthday skips the role removal and get_upcoming_birthdays keeps every row.
- Scheduling: call ensure_tick_job(&App, g, Some(&settings), soon) after any birthday settings write, dashboard writes included. Also on_guild_ready, run_birthday_tick and run_birthday_tick_with.
- Keys: tick_key and tombstone_key.

## Known gaps

- birthdayService.js fetchMembers: the JS asks the gateway for up to 100 ids at a time (10 s timeout). Rust (SerenityBirthdays::members) reads the serenity cache, then makes one REST get_member call per uncached id. With up to 500 markers plus 51 candidates in a tick this can hit rate limits or the 60 s job timeout.
- achievement-config.js collectRewards: role checks use the cached guild (check_assignable_role_cached). When the guild or role is not cached, Rust answers 'Role not found.' where the JS checked the role object from the option.
- achievementService.js shutdown flush (bot.js flushMessageCounts final): it is registered with on_shutdown on the first messageCreate dispatch of an app, not at boot, because register() gets no App and plugins.json requires hasInit=false. A bot that receives no message before shutdown has nothing waiting to flush anyway.
- achievementService.js message buffer timers: setTimeout becomes a spawned tokio sleep task per waiting member, aborted when the member is flushed. Behaviour is the same.
- birthdayService.js tickGuild: the 40 s deadline uses std::time::Instant (real time), as Date.now() does in the JS. The tick's 'now' parameter only sets the local date and hour.
- achievementService.js loadMemberView: takes a boolean 'boosting' (member.premiumSinceTimestamp is set) instead of the member object.
