# feat-general: public API (from its author)

Ported all seven JS files to rust/crates/feat-general. The entry point is `register()` in src/lib.rs, which registers /module, /settings, /help, /info, /owner and the bot-joins plugin.
- src/services/botJoinService.js -> src/bot_join_service.rs: `find_inviter`, `record_bot_join` and `list_bot_joins`, plus pure audit-log and integration pickers.
- src/plugins/bot-joins.js -> src/plugins/bot_joins.rs: the guildReady hook, module null, 15 s timeout.
- src/commands/utility/help.js -> src/commands/help.rs: works from the app's command registry. The list, `/help command:` and the autocomplete all match the JS for the full bot's 79 commands.
- src/commands/utility/info.js -> src/commands/info.rs: user, server and role subcommands. The embeds are built purely from plain views, and the REST counts are kept for 60 s per app (at most 500 entries).
- src/commands/utility/owner.js -> src/commands/owner.rs: owner.js has no subcommands and uses no Node internals, so there was nothing to emulate. It is a straight port of the single command.
- src/commands/admin/module.js -> src/commands/module.rs: list, enable, disable and the autocomplete. Disabling leveling clears `voice:start:{g}:*`; enabling invites spawns `feat_logging::invite_tracker::refresh_snapshot`.
- src/commands/admin/settings.js -> src/commands/settings.rs: show, timezone, color, prefix, urban, and timezone-role add/remove/list, plus the autocomplete. It re-exports the timezone helpers from bot-util, as the JS does.

Fixtures:
- rust/fixtures/gen/feat-general-golden.mjs produces rust/fixtures/golden/feat-general/cases.json from the JS. help.js and module.js keep their builders private, so the script imports a temporary patched copy that is deleted afterwards. For /module it uses a settings stub.
- rust/fixtures/gen/feat-general-js.mjs is the JS side of the cross-language database tests.

Tests are all in tests/general.rs (28 tests). They cover:
- the payloads and the plugin against commands.json and plugins.json;
- the full-bot /help groups, pages, details for every command and the autocomplete against the JS output;
- the /module list, autocomplete and key resolution against the JS output;
- the /settings colour, prefix and overview builders against the JS output, and the /info builders against the JS output on the fakes from mod-utilities.test.js;
- the bot-joins.test.js cases, including a fake Discord REST that checks the query parameters;
- every command through the router, using fake guilds and the fake REST;
- the database tests: BotJoin and settings in both directions, and the /help custom-command group compared with the JS `customCommandGroup`.

## API

Entry point: `feat_general::register(&mut Registry)`.

bot_join_service:
- `list_bot_joins(db: &PgPool, guild_ids: &[String]) -> anyhow::Result<HashMap<String, BotJoinEntry>>` is the JS listBotJoins Map keyed by guild id, the one src/dashboard/server.js uses. `BotJoinEntry { inviter_id, inviter_name, source, joined_at }` serializes camelCase, with joinedAt as a JS date string. It fails on a database error.
- `record_bot_join(&App, GuildId, GuildReadyReason)`; `record_bot_join_with_lag` takes the join lag in ms.
- `find_inviter(&App, GuildId, bot_id, app_id) -> Inviter` and `find_inviter_with(&Http, GuildId, Option<Permissions>, bot_id, app_id)`.
- Pure pickers: `inviter_from_audit(&Value, bot_id)` and `inviter_from_integrations(&Value, bot_id, app_id)`.
- Also `save_bot_join`, `raw_user_tag` and `rest_get_json`.

commands::help:
- `custom_prefix_of(&Value) -> Option<String>`, `custom_command_line(name, description, prefix)`, `tag_roles_allow(allow, deny, member_roles)`.
- `custom_command_group(db, guild_id, settings, viewer_roles) -> Option<Group { label, lines }>`; `custom_group_from` is its pure part.
- `build_groups(&[Arc<CommandDef>], level, settings)`, `build_pages`, `visibility`, `command_embed`, `help_choices`, `usage_of`, `subcommand_lines`, `wanted_name`.
- `CATEGORY_LABELS` and `embed_json`.

commands::module:
- `resolve_module_key`, `module_is_on`, `module_list_lines`, `module_list_embed`, `module_choices`, `toggled_embed`.

commands::settings:
- `parse_color_input -> Option<ColorInput { color: Option<u32>, adjusted }>`, `suggest_colors(query, current) -> Vec<Choice>`, `validate_prefix -> Result<String, PrefixError>` ('' means off; PrefixError has `code()` and `message()`).
- `build_overview_embed(&GuildSettings, guild_name, role_exists, now_ms)`.
- `timezone_role_list_embed`, `timezone_role_line`, `timezone_roles_json`, `suggest_timezone_role_zones`, `settings_choices`, `tz_error`, `shown`.
- Constants `COLOR_NAMES`, `COLOR_RESET_WORDS` and `PREFIX_MAX`.
- Re-exports of the bot-util timezone helpers (`LEGACY_TZ_NAMES`, `POPULAR_TIMEZONES`, `list_time_zones`, `normalize_time_zone`, `utc_offset_label`, `search_time_zones`, `describe_local_time`).

commands::info:
- `key_permission_names(Permissions)`, `format_role_mentions(&[(id, position)], guild_id, max)`, `count_channels(&[u8])`, `is_public_feature`, `feature_label`, `access_level_label`.
- Constants `KEY_PERMISSIONS`, `VERIFICATION_LABELS`, `CONTENT_FILTER_LABELS` and `USER_BADGES`.
- Views `ServerView`, `RoleView`, `UserView` and `MemberView`, each with an `of(..)` constructor from serenity.
- Builders `build_server_embed`, `build_role_embed`, `build_user_embed` and `role_member_text`.
- `get_guild_counts` and `get_role_counts`, memoised in `app.ext::<InfoCounts>()`.

commands::owner: `owner_id_of(raw)` and `owner_line(id)`.

## Known gaps

- src/commands/info.rs RoleView::of: serenity always has a RoleTags struct, while discord.js has `role.tags === null` when Discord sent none. A role counts as having tags when any tag field is set, so a role sent with an empty `tags: {}` object and not managed gets no 'Managed by' field, where the JS would show 'Unknown'.
- src/commands/info.rs UserView::of: badges come from serenity's `public_flags`, which is u32. Flags above bit 31 (Quarantined and others) are not visible, but none of them is a badge, so the output is the same.
- src/commands/info.rs fetch_member: a member fetched over REST (when it is not cached) is not added to the serenity cache, whereas discord.js `members.fetch` caches it.
- src/commands/owner.rs execute: BOT_OWNER_ID is read from AppConfig, which is loaded at boot, instead of from process.env on every call.
- src/commands/help.rs custom_command_group: the 3 s timeout cancels the CustomTag query, while the JS withTimeout lets it keep running. The debug log text on a timeout may also differ from the JS withTimeout message.
- src/bot_join_service.rs find_inviter_with / try_record: the audit-log and integration reads are raw REST GETs parsed as JSON, mirroring what discord.js reads. `client.application.id` comes from serenity's `http.application_id()` (set by the client at ready), falling back to the bot user id as in the JS.
- All reply paths that use interaction_helpers Payload::no_mentions send `allowed_mentions: { parse: [], users: [], roles: [] }` where the JS sends `{ parse: [] }`. Discord treats the two the same.
- src/commands/help.rs build_groups: categories come from each crate's CommandDef.category. The output matches the JS only if every other crate declares its JS folder name; that holds for all 79 commands in the commands.json reconstruction.
