# feat-moderation: public API (from its author)

Picked up after the crash from the empty skeleton and ported all 13 files.
- src/services/moderationService.js -> src/moderation_service.rs: every export. Discord calls go through the new src/guild_ops.rs, which has the ModRest trait (one method per discord.js call), the serenity implementation, and views of the guild, members and roles with discord.js manageable/kickable/bannable/moderatable.
- src/plugins/moderation.js -> src/plugin.rs: the guildReady, guildMemberAdd, guildBanAdd and guildBanRemove hooks, the mod.unban and mod.expire jobs, and the mod:bansync Redis claim.
- src/commands/moderation/{ban,case,kick,modconfig,purge,slowmode,softban,timeout,unban,unmute,warn}.js -> src/commands/<name>.rs, plus commands/common.rs for the shared options and reply helpers.
- Commands and plugin match commands.json and plugins.json byte for byte.
- New JS fixture script: rust/fixtures/gen/feat-moderation-js.mjs.
- Tests: tests/moderation.rs (offline; the pure helpers are swept against the JS) and tests/moderation_db.rs (mock database, a fake REST, an offline serenity cache; both cross-language directions). JS-written ScheduledJob rows, including ones written by JS reconcileExpiries, run through Scheduler::run_job.
- A real bug turned up and was fixed: raw queries now read the timestamp(3) columns as NaiveDateTime, not DateTime<Utc>.

## API

Every function takes &App plus ids. None calls a command.

**Main entry: `execute_mod_action`**
- `moderation_service::execute_mod_action(&app, ModActionInput) -> BoxFuture<Result<ModSuccess, ModError>>`. It never fails: the Discord action runs first, then the case is recorded, the target DMed (per modDmTargets), the case logged, the expiry scheduled, and escalation run for a WARN.
- Build the input with `ModActionInput::new(guild_id, target_id, moderator: UserRef, action)`. Optional fields:
  - `target_member` / `target_user`
  - `reason`, `duration_ms` (f64), `delete_message_seconds` (0..=604800), `proof_url`
  - `source`: command | automod | escalation | captcha | dashboard | antiraid
  - `silent`, `skip_escalation`, `escalation_tier_id` (honoured only for source escalation from the bot), `log_fields` (at most 3)
- `ModSuccess` has `infraction` (bot_db::Infraction), `action` (the effective action: TEMPBAN, or MUTE for a long timeout), `dm` (DmStatus Sent / Failed / Skipped), `warnings` and `escalation` (Option<EscalationOutcome>).
- `ModError` has `code` (ModCode, with `.as_str()` and `.title()`), `error` (the user text) and `applied` (true when Discord acted but the record failed).

**Relief actions** (no immunity check; never fail)
- `execute_unban(&app, guild_id, user_id, &moderator, reason, source) -> Result<UnbanSuccess { infraction, user, cleared }, ModError>`
- `execute_unmute(&app, guild_id, &target UserRef, target_member: Option<Member>, &moderator, reason, source) -> Result<UnmuteSuccess { infraction, removed, cleared, note }, ModError>`

**Immunity**
- `is_immune(member: Option<&MemberView>, owner_id, settings, bot_owner_id)`
- `is_immune_member(&app, GuildId, &serenity Member, settings)`: the automod / anti-raid shortcut.
- `has_immune_role`
- `preflight_mod_action(&Preflight)`, `assert_hierarchy`

**Recording and escalation**
- `create_infraction(&app, GuildId, &target, &moderator, InfractionType, CreateOpts { reason, proof_url, duration_ms, expires_at, skip_escalation, silent, source, log_fields }) -> anyhow::Result<Infraction>`: records only (no Discord action), then DM, mod log and escalation, best-effort.
- `record_infraction(&app, NewInfraction)` and `record_infractions_bulk(&app, guild_id, Vec<BulkInfraction>)`: advisory-locked case numbers, shared with the JS.
- `resolve_infractions(&app, guild_id, user_id, &[InfractionType])`, `close_infractions`, `find_infraction`, `find_case`, `count_active_warns`.
- `run_escalation(&app, GuildId, user_id) -> BoxFuture<Option<EscalationOutcome>>`
- `normalize_escalation_tiers(Option<&Value>) -> Vec<Tier>`, `describe_tier`, `select_escalation_tier`, `escalation_reason`

**Mod log and DMs**
- `post_mod_log(&app, GuildId, CreateEmbed)` and `send_mod_log(&app, GuildId, &Infraction, &target, &moderator, source, &log_fields)`: independent of loggingEnabled, never fail.
- `build_mod_log_embed(&ModLogInput)`
- `notify_target(&app, user_id, &Notify)`, `build_notify_embed`

**Ban markers**
- `mark_bot_ban_action(&app, guild_id, user_id, "ban" | "unban", ttl_ms)`
- `was_bot_ban_action(...)`

**Sync, reconcile and jobs**
- `sync_guild_bans(&app, GuildId, bot_id) -> Option<BanSync>`
- `reconcile_expiries(&app, guild_id) -> Reconciled`
- `count_overdue_bans`
- `run_unban_job` / `run_expire_job (Job, JobCtx)`
- Job builders: `unban_job` / `expire_job`. Dedupe keys: `unban_key` -> `mod.unban:{g}:{u}`, `expire_key` -> `mod.expire:{id}`.

**Hooks**
- `reapply_mute_on_join`
- `record_external_ban` / `handle_external_unban`: they return the detached task's JoinHandle.

**Replies**
- `deliver_mod_reply(&impl Interactive, settings, ModReply { title, lines, warnings })`: applies modVisibility.
- `deliver_lookup`, `reply_mod_error`, `mod_error_embed`

**Pure helpers and constants**
- `resolve_effective_action`, `map_discord_error`, `build_audit_reason`, `display_type`
- `action_meta`, `tier_action_label`, `source_label`
- `msg::*` (the user texts verbatim)
- `MOD_ACTIONS`, `MOD_SOURCES`, `RAW_SQL` / `LOCK_CASES_SQL`, and the duration limits

**guild_ops**
- `load_guild(&app, GuildId) -> Option<LoadedGuild { view: GuildView, guild }>`
- `MemberView` / `RoleView` / `UserRef`
- `GuildView::check_assignable_role`
- `rest(&app) -> Arc<dyn ModRest>`
- `set_rest(&app, Arc<dyn ModRest>)` installs a fake for tests and tools; offline the default is `OfflineRest`.

**Plugin and commands**
- `plugin::claim_ban_sync`, `bansync_key`
- `commands::modconfig::{validate_tier, add_tier, tier_choices, apply_mute_overwrites}`
- `commands::purge::{collect_purge_targets, delete_in_chunks, build_purge_report, matches_filters}`
- `commands::slowmode::{parse_slowmode, format_slowmode, build_slowmode_embed}`

**Registration**
- Single entry point: `feat_moderation::register(&mut Registry)`. `COMMANDS` lists the 11 command names.

## Known gaps

- moderation_service.rs post_mod_log, commands/modconfig.rs show_command / log_channel_command, commands/slowmode.rs execute: the JS falls back to a REST channel fetch (guild.channels.fetch) for an uncached channel. Rust reads the serenity cache only, so an uncached channel counts as unusable or not found.
- commands/purge.rs deletable(): approximates discord.js message.deletable by message type (1-5, 14-17 and 21 are not deletable). The bot already has Manage Messages there, but the per-message channel-permission recheck is not repeated.
- commands/purge.rs execute: the JS outer try/catch 'Something went wrong while purging' reply has no Rust path, because the collect/delete/report functions never fail. A panic is caught by the router, and a Drop guard releases the per-channel lock (the JS finally).
- moderation_service.rs create_infraction: when the guild is not cached, the DM title uses an empty guild name. The JS always had guild.name.
- moderation_service.rs perform (step n), unmute_locked close_rows, sync_inner, reconcile_inner: the JS mapLimit(…, 4) concurrency for job cancels and schedules runs sequentially. The result is the same; only the latency differs.
- moderation_service.rs deliver_mod_reply ANONYMOUS: the JS rethrew an interaction-gone error from channel.send. Rust's channel post is a REST call, so any failure takes the 'couldn't post the public notice' path, and only the following editReply error is checked for gone/logged.
- moderation_service.rs is_snowflake / execute_unban: user ids are validated with /^\d{17,20}$/ as in the JS. Rust then parses them to u64, so a 20-digit id above u64::MAX would be treated as user 1 (Discord ids never reach that).
- commands/slowmode.rs: a picked channel absent from the cache answers 'Channel Not Found'. The JS did a bounded REST fetch first.
- commands/modconfig.rs mute_role_command: a role just created by new_role is checked using its raw Discord position from data.resolved, because it is not in the cache yet. The JS used discord.js's ranked role.position.
- moderation_service.rs find_audit_entry: the 1.5 s wait is per-app configurable (AuditDelay, tests set 0). The production default is 1500 ms, as in the JS.
