# feat-feeds: public API (from its author)

All 7 JS files are ported into rust/crates/feat-feeds:
- src/services/feedParsers.js → src/feed_parsers.rs. Two support modules sit next to it: src/xml.rs, a port of the fast-xml-parser 5.11 scanner, node2json and the @nodable/entities decoder with the bot's XML_PARSER_OPTIONS; and src/js_date.rs, a port of V8's Date.parse (the ES5 format plus the legacy parser).
- src/services/feedService.js → src/feed_service.rs.
- src/services/reminderService.js → src/reminder_service.rs.
- src/plugins/feeds.js → src/plugins/feeds.rs.
- src/plugins/reminders.js → src/plugins/reminders.rs, including the reminder-modal handler.
- src/commands/admin/feed.js → src/commands/feed.rs.
- src/commands/admin/reminder.js → src/commands/reminder.rs.
- src/net.rs holds the outbound seams, stored per app in app.ext. FeedFetcher defaults to bot_util::safe_fetch and is the JS __setFetchImpl. DiscordRest defaults to app.http() and covers posting, fetching a channel and reading messages after one.

Why there is no XML crate: the JS parser is tolerant and does not validate. It accepts unclosed tags, stray closing tags, a raw & and `<!foo>` read as an element, so a strict crate would change what feeds parse to. The port keeps the JS behaviour exactly.

Parity is proven against JS-generated fixtures:
- rust/fixtures/gen/feat-feeds-parsers.mjs writes golden/feat-feeds/parsers.json:
  - 53 documents, each compared on both the raw fast-xml-parser tree and the parseXmlFeed output. They cover CDATA, entities, namespaces, missing fields, depth limits, unclosed tags, stray closing tags, the prototype names, CRLF and 5 DOCTYPEs.
  - 64 odd date strings.
  - Bluesky and iTunes JSON, stripHtml, oneLine, looksLikeHtml, discoverFeedLinks, stripDoctype, charset decoding, itemKey and the four input parsers.
- feat-feeds-service.mjs writes service.json with the keys, nonces, customIds, describeFetchError, templateWarnings, computeNextPollAt, select/window, post payloads, validateReminderInput, schedules in 4 time zones, the activity gate, send payloads and summary lines.
- feat-feeds-js.mjs is the live JS side of the cross-language tests:
  - Feed rows, jobs and the feed:fetch cache are checked in both directions, plus a reddit block set by JS stopping a Rust fetch.
  - For reminders, the rows and jobs are checked in both directions.
- feat-feeds-sqlprobe.mjs generates no fixture. It records the SQL Prisma produces for the ILIKE / increment / OR queries that were ported.

The plugins match plugins.json and both commands match commands.json byte for byte. Clippy is clean.

## API

Entry point: feat_feeds::register(reg). It registers the feed and reminder commands and the feeds and reminders plugins. Every module is pub.

feed_parsers:
- Parsers: parse_xml_feed, parse_bluesky_author_feed, parse_itunes_search.
- Text and HTML helpers: strip_html, one_line, decode_xml_buffer, looks_like_html, discover_feed_links, strip_doctype, item_key.
- Input parsers: normalize_feed_url, parse_youtube_channel_input, parse_subreddit_input, parse_bluesky_actor_input.
- Types: FeedItem (serialised as the JS cache shape), ParsedFeed, FeedParseError, ITEM_ID_MAX.

feed_service:
- Constants and key helpers: FEED_TYPES, base_interval_ms, max_bytes, feed_job_key, fetch_cache_key, cache_ttl_seconds, the REDDIT_*/ITUNES_* keys, FEED_CHANNEL_TYPES, SEEN_CAP, WINDOW_SIZE, MAX_FAILURES.
- Per-type lookups: type_label, type_emoji, type_color, default_template, type_vars, COMMON_VARS, is_card_type, bsky_appview.
- Pure helpers:
  - parse_retry_after_ms, compute_next_poll_at(NextPoll), window_items, select_new_items(SelectInput) -> Selection.
  - var_help, parse_message_option -> Result<Option<String>, String>, allowed_tokens, template_warnings, escape_third_party.
  - build_feed_vars, build_item_embed, build_post_payload(app, &Feed, &FeedItem, &PostCtx) -> PostPayload with to_json().
  - display_name_update, source_link, describe_fetch_error(FeedType, &FetchError), feed_status -> (icon, text), serialize_for_cache, revive_cached, bot_member.
- Fetching and resolution:
  - fetch_and_parse(app, url, type, headers) -> FetchOutcome.
  - fetch_feed_source(app, &Feed, use_cache, conditional) -> SourceResult.
  - resolve_feed_source(app, type, &SourceInput) -> Result<Resolved{source, fetch_url, display_name, probe: Probe, warnings, image}, Refusal{title, message}>.
- Database:
  - find_feed_for_guild, get_feed, list_feeds, count_feeds.
  - create_feed_row(app, NewFeed): a FeedUserError comes back inside the anyhow error; downcast it.
  - schedule_feed_job(app, guild, id, run_at), disable_feed, send_disable_notice, disable_notice_embed, disable_feeds_for_channel, ensure_guild_feed_jobs.
- Job handler: poll_feed_job(job, app), plus poll_feed_row(app, &Feed) for a row already read.
- Per-app state: FeedsState (create lock, /feed test throttle, local Reddit budget), reset_local_state.

reminder_service:
- Constants and small helpers: DEFAULT_CONTENT, default_template(), mode_from_choice, mode_label, REMINDER_CHANNEL_TYPES, MODULE_OFF_LINE, resolve_time_zone, job_key, module_off_line, paused_line.
- Schedule (From<&Reminder>), ScheduleInput, validate_reminder_input(input, tz, now, base) -> Result<Schedule, String>.
- Schedule maths: compute_next_run, job_run_at, late_tolerance_ms, due_slot.
- Templates, payloads and text:
  - is_empty_template, has_user_tokens.
  - build_send_payload(app, &Reminder, guild, channel, settings, ping) -> SendPayload with to_json(nonce).
  - describe_schedule, summary_lines, reminder_warnings.
- Activity gate: ActivityState, activity_allows_send.
- Database:
  - find_reminder_by_name, get_reminder, list_reminders, search_reminders.
  - create_reminder_row(app, NewReminder): ReminderUserError comes back inside the anyhow error.
  - update_reminder(app, id, &ReminderUpdate), sync_reminder_job(app, &r, settings, now), pause_reminder, delete_reminder, auto_disable, disable_reminders_for_channel, reconcile_guild_reminders.
- Retry helpers: is_transient_send_error(&RestError), scheduler_retry_delay_ms, may_retry, occurrence_nonce.
- Job handler: fire_reminder(job, app, attempt).

Commands:
- commands::feed::{command, data, read_add_source, feed_summary_lines, list_entry, search_feeds, MODULE_OFF_NOTE}.
- commands::reminder::{command, data, modal_for, is_done, action_row_builder}.

Seams: net::{set_fetcher, set_rest, fetcher, rest, FeedFetcher, DiscordRest, FetchFailure, RestError, MessageLite}.

## Known gaps

- src/js_date.rs date_parse: a date string without a zone is read in the zone named by the TZ environment variable (UTC when unset or invalid). V8 uses the OS local zone. For a DST gap the offset from 3 hours earlier is used.
- src/xml.rs skip_doctype: only part of fast-xml-parser's DocTypeReader is ported. <!ELEMENT, <!ATTLIST and <!NOTATION are skipped to the next '>', and an <!ENTITY inside one is an error. The result is the same in practice, because feedParsers.parseXmlFeed refuses any <!ENTITY, and only a 5th or later DOCTYPE ever reaches the parser.
- src/feed_parsers.rs looks_like_html / entry_summary and src/feed_service.rs: the JS cuts strings by UTF-16 units (slice(0, 1000) and slice(0, 5000)) and can leave a lone surrogate. Rust's utf16_prefix drops the half pair instead.
- src/feed_service.rs revive_cached: cached item fields with the wrong type (for example a non-string title) become ''/None/false. The JS spreads whatever the cache holds. This only matters for a foreign writer, since both bots write the same shape.
- src/net.rs SerenityRest: the production Discord calls are tested only through the DiscordRest test seam, never against Discord. These are send_message (raw create-message JSON, which carries nonce + enforce_nonce for reminders), fetch_channel and messages_after.
- src/net.rs is_system / src/reminder_service.rs check_activity: discord.js message.system is approximated as a message type other than Regular, InlineReply, ChatInputCommand or ContextMenuCommand.
- src/commands/feed.rs and src/commands/reminder.rs: the reply after a defer goes through safe_reply (editReply, falling back to followUp when the original was deleted), where the JS calls editReply directly. no_mentions() serialises as {parse:[],users:[],roles:[]} instead of {parse:[]}; both mean the same to Discord.
- src/commands/reminder.rs create_cmd (the form path): the row is created and then scheduled in a spawned task, and the modal waits for the row over a oneshot channel. In the JS, scheduling is chained on the create promise. Order and behaviour are otherwise the same, including the 'Still Saving' timeout.
- src/commands/feed.rs / src/commands/reminder.rs: whether the ping_role option is mentionable is read from the guild cache first and then from the resolved role. The JS reads the resolved option role first.
