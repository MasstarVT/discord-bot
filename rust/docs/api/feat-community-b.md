# feat-community-b: public API (from its author)

Started over from the skeleton after the crash. All 9 JS files are fully ported in rust/crates/feat-community-b:
- src/services/modmailService.js -> src/modmail_service.rs
- src/services/boostService.js -> src/boost_service.rs (also holds the hook bodies and the boosts.sweep job, as in the JS)
- src/services/autoThreadService.js -> src/auto_thread_service.rs
- src/plugins/modmail.js -> src/plugins/modmail.rs
- src/plugins/boosts.js -> src/plugins/boosts.rs
- src/plugins/auto-threads.js -> src/plugins/auto_threads.rs
- src/commands/admin/modmail.js -> src/commands/modmail.rs
- src/commands/community/boost.js -> src/commands/boost.rs
- src/commands/admin/autothread.js -> src/commands/autothread.rs

The one entry point is `register`. Every subcommand, option, button, select, hook and the boosts.sweep job exists, with the JS text. The JS regexes run through bot_util::jsre, so they have V8 semantics. Discord REST goes through serenity's Http with raw JSON bodies that copy discord.js's bodies: role create/edit with `colors.primary_color`, a `data:image/jpg;base64` icon and `unicode_emoji`; thread create with type, invitable and the archive fallback.

Tests: tests/community_b.rs (42 offline) and tests/community_b_db.rs (17 database and cross-language).
- Golden checks of the commands and the three plugins.
- 32 sweeps the JS produced (rust/fixtures/gen/feat-community-b-golden.mjs -> rust/fixtures/golden/feat-community-b/pure.json): plainText, contentTitle, both threadName functions, unknownTokens, validateRoleName, parseEmoji, parseColor, planSweep, renderThankYou, problemEmbed, relay parts, prompts, embeds, customIds, publish limiter and isPublishable.
- Both directions against the JS services (rust/fixtures/gen/feat-community-b-js.mjs): ModmailThread rows, BoosterRole rows, the boosts.sweep job row (full row equality, maxAttempts 5) and the settings lists.
- The commands through the router against a cached fake guild.
- These modules have no Redis keys, so there are no cache tests.

## API

Entry point: `feat_community_b::register(&mut Registry)`. Public modules:

- modmail_service:
  - Rules: DEFAULT_GREETING, STAFF_HELP, limits::* (MODMAIL), STAFF_THREAD_PERMISSION, is_internal_note, is_blocked, accepts_conversations, is_pending_thread_id, RelayMessage{text, of}, attachment_lines, has_content, relay_parts, thread_name, staff_author, is_staff_author(level, role_ids, settings, channel_perms), who_lines.
  - Table: open_lock_key, run_open_locked, find_by_thread(pool, thread), find_in_guild, find_for_user, find_open_in_guild, find_open_for_user(pool, user, limit), reserve_open(app, g, u) -> Reserved::{Row, Existing}, attach_thread(app, id, thread), release_reserved, close_row(app, id, closed_by, reason) -> bool, close_by_thread(app, thread, reason) -> u64, list_open_in_guild, touch / touch_now.
- plugins::modmail (Discord side, used by /modmail and callable by others): relay_to_member, relay_to_thread, close_and_notify, close_conversation, archive_thread, channel_problems, open_conversation, list_open_conversations, server_for, find_servers, resolve_thread; payload builders confirm_payload, open_pick_payload, relay_pick_payload, intro_payload, member_message_embed, staff_message_embed, closed_embed, greeting_text; handle_direct_message, handle_button, handle_select, plugin().
- boost_service:
  - consts::* (BOOST), DEFAULT_BOOST_MESSAGE.
  - Pure: is_boosting, boost_transition, grace_ms, tier_number, message_template, render_thank_you(settings, &ThankYouSource, max, preview), parse_color, parse_emoji, validate_role_name, plan_sweep, placement_for, role_editable, member_problem, problem_embed, module_off_embed, prepare_look, create_role_body / edit_role_body.
  - Table: load_row, list_rows, count_rows, delete_row, delete_rows_by_role, mark_ended, clear_ended, insert_row, RAW_SQL.
  - Job: sweep_key, ensure_sweep_job(app, g, Option<run_at>), sweep_job.
  - Discord: remove_role, remove_booster_role, create_booster_role, edit_booster_role, delete_own_role, remove_member_role, post_thank_you / thank_you_message.
  - Cooldown: cooldown_left_ms, note_edit, reset_cooldowns, set_icon_fetch (test seam).
  - Sweep: sweep_guild, sweep_job_handler / sweep_job_run.
  - Hooks: on_member_update, on_member_remove, on_role_delete, on_channel_delete, on_guild_ready.
- auto_thread_service: consts::* (AUTOTHREAD), archive_label, Entry, normalize_entry, read_entries(&Value), read_publish_ids, channel_index, listed_channel, plain_text, content_title, thread_name(template, &NameSource), unknown_tokens, thread_needs, PUBLISH_NEEDS, missing_names, close_row, post_close_button, detach / settled, create_thread, PublishLimiter, is_publishable, publish_message, set_thread_channel(app, g, ThreadChange) -> SetResult (Err downcasts to AutoThreadError), remove_thread_channel, add_publish_channel, remove_publish_channel, drop_channel.

All per-process state (locks, throttles, cooldowns, limiter, not-modmail memo) lives in app.ext structs, with no statics.

## Known gaps

- boostService.js moveToSpot: Rust moves the role with serenity `edit_role_position`, which sends one `{id, position}` entry with the discord.js target position. discord.js `setPosition` PATCHes the whole reordered list. `boost_service::role_positions_body` computes that list and is tested, but it is not sent: a raw PATCH with an audit-log reason needs serenity's `Headers` type, which is not reachable (see core_requests).
- plugins/modmail.js memberMessageEmbed, staffMessageEmbed, staffAuthor and commands/admin/modmail.js reply: avatar and server-icon URLs come from bot_core::adapters as png?size=256. discord.js `displayAvatarURL()` and `iconURL()` with no options default to webp.
- plugins/auto-threads.js onMessage (createThread's `message.cleanContent`): Rust uses serenity `Message::content_safe`. User mentions become `@username` (plus `#discriminator` for legacy users), where discord.js uses the member's display name. Role and channel mentions match. This only changes `{content}` thread names that quote a user mention.
- plugins/modmail.js handleCloseButton: the settings load before the level check uses bot-core `permission_level_of`, which caps the budget at 1000 ms. The JS waits up to `ackBudgetMs`.
- services/boostService.js sweepJobHandler: whether the guild is usable comes from the app's guild directory, not `ctx.guild.available`. Same outcomes: retry in 5 minutes, end with no rows, retry in 2 minutes without settings, reschedule.
- services/boostService.js onGuildReady: the startup jitter uses a clock-seeded hash, not Math.random(). Same range: 60 s plus 0 to 120 s.
- plugins/modmail.js listOpenConversations and serverFor: a guild counts as available from the guild directory. The server name comes from the serenity cache, empty if not cached.
- Ported only as pure decision functions or refusal paths, because every Discord REST call fails offline. Not ported end to end from test/modmail.test.js: 'opening creates a private thread, pings only the configured roles and greets the member' (the payload is tested), 'private threads failing falls back to a public thread and says so', 'without Create Private Threads the thread is public, and says so', 'a thread that cannot be created drops the reserved row; a failing intro deletes the thread', 'member DMs go to the thread; a ✅ marks delivery', 'a failed delivery gets ❌, an archived thread is reopened', 'a deleted thread closes the conversation and the member is asked again', 'staff messages reach the member, with name or anonymous; ✅ on delivery', 'staff relay: ❌ when the DM fails, only staff may speak, closed threads stay quiet', 'no shared server with modmail: a short notice', 'one server: asks first with a button', 'several servers: a select menu', 'the open button opens the conversation and relays the message; a double click acts once', 'DMs sent while the prompt is open get ⏳ and all reach staff' (the selection logic `waiting_run` is tested), 'DMs that waited reach the conversation chosen from the menu', 'a conversation that opened meanwhile still gets the first message', 'a failing look-up of the waiting DMs still sends the message', 'the open button re-checks the server', 'the server menu opens in the chosen server', 'several open conversations: the member picks', 'a member cannot pick someone else's conversation', 'a member who floods is cut off with ❌' (`within_rate` is tested), 'closing: row closed once, member told, thread locked and archived', 'archiveThread only archives without Manage Threads and says so'.
- Not ported end to end from test/boosts.test.js: the success paths of 'create: no permissions, not hoisted, ...', 'create: placed just below the anchor role', 'create: a booster role goes just ABOVE the Nitro Booster role' (the placement decision and the request body are tested), 'create: two clicks at once make one role', 'create: any failure after the role exists deletes the role again', 'create: a role that cannot be moved is kept and reported', the edit/delete/remove success paths, 'postThankYou: pings only the booster' (the message and its allowed_mentions are tested), and the role deletions of 'sweep: deletes roles of members who left or whose grace is over' (offline the deletes fail and the rows are kept, which is tested). Also the role-delete and thank-you calls in the guildMemberUpdate and guildMemberRemove hooks; their database effects are tested.
- Not ported end to end from test/auto-threads.test.js: 'createThread: names the thread, archives per the entry, posts a close button' and its error-code tests, 'publishMessage: crossposts a post', 'publishMessage: past the hourly limit ... no request is made', 'publishMessage: a message type Discord refuses (50068) gives its slot back', 'onMessage: starts the thread of a listed channel and publishes in the background', 'onBotMessage: publishes ...', and the successful archive of 'handleClose'. The limiter, publishability, names, filters and the close-button refusal and update path are tested.
