# feat-tags: public API (from its author)

Ported all three owned JS files in full (rust/crates/feat-tags):
- src/services/tagService.js -> src/tag_service.rs. Covers every export: names, prefix and argument parsing, the action model, usage text, access and scope, payload rendering, cooldowns, use counters, RAW_SQL, the shared executor run_tag, both hook bodies, the trigger cache and memo with safe_regex compilation and selection, resolve_trigger and trigger_number, create/write with the advisory lock, the cap and optimistic concurrency, and the __testing seams as prime_tags, prime_triggers and configure_count_uses.
- src/tag_io.rs (new): the production TagIo. The executor reaches Discord through this trait (serenity cache and REST, plus the interaction Responder), so it can be tested offline with a fake.
- src/plugins/custom-commands.js -> src/plugin.rs: the custom-commands hook (timeout 15 s; calls ctx.stop() only after it deletes the usage) and the custom-commands:triggers hook, both with their sync filters.
- src/commands/utility/tag.js -> src/commands/tag.rs: every subcommand and group (use, create, edit, delete, toggle, info, raw, list, placeholders, cooldown, permissions, action add/remove/list, response add/remove, trigger enable/disable/add/edit/remove/toggle/info/list), plus the name, action, reply and trigger autocompletes and the module-off refusal. Text is copied verbatim.
- rust/fixtures/gen/feat-tags-js.mjs: the JS side of the cross-language tests.
- Tests are in tests/tags.rs (55, one binary): the JS tests ported from test/custom-commands.test.js sections 1-17; golden checks for commands.json and plugins.json; sweeps of the pure functions against the JS; and database and Redis tests in both directions. The two-way checks cover rows (find, write, create), the guild:{g}:triggers cache value, tag:cd cooldown keys and values, use counters, resolveTrigger and triggerNumber, the Prisma ILIKE autocomplete queries, and /tag end to end through the router.
- Measured, not assumed: Prisma's insensitive contains/equals/startsWith sends ILIKE/LIKE with the text unescaped (equals adds ORDER BY id when take is set). The port does the same.
- discord.js includes @everyone in member.roles.cache and drops deleted roles. member_role_ids models this.

## API

Entry point: feat_tags::register(&mut Registry). The public service is feat_tags::tag_service. Every function takes &App and plain ids, and errors are anyhow::Error; downcast with as_tag_error(&err) -> Option<&TagError{code,title,message}>, and use is_unique_violation(&err) for P2002.
- Prefix / names: effective_prefix(Option<&str>) -> Option<String> (feat-automations uses this), normalize_tag_name, validate_new_tag_name -> Result<String,&str>, is_tag_name, parse_invocation, parse_args, build_usage(&CustomTag, prefix, slash).
- Actions: normalize_actions(Option<&Value>) -> Option<Vec<TagAction>>, get_effective_actions(&CustomTag), legacy_actions, actions_to_json (stored JSON in the JS key order), sync_legacy_columns -> Patch, primary_message_index, channel_reply_index, describe_action, action_label, cooldown_label, format_id_list.
- Access: check_tag_access / check_tag_roles(AccessLists::of(&tag), roles, scope), trigger_applies_here, channel_scope_ids, member_role_ids (discord.js role-cache semantics).
- Rows: find_tag, find_tag_by_id, find_tag_row, has_tag_name, invalidate_tag_index; create_tag_row(NewTag) and create_tag_row_with(app, guild, Patch) (the dashboard form, any columns, same lock and cap); write_tag(app, guild, name, FnMut(&CustomTag) -> Result<Patch, TagError>) with optimistic retry; delete_tag_row. Patch = Vec<(&'static str, serde_json::Value)>, using Prisma field names.
- Triggers: create_trigger_row / create_trigger_row_with, write_trigger(app, guild, &row, Patch), delete_trigger_row, list_triggers, resolve_trigger, trigger_number, validate_trigger_pattern -> Result<(), String>, invalidate_trigger_cache, get_trigger_set, select_trigger, matches_plain, trigger_quarantined, TriggerProjection (the cached shape), parse_cached_projections, trigger_cache_key.
- Cooldowns: tag_cooldown_key, trigger_cooldown_key, acquire_cooldown -> Cooldown{Ok,Unavailable,Until}, release_cooldown, clear_tag_cooldowns.
- Counters: increment_tag_uses / increment_trigger_uses (fire-and-forget).
- Executor: run_tag(app, &tag, RunOpts, &impl TagIo) -> RunResult. TagIo is the executor's Discord; crate::tag_io::DiscordIo::new(app, guild_id, Option<Responder>) is the production one. The hook bodies are handle_prefix_invocation and handle_triggers. Hook state: ctx.state key CUSTOM_COMMAND_STATE ("customCommand") holds CustomCommandState{tag_id,name} (re-exported in plugin).
- Command helpers that the dashboard can reuse (commands::tag): length_problem, clean_description, edit_patch, append_action, remove_action, add_response, remove_response, apply_access_rule, cooldown_patch, trigger_edit_plan, trigger_choices, name_rows.

## Known gaps

- tagService.js getEffectiveActions: the 'unknown actions format' warning (once per tag per hour) is logged from run_tag, /tag info and /tag action list. It is not logged from buildUsage, /tag list, the autocomplete or the role-hierarchy guard, because get_effective_actions is pure in Rust and the throttle lives in per-app state.
- tagService.js renderCtxFor: not a separate function. Callers build the RenderContext with bot_core::adapters::render_context, and run_tag fills in args.
- tagService.js __testing.reset: no global reset. State is per app (app.ext::<TagState>()), so each test app starts clean. primeTags, primeTriggers and configure are ported as prime_tags, prime_triggers and configure_count_uses.
- tag.js actionAdd: when the guild or channel is not in the serenity cache, the ping_role check (pingableRoleIds) and the role check (check_assignable_role_cached returns 'Role not found.') fail closed. The JS always had live discord.js objects. In production the guild is cached, so behaviour matches.
- tag.js triggerChannelProblem: forum detection uses the cached channel, else the resolved option's kind. An uncached channel gets check_channel_usable's 'Channel not found' reason.
- tag_io.rs DiscordIo: snapshots the cached guild once per invocation, lazily on first use. The JS read the live cache at each step, so a role or channel change during one run is not seen.
- tag.js triggerInfo creator: read from the serenity user cache first, then GET /users over REST, falling back to the id. The JS used client.users.fetch.
- tagService.js sendNotice: the auto-delete after NOTICE_TTL_MS is a spawned tokio task (sleep, then mark_bot_deletion and delete) instead of an unref'd setTimeout. It is lost if the process stops within those 10 s, as in the JS.
