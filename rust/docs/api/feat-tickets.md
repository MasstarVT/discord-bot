# feat-tickets: public API (from its author)

I ported all four JS files in full. Nothing from the earlier attempt had survived the crash, so this started from the skeleton.

- **src/services/ticketService.js** is now `src/ticket_service/`, split into submodules that are all re-exported flat from `feat_tickets::ticket_service`:
  - `rules.rs`: constants, `canPerform`, `transitionFor`, names and numbers, option emoji, row classification, the rename window, overwrites, templates, `describeDiscordError`, and the `mergeManagerSync` / `managerSyncPayload` helpers.
  - `messages.rs`: `errorPayload`, `panelErrorPayload`, `refreshNote`, `buildNotice` and the views.
  - `components.rs`: panel buttons and dropdown, the intro and closed rows, `disableButtons`, the reason form.
  - `transcript.rs`: `normalizeMessage`, `renderTranscriptHtml`, `fetchTranscriptMessages`, `buildTranscript`.
  - `io.rs`: the Discord seam (`TicketIo`, with the serenity implementation `SerenityTicketIo`, which also writes created, edited and deleted channels into the cache as discord.js does), the in-process state, `resolveChannel` and `confirmGoneChannels`.
  - `db.rs`: reads, writes under advisory and row locks, `RAW_SQL`, `onChannelDelete`, reconcile, `listPanelsView`, `syncManagerOverwrites`, `runManagerSyncJob`.
  - `panels.rs`: `ensureCategory`, `resolveOpenParent`, `moveChannel`, `buildPanelPayload`, `publishPanel`, `refreshPanelMessage`.
  - `open.rs`: `openTicket`, `postIntro`, `postNotice`, `listLiveTickets`.
  - `lifecycle.rs`: claim, close, reopen, add, remove, rename, delete, `transcriptForActor`, `isTicketStaff`.
  - `handlers.rs`: every component and modal handler.
- **src/plugins/tickets.js** is `src/plugin.rs`: the channelDelete hook (`always`, filtered to text, announcement and category channels), the `ticket` button, the `ticket-select` dropdown, the `ticket-modal` form (not module-gated) and the `ticket.managersync` job.
- **src/commands/admin/ticket-panel.js** is `src/commands/ticket_panel.rs`: every subcommand, option group and autocomplete.
- **src/commands/community/ticket.js** is `src/commands/ticket.rs`.

**Tests**
- Golden checks pass: both command payloads are byte-identical to `commands.json`, and the plugin matches `plugins.json`.
- `rust/fixtures/gen/feat-tickets-golden.mjs` writes `rust/fixtures/golden/feat-tickets/pure.json` from the JS service itself. The Rust output matches it for:
  - every error and panel-error payload, every notice, components, customIds, overwrites and manager-sync merges;
  - Discord error wording, name and emoji sweeps, transitions and permissions, templates;
  - transcripts byte for byte across several time zones and the size cap;
  - `normalizeMessage`.
- `rust/fixtures/gen/feat-tickets-js.mjs` covers both cross-language directions:
  - JS `seed` then Rust reads and acts on the data;
  - Rust writes then JS `read` reads it back (panel, ticket, the merged job, and the components built from those rows).
- The DB cases of `test/tickets.test.js` are ported, using a fake Discord seam (`tests/common`) and the mock PostgreSQL. They include:
  - caps under parallel calls, distinct ticket numbers, and "a cache miss is not a deletion";
  - failures mapped to codes, the full lifecycle, delete with transcripts, channelDelete, reconcile, publish and refresh;
  - the manager-sync job, both through the scheduler and settled directly;
  - router flows: the clicked message's real components survive the round trip, and the confirm dialogs work.

## API

Entry point: `feat_tickets::register(reg)`. The service is `feat_tickets::ticket_service`, re-exported flat. Calls take `&App` plus plain ids. A refusal is `Fail { code, data }` (the JS `{ ok: false, code, data }`; `Fail::to_json`). Database errors come back as anyhow::Error.

**For the dashboard** (ticketPanels.store / dashboardTasks):
- `create_panel(app, NewPanel)`
- `update_panel_fields(app, panel_id, guild_id, &[(prismaField, Value)])`
- `invalidate_panel` / `invalidate_guild_panels`
- `find_panel(db, guild, ref)` and `panel_by_id`
- `parse_option_emoji(input, bot_has_emoji, guild_emojis)`, `template_uses_token`, `validate_name_format`
- `publish_panel(app, guild_id, panel_id, &GuildChannel, settings)`, returning `Published` or `PublishError`
- `refresh_panel_message(app, guild_id, panel_id)`, returning `Refresh` (`to_json` gives the JS shape)
- `delete_panel(app, guild_id, &TicketPanel)`
- `live_manager_role_ids` / `live_managers`
- `manager_sync_payload(panel_id, roles)`, `merge_manager_sync(existing, incoming)` (use it as `ScheduleJob::merge`), `manager_sync_dedupe_key(guild)`
- `describe_discord_error`, `OPEN_REQUIRED_PERMS`, `is_empty_template`, `default_panel_template`, `default_intro_template`

**Other service functions:**
- Options and managers: `add_option`, `update_option`, `remove_option`, `update_managers(app, panel_id, guild_id, mutate)`, `sync_manager_overwrites`, `run_manager_sync_job(app, &Job, guild_id, budget_ms)`.
- Tickets: `list_panels_view`, `reconcile_guild_tickets`, `get_ticket_by_id` / `get_ticket_by_channel` (return `TicketFull` with panel and option), `open_ticket(app, guild_id, &Opener, &PanelWithOptions, option, reason, settings)`.
- Lifecycle: `claim_ticket`, `close_ticket`, `reopen_ticket`, `add_member`, `remove_member`, `rename_ticket`, `delete_ticket(app, ActionCtx, Option<OnSaved>)`, `transcript_for_actor`, `build_transcript`, `on_channel_delete`.
- Messages: `error_payload` / `panel_error_payload`.

**Testing seam:** set it with `ticket_service::set_io(app, Some(Arc<dyn TicketIo>))`. Tests shorten the 5 s delete pause with `set_delete_delay_ms`.

**Redis:** the crate builds no Redis keys and caches no values (the JS ticket service uses none). The panel memo, throttles, mutexes and rename log are per-app state in `app.ext`.

## Known gaps

- src/commands/community/ticket.js execute (transcript): the ticket channel is read from serenity's cache. If it is not cached, the reply is NOT_TICKET; the JS uses interaction.channel, which is always present.
- src/commands/admin/ticket-panel.js cached(): a picked channel or category is taken only from the cache. The JS falls back to the raw option object, so a channel missing from the cache reads as 'Channel not found'.
- ticketService.js refreshPanelMessage and deleteTicket (transcript channel): when a channel was found by a REST fetch but is not in the cache, the bot-permission check (checkChannelUsable) is skipped, because there are no cached overwrites to compute it from. The edit or post is attempted instead.
- ticketService.js publishPanel: the published message is edited directly instead of fetched first and then edited. A 10008 on the edit leads to a new post, which has the same effect as the JS fetch failing.
- ticketService.js publishPanel / refreshPanelMessage: a database error is described as 'Discord error unknown'. describeDiscordError on a Prisma error would print its P-code.
- ticketService.js handleTemplateSubmit: if saving the template to the database fails, the user gets the 'Something Went Wrong' panel error. The JS throws to the component router instead.
- ticketService.js renderTranscriptHtml: Intl.DateTimeFormat('en-GB', medium/short) is reimplemented in Rust (format_en_gb, with 'Sept'). It is checked byte for byte against the JS output in several time zones. Years before 1 rely on bot_util's era years.
- ticketService.js parseOptionEmoji: grapheme splitting uses unicode-segmentation rather than Intl.Segmenter. The emoji sweep, which covers ZWJ sequences, flags, keycaps and tag sequences, matches the JS.
- ticketService.js transitionFor with an unknown action (UNKNOWN): Rust actions are an enum, so an unknown action string is rejected when it is parsed (Action::parse returns None). The handler then answers 'This button is no longer active.', as the JS does.
