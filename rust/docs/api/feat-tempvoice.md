# feat-tempvoice: public API (from its author)

All seven JS files are ported in full. The crate was only a skeleton when I started, so everything below is new.

Ports (JS file → Rust file):
- src/services/tempVoiceService.js → src/temp_voice_service.rs. Covers every constant, MSG and okEmbed, the pure helpers, overwrite computation, the panel, picker and modal builders, the guild index memo (TTL, deduplicated loads, generation counter), the hub:/ch: KeyedMutex locks, rule-20 confirmChannel, createForMember / queueHubJoin with the cooldown retry, onJoinTemp / onLeaveTemp, the tempvoice.cleanup job, runVoiceAction (all 11 actions), reconcileGuild, cleanEmptyChannels, the button, select and modal handlers, hubSummary / hubProblems, and the CRUD (createHub under the GuildSettings row lock, updateHub, updateHubRoleList under the hub row lock, deleteHub, listHubs and the others).
- src/services/voiceRoleService.js → src/voice_role_service.rs. Covers the settings shapes, desired roles, the role overlay, applyMember (serialised per member), reconcileGuild, releaseDroppedRoles, the four settings edits and the drop patches.
- src/plugins/temp-voice.js → src/plugins/temp_voice.rs. The voiceStateUpdate hook takes its first ch: lock place synchronously, then runs detached.
- src/plugins/voice-roles.js → src/plugins/voice_roles.rs. Includes the one-at-a-time startup queue and the detached role releases.
- src/commands/admin/voice-hub.js → src/commands/voice_hub.rs
- src/commands/admin/voicerole.js → src/commands/voicerole.rs
- src/commands/community/voice.js → src/commands/voice.rs

Every Discord REST call goes through a new seam, src/discord_port.rs. The `VoiceRest` trait has a serenity implementation, and tests replace it per app. Reads always come from the serenity cache.

JS-side scripts:
- rust/fixtures/gen/feat-tempvoice-golden.mjs writes rust/fixtures/golden/feat-tempvoice/golden.json.
- rust/fixtures/gen/feat-tempvoice-js.mjs runs the two cross-language directions.

Tests are in tests/offline.rs and tests/db.rs, with shared helpers in tests/common/mod.rs. They include:
- golden checks for the three commands and both plugins;
- the ported pure tests from both JS test files, plus name and format sweeps against JS output;
- the full lifecycle through the hooks and the router: hub join, panel, keep-alive, cleanup job, rule 20, every action, panel components, reconcile, channelDelete, deleteHub, /voice clean, /voice-hub create, edit, list, roles and delete with confirmation, /voicerole;
- the voice-roles flows;
- cross-language checks both ways. JS writes hub, row, job, Redis rename stamps and voice-role links; Rust reads them and runs the JS job. Rust writes the same kinds of data; JS reads them back and the results are compared as JSON.

The suite passed with TEST_DATABASE_URL and TEST_REDIS_URL set and nothing skipped. `cargo clippy -p feat-tempvoice --all-targets` is clean.

## API

Entry point: `feat_tempvoice::register(&mut Registry)`. Modules: `temp_voice_service`, `voice_role_service`, `discord_port`, `plugins::{temp_voice, voice_roles}`, `commands::{voice, voice_hub, voicerole}`.

temp_voice_service, for feat-events:
- `count_humans(&App, GuildId, ChannelId) -> usize` (`countHumans`); `count_humans_in(&Guild, ChannelId)` and `human_ids_in` work on a cached guild.

temp_voice_service, for the dashboard:
- `validate_name_format(Option<&str>, &NameContext) -> NameFormatCheck`. A `NameContext` can be built from bot_util placeholder structs, or with `NameContext::of(&Member, Option<&Guild>)`.
- `count_hubs(&PgPool, guild)`, `find_hub_by_channel(&PgPool, guild, channel)`, `is_temp_channel(&PgPool, channel)`, `list_hubs(&PgPool, guild) -> Vec<HubWithCount{hub, channels}>`, `list_hub_channels(&PgPool, hub_id)`.
- `create_hub(&App, guild, HubData)`: `HubData` is `Vec<(&'static str /*Prisma field*/, Value)>`. At the cap it fails with `HubLimitError`; `is_duplicate_hub(&anyhow::Error)` is the JS P2002 check.
- `update_hub(&App, guild, hub_id, HubData) -> Option<TempVoiceHub>`.
- `update_hub_role_list(&App, guild, hub_id, field, op, role) -> RoleListOutcome`.
- `delete_hub(&App, GuildId, &hub, delete_hub_channel, actor_tag) -> DeleteHubResult`.
- `invalidate_index(&App, guild)`, `peek_index`, `load_index`.

temp_voice_service, other public functions:
- Pure helpers, each mirroring its JS export: `lowest_free_index`, `normalize_channel_name`, `render_channel_name`, `text_channel_name`, `effective_bitrate`, `maximum_bitrate`, `parse_limit_input`, `check_rename_window`, `claim_status`, `target_problem`, `can_control`, `is_temp_voice_mod`, `is_protected_target`, `exempt_role_ids`, `reconcile_decision`, `compute_voice_overwrites`, `compute_text_overwrites`, `grantable_bits`, `build_control_panel`, `build_member_picker`, `build_value_modal`, `describe_discord_error`.
- Messages: `Msg` / `ok_embed`.
- Keys and constants: `cleanup_key`, `rename_key`, `RAW_SQL`.
- Flows: `run_voice_action(&App, VoiceActionReq) -> ActionResult{ok, embed}`, `clean_empty_channels`, `reconcile_guild`, `on_join_temp`, `on_leave_temp`, `handle_cleanup_job`, `confirm_channel`, `discord_call`, `delete_discord_channel`, `delete_temp_channel`, `purge_row`, `sync_text_overwrites`, `request_text_sync`, `post_control_panel`, `rename_stamps`.
- Handlers: `handle_button`, `handle_select`, `handle_modal`.
- `idle(&App)` waits for queued work (tests).

voice_role_service:
- For role persistence: `managed_role_ids(Option<&GuildSettings>) -> Vec<String>`.
- Settings shapes: `links_of`, `links_of_value`, `any_roles_of`, `has_voice_roles`, `voice_of`, `desired_role_ids`.
- Role changes: `apply_member(&App, GuildId, &Member, Option<Settings>, ApplyOptions) -> ApplyResult`, `reconcile_guild(&App, GuildId, Option<Settings>, max)`, `release_dropped_roles`, `is_reconciling`, `run_locked`.
- Settings edits: `add_link`, `remove_link`, `add_any_role`, `remove_any_role` (they return `AddOutcome` or `RemoveOutcome{changed, still_managed}`).
- Drop patches: `patch_drop_channel`, `patch_drop_role` (they return `Option<SettingsPatch>`).

discord_port:
- `VoiceRest` trait (the REST seam), `SerenityRest` (the production implementation), `rest(&App)`, `set_rest(&App, Arc<dyn VoiceRest>)`, `Overwrite`, `NewChannel`, `RestError`.

## Known gaps

- tempVoiceService.js snapshotOverwrites / grantableBits: serenity 0.12.5 drops permission bits it does not know when it reads `Permissions` (`from_bits_truncate`), so PinMessages (bit 51) is lost from cached template overwrites and from the bot's grantable bits. When the bot is not Administrator, its text-channel overwrite therefore never carries PinMessages. Pinning the panel still works through the Manage Messages fallback, as in the JS `canPin` check.
- tempVoiceService.js queueHubJoin: to tell whether the new channel still exists before posting the panel, Rust checks that its row still exists instead of `guild.channels.cache.has(voice.id)`. Both run under the same ch: lock, and the row is purged in the same step as the channel delete.
- tempVoiceService.js createForMember and voice-hub.js runCreate: serenity does not cache channels it creates over REST; discord.js does. Rust inserts the returned channel into the serenity cache itself (`cache_created_channel`); without that, the control panel would be skipped whenever the gateway echo is late. Behaviour now matches the JS, and a test covers the case.
- voice-roles.js onGuildReady: the place in the one-at-a-time startup queue is taken when the detached task first runs, not when the hook is called.
- voiceRoleService.js collectTargets: candidate members are taken in user-id order, not discord.js cache insertion order. This only changes which members are checked when the 1000-member limit (RECONCILE_MAX) truncates the list.
- temp-voice.js guildReady hook and tempVoiceService.js onVoiceStateUpdate: Rust loads the settings with get_settings_safe (memoised) instead of `ctx.getSettings()`. The result is the same.
