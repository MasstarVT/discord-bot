# dash-collections: public API (from its author)

All files are under rust/crates/dash-collections/src/.
- src/dashboard/collections/index.js → index.rs. Covers COLLECTION_LIST/COLLECTIONS as `Collections`, publicCollection, validateRecordInput, lintCollections, COLLECTION_KEY_RE and RECORD_ID_RE.
  - Descriptors are generated data in collections_data.json, written by rust/fixtures/gen/dash-collections-data.mjs from the JS definitions. Function hooks are left out of the data and restated in Rust; a test checks that the Rust hook set equals the JS one.
- errors.js → errors.rs. channelAccess.js → channel_access.rs. catalog.js → catalog.rs.
- Collection files → collections/*.rs:
  - embeds.rs
  - tags.rs
  - triggers.rs
  - reminders.rs
  - feeds.rs (reuses feat-feeds parsers)
  - small.rs: levelRoles, shopItems, economyBoosts, achievements, starboards, tempVoiceHubs, statChannels, translateChannels
  - translate_stats.rs
  - cases.rs
  - levels.rs: levels and balances, plus the shared member-list helpers
  - giveaways_polls.rs
  - reaction_roles.rs
  - ticket_panels.rs
  - automations.rs
- Every exported constant and helper is ported: item builders, stored↔form converters, planButtons, shapeProblems, diff and normalize helpers, role policies, renderContextFor, toRestMessage and discordFailure.
- New seams:
  - store.rs: RecordStore, StoreCtx/ReadCtx, StoreError, TaskRunner, StoreRegistry.
  - bridge.rs: RecordsBridge, SettingsBridge.
  - discord.rs: DashboardDiscord, DiscordResources, DiscordApiError.
  - testing.rs: in-memory doubles.
- Tests (2 files):
  - tests/golden.rs. These match the JS byte for byte: every publicCollection and the whole catalog against dashboard-catalog.json. Also JS-equal: validateRecordInput over 8,763 create/update/action inputs and 898 helper calls. All goldens come from rust/fixtures/gen/dash-collections-golden.mjs with Date.now frozen.
  - tests/behaviour.rs: ported catalog, descriptor, request, channel-access and member-helper cases; drift guards against feat-economy, starboard-stats, tempvoice, tickets, reactionroles, achievements, giveaways-polls, moderation and translation; the doubles; and a database test. That test reads the 30 seeded rows through bot-db, checks they equal what Prisma reads, and checks the item builders on them equal the JS answers.

## API

Crate dash_collections. The main types are re-exported at the root.

**Collections (index.rs)**
- `Collections::builtin()` holds the 22 definitions in display order. `Collections::new(Vec<Collection>)` builds an injectable set, server.js `deps.collections`.
- Methods on a set: `.get(key)`, `.iter()`, `.keys()`, `.validate_record_input(key, &data, Mode::{Create,Update,Action}, action_id: Option<&str>, current: Option<&Value>, now_ms: f64) -> Result<ValidInput{data: Map, refs: dash_schema::Refs}, StoreError>`, `.lint()`.
- Free functions: `validate_record_input(...)` and `lint_collections()` on the builtin set; `is_record_id`, `is_collection_key`. The server passes `now_ms` = Date.now().
- `validate_record_input` errors are always `StoreError::Field` (with path) or `StoreError::Record` (not_found: 'Unknown list.' / 'Unknown action.').
- `Collection` exposes:
  - fields: `key`, `fields: Vec<dash_schema::Field>`, `read_only`, `create_only`, `actions: Vec<CollectionAction{id, fields, check}>`;
  - accessors: `limit() -> Value`, `singular()`, `module()`, `label()`, `searchable()`, `check_stored_roles()`, `action(id)`, `all_fields()`, `public_json()` (publicCollection);
  - ref keys: `record_refs(item) -> (StoredRefs, IndexSet<keys>)` (server.js recordRefs, refKeys applied), `ref_key_for(&RoleRef, &data)` (refKeyOf or refKey).
- Build a fake collection with `Collection::from_json(json).with_check(fn).with_action_check(id, fn).with_ref_keys(..)`. `CheckFn = fn(&Map, &CheckCtx{mode, current, now_ms}) -> Option<Problem{path, message}>`.

**Catalog**
- `build_catalog() -> &'static Value` and `catalog_json() -> &str` (GET /api/catalog, byte-identical to the JS).
- `build_catalog_with(&Collections)`.
- `catalog::{CATEGORIES, DESCRIPTIONS, CODES, SUB_TOGGLE_FIELDS, sub_toggles, placeholder_group, description, code}`.

**Errors (errors.rs)**
- `RecordError{code: RecordErrorCode, message, path: Option, saved: bool}`.
  - Constructors: `RecordError::new/invalid/not_found/unavailable/discord/failed/from_code`, plus `.with_path(p)` and `.saved()`.
  - `.status()`: invalid 400, not_found 404, conflict 409, limit 409, discord 502, failed 422, unavailable 503. `code.as_str()` gives the JS code.
- `TaskResult{status: TaskStatus::{Done,Failed,Pending,Offline}, message, data: Option<Value>}`. `task_outcome(Option<&TaskResult>) -> Result<TaskOutcome, RecordError>` is taskOutcome.
- `Saved{item, message: Option}` replaces withMessage/unwrapResult; `Saved::item(v)` and `Saved::with_message(v, msg)`.

**Store seam (store.rs)**
- Object-safe async (async-trait) `RecordStore: Send+Sync`:
  - `list(&self, guild, &ListOpts{query, cursor}, &ReadCtx) -> Result<ListAnswer{items: Vec<Value>, next_cursor: Option<String>}, StoreError>`
  - `get(guild, id, &ReadCtx) -> Result<Option<Value>, StoreError>`
  - `create(guild, data: Map, &StoreCtx) -> Result<Saved, StoreError>`
  - `update(guild, id, data: Map, &StoreCtx) -> Result<Saved, StoreError>`
  - `remove(guild, id, &StoreCtx) -> Result<(), StoreError>`
  - `action_ids(&self) -> &[&'static str]` (default empty)
  - `action(guild, id, action_id, data: Map, &StoreCtx) -> Result<ActionAnswer{item: Option<Value>, message: Option<String>}, StoreError>` (default: not_found 'Unknown action.')
- `StoreError` has four variants, mapped by the server as recordFailure does:
  - `Field(FieldError)`: 400 invalid_request with path;
  - `Record(RecordError)`: its status, code mapped by RECORD_ERROR_CODES, plus path and saved;
  - `Discord(DiscordApiError)`: 502 discord_failed;
  - `Internal(anyhow::Error)`: 500 save_failed on a write, 503 unavailable on a read.
  - `From` impls exist for each. `.to_json()`, `.as_record()`, `.as_field()`.
- Contexts:
  - `ReadCtx{guild_id, discord: Option<Arc<dyn DashboardDiscord>>}`.
  - `StoreCtx{guild_id, user_id, user: Option<DashboardUser{id, username, globalName, avatar}>, member: Option<DashboardMember{owner, permissions: String, highest_role_position}>, discord, tasks: Option<Arc<dyn TaskRunner>>}`.
  - `StoreCtx::for_actor(guild, &Actor{user_id, user, member}, discord, tasks)` is what recordsBridge's ctx() builds.
  - `ctx.run_task(kind, payload) -> anyhow::Result<Option<TaskResult>>` returns Ok(None) when there is no runner. Also `can_run_tasks()` and `read_ctx()`.
- `TaskRunner::run(guild, kind, payload, TaskOpts{actor_id, wait_ms: Option<u64>}) -> anyhow::Result<TaskResult>` is tasks.js run. The implementer's default wait is 12 s; Err means a queueing DB failure.
- `StoreRegistry::new()` (builtin collections) or `with_collections(set)`:
  - `.register(key, Arc<dyn RecordStore>)` errors on an unknown key; `.with(key, store)` chains it.
  - `.store(key) -> Result<&Arc<dyn RecordStore>, RecordError('Unknown list.')>`, `.collection(key)`.
  - `.missing()` lists the keys without a store. bins/dashboard should assert it is empty at startup.

**Bridges (bridge.rs)**
- `RecordsBridge` (server's `deps.records`): `list(guild, key, ListOpts)`, `get(guild, key, id) -> Option<Value>`, `create(guild, key, data, &Actor) -> Saved`, `update(guild, key, id, data, &Actor) -> Saved`, `remove(guild, key, id, &Actor)`, `action(guild, key, id, action_id, data, &Actor) -> ActionAnswer`. All return `Result<_, StoreError>`.
- `SettingsBridge`:
  - `read_guild_settings(guild) -> anyhow::Result<Value>`: the normalizeRow'd GuildSettings row JSON that `dash_schema::read_settings_values` takes; Err gives 503 settings_unavailable.
  - `write_settings(guild, changes: Map, SettingsWriteCtx{user_id, discord}) -> Result<SettingsWritten{row: Value, notices: Vec<String>}, SettingsWriteError::{Field(FieldError) → 400, Internal → 500}>`.

**Discord seam (discord.rs), implemented by dash-server's discordApi port**
- `DashboardDiscord: Send+Sync` (async-trait):
  - `get_guild_resources(guild, fresh) -> Arc<DiscordResources>`
  - `get_guild_resources_display(guild)`: for display reads only, never a check of ids or roles. Defaults to the plain 30 s read; dash-server's client may answer from a copy up to 10 minutes old while it reloads.
  - `get_member(guild, user) -> Option<MemberRoles{roles}>`
  - `search_members(guild, query) -> Vec<MemberProfile>`
  - `get_users(&[String]) -> HashMap<id, Option<MemberProfile>>`
  - `send_message(ch, Value) -> MessageRef{id, channelId}`
  - `edit_message(ch, msg, Value) -> MessageRef`
  - `delete_message(ch, msg)`
  - `create_invite(ch, reason) -> InviteRef`
  - All return `Result<_, DiscordApiError{message, status, code, retry_after, discord_code}>`.
- `DiscordResources{guild: ResGuild, channels: Vec<ResChannel>, roles: Vec<ResRole>, emojis, bot: ResBot}` serialises to the exact mapGuildResources JSON (camelCase, channel `type`). Fill ResRole `mentionable`/`assignable`/`dangerous` as `Some(..)` always; None means 'could not check' as for JS undefined.
- `MemberProfile{id, username, globalName, nick, avatarUrl: String, bot}`: avatar_url must be a string.

**Row convention for stores**
- Item builders take `serde_json::to_value(bot_db model)`, with dates as ISO strings.
- Stores add `_count`, `items` and `options` themselves.
- A key the JS copies directly (`row.x`) that is missing from the row is omitted from the item, as JSON.stringify does.

**Collection helpers**
- channel_access: `require_channel_access(&StoreCtx, &[ChannelCheck::new(id).needs(&SEND_NEEDS|&VIEW_NEEDS).path(p).optional()], lead) -> Result<(), StoreError>`, `is_channel_exempt`, `access_payload`.
- collections::embeds: `message_url`, `embed_item`, `render_context_for`, `rest_allowed_mentions`, `to_rest_message`, `member_for`, `member_can_mention_all`, `ping_role_problem`, `split_pingable_roles`, `assign_role_problem`, `discord_failure(StoreError, &FailureInfo)` / `discord_failure_of`, `discord_codes`, `SLUG_PATTERN`.
- tags: `check_tag`, `action_to_form`, `action_to_stored`, `tag_item`.
- triggers: `trigger_item`.
- reminders: `schedule_input`, `weekday_names` / `weekday_numbers`, `schedule_text`, `is_done`, `reminder_item`.
- feeds: `parse_feed_source -> Result<FeedInput, String>`, `feed_item`.
- small: `total_xp_for_level`, `highest_reachable_level`, `is_emoji_entry`, `KIND_INFO`, `normalize_stat_template`, `translate_languages`, `describe_languages`, plus the limits.
- translate_stats: `build_stat_items`.
- cases: `action_payload`, `case_type`, `case_status(row, now)`, `reason_text`, `case_item(row, &profiles, now)`, `case_number_query`.
- levels:
  - `PAGE_SIZE`, `rank_cursor`, `read_rank_cursor -> RankCursor{sql_after(col, $v, $u)}`, `after_rank_cursor`;
  - async `load_profiles(Option<&Arc<dyn DashboardDiscord>>, ids)` and async `search_user_ids(..)`;
  - `profile_name`, `profile_label`, `avatar_for`, `level_progress`, `level_item`, `empty_level_item`, `balance_item(.., &Currency)`, `PENDING_CREATE`.
- giveaways_polls: `end_time_problem(data, required, now)`, `check_giveaway`, `giveaway_summary`, `giveaway_item(row, &names)`, `poll_duration_problem`, `check_poll`, `poll_summary`, `poll_item(row, Option<&PollTally>, &names)`.
- reaction_roles: `emoji_key`, `parse_custom_emoji`, `items_problem`, `diff_panel_items -> PanelItemsDiff`, `normalize_items -> Result<_, RecordError>`, `panel_item`, `template_options()`.
- ticket_panels: `default_panel_template`, `default_intro_template`, `sanitize_channel_name`, `validate_name_format`, `is_empty_template`, `to_json`, `canonical_emoji`, `normalize_options`, `diff_options -> OptionsDiff`, `is_default_template`, `option_item`, `panel_item(row, open)`.
- automations:
  - tables: `TRIGGER_RULES`, `trigger_sets`, `CONDITION_RULES`, `ACTION_RULES`, `PRESETS`, `PRESET_FIELDS`, `AUTOMATION_VARIABLES`, `SELF_REF_ID`, `problem_key`;
  - converters: `blank_condition` / `blank_action`, `flags_to_stored`, `condition_to_form(c, &nameOf)` / `condition_to_stored(f, &idOf)`, `action_to_form`, `action_to_stored(f, Option<i64>)`;
  - rules: `plan_buttons(actions, existing, button_seq, assign) -> Result<ButtonPlan, Problem>`, `shape_problems`, `check_automation`;
  - items and keys: `summary_of`, `automation_item(row, &nameOf, problems, creator)`, `action_role_keys`, `action_role_key_of`.

**testing module (testing.rs)**
- `MemoryStore` (generic CRUD, scripted actions and failures, call log).
- `RegistryBridge{registry, discord, tasks}`: the reference dispatch for the real records bridge.
- `MemorySettings`, `MemoryTasks`, `FakeDiscord`.

## Known gaps

- schema.js validateValue 'datetime' (inherited dash-schema gap), seen in collections/index.rs validate_record_input: 2 of the 8,765 golden validation cases differ. Both are reminders.runAt = '<@123>' (create and update). V8's legacy Date.parse accepts it; bot_util refuses it. The test skips them as known gaps.
- translateStats.js languageLine/buildStatItems localeCompare → collections/translate_stats.rs locale_compare: approximated as ASCII case-insensitive comparison with lower case first. It is exact for the lowercase language codes, snowflake ids and ISO dates it sorts; it is not full ICU.
- levels.js loadProfiles(discord, ids, logger) and all ctx.logger uses: the logger argument is replaced by tracing::warn.
- levels.js afterRankCursor returns a Prisma `where`. levels.rs after_rank_cursor returns that same JSON (for parity), and RankCursor::sql_after gives the SQL condition the Rust stores need.
- embeds.js renderContextFor returns objects with closures (iconURL, displayAvatarURL). embeds.rs render_context_for returns bot_util's RenderContext with the URLs precomputed. Rendered output was checked equal for 28 placeholder tokens.
- embeds.js toRestMessage(rendered) takes discord.js builders. embeds.rs to_rest_message takes the JSON of bot_core::custom_embed::Rendered::to_json().
- channelAccess.js requireChannelAccess: a problems[].index that is a numeric string ('0') indexes the checks in JS; Rust uses numeric indexes only, so the refusal then has no path.
- reactionRoles.js diffPanelItems / ticketPanels.js diffOptions use JS Map lookups (SameValueZero). Rust matches by the JSON text of roleId/id; the only difference is 1 vs 1.0 numbers, never strings.
- embeds.js memberFor: when ctx.member is null and the user is the owner, JS returns {owner:true}. Rust returns DashboardMember{owner:true, permissions:""}, which every rule reads the same way.
- Store 'counts' (task text): no *.store.js exports counts, so nothing was added. Stores export list/get/create/update/remove/actions (RAW_SQL is a JS test hook).
- automations.js tables (TRIGGER_RULES, CONDITION_RULES, ACTION_RULES, texts) are drift-checked against automationService only in the JS. feat-automations has no finished API yet, so there is no Rust drift test against it; they are covered by the JS goldens.
