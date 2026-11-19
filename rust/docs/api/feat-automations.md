# feat-automations: public API (from its author)

I ported all three JS files in full into rust/crates/feat-automations. The service is one public module, split by concern, so it can be read next to the JS:
- src/services/automationService.js → src/automation_service/{tables,pure,store,options,runtime}.rs, re-exported as feat_automations::automation_service. Every JS export has a public Rust counterpart, including RAW_SQL, the trackers, every hook body and the button handler.
- src/plugins/automations.js → src/plugin.rs: 8 gated hooks, all with id `automations` and a sync filter, plus the persistent `automation` button.
- src/commands/admin/automation.js → src/commands/automation.rs: all 14 subcommands and groups, every autocomplete, the private delete confirm, export as a file, and import. The payload is byte-identical to commands.json, and the plugin manifest matches plugins.json.
- Fixtures: rust/fixtures/gen/feat-automations-golden.mjs writes rust/fixtures/golden/feat-automations/pure.json, a sweep of every pure helper produced by the JS (inputs kept as JSON text). rust/fixtures/gen/feat-automations-js.mjs is the JS side of the cross-language tests.
- Tests: one binary, tests/automations.rs.
  - Golden sweeps compare key order byte for byte, including the export file.
  - Ported cases from test/automations.test.js and the automation cases of review-automations.test.js.
  - Runtime tests use FakeGuild plus FakeRest.
  - DB tests run the raw SQL in a rolled-back transaction, and `/automation` runs end to end through the router.
  - Both cross-language directions are covered for rows, the `guild:{g}:automations` cache value and TTL, cooldown keys, problem keys and the export JSON.

## API

The entry point is feat_automations::register(reg). The service is feat_automations::automation_service (`svc` below).

**What the dashboard store imports:**
- svc::create_automation(app, guild_id, &AutomationInput{name,trigger,conditions,actions,enabled,cooldown_seconds}, created_by, self_ref_id: Option<&str>) -> anyhow::Result<AutomationRecord>
- svc::mutate_automation(app, guild_id, name, FnOnce(&AutomationRecord) -> anyhow::Result<Mutation<R>>) -> anyhow::Result<(AutomationRecord, R)>
  - Mutation { data: Vec<(&'static str, Value)> with Prisma field names, result }.
  - The closure must be pure and Send + 'static. It runs under the row lock, and the shape is validated before the write.
- svc::replace_automation(app, g, id, &AutomationInput)
- svc::delete_automation(app, g, id) -> u64
- svc::normalize_condition(&Value, keep_hints) and svc::normalize_action(&Value), each -> Result<Value, String>
- svc::normalize_list(&Value, ListKind, keep_hints) -> {items, invalid}, and normalize_items
- svc::normalize_automation_name(&str)
- svc::build_preset_automation(preset, &PresetOptions)
- svc::is_unique_violation(&anyhow::Error), plus svc::as_automation_error(&err) -> AutomationError{message, code: ErrorCode}
- svc::executor_channel_needs(type) and svc::problem_key(id)
- svc::automation_problems(app, Option<&Guild>, &AutomationRecord, Option<&GuildSettings>)
- AutomationRecord::to_json() gives the row with Prisma field names.

**Reads:**
- get_automation_by_name(pool, g, name)
- list_automations, list_automation_refs, list_automation_buttons
- search_automations(pool, g, q): ILIKE, unescaped, as Prisma does
- search_saved_embed_names
- get_automations(app, g) -> Option<Arc<ByTrigger>>: memo, then Redis {v:1, byTrigger}, then Postgres
- peek_has_trigger, invalidate_automations

**Pure helpers:** validate_automation_shape, add_condition, append_action, remove_at, next_button_index / next_button_seq, evaluate_condition(s) over Facts, split_conditions, role_diff, voice_triggers, describe_condition / describe_action, to_export_object / to_export_json, parse_automation_json -> ParsedImport, discord_error_text(ActionError), the emoji, id and word parsers, attachment_kind, and RunBudget / CauseTracker / LocalExpiringSet.

**Configuration-time checks:**
- condition_params_from_options and action_params_from_options -> ParamsResult{ok, params, error, error_kind, missing, channel, reward_kind, embed_name, ignored}
- import_role_hierarchy_problem, action_channel_access_problem, role_ping_refusal, import_role_ping_problem
- resolve_import_references(app, guild, &ImportData, settings, existing_row_id)
- emoji_resolver_app(app, guild) searches the guild first, then every cached guild.

**Runtime:**
- dispatch(app, trigger, Base, await_runs)
- execute_automation(app, &AutomationLite, &Base)
- build_facts, build_vars, on_automation_button
- Cache-key builders: automations_cache_key, cooldown_key, repost_key, problem_key.

**Command helpers:** commands::automation::import_from_text(app, ImportRequest, text, &mut name) is the import after the download, reusable by the dashboard. Also summary_of and fields_for.

**Test seams:** set_automation_memo, settle_runs, hold_run_queue, pending_runs.

**Shared hook state:** the MEMBER_JOINED hook reads ctx.state `captchaPending` as a bool, the same way feat-welcome does.

## Known gaps

- automation_service/runtime.rs run_pin (JS ACTION_RUNNERS.PIN): the PinMessages pre-check is skipped, because serenity drops permission bit 1<<51 from cached roles and overwrites. When the bot lacks it, Discord's 50013 is recorded as "I'm missing a permission for this" instead of "I need Pin Messages in <#…>". The action also counts as attempted, so `uses` is bumped where the JS would not bump it.
- automation_service/options.rs import_role_hierarchy_problem (JS importRoleHierarchyProblem): the 'Your roles could not be checked.' branch cannot happen, because Rust member roles are always readable. Positions use bot-core's computed role order, where ties are broken by id, not the raw role.position.
- automation_service/options.rs action_params_from_options / condition_params_from_options and commands/automation.rs create (JS actionParamsFromOptions, create): a target channel that is not in the serenity cache is judged as 'Channel not found', and its role-ping check is skipped. The JS had the interaction's resolved channel object. This is the same gap feat-tags reports.
- automation_service/runtime.rs run_reply / usable_target (JS ACTION_RUNNERS.REPLY / usableTarget): a trigger channel missing from the cache records a problem, where the JS always had the channel object.
- automation_service/runtime.rs allowed_mentions_for (JS allowedMentionsFor): serenity always sends users: [] and roles: [] next to parse. Discord reads that the same way. This is the same gap feat-embeds reports.
- automation_service/pure.rs discord_error_text (JS discordErrorText): the message text comes from bot_core::discord::error_message, not discord.js DiscordAPIError.message, so the 50035 and default texts can be worded slightly differently.
- automation_service/runtime.rs on_automation_button (JS onAutomationButton): the button index is parsed as a decimal integer. JS Number() also accepts forms like '0x5'.
- automation_service/runtime.rs record_problem (JS recordProblem): the Redis write of the problem key is awaited rather than fire-and-forget. Only the ordering differs.
- automation_service/runtime.rs evaluate_condition MATCHES_REGEX (JS compiledRegex): it uses bot_util's shared compile cache instead of a local 500-entry Map. The behaviour is the same.
