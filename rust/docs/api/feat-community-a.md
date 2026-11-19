# feat-community-a: public API (from its author)

The crate is ported and everything passes: 70 tests run with the mock database variables set (none skipped), and cargo check and clippy --all-targets are clean.

JS files and their Rust ports (under rust/crates/feat-community-a/src/):
- src/services/bumpService.js -> bump_service.rs
- src/services/suggestionService.js -> suggestion_service.rs
- src/services/confessionService.js -> confession_service.rs
- src/plugins/bumps.js, suggestions.js, confessions.js -> plugins/bumps.rs, plugins/suggestions.rs, plugins/confessions.rs
- src/commands/community/bumps.js, suggestion.js, confession.js -> commands/bumps.rs, commands/suggestion.rs, commands/confession.rs

All Discord REST calls go through one trait in io.rs (CommunityIo). Production uses serenity's HTTP client; tests swap in a recorder with set_io, the way the JS tests stub channel.send.

Every subcommand, option, button, modal, hook, the bump.remind job, the raw SQL, the debounced render and the review, block and reveal flows are ported, with the user-visible text copied from the JS. The three slash command payloads are byte-identical to commands.json and the three plugins match plugins.json.

Tests (one shared helper module, tests/common/mod.rs):
- tests/offline.rs, 23 tests: golden checks, the pure rules, and builder output compared with JS-produced fixtures (embeds, buttons, modals, panel, customIds, DMs, Disboard success cases, link detection, thanks and reminder texts, image refusals, job payload and dedupe key). It also includes a source scan that limits where author_id may appear.
- tests/db.rs plus tests/db/suggestions.rs and tests/db/confessions.rs, 47 tests: ports of the behaviour tests from test/bumps.test.js, test/suggestions.test.js and test/confessions.test.js. They run against PostgreSQL, through the router, against an offline fake guild. Each module's raw SQL is executed inside a rolled-back transaction. One test sweeps every flow and checks that no message the bot sends, no staff reply and no process log line contains the confession author.
- Cross-language tests in both directions use rust/fixtures/gen/feat-community-a-js.mjs:
  - JS records a bump and Rust runs the JS-scheduled ScheduledJob; Rust records bumps and JS reads them back identically.
  - JS creates a suggestion that Rust votes on; JS votes on, reviews and reads Rust suggestions.
  - JS creates a pending confession that Rust approves; JS blocks the author of a Rust confession and deletes it.
- rust/fixtures/gen/feat-community-a-golden.mjs writes rust/fixtures/golden/feat-community-a/{bumps,suggestions,confessions}.json.

This crate builds and reads no Redis keys of its own; settings caching goes through bot-core.

## API

Entry point: feat_community_a::register(&mut Registry) registers /suggestion, /confession, /bumps and the suggestions, confessions and bumps plugins. The public modules below are for other crates and the dashboard.

feat_community_a::bump_service
- Site table: BUMP_SITES, site(key), site_key_for_bot, is_disboard_success(&[Embed]), bumper_of(&Message), is_bump_site_message.
- Recording: record_bump(app, guild, site, channel, bumper, at) -> Recorded { recorded, count, next_bump_at }.
- Reads: get_bump_state, get_bump_states(pool, g), top_bumpers(pool, g, limit), bump_count_of. reset_counts(pool, g, Option<user>) returns the rows removed.
- Job: reminder_job and reminder_dedupe_key build the bump.remind job; handle_remind_job(Job, JobCtx) runs it.
- Hooks: on_bot_message, on_channel_delete, on_role_delete, and the filters channel_delete_filter and role_delete_filter.
- Texts and helpers: thanks_text, reminder_content, module_off_embed, mark_seen / reset_seen, raw_sql(), DEFAULT_REMINDER_MESSAGE, DEFAULT_THANKS_MESSAGE, BUMP_COUNT_MAX, LEADERBOARD_SIZE.

feat_community_a::suggestion_service
- Rules: status_info, is_voting_open, parse_content, cooldown_remaining_ms, toggle_vote, jump_url, submit_problem, cooldown_embed, module_off_embed.
- Builders: build_suggestion_embed / _components / _payload / _modal, build_review_dm.
- Lookups: author_for, resolve_channel, find_by_id, find_by_number, cooldown_left_ms.
- Create: create_suggestion(app, guild, user, content, anonymous, &GuildSettings) -> Result<Created, CreateError>; create_failure_embed; submit_from_interaction(&impl Interactive, ...).
- Votes and components: apply_vote(pool, id, user, ±1) -> VoteOutcome; handle_vote_button; handle_suggestion_modal.
- Render: render_now, request_render, cancel_render, render_suggestion_message. Test seams: set_render_impl and set_render_interval.
- Staff: review_suggestion(app, guild, number, status, note, reviewer, settings) -> Option<Reviewed>; delete_suggestion -> Deleted.
- Hooks: on_message_delete, on_channel_delete. Also raw_sql() and send_failure_reason.

feat_community_a::confession_service
- Rules: status_info, err_tag, parse_content, contains_link, image_problem, submit_problem, cooldown_embed, jump_url, module_off_embed.
- Builders: build_confession_embed(number, content, image_name, now), build_review_embed, build_review_components, build_confession_modal, build_panel_payload.
- Targets and images: resolve_targets, download_image; test seam set_image_fetch.
- Create and forms: cooldown_left_ms; create_confession -> Result<Created, CreateError>; create_failure_embed; submit_from_interaction; settings_within_budget; open_confession_form; handle_confession_modal; handle_button (the panel button and Approve / Reject).
- Staff: delete_confession -> Deleted; set_blocked(app, g, number, blocked) -> BlockResult; reveal_confession -> Revealed; describe_author.
- Hooks: on_message_delete, on_channel_delete; channel_delete_update is the pure decision of the channel-delete hook. Also raw_sql() and the find_by_id / find_by_number reads.

feat_community_a::io
- The CommunityIo trait (send, edit, delete_message, fetch_message, fetch_channel, start_thread, delete_channel, fetch_user, dm), with SerenityIo as the production implementation.
- io(app) returns the implementation in use; set_io(app, Some(..)) installs a fake for tests.

The command modules expose small pure helpers (status_fields, leaderboard_lines, config_lines, review_lines, reveal_lines) plus data() and command(). commands::bumps::settings_in_budget_labeled is the peekSettings-or-bounded-load helper the modal-first paths use.

## Known gaps

- Channel lookups only use the serenity cache. The JS falls back to a REST fetch (guild.channels.fetch) when a channel is not cached. Affected: suggestionService.js createSuggestion (the channel is reported as gone), confessionService.js resolveTargets and revealConfession (the log channel), and bumpService.js reminderChannel. reminderChannel also loses the JS path that rethrows a transient fetch error so the job retries; Rust skips the channel instead.
- suggestionService.js authorFor: the author avatar URL comes from bot_core::adapters, which builds png?size=256 URLs. The JS calls displayAvatarURL(), which defaults to webp with no size. Only the image URL differs.
- confessionService.js errTag: the log tags are 'DbError <sqlstate>', 'DiscordAPIError <code>' or 'Error'. The JS prints the Prisma error name and code (for example 'PrismaClientKnownRequestError P2002'). Both log by name and code only, never the arguments.
- confessionService.js handleReviewButton: interaction.inCachedGuild() is approximated as 'the guild is in the serenity cache and the interaction has a member'.
- bumpService.js isDisboardSuccess takes serenity Embed values. The JS also accepts the discord.js builder shape ({ data: { description, image } }), which never arrives from the gateway.
- bumpService.js __setScheduleImpl has no Rust equivalent. scheduleReminder's 'retry once, then log' path is implemented but not tested, because DbTestApp offers no way to make schedule_job fail.
- Three JS tests are not ported. From test/confessions.test.js: 'createConfession: a delete that races the save takes the post back' (the code path is ported); the ANONYMITY test of a slow or failing cooldown lookup in the form pre-check, logged by code only (ported code: openConfessionForm); and the ANONYMITY test of a Prisma error that echoes its arguments. The all-flows anonymity sweep, including the process log, is ported.
- The JSON sent to Discord differs from discord.js in ways Discord treats the same: allowed_mentions always lists users and roles (empty unless set); embed fields always carry inline:false; embeds carry type:'rich'; embed images carry null proxy_url, height and width; buttons carry disabled:false and no animated:false on their emoji. The golden comparisons normalise only these keys.
- suggestionService.js requestRender: the debounce window is measured on the app clock (app.clock()), while the timer itself sleeps in real time with tokio. In production both are the system clock, so behaviour is the same.
