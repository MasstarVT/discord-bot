# feat-embeds: public API (from its author)

Nothing had been written before the crash, so the port was done from scratch. It is complete: cargo check, cargo test and cargo clippy --all-targets are clean, and all 39 tests pass against both mock servers.

Files ported:
- src/services/embedService.js -> rust/crates/feat-embeds/src/embed_service.rs. Every export is ported: names, the URL and emoji checks, validateForSave, rendering and the mention policy, the pure mutators, import parsing, the welcome presets, the messageDelete filter and body, data access (raw ILIKE for search, matching Prisma's unescaped contains), the cache, the per-embed KeyedMutex in app.ext, publish/update/delete, setUpWelcomeChannel and the embed-modal submit handler.
- src/plugins/embeds.js -> src/plugin.rs: the messageDelete hook (always, timeoutMs 5000, sync filter) and the embed-modal modal handler.
- src/commands/admin/embed.js -> src/commands/embed.rs: all 21 subcommands with their exact texts, sendError, precheck and autocomplete for name and index.
- src/lib.rs: register(reg) only.

Discord calls go through the EmbedDiscord trait. HttpDiscord is the real implementation, on serenity http plus the cache. set_discord_ops lets tests drive the flows with a fake Discord, the way the JS tests use fake channels.

Fixtures:
- rust/fixtures/gen/feat-embeds-golden.mjs writes rust/fixtures/golden/feat-embeds/pure.json, the JS outputs for sweeps over names, emoji, URLs, validation, mutators, imports, presets, modals and render.
- rust/fixtures/gen/feat-embeds-js.mjs runs the JS side of the cross-language tests against the DB and Redis.

The tests are one binary, rust/crates/feat-embeds/tests/embeds.rs. They cover:
- the test/embeds.test.js cases, plus the embed assertions from review-automations, sec-dashboard and dashboard-collections;
- the golden command and plugin checks (the payload is byte-identical);
- DB flows, every subcommand through the router (including the confirm collector and the paginator), the form, welcome-channel, raw SQL in a rolled-back transaction;
- both cross-language directions on PostgreSQL and Redis.

## API

Entry point: `feat_embeds::register(reg)`. The service is the public module `feat_embeds::embed_service` (svc below).

Calls for other crates:
- `svc::get_saved_embed_template(app: &App, guild_id, name) -> Option<Value>`: the raw template, placeholders unexpanded. It normalises the name, ignores the module toggle, never fails, and uses the Redis cache `guild:{g}:embed:{name}` (300 s, a miss 60 s).
- `svc::search_saved_embeds(pool: &PgPool, guild_id, query, take: i64) -> Vec<SearchRow{id,name,channel_id,message_id}>`: serializes camelCase, never fails.
- `svc::normalize_embed_name(&str) -> Option<String>`.

Data access:
- `get_saved_embed(pool, g, name)` and `get_saved_embed_by_id(pool, Option<g>, id)`
- `require_saved_embed`, `count_saved_embeds`, `list_saved_embeds -> Vec<ListedEmbed>`
- `create_saved_embed(app, g, name, &tpl, created_by)`
- `mutate_saved_embed(app, g, name, |tpl| -> Result<Value, EmbedEditError>)` and `mutate_saved_embed_with`, whose closure also returns an extra value
- `locked_update`, `replace_template(app, g, id, tpl)`, `set_published(pool, id, channel, message)`
- `on_embed_message_delete(pool, g, message_id)`

Failures are `anyhow` errors. Downcast to `EmbedEditError {message, code: EditCode::{Invalid, NotFound, Taken, Limit}, errors, template}`.

Discord flows:
- `publish_saved_embed(app, &row, guild_id, channel_id, member, settings) -> PublishOutcome`
- `update_published_embed(app, &row, guild_id, member, settings) -> UpdateOutcome`
- `delete_saved_embed_and_message(app, guild_id, &row, delete_message) -> DeleteOutcome`
- `set_up_welcome_channel(app, WelcomeArgs) -> WelcomeOutcome`
- `handle_embed_modal_submit(ModalCtx)`
They call Discord through the `EmbedDiscord` trait. `set_discord_ops(app, Arc<dyn EmbedDiscord>)` swaps in a fake for tests.

Pure helpers:
- `render_saved_embed(tpl, &RenderContext, settings, &MentionPolicy) -> Option<Rendered>`, with `embed_render_context`, `mention_policy`, `resolve_publish_mentions` and `extract_role_mention_ids`
- `validate_for_save`, `parse_import_payload`, `build_welcome_presets`
- mutators: `add_field`, `edit_field`, `remove_field`, `add_button`, `remove_button`, `set_images`, `set_author`, `set_footer`, `set_color`, `set_title_url` (`Change::{Keep, Clear, Set}` stands in for JS undefined/null/value)
- `check_image_url`, `check_link_url`, `is_emoji_syntax`, `parse_button_emoji`
- `collect_unknown_tokens`, `unknown_variables_note`, `publish_hint`, `draft_attachment`, `embed_cache_key`

## Known gaps

- embedService.js publishSavedEmbed / setUpWelcomeChannel: the target channel is read from the serenity cache, where the JS used the interaction's resolved channel. A channel or thread that is not cached gets the 'Channel not found' refusal.
- embed.js E_DISCORD and the welcome-channel PARTIAL text: the error text comes from serenity's error_message, not discord.js DiscordAPIError.message, so the wording can differ slightly.
- embedService.js parseImportPayload: serde_json rejects some inputs JSON.parse accepts, such as lone surrogate escapes and 1e400. With duplicate keys the last value wins but keeps the first key's position.
- embedService.js updatePublishedEmbed: a stored channelId or messageId that is not a valid snowflake is treated as CHANNEL_GONE and the row becomes a draft again. The JS would have got a REST error.
- allowedMentions (renderSavedEmbed, publishSavedEmbed, updatePublishedEmbed, the presets): serenity always sends users: [] and roles: []. Discord reads that the same as the JS { parse: [] } and { parse: ['users'], roles }.
- Without a Discord connection, every Discord write fails as a non-API error, so commands answer 'Something Went Wrong'. isEmbedDeleteCandidate then passes only uncached messages, because the bot's id is unknown.
