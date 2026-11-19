# feat-automod: public API (from its author)

I ported all six JS files in full. Every exported function, subcommand, option, autocomplete, button, hook and job exists, and the user-visible text is copied verbatim.

- src/services/automodService.js -> src/automod_service.rs. This covers:
  - the config model (v2 shape, v1 upgrade, normalisation);
  - the text helpers and detectors;
  - the per-app compiled-matcher memo;
  - outcome resolution, reasons, log fields and notices;
  - the punishment window;
  - the full runAutomod pipeline: delete, ctx.stop, the shared automodAction state, punishment through feat_moderation::execute_mod_action with source "automod", the in-channel notice, and at most one mod-log embed.
  - The fixed JS regexes run on regress, in the private helper src/js_re.rs. User patterns go through bot_util::safe_regex.
- src/services/antiRaidService.js -> src/anti_raid_service.rs. This covers:
  - the join windows, memo, tallies, keyed mutex, semaphore and throttles (all held in app.ext);
  - the send-bit maths;
  - startLockdown and endLockdown on the AntiRaidState table and the antiraid.unlock job. The bits are saved before @everyone is edited, the end is idempotent, a failed restore is retried, and a bot without a send bit gets a partial restore.
  - invite pauses through the incident-actions endpoint;
  - the alerts, embeds and End button;
  - removeMember through execute_mod_action with source "antiraid".
- src/plugins/automod.js -> src/plugins/automod.rs (the messageCreate and messageUpdate hooks, filter, 15 s timeout).
- src/plugins/antiraid.js -> src/plugins/antiraid.rs (the guildMemberAdd and channelDelete hooks, the ungated antiraid:end button, the antiraid.unlock job handler).
- src/commands/admin/automod.js -> src/commands/automod.rs (all 17 subcommand routes and the autocomplete).
- src/commands/admin/antiraid.js -> src/commands/antiraid.rs (status, lockdown start and end, config).

Tests:
- tests/automod.rs (41 tests) ports test/automod.test.js and the automod parts of test/sec-forwards.test.js. It adds:
  - golden checks for both commands and both plugins;
  - a sweep of about 900 pure-function calls compared with the JS;
  - Redis keys compared with keys produced by the JS runAutomod;
  - the pipeline against a FakeGuild and FakeRest, and the command against the in-memory store;
  - database and Redis tests in both directions: the config row and the shared anti-spam window.
- tests/antiraid.rs (25 tests) ports test/antiraid.test.js, with embeds and texts compared with the JS. The lockdown, join, unlock-job, button and command cases run on the mock database with a FakeRest. Lockdowns are checked across languages: JS startLockdown read and ended by Rust, and Rust start read by the JS loadState and getJob.
- The JS side of the cross-language tests is rust/fixtures/gen/feat-automod-js.mjs.

## API

Entry point: feat_automod::register(&mut Registry). It registers /automod, /antiraid, and the automod and antiraid plugins. feat_automod::COMMANDS lists the two command names.

feat_automod::automod_service (other crates and the dashboard):
- Constants and tables: RULE_KEYS, RULES and rule_meta(key) (label, emoji, threshold, on_edit, description); ACTIONS, action_label, is_deleting_action, punishment_of, punishment_rank; the honeypot constants plus honeypot_action_label and honeypot_punishment; DEFAULT_ALLOWED_DOMAINS and the timing constants.
- Config: normalize_config(Option<&Value>) -> AutomodConfig. Its .to_json() is the JS v2 shape and key order; .rule(key) and .rule_mut(key) give access to a rule. Also upgrade_v1, default_config, is_legacy_config, active_rule_keys.
- Compiled config: get_automod_config(&App, Option<&GuildSettings>) -> Arc<CompiledConfig { config, matchers }> (memoised); compile_config; build_matchers.
- Text and detectors: text_for_words, text_for_links, normalize_word, normalize_domain, strip_invisible, find_invites, extract_link_hosts, host_matches, caps_stats, count_emojis, count_spoilers, count_mentions, max_combining_run, duplicate_fingerprint, is_spaceless_word, check_bad_words, evaluate_content_rules(&AutomodMessage, &[&str], &AutomodConfig, &Matchers, &GuildInvites).
- Async detectors and cache keys: check_repeated_text and check_spam; dup_key and rate_key (the JS Redis keys).
- Scope and outcome: scope_channel_ids, is_honeypot_channel, rule_applies, is_staff, resolve_outcome, honeypot_outcome, build_reason, build_punishment_log_fields, build_notice_text, build_log_embed, post_mod_log.
- Descriptions: resolve_rule_key, format_threshold, describe_rule_line, describe_honeypot(_value), describe_links_mode.
- Pipeline: run_automod(&App, &dyn AutomodSink, settings, &AutomodInput, is_edit) and apply_outcome. HookCtx<E> implements AutomodSink: ctx.stop("automod") and state.set_automod_action, which is bot-core's shared AutomodAction that feat-translation reads.
- State: claim_punishment and PunishmentSlots; reset_automod_state(&App).
- set_mod_executor(&App, ModExecutor) replaces execute_mod_action for both automod and anti-raid (a test seam).

feat_automod::anti_raid_service:
- Constants: raid::* (ranges and timings), SEND_MASK.
- Pure helpers: clamp_int, harsher, moderation_source, split_send, parse_prior, restored_bits, bot_send_bits, everyone_bits, bit_names, blocked_chat_text, detection_of, young_account, is_active, permission_problems, config_lines.
- Joins and memo: record_join, count_join, peek_locked, mark_active, is_locked, tally_of, reset_memory.
- State: load_state(&PgPool, guild).
- Lockdowns: start_lockdown(&App, GuildId, StartInput) -> Started; end_lockdown(&App, GuildId, only_if_due) -> anyhow::Result<Ended>; unlock_job(guild, run_at) is the dedupe key antiraid.unlock:{g} with payload {}; incidents_of.
- Alerts: build_start_embed, build_end_embed, build_young_embed, end_button_row and end_button_id ("antiraid:end"), alert_channel, post_alert, announce_start, announce_end with EndAnnounce, report_young.
- remove_member(&App, GuildId, &Member, action, reason, detail) -> Removed.

feat_automod::plugins::antiraid:
- handle_member_add(&App, Option<&HookState>, GuildId, &Member, Option<Settings>) -> Option<JoinOutcome>. It sets ctx.state "antiraid" to the JSON {action}.
- handle_unlock_job, handle_end_button, on_channel_delete, may_be_alert_channel, start_and_announce.

Commands:
- feat_automod::commands::automod: write_config(&App, guild, mutate) (the row-locked read-modify-write), collect_problems, collect_honeypot_problems, honeypot_warning_embed, status_parts, rule_choices, list_choices, parse_word_list, pack_items, spoilered.
- feat_automod::commands::antiraid: OPTION_FIELDS, OFF_NOTE.

## Known gaps

- automod_service.rs guild_invites (JS checkInvites guild.invites.cache): the server's own invite codes come from feat-logging's invite tracker snapshot (invite_tracker::current_snapshot), because serenity has no invite cache. A code the tracker has not recorded counts as a foreign invite. The vanity code is read from the cache, as in the JS.
- anti_raid_service.rs incidents_of / set_incident_actions (JS guild.incidentsData, which discord.js updates from setIncidentActions): serenity does not update the cached guild from the REST answer. Rust keeps the answer for at most 60 s, and only while the cache still shows the value it had before the call (until GUILD_UPDATE arrives). After that the cache is used.
- automod_service.rs AutomodMessage::of (JS countMentions mentions.roles): every id in mention_roles is counted. discord.js counted only roles it had cached.
- automod_service.rs AutomodInput::scope (JS scopeChannelIds): when neither the payload nor the cache has the channel, it is scoped as a plain channel with no parent or category, so a thread's parent and category exemptions are missed in that case.
- automod_service.rs is_staff (JS isStaff/isImmune): feat_moderation::is_immune_member returns false for an uncached guild, so the immune-role and owner part of the check is lost then. The MODERATOR-level check and the Manage Messages check still run.
- automod_service.rs count_emojis (JS Intl.Segmenter): grapheme clusters come from unicode-segmentation (UAX #29), not ICU. The tested sequences (ZWJ families, flags, keycaps) agree.
- Embeds in both services and both commands: serenity writes "inline": false on fields where discord.js leaves the key out. Discord treats both the same.
- anti_raid_service.rs start_lockdown returns an enum Started { Ok { started, state, minutes, effects } | Failed { error } } instead of the JS { ok, ... } object. end_lockdown returns anyhow::Result<Ended>. The JS deps injection is replaced: scheduling uses app.scheduler(), and executeModAction can be swapped with automod_service::set_mod_executor (a test seam).
- anti_raid_service.rs set_incident_actions: the body is sent as discord.js sends it (invites_disabled_until is null to resume, and dms_disabled_until is included only when kept). end_button_row serialises "disabled": false and omits emoji.animated, unlike discord.js; the meaning is the same.
- automod_service.rs send_notice (JS setTimeout delete after 6 s): this is a spawned tokio sleep followed by the delete. Like the JS timer, it is lost at shutdown.
- automod_service.rs get_automod_config: the JS WeakMap keyed by the frozen automodConfig object becomes a per-app memo keyed by guild id and compared on the raw JSON value. The results are the same.
