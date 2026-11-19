# feat-events: public API (from its author)

All 7 JS files are ported in rust/crates/feat-events:
- src/services/eventService.js → src/event_service.rs: every export, plus the per-app state (render throttle, waiting voice channels, warn throttle, render/user/category KeyedMutexes in app.ext).
- src/services/rolePersistService.js → src/role_persist_service.rs.
- src/utils/calendar.js → src/calendar.rs. The foundation had already ported it as bot_util::calendar, so this re-exports it and adds the JS default arguments (google_url, outlook_url, ics). It is verified against JS goldens.
- src/plugins/events.js → src/plugins/events.rs.
- src/plugins/role-persist.js → src/plugins/role_persist.rs.
- src/commands/community/event.js → src/commands/event.rs.
- src/commands/admin/rolepersist.js → src/commands/rolepersist.rs.

Fixtures:
- rust/fixtures/gen/feat-events-golden.mjs writes rust/fixtures/golden/feat-events/golden.json: cards, buttons, reminders, lines, reply texts, calendar entries/payloads/links/ICS, edit panel and modals, formatStartForEdit and parseEventWhen at DST edges for 9 zones, field and limit parsers, keys, customIds, overwrites, RAW_SQL, role-persist rules.
- rust/fixtures/gen/feat-events-js.mjs is the JS side of the cross-language tests (createEventRow, applyRsvp, getTally, scheduleEventJobs, closeEvent, listOpenEvents, resolveEventRef, findEventsForAutocomplete, saveRow/loadRow, lifecycleSkips, a MUTE infraction).

Tests:
- tests/events.rs, with submodules tests/events_db/mod.rs and tests/events_db/channels.rs.
- tests/role_persist.rs.
- Both use the shared helpers in tests/common/mod.rs.
- Both commands are byte-identical to commands.json and both plugins match plugins.json.
- The cross-language checks run both directions: GuildEvent and EventRsvp rows, the updatedAt ordering, capacity, ScheduledJob dedupe keys and payloads (Rust runs the JS event.end job), the ILIKE wildcard semantics, PersistedMember, and the event-role markers.

## API

Entry point: `feat_events::register(&mut Registry)`. The public modules are `event_service`, `role_persist_service`, `calendar`, `plugins::{events, role_persist}` and `commands::{event, rolepersist}`. Functions take `&App` and plain ids.

**event_service (eventService.js)**
- Constants and keys:
  - `consts` (EVENT), `OPEN_STATUSES`, `REMIND/END/VOICE/CLEANUP_JOB_TYPE`.
  - `remind_job_key` / `end_job_key` / `voice_job_key` / `cleanup_job_key(id)`.
  - `CREATE_LOCK_SQL`, `EVENT_LOCK_SQL`, `raw_sql()`.
  - `status_by_action`, `rsvp_label`, `rsvp_emoji`, `NOT_YOURS_TEXT`, `EDIT_FORMS`, `CALENDAR_HINT`, `TEXT_ALLOW` / `VOICE_ALLOW`.
- Pure helpers:
  - Times: `is_open(Option<&GuildEvent>)`, `event_end`, `remind_at` / `remind_at_of`, `voice_lead(Option<&GuildSettings>)`, `voice_lead_text`, `voice_at`, `shown_end`, `start_problem(Option<i64>, now) -> Option<StartProblem>`, `format_start_for_edit`, `format_start_fields`.
  - Text and ids: `text_channel_name`, `event_overwrites(guild, role, bot, allow) -> Vec<EventOverwrite>`, `jump_url`, `calendar_file_name`.
  - Durations: `parse_duration_input(Option<&str>) -> Result<Option<i64>, DurationError>`, where `.text()` is DURATION_ERRORS.
  - Card and lines: `Tally`, `tally_from_groups`, `empty_tally`, `build_event_embed` / `build_event_components` / `build_event_payload` (settings, now_ms, names), `reminder_messages -> Vec<ReminderMessage>`, `list_line`, `attendee_lines(&[(user, RsvpStatus)])`, `rsvp_reply_text(&RsvpOut, Option<&GuildEvent>, RsvpStatus)`.
  - Calendar: `plain_discord_text(Option<&str>, &dyn NameSource)` (`NameSource` is implemented by `NoNames`, `NameMaps` and `GuildNames`), `calendar_entry`, `build_calendar_payload(event, names, now_ms, content) -> Payload`, `guild_name_maps`.
  - Edit panel: `build_edit_panel -> Payload`, `build_edit_modal(event, form, zone) -> CreateModal`.
  - Parsers: `parse_event_fields`, `parse_event_details`, `parse_event_when(start, duration, resolved, zone, now, current, examples)` and `parse_event_limits`, all returning `Result<_, FieldError{title,text}>`. Also `EventFields`, `CurrentEvent`, `CREATE_EXAMPLES` / `FORM_EXAMPLES`.
  - Permissions: `is_event_staff(app, Option<&MemberAccess>, settings)`, `can_manage_event`.
  - `discord_event_options -> DiscordEventOptions`.
- Database:
  - Reads: `find_event`, `find_guild_event`, `get_tally`, `resolve_event_ref(app, guild, raw)`, `list_open_events -> (Vec<GuildEvent>, HashMap<id, going>)`, `find_events_for_autocomplete(app, guild, sub, q, host) -> Vec<EventChoice>`.
  - Writes: `create_event_row(app, guild, &NewEvent, channel, host_max) -> CreateRow::{Created, Limit, HostLimit}`, `apply_rsvp(app, event, guild, user, RsvpStatus) -> RsvpOut{result, previous, note}`, `mark_role_granted`.
- Flows:
  - `load_zone(app, guild, user, member, budget) -> LoadedZone{resolved, settings, personal_unknown}`.
  - `create_event(app, guild, channel, settings, &NewEvent, host_max) -> Result<(GuildEvent, notes), CreateFailure>`.
  - `create_discord_event`, `create_event_chat`, `sync_access_role`, `sync_rsvp_role`, `schedule_event_jobs -> failed`.
  - Rendering: `member_names`, `request_render`, `cancel_render`, `render_now`, `render_event_message`, `set_render_impl`.
  - Closing: `reconcile_overdue`, `close_event(app, id, status, CloseReason, guild) -> CloseOutcome`, `cancel_event`, `strip_roles(app, event, after, run) -> next cursor`.
  - Editing: `edit_event(app, guild, event, &EventChanges) -> Option<(GuildEvent, notes)>`.
  - Lookups: `resolve_channel -> ChannelLookup`, `fetch_member`, `is_waiting_voice`.
- Handlers:
  - Components: `handle_rsvp_button`, `handle_edit_button`, `handle_edit_modal`.
  - Hooks: `on_message_delete`, `on_channel_delete`, `on_discord_event_cancelled`, `on_waiting_voice_left`, `on_role_delete`.
  - Jobs: `handle_remind_job`, `handle_end_job`, `handle_voice_job`, `handle_cleanup_job`.

**role_persist_service (rolePersistService.js)**
- Constants: `consts` (ROLEPERSIST).
- Locking: `run_locked(app, g, u, f)`.
- Retention: `retention_days(Option<&GuildSettings>)`, `retention_days_of(Option<&Value>)` for raw input, `expires_at`, `is_stale`.
- Table: `load_row`, `count_rows`, `delete_row`, `save_row(app, g, u, &Snapshot, now, settings)`, `prune_expired`, `reset_prune_stamps`.
- Rules: `merge_snapshot`, `skipped_role_ids`, `module_role_reasons`, `snapshot_member(Option<&Member>, &Guild, settings)`, `plan_roles(guild, bot_id, ids, settings, held, skips) -> Plan{give, skipped}`.
- Event markers: `lifecycle_skips -> LifecycleSkips{skips, markers}`, `clear_event_markers`.
- Gates: `gate_of(guild, member, settings, captcha_pending) -> Option<Gate>`.
- Waiting set: `mark_waiting`, `clear_waiting`, `is_waiting`, `reset_waiting`, `waiting_len`.

**plugins::role_persist**
- `AUDIT_REASON`.
- `restore_member(app, guild, member, settings, captcha_pending, now) -> Restore`.
- The filters and hook bodies: `leave_filter`, `join_filter`, `gate_may_have_cleared`, `role_delete_filter`, `on_leave`, `on_join`, `on_update`, `on_role_delete`.

**Other modules**
- `commands::event`: `info_lines`, `choice_name`, `status_emoji`, `status_text`.
- `commands::rolepersist`: `status_fields`, `OFF_NOTE`.
- `calendar`: re-exports `bot_util::calendar`, plus `google_url`, `outlook_url` and `ics` with the JS defaults.

## Known gaps

- commands/community/event.js create: the posting channel (the picked one, the configured one or the current one) is judged from the serenity cache only. An uncached channel answers 'Channel not found', where the JS falls back to the option's resolved partial channel object.
- eventService.js resolveChannel: a fetched channel that is not a guild channel counts as noAccess; the JS would return it as found. Event channels are always guild channels.
- eventService.js __setJobImpl: the test seam is not ported. The Rust tests use the real scheduler stores, PostgreSQL included. __setRenderImpl is ported as event_service::set_render_impl.
- eventService.js requestRender: the trailing render is a tokio::spawn'd task (sleep, then render), not an unref'd setTimeout. A render that is due can still run during shutdown; it is idempotent.
- eventService.js plainDiscordText / buildCalendarPayload: guild names come from a snapshot of only the ids the description mentions (guild_name_maps), taken under the cache lock, instead of live discord.js cache lookups. The output is the same.
- eventService.js memberNames: an uncached member is fetched with bot_core::guild_members::fetch_members_by_ids (the gateway, else one REST call per id) and a user with http.get_user. The display name is serenity's nick > global_name > username, the same order as discord.js displayName.
- rolePersistService.js snapshotMember: discord.js's partial member (uncached at guildMemberRemove) is the payload's member = None in Rust, which saves nothing, as the JS does.
- plugins/role-persist.js canSetNickname: member.manageable is computed by hand (not the owner, not the bot, else the bot is owner or its highest role position is above the member's) with bot_core::permissions.
- Time zones use bot_util::time / tzdata (the foundation's own tz tables), not chrono-tz. formatStartForEdit, formatStartFields and parseEventWhen match the JS Intl goldens at the 2026 DST edges of America/New_York, Europe/London, Australia/Lord_Howe, Pacific/Chatham and others.
- eventService.js parseEventWhen and the edit form zone note: zoneLabel uses the `now` passed in, not `new Date()`. They are the same instant in production.
- eventService.js createDiscordEvent / updateDiscordEvent / voice-job notes: the error text in '(...)' comes from bot_core::discord::error_message (Discord's message), clamped to 120. discord.js's err.message carries the same Discord message.
- eventService.js deleteEventChannel / handleVoiceJob: serenity's delete_channel parses the channel Discord returns. Discord always returns it, so there is no behaviour change, but a 204 from a proxy would count as a failure and retry.
