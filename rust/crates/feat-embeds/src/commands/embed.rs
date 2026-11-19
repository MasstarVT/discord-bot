//! Port of `src/commands/admin/embed.js`: `/embed` (pkg-embeds). Saved embed builder (fields, link buttons), publish
//! to a channel, edit the live message later, and the Welcome Channel presets. Thin: data access, validation and
//! rendering live in [`crate::embed_service`]. Gated on the Embed Messages module, ADMIN level.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::custom_embed::{build_template_modal, empty_template, format_color, limits as tpl_limits, normalize_template, parse_color_str, template_to_json};
use bot_core::interaction::InteractionError;
use bot_core::interaction_helpers::{Choice, ConfirmOptions, Payload, await_confirm, cached_channel, cached_guild, is_interaction_gone, respond_autocomplete};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_util::js::{lower, trim};
use bot_util::placeholder_parser::{RenderContext, placeholder_help};
use bot_util::text::clamp_text;
use serde_json::{Value, json};
use serenity::all::{ChannelId, EmojiId, GuildId, Permissions};
use serenity::builder::{CreateAttachment, CreateEmbed, CreateInteractionResponseFollowup, EditInteractionResponse};

use crate::embed_service::{
    self as svc, Change, EditCode, EmbedEditError, FieldEdit, IMPORT_MAX_BYTES, MentionPolicy, PublishOutcome, SAVED_EMBEDS_LIMIT, UpdateOutcome, WelcomeArgs,
    WelcomeOutcome,
};

/// `TEXT_TYPES`.
const TEXT_TYPES: [u8; 5] =
    [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT, channel_type::PUBLIC_THREAD, channel_type::PRIVATE_THREAD, channel_type::ANNOUNCEMENT_THREAD];

fn name_opt() -> Opt {
    Opt::string("name", "Saved embed name").required(true).autocomplete(true).max_length(32)
}

fn index_opt(what: &str) -> Opt {
    Opt::integer("index", format!("{what} number")).required(true).min_value(1).max_value(25).autocomplete(true)
}

fn pos_opt() -> Opt {
    Opt::integer("position", "Position (1 = first; default: last)").min_value(1).max_value(25)
}

fn url_opt(n: &str, d: &str) -> Opt {
    Opt::string(n, d).max_length(2048)
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("embed", "Embed Messages: build, publish and edit saved embeds (fields, link buttons)")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("create", "Create a saved embed and open the editor form")
                .option(Opt::string("name", "New embed name (letters, numbers, - and _)").required(true).max_length(32)),
        )
        .option(Opt::sub_command("edit", "Edit the message text, title, description, colour and footer in a form").option(name_opt()))
        .option(
            Opt::sub_command_group("field", "Embed fields (up to 25)")
                .option(
                    Opt::sub_command("add", "Add a field")
                        .option(name_opt())
                        .option(Opt::string("title", "Field title").required(true).max_length(256))
                        .option(Opt::string("value", "Field text").required(true).max_length(1024))
                        .option(Opt::boolean("inline", "Show next to other inline fields"))
                        .option(pos_opt()),
                )
                .option(
                    Opt::sub_command("edit", "Change a field")
                        .option(name_opt())
                        .option(index_opt("Field"))
                        .option(Opt::string("title", "New title").max_length(256))
                        .option(Opt::string("value", "New text").max_length(1024))
                        .option(Opt::boolean("inline", "Show next to other inline fields"))
                        .option(pos_opt()),
                )
                .option(Opt::sub_command("remove", "Remove a field").option(name_opt()).option(index_opt("Field")))
                .option(Opt::sub_command("list", "List the fields of an embed").option(name_opt())),
        )
        .option(
            Opt::sub_command_group("button", "Link buttons (up to 25)")
                .option(
                    Opt::sub_command("add", "Add a link button")
                        .option(name_opt())
                        .option(Opt::string("label", "Button text").required(true).max_length(80))
                        .option(Opt::string("url", "Link (https://…)").required(true).max_length(512))
                        .option(Opt::string("emoji", "Emoji shown on the button").max_length(64))
                        .option(pos_opt()),
                )
                .option(Opt::sub_command("remove", "Remove a link button").option(name_opt()).option(index_opt("Button"))),
        )
        .option(
            Opt::sub_command("image", "Set the large image and/or thumbnail (none = remove)")
                .option(name_opt())
                .option(url_opt("image_url", "Large image URL, {user.avatar} or {server.icon}"))
                .option(url_opt("thumbnail_url", "Thumbnail URL, {user.avatar} or {server.icon}")),
        )
        .option(
            Opt::sub_command("author", "Set the author line (text: none = remove)")
                .option(name_opt())
                .option(Opt::string("text", "Author name").max_length(256))
                .option(url_opt("icon_url", "Author icon URL"))
                .option(url_opt("url", "Link on the author name")),
        )
        .option(
            Opt::sub_command("footer", "Set the footer and timestamp (text: none = remove)")
                .option(name_opt())
                .option(Opt::string("text", "Footer text").max_length(2048))
                .option(url_opt("icon_url", "Footer icon URL"))
                .option(Opt::boolean("timestamp", "Show the time the message was posted")),
        )
        .option(
            Opt::sub_command("color", "Set the side colour (none = server default)")
                .option(name_opt())
                .option(Opt::string("color", "Hex like #5865F2 or a colour name").required(true).max_length(16)),
        )
        .option(
            Opt::sub_command("title-url", "Make the title a link (none = remove)")
                .option(name_opt())
                .option(Opt::string("url", "Link (https://…) or none").required(true).max_length(2048)),
        )
        .option(Opt::sub_command("preview", "Preview a saved embed (only you can see it)").option(name_opt()))
        .option(
            Opt::sub_command("publish", "Post a saved embed in a channel")
                .option(name_opt())
                .option(Opt::channel("channel", "Channel to post in").required(true).channel_types(&TEXT_TYPES)),
        )
        .option(Opt::sub_command("update", "Apply your changes to the published message").option(name_opt()))
        .option(Opt::sub_command("list", "List the saved embeds of this server"))
        .option(
            Opt::sub_command("delete", "Delete a saved embed")
                .option(name_opt())
                .option(Opt::boolean("delete_message", "Also delete the published message (default: no)")),
        )
        .option(Opt::sub_command("export", "Download a saved embed as a JSON file").option(name_opt()))
        .option(
            Opt::sub_command("import", "Create or replace a saved embed from JSON")
                .option(Opt::string("name", "Embed name (letters, numbers, - and _)").required(true).max_length(32))
                .option(Opt::attachment("file", "JSON file (max 20 KB)"))
                .option(Opt::string("json", "JSON text (instead of a file)").max_length(6000))
                .option(Opt::boolean("overwrite", "Replace the embed if the name exists (default: no)")),
        )
        .option(
            Opt::sub_command("welcome-channel", "MEE6 Welcome Channel: post greeting, rules, links and staff embeds")
                .option(Opt::boolean("create", "Create a read-only #welcome channel (default: yes)"))
                .option(
                    Opt::channel("channel", "Post in this existing channel instead")
                        .channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]),
                ),
        )
}

/// The command (`module 'embeds'`, `gated = true`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Embeds), execute)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Reply texts (error catalogue) ──────────────────────────────────────────────

fn e_name() -> CreateEmbed {
    error_embed("Invalid Name", Some("Embed names can use letters, numbers, - and _ (up to 32 characters)."), None)
}

fn e_notfound(name: &str) -> CreateEmbed {
    error_embed("Embed Not Found", Some(&svc::not_found_text(name)), None)
}

fn e_taken(name: &str, extra: &str) -> CreateEmbed {
    error_embed("Name Taken", Some(&format!("{}{extra}", svc::taken_text(name))), None)
}

fn e_limit() -> CreateEmbed {
    error_embed("Limit Reached", Some(&svc::limit_text()), None)
}

fn e_nothing() -> CreateEmbed {
    error_embed("Nothing To Change", Some("Give at least one of the optional values."), None)
}

fn e_discord(message: Option<&str>) -> CreateEmbed {
    error_embed("Discord Rejected It", Some(&clamp_text(&format!("Discord refused the message: {}", message.unwrap_or("unknown error")), 1000)), None)
}

fn e_db() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("I could not save that right now. Please try again in a moment."), None)
}

fn e_empty() -> CreateEmbed {
    error_embed("Nothing To Show", Some("This embed renders empty. Add a title, description, field or content."), None)
}

fn e_slow() -> CreateEmbed {
    error_embed("Try Again", Some("The database is slow right now. Please try again in a moment."), None)
}

/// `embedForError(err)`.
pub fn embed_for_error(err: &EmbedEditError) -> CreateEmbed {
    let title = match err.code {
        EditCode::NotFound => "Embed Not Found",
        EditCode::Taken => "Name Taken",
        EditCode::Limit => "Limit Reached",
        EditCode::Invalid => "Could Not Save",
    };
    error_embed(title, Some(&err.message), None)
}

/// `sendError(interaction, embed)`: answers in the way that fits the interaction's state, tolerating an expired
/// interaction (not acknowledged: an ephemeral reply; deferred: the reply, its buttons and files cleared; replied: an
/// ephemeral follow-up).
pub async fn send_error(r: &Responder, embed: CreateEmbed) {
    let s = r.state();
    let result = if !s.deferred && !s.replied {
        r.reply(Payload::embed(embed).ephemeral(true).to_reply()).await
    } else if s.deferred && !s.replied {
        r.edit_reply(EditInteractionResponse::new().embeds(vec![embed]).components(Vec::new()).clear_attachments()).await.map(|_| ())
    } else {
        r.follow_up(CreateInteractionResponseFollowup::new().embed(embed).ephemeral(true)).await.map(|_| ())
    };
    if let Err(e) = result
        && !is_interaction_gone(&e)
    {
        bot_util::log_warn!("embeds: could not send an error reply", e.to_string());
    }
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/embed outside a guild"))
}

/// `renderCtxOf(interaction)`, with another channel when given.
fn render_ctx_of(ctx: &CommandCtx, channel: Option<ChannelId>) -> RenderContext {
    let g = ctx.guild_id();
    let guild = g.and_then(|g| cached_guild(&ctx.app, g));
    let channel = g.and_then(|g| cached_channel(&ctx.app, g, channel.unwrap_or(ctx.channel_id())));
    svc::embed_render_context(guild.as_ref(), ctx.member(), Some(ctx.user()), channel.as_ref())
}

/// `nameOrReply(interaction)`: the normalised `name` option, or `None` (the E-NAME reply was sent).
async fn name_or_reply(ctx: &CommandCtx) -> Option<String> {
    let name = svc::normalize_embed_name(&ctx.get_string("name").unwrap_or_default());
    if name.is_none() {
        send_error(&ctx.responder, e_name()).await;
    }
    name
}

/// `replyChanged(interaction, saved, title, text)`: where to apply the change, and unknown variables.
async fn reply_changed(ctx: &CommandCtx, saved: &bot_db::SavedEmbed, title: &str, text: &str) -> anyhow::Result<()> {
    let tpl = normalize_template(&saved.template);
    let note = format!("{}{}", svc::publish_hint_of(saved), svc::unknown_variables_note(&tpl, &render_ctx_of(ctx, None)));
    ctx.edit_reply(EditInteractionResponse::new().embed(success_embed(title, Some(&format!("{text}{note}")), None))).await?;
    Ok(())
}

/// `clampPosition(position, length)`.
fn clamp_position(position: i64, length: usize) -> i64 {
    position.max(1).min(length as i64)
}

async fn defer(ctx: &CommandCtx, ephemeral: bool) -> anyhow::Result<()> {
    ctx.defer_reply(ephemeral).await?;
    Ok(())
}

// ── Subcommands ────────────────────────────────────────────────────────────────

/// `precheck(interaction, promise)`: pre-modal checks must finish inside the 3 s acknowledgement window (`None`: the
/// E-SLOW reply was sent).
async fn precheck<T, F>(ctx: &CommandCtx, fut: F) -> anyhow::Result<Option<T>>
where
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    match bot_util::async_util::with_timeout(fut, ctx.ack_budget_ms(), "embed precheck").await {
        Ok(v) => Ok(Some(v)),
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            send_error(&ctx.responder, e_slow()).await;
            Ok(None)
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => Err(e),
    }
}

async fn create_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(name) = svc::normalize_embed_name(&ctx.get_string("name").unwrap_or_default()) else {
        send_error(&ctx.responder, e_name()).await;
        return Ok(());
    };
    let g = guild_of(ctx)?.to_string();
    let (pool, gid, n) = (ctx.app.db().clone(), g.clone(), name.clone());
    let checked = precheck(ctx, async move {
        let existing = svc::get_saved_embed(&pool, &gid, &n).await?;
        let count = svc::count_saved_embeds(&pool, &gid).await?;
        Ok((existing, count))
    })
    .await?;
    let Some((existing, count)) = checked else { return Ok(()) };
    if existing.is_some() {
        send_error(&ctx.responder, e_taken(&name, "")).await;
        return Ok(());
    }
    if count >= SAVED_EMBEDS_LIMIT as i64 {
        send_error(&ctx.responder, e_limit()).await;
        return Ok(());
    }
    // Nothing is written until the form is submitted: a closed form leaves no row.
    let id = bot_config::custom_id!("embed-modal", "new", name)?;
    let modal = build_template_modal(&id, Some(&format!("New embed: {name}")), &empty_template(), false)?;
    ctx.show_modal(modal).await?;
    Ok(())
}

/// `FORM_LABELS`.
fn form_label(key: &str) -> &str {
    match key {
        "content" => "message text",
        "title" => "title",
        "description" => "description",
        "color" => "colour",
        "footer" => "footer",
        other => other,
    }
}

async fn edit_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(name) = svc::normalize_embed_name(&ctx.get_string("name").unwrap_or_default()) else {
        send_error(&ctx.responder, e_name()).await;
        return Ok(());
    };
    let (pool, gid, n) = (ctx.app.db().clone(), guild_of(ctx)?.to_string(), name.clone());
    let Some(row) = precheck(ctx, async move { svc::get_saved_embed(&pool, &gid, &n).await }).await? else { return Ok(()) };
    let Some(row) = row else {
        send_error(&ctx.responder, e_notfound(&name)).await;
        return Ok(());
    };
    let id = bot_config::custom_id!("embed-modal", row.id)?;
    match build_template_modal(&id, Some(&format!("Edit embed: {}", row.name)), &normalize_template(&row.template), true) {
        Ok(modal) => {
            ctx.show_modal(modal).await?;
        }
        Err(err) => {
            let parts = err.fields.iter().map(|f| form_label(f)).collect::<Vec<_>>().join(", ");
            let text = format!(
                "This embed has a field longer than the form allows ({parts}). Edit it with /embed field or /embed author instead, or use `/embed export`, edit the JSON and `/embed import` with `overwrite:true`."
            );
            send_error(&ctx.responder, error_embed("Too Long For The Form", Some(&text), None)).await;
        }
    }
    Ok(())
}

fn fields_len(tpl: &Value) -> usize {
    tpl["embed"].get("fields").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
}

async fn field_add(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let position = ctx.get_integer("position");
    let title = ctx.get_string("title").unwrap_or_default();
    let value = ctx.get_string("value").unwrap_or_default();
    let inline = ctx.get_boolean("inline").unwrap_or(false);
    let (saved, pos) = svc::mutate_saved_embed_with(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| {
        let n = fields_len(tpl);
        let pos = position.map(|p| clamp_position(p, n + 1)).unwrap_or(n as i64 + 1);
        Ok((svc::add_field(tpl, &title, &value, inline, position)?, pos))
    })
    .await?;
    reply_changed(ctx, &saved, "Field Added", &format!("Added field {pos} to **{name}**.")).await
}

async fn field_edit(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let index = ctx.get_integer("index").unwrap_or(0);
    let edit = FieldEdit { title: ctx.get_string("title"), value: ctx.get_string("value"), inline: ctx.get_boolean("inline"), position: ctx.get_integer("position") };
    if edit == FieldEdit::default() {
        send_error(&ctx.responder, e_nothing()).await;
        return Ok(());
    }
    let (saved, new_pos) = svc::mutate_saved_embed_with(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| {
        let n = fields_len(tpl);
        let new_pos = edit.position.map(|p| clamp_position(p, n)).unwrap_or(index);
        Ok((svc::edit_field(tpl, index, &edit)?, new_pos))
    })
    .await?;
    reply_changed(ctx, &saved, "Field Updated", &format!("Updated field {new_pos} of **{name}**.")).await
}

async fn field_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let index = ctx.get_integer("index").unwrap_or(0);
    let (saved, old_title) = svc::mutate_saved_embed_with(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| {
        let old = usize::try_from(index - 1)
            .ok()
            .and_then(|i| tpl["embed"].get("fields").and_then(|f| f.get(i)))
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok((svc::remove_field(tpl, index)?, old))
    })
    .await?;
    reply_changed(ctx, &saved, "Field Removed", &format!("Removed field {index} (\"{}\") from **{name}**.", clamp_text(&old_title, 60))).await
}

/// The `/embed field list` text.
pub fn field_list_body(tpl: &Value) -> String {
    let fields = tpl["embed"].get("fields").and_then(Value::as_array).cloned().unwrap_or_default();
    if fields.is_empty() {
        return "This embed has no fields. Add one with `/embed field add`.".into();
    }
    let lines: Vec<String> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let inline = if f["inline"].as_bool().unwrap_or(false) { " · inline" } else { "" };
            format!(
                "**{}.** {}{inline}\n{}",
                i + 1,
                clamp_text(f["name"].as_str().unwrap_or(""), 100),
                clamp_text(f["value"].as_str().unwrap_or(""), 150)
            )
        })
        .collect();
    clamp_text(&lines.join("\n\n"), 4096)
}

async fn field_list(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let g = guild_of(ctx)?.to_string();
    let row = svc::require_saved_embed(ctx.app.db(), &g, &name).await?;
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let tpl = normalize_template(&row.template);
    let footer = format!("{}/{} fields", fields_len(&tpl), tpl_limits::FIELDS);
    let embed = guild_embed(settings.as_deref(), Some(&format!("Fields of {name}")), Some(&field_list_body(&tpl)), Some(&footer));
    ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await?;
    Ok(())
}

/// `client.emojis.cache.has(id)`: whether a custom emoji is in a guild the bot is in.
fn bot_knows_emoji(app: &App, id: &str) -> bool {
    let (Some(d), Some(id)) = (app.discord(), id.parse::<u64>().ok().filter(|v| *v != 0)) else { return false };
    let id = EmojiId::new(id);
    d.cache.guilds().into_iter().any(|g| d.cache.guild(g).is_some_and(|guild| guild.emojis.contains_key(&id)))
}

async fn button_add(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let emoji = match ctx.get_string("emoji") {
        None => None,
        Some(raw) => Some(svc::parse_button_emoji(&raw, |id| bot_knows_emoji(&ctx.app, id))?),
    };
    let position = ctx.get_integer("position");
    let label = ctx.get_string("label").unwrap_or_default();
    let url = ctx.get_string("url").unwrap_or_default();
    let (saved, pos) = svc::mutate_saved_embed_with(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| {
        let n = tpl["buttons"].as_array().map(Vec::len).unwrap_or(0);
        let pos = position.map(|p| clamp_position(p, n + 1)).unwrap_or(n as i64 + 1);
        Ok((svc::add_button(tpl, &label, &url, emoji.as_deref(), position)?, pos))
    })
    .await?;
    reply_changed(ctx, &saved, "Button Added", &format!("Added button {pos} to **{name}**.")).await
}

async fn button_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let index = ctx.get_integer("index").unwrap_or(0);
    let saved = svc::mutate_saved_embed(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| svc::remove_button(tpl, index)).await?;
    reply_changed(ctx, &saved, "Button Removed", &format!("Removed button {index} from **{name}**.")).await
}

/// An optional URL option: absent keeps, a clear word clears, anything else is checked.
fn pick(raw: Option<String>, check: impl Fn(&str) -> Result<String, EmbedEditError>) -> Result<Change<String>, EmbedEditError> {
    match raw {
        None => Ok(Change::Keep),
        Some(r) if svc::is_clear_word(&r) => Ok(Change::Clear),
        Some(r) => check(&r).map(Change::Set),
    }
}

async fn set_image(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let (image_raw, thumb_raw) = (ctx.get_string("image_url"), ctx.get_string("thumbnail_url"));
    if image_raw.is_none() && thumb_raw.is_none() {
        send_error(&ctx.responder, e_nothing()).await;
        return Ok(());
    }
    let image = pick(image_raw, svc::check_image_url)?;
    let thumbnail = pick(thumb_raw, svc::check_image_url)?;
    let saved = svc::mutate_saved_embed(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| Ok(svc::set_images(tpl, &image, &thumbnail))).await?;
    reply_changed(ctx, &saved, "Images Updated", &format!("Updated the images of **{name}**.")).await
}

async fn set_author_line(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let (text, icon_raw, url_raw) = (ctx.get_string("text"), ctx.get_string("icon_url"), ctx.get_string("url"));
    if text.is_none() && icon_raw.is_none() && url_raw.is_none() {
        send_error(&ctx.responder, e_nothing()).await;
        return Ok(());
    }
    let icon = pick(icon_raw, svc::check_image_url)?;
    let url = pick(url_raw, |s| svc::check_link_url(s, 2048))?;
    let removed = text.as_deref().is_some_and(svc::is_clear_word);
    let t = text.clone();
    let saved = svc::mutate_saved_embed(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| svc::set_author(tpl, t.as_deref(), &icon, &url)).await?;
    let (title, body) =
        if removed { ("Author Removed", format!("Removed the author line of **{name}**.")) } else { ("Author Updated", format!("Updated the author line of **{name}**.")) };
    reply_changed(ctx, &saved, title, &body).await
}

async fn set_footer_line(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let (text, icon_raw, timestamp) = (ctx.get_string("text"), ctx.get_string("icon_url"), ctx.get_boolean("timestamp"));
    if text.is_none() && icon_raw.is_none() && timestamp.is_none() {
        send_error(&ctx.responder, e_nothing()).await;
        return Ok(());
    }
    let icon = pick(icon_raw, svc::check_image_url)?;
    let removed = text.as_deref().is_some_and(svc::is_clear_word);
    let t = text.clone();
    let saved = svc::mutate_saved_embed(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| svc::set_footer(tpl, t.as_deref(), &icon, timestamp)).await?;
    let stamp = match timestamp {
        None => String::new(),
        Some(on) => format!(" The timestamp is {}.", if on { "on" } else { "off" }),
    };
    let head = if removed { format!("Removed the footer of **{name}**.") } else { format!("Updated the footer of **{name}**.") };
    reply_changed(ctx, &saved, if removed { "Footer Removed" } else { "Footer Updated" }, &format!("{head}{stamp}")).await
}

async fn set_side_color(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let raw = ctx.get_string("color").unwrap_or_default();
    let v = trim(&raw).to_string();
    let clear = svc::is_clear_word(&v) || lower(&v) == "default";
    let color = if clear { None } else { parse_color_str(&v) };
    if !clear && color.is_none() {
        let text = "Use a hex code like #5865F2, or one of: red, green, blue, blurple, yellow, orange, purple, pink, white, black, grey.";
        send_error(&ctx.responder, error_embed("Unknown Colour", Some(text), None)).await;
        return Ok(());
    }
    let saved = svc::mutate_saved_embed(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| Ok(svc::set_color(tpl, color))).await?;
    let what = match color {
        None => "the server default colour".to_string(),
        Some(c) => format_color(&json!(c)),
    };
    reply_changed(ctx, &saved, "Colour Updated", &format!("**{name}** now uses {what}.")).await
}

async fn set_title_link(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let raw = ctx.get_string("url").unwrap_or_default();
    let url = if svc::is_clear_word(&raw) { None } else { Some(svc::check_link_url(&raw, 2048)?) };
    let u = url.clone();
    let saved = svc::mutate_saved_embed(&ctx.app, &guild_of(ctx)?.to_string(), &name, move |tpl| Ok(svc::set_title_url(tpl, u.as_deref()))).await?;
    let has_title = normalize_template(&saved.template)["embed"].get("title").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
    let mut text = match &url {
        Some(u) => format!("The title of **{name}** now links to {}.", clamp_text(u, 200)),
        None => format!("Removed the title link of **{name}**."),
    };
    if url.is_some() && !has_title {
        text.push_str("\nNote: the link only shows when the embed has a title.");
    }
    reply_changed(ctx, &saved, "Title Link Updated", &text).await
}

async fn preview_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, true).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let g = guild_of(ctx)?.to_string();
    let row = svc::require_saved_embed(ctx.app.db(), &g, &name).await?;
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let tpl = normalize_template(&row.template);
    let render_ctx = render_ctx_of(ctx, None);
    let Some(rendered) = svc::render_saved_embed(&tpl, &render_ctx, settings.as_deref(), &MentionPolicy::none()) else {
        send_error(&ctx.responder, e_empty()).await;
        return Ok(());
    };
    ctx.edit_reply(rendered.to_payload().to_edit()).await?;
    let unknown = svc::collect_unknown_tokens(&tpl, &render_ctx);
    if !unknown.is_empty() {
        let text = format!(
            "These variables are shown as typed: {}\n\n{}",
            unknown.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>().join(", "),
            clamp_text(&placeholder_help(&["server", "channel"]), 1500)
        );
        let follow = CreateInteractionResponseFollowup::new().embed(warn_embed("Unknown Variables", Some(&text), None)).ephemeral(true);
        if let Err(e) = ctx.follow_up(follow).await
            && !is_interaction_gone(&e)
        {
            bot_util::log_warn!("embeds: unknown-variables follow-up failed", e.to_string());
        }
    }
    Ok(())
}

/// `rolesNote(refused, channel)`.
pub fn roles_note(refused: &[String], channel_id: &str) -> String {
    if refused.is_empty() {
        return String::new();
    }
    format!(
        "\n⚠️ These roles will not be pinged (not mentionable, and you lack **Mention @everyone, @here and All Roles** in <#{channel_id}>): {}",
        refused.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(", ")
    )
}

async fn publish_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let row = svc::require_saved_embed(ctx.app.db(), &g, &name).await?;
    let channel_id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let channel = format!("<#{channel_id}>");
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    match svc::publish_saved_embed(&ctx.app, &row, guild_id, channel_id, ctx.member(), settings).await? {
        PublishOutcome::Posted { message, previous, refused } => {
            let prev = match previous {
                Some((c, _)) => format!("\nThe previous copy in <#{c}> is no longer tracked; it will not change on /embed update."),
                None => String::new(),
            };
            let tpl = normalize_template(&row.template);
            let text = format!(
                "Posted **{name}** in {channel}. [Jump to message]({}){prev}{}{}",
                message.url,
                roles_note(&refused, &channel_id.to_string()),
                svc::unknown_variables_note(&tpl, &render_ctx_of(ctx, Some(channel_id)))
            );
            ctx.edit_reply(EditInteractionResponse::new().embed(success_embed("Embed Published", Some(&text), None))).await?;
        }
        PublishOutcome::MemberNoAccess => {
            let text = format!("You need **View Channel** and **Send Messages** in {channel} to post there.");
            send_error(&ctx.responder, error_embed("No Access", Some(&text), None)).await;
        }
        PublishOutcome::Channel { missing, reason } => {
            let text = if !missing.is_empty() {
                format!("I need **{}** in {channel} to post there.", permission_names(&missing).join(", "))
            } else {
                reason.unwrap_or_else(|| format!("I can't post in {channel}."))
            };
            send_error(&ctx.responder, error_embed("Missing Permissions", Some(&text), None)).await;
        }
        PublishOutcome::Empty => send_error(&ctx.responder, e_empty()).await,
        PublishOutcome::Discord { error } => send_error(&ctx.responder, e_discord(Some(&error.message))).await,
    }
    Ok(())
}

async fn update_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let row = svc::require_saved_embed(ctx.app.db(), &g, &name).await?;
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let channel_id = row.channel_id.clone().unwrap_or_else(|| "null".into());
    let r = &ctx.responder;
    match svc::update_published_embed(&ctx.app, &row, guild_id, ctx.member(), settings).await? {
        UpdateOutcome::Updated { message, channel_id: c } => {
            let text = format!("Updated the published **{name}** in <#{c}>. [Jump]({})", message.url);
            ctx.edit_reply(EditInteractionResponse::new().embed(success_embed("Embed Updated", Some(&text), None))).await?;
        }
        UpdateOutcome::MemberNoAccess => {
            let text = format!("You need **View Channel** in <#{channel_id}> to edit the message published there.");
            send_error(r, error_embed("No Access", Some(&text), None)).await;
        }
        UpdateOutcome::NotPublished { row: fresh } => {
            let was = match fresh.channel_id.as_deref().filter(|c| !c.is_empty()) {
                Some(c) => format!(" (It was last posted in <#{c}>, but that message was deleted.)"),
                None => String::new(),
            };
            let text = format!("**{name}** isn't published. Post it with `/embed publish`.{was}");
            send_error(r, warn_embed("Not Published", Some(&text), None)).await;
        }
        UpdateOutcome::ChannelGone => {
            let text = format!("The channel **{name}** was posted in no longer exists, so it is a draft again. Post it with `/embed publish`.");
            send_error(r, warn_embed("Channel Deleted", Some(&text), None)).await;
        }
        UpdateOutcome::MessageGone => {
            let text = format!("The published message was deleted, so **{name}** is a draft again. Post it with `/embed publish`.");
            send_error(r, warn_embed("Message Deleted", Some(&text), None)).await;
        }
        UpdateOutcome::NoAccess { missing } => {
            let names = if missing.is_empty() { "View Channel, Read Message History, Embed Links".to_string() } else { permission_names(&missing).join(", ") };
            let text = format!("I need **{names}** in <#{channel_id}> to edit the message.");
            send_error(r, error_embed("Missing Permissions", Some(&text), None)).await;
        }
        UpdateOutcome::Archived => {
            send_error(r, error_embed("Thread Archived", Some("That thread is archived or locked. Unarchive it and try again."), None)).await;
        }
        UpdateOutcome::Empty => send_error(r, e_empty()).await,
        UpdateOutcome::Unavailable => {
            let text = "Discord reports this server as temporarily unavailable. Please try again in a moment.";
            send_error(r, error_embed("Server Unavailable", Some(text), None)).await;
        }
        UpdateOutcome::Discord { error } => send_error(r, e_discord(Some(&error.message))).await,
    }
    Ok(())
}

/// One `/embed list` line.
pub fn list_line(guild_id: &str, r: &svc::ListedEmbed) -> String {
    let sec = r.updated_at.timestamp_millis().div_euclid(1000);
    match (r.message_id.as_deref().filter(|m| !m.is_empty()), r.channel_id.as_deref().filter(|c| !c.is_empty())) {
        (Some(m), Some(c)) => format!("`{}` · 📤 <#{c}> [jump](https://discord.com/channels/{guild_id}/{c}/{m}) · edited <t:{sec}:R>", r.name),
        _ => format!("`{}` · 📝 draft · edited <t:{sec}:R>", r.name),
    }
}

async fn list_embeds(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let g = guild_of(ctx)?.to_string();
    let rows = svc::list_saved_embeds(ctx.app.db(), &g).await?;
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    if rows.is_empty() {
        let text = "Create one with `/embed create`, or post the Welcome Channel presets with `/embed welcome-channel`.";
        ctx.edit_reply(EditInteractionResponse::new().embed(info_embed("No Saved Embeds", Some(text), None))).await?;
        return Ok(());
    }
    let lines: Vec<String> = rows.iter().map(|r| list_line(&g, r)).collect();
    let title = format!("🧱 Saved embeds ({}/{SAVED_EMBEDS_LIMIT})", rows.len());
    let pages: Vec<CreateEmbed> = lines.chunks(15).map(|chunk| guild_embed(settings.as_deref(), Some(&title), Some(&chunk.join("\n")), None)).collect();
    paginate(ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}

async fn delete_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(name) = svc::normalize_embed_name(&ctx.get_string("name").unwrap_or_default()) else {
        send_error(&ctx.responder, e_name()).await;
        return Ok(());
    };
    let guild_id = guild_of(ctx)?;
    let (pool, gid, n) = (ctx.app.db().clone(), guild_id.to_string(), name.clone());
    let Some(row) = precheck(ctx, async move { svc::get_saved_embed(&pool, &gid, &n).await }).await? else { return Ok(()) };
    let Some(row) = row else {
        send_error(&ctx.responder, e_notfound(&name)).await;
        return Ok(());
    };
    // A published embed lives in a channel: removing its record (and maybe the message) needs access to that channel.
    let place = row.channel_id.as_deref().and_then(|c| c.parse::<u64>().ok()).filter(|v| *v != 0).and_then(|c| cached_channel(&ctx.app, guild_id, ChannelId::new(c)));
    if let Some(problem) = svc::member_problem(&ctx.app, guild_id, place.as_ref(), ctx.member(), &["ViewChannel"]) {
        let text = format!("**{name}** is published in <#{}>. {problem}", row.channel_id.clone().unwrap_or_default());
        send_error(&ctx.responder, error_embed("No Access", Some(&text), None)).await;
        return Ok(());
    }
    let delete_message = ctx.get_boolean("delete_message").unwrap_or(false);
    let consequence = match (row.message_id.as_deref().is_some_and(|m| !m.is_empty()), delete_message) {
        (false, _) => "",
        (true, true) => " The published message will be deleted too.",
        (true, false) => " The published message stays but will no longer be tracked.",
    };
    let opts = ConfirmOptions {
        title: Some("Delete embed?".into()),
        description: Some(format!("Delete **{name}**?{consequence}")),
        confirm_label: "Delete".into(),
        ephemeral: true,
        ..ConfirmOptions::default()
    };
    if !await_confirm(ctx, opts).await {
        return Ok(()); // awaitConfirm already showed the outcome
    }
    let result = svc::delete_saved_embed_and_message(&ctx.app, guild_id, &row, delete_message).await?;
    if !result.deleted {
        ctx.follow_up(CreateInteractionResponseFollowup::new().embed(e_notfound(&name)).ephemeral(true)).await?;
        return Ok(());
    }
    let note = if result.message_deleted {
        " The published message was deleted too."
    } else if delete_message && row.message_id.as_deref().is_some_and(|m| !m.is_empty()) {
        " (I could not delete the published message.)"
    } else {
        ""
    };
    let embed = success_embed("Embed Deleted", Some(&format!("Deleted **{name}**.{note}")), None);
    ctx.follow_up(CreateInteractionResponseFollowup::new().embed(embed)).await?;
    Ok(())
}

/// The `/embed export` file name.
pub fn export_file_name(name: &str) -> String {
    format!("embed-{name}.json")
}

async fn export_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let row = svc::require_saved_embed(ctx.app.db(), &guild_of(ctx)?.to_string(), &name).await?;
    let json = template_to_json(&normalize_template(&row.template));
    let text = format!("Here is **{name}** as JSON. Re-import it with `/embed import`.");
    let edit = EditInteractionResponse::new()
        .embed(success_embed("Embed Exported", Some(&text), None))
        .new_attachment(CreateAttachment::bytes(json.into_bytes(), export_file_name(&name)));
    ctx.edit_reply(edit).await?;
    Ok(())
}

async fn import_embed(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let Some(name) = name_or_reply(ctx).await else { return Ok(()) };
    let attachment = ctx.get_attachment("file");
    let pasted = ctx.get_string("json");
    if attachment.is_none() && pasted.is_none() {
        send_error(&ctx.responder, error_embed("Nothing To Import", Some("Attach a .json file or paste JSON into the json option."), None)).await;
        return Ok(());
    }
    if attachment.is_some() && pasted.is_some() {
        send_error(&ctx.responder, error_embed("Pick One", Some("Use either file or json, not both."), None)).await;
        return Ok(());
    }
    let text = match attachment {
        Some(a) => {
            if a.size as usize > IMPORT_MAX_BYTES {
                send_error(&ctx.responder, error_embed("File Too Large", Some("The file must be 20 KB or smaller."), None)).await;
                return Ok(());
            }
            let opts = bot_util::safe_fetch::FetchOptions { timeout_ms: 8000, max_bytes: IMPORT_MAX_BYTES as u64, ..Default::default() };
            match bot_util::safe_fetch::safe_fetch_text(&a.url, &opts).await {
                Ok(t) => t,
                Err(e) => {
                    bot_util::log_warn!(format!("embeds: import download failed ({})", e.code));
                    send_error(&ctx.responder, error_embed("Download Failed", Some("I could not download that file. Please try again."), None)).await;
                    return Ok(());
                }
            }
        }
        None => pasted.unwrap_or_default(),
    };
    let parsed = svc::parse_import_payload(&text);
    let Some(template) = parsed.template else {
        let list = parsed.errors.iter().take(10).map(|e| format!("• {e}")).collect::<Vec<_>>().join("\n");
        send_error(&ctx.responder, error_embed("Invalid JSON", Some(&list), None)).await;
        return Ok(());
    };
    let g = guild_of(ctx)?.to_string();
    let existing = svc::get_saved_embed(ctx.app.db(), &g, &name).await?;
    let (saved, title) = match existing {
        Some(row) => {
            if !ctx.get_boolean("overwrite").unwrap_or(false) {
                send_error(&ctx.responder, e_taken(&name, " Add overwrite:true to replace it.")).await;
                return Ok(());
            }
            (svc::replace_template(&ctx.app, &g, &row.id, template).await?, "Embed Replaced")
        }
        None => (svc::create_saved_embed(&ctx.app, &g, &name, &template, &ctx.user().id.to_string()).await?, "Embed Imported"),
    };
    let extra: String = parsed.warnings.iter().map(|w| format!("\n⚠️ {w}")).collect();
    let verb = if title == "Embed Imported" { "Imported" } else { "Replaced" };
    reply_changed(ctx, &saved, title, &format!("{verb} **{name}**.{extra}")).await
}

fn quoted(names: &[String]) -> String {
    names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
}

/// The `/embed welcome-channel` success text.
pub fn welcome_ready_text(
    channel_id: &str,
    created_channel: bool,
    created_presets: &[String],
    reused_presets: &[String],
    invite: Option<&svc::InviteInfo>,
    tip: Option<&str>,
) -> String {
    let created = if created_presets.is_empty() { String::new() } else { format!("Created: {}.", quoted(created_presets)) };
    let reused = if reused_presets.is_empty() { String::new() } else { format!("Reused (not overwritten): {}.", quoted(reused_presets)) };
    let invite = match invite.map(|i| i.source) {
        Some("vanity") => "\nThe links embed uses your vanity invite.".to_string(),
        Some("created") => "\nI created a permanent invite for the links embed.".to_string(),
        Some(_) => "\nI could not add an invite button (no vanity URL, and I lack **Create Invite**); add one with `/embed button add name:welcome-links`.".to_string(),
        None => String::new(),
    };
    let both: Vec<&str> = [created.as_str(), reused.as_str()].into_iter().filter(|s| !s.is_empty()).collect();
    format!(
        "Posted the greeting, rules, links and staff embeds in <#{channel_id}>.{}\nEdit them like any saved embed (for example `/embed edit name:welcome-rules` or `/embed button add name:welcome-links`), then run `/embed update` to refresh the posts.{}{invite}{}",
        if created_channel { " I created the channel; members can read but not write there." } else { "" },
        if both.is_empty() { String::new() } else { format!("\n{}", both.join(" ")) },
        tip.map(|t| format!("\n{t}")).unwrap_or_default(),
    )
}

async fn welcome_channel(ctx: &CommandCtx) -> anyhow::Result<()> {
    defer(ctx, false).await?;
    let guild_id = guild_of(ctx)?;
    let settings = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    let args = WelcomeArgs {
        guild_id,
        user: ctx.user().clone(),
        member: ctx.member().cloned(),
        interaction_channel_id: ctx.channel_id(),
        create: ctx.get_boolean("create").unwrap_or(true),
        channel_id: ctx.get_channel_id("channel"),
        settings,
    };
    let r = &ctx.responder;
    match svc::set_up_welcome_channel(&ctx.app, args).await? {
        WelcomeOutcome::Ready { channel_id, created_channel, created_presets, reused_presets, tip, invite, .. } => {
            let text = welcome_ready_text(&channel_id, created_channel, &created_presets, &reused_presets, invite.as_ref(), tip.as_deref());
            ctx.edit_reply(EditInteractionResponse::new().embed(success_embed("Welcome Channel Ready", Some(&text), None))).await?;
        }
        WelcomeOutcome::Limit { needed, count } => {
            let text = format!("The Welcome Channel needs {needed} free embed slots; this server has {count}/{SAVED_EMBEDS_LIMIT}.");
            send_error(r, error_embed("Limit Reached", Some(&text), None)).await;
        }
        WelcomeOutcome::Channel { channel_id, missing, reason } => {
            let text = if !missing.is_empty() {
                format!("I need **{}** in <#{channel_id}> to post there.", permission_names(&missing).join(", "))
            } else {
                reason.unwrap_or_else(|| format!("I can't post in <#{channel_id}>."))
            };
            send_error(r, error_embed("Missing Permissions", Some(&text), None)).await;
        }
        WelcomeOutcome::Already { channel_id } => {
            let text = format!(
                "The Welcome Channel is already in <#{channel_id}>. Edit the presets (welcome-greeting, welcome-rules, welcome-links, welcome-team) and run `/embed update`, or run this again with channel: to post them somewhere else."
            );
            send_error(r, warn_embed("Already Set Up", Some(&text), None)).await;
        }
        WelcomeOutcome::BotPerms { missing } => {
            let text = format!("I need **{}** to create the welcome channel.", permission_names(&missing).join(", "));
            send_error(r, error_embed("Missing Permissions", Some(&text), None)).await;
        }
        WelcomeOutcome::TooManyChannels => send_error(r, error_embed("Too Many Channels", Some("This server has reached Discord's channel limit."), None)).await,
        WelcomeOutcome::WrongChannel => send_error(r, error_embed("Wrong Channel", Some("Run this in a text channel, or pick one with channel:."), None)).await,
        WelcomeOutcome::Partial { channel_id, posted, unposted, error } => {
            let done = if posted.is_empty() { "nothing".to_string() } else { quoted(&posted) };
            let why = if error.is_empty() { String::new() } else { format!(" ({})", clamp_text(&error, 200)) };
            let text = format!(
                "I posted {done} in <#{channel_id}>, then Discord refused the next one{why}. Not posted: {}. Fix the problem and run this again to post them.",
                quoted(&unposted)
            );
            send_error(r, error_embed("Partly Done", Some(&text), None)).await;
        }
        WelcomeOutcome::Discord { error } => send_error(r, e_discord(Some(&error.message))).await,
        WelcomeOutcome::Failed(msg) => anyhow::bail!(msg),
    }
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let key = match ctx.subcommand_group() {
        Some(g) => format!("{g} {sub}"),
        None => sub,
    };
    let result = match key.as_str() {
        "create" => create_embed(&ctx).await,
        "edit" => edit_embed(&ctx).await,
        "field add" => field_add(&ctx).await,
        "field edit" => field_edit(&ctx).await,
        "field remove" => field_remove(&ctx).await,
        "field list" => field_list(&ctx).await,
        "button add" => button_add(&ctx).await,
        "button remove" => button_remove(&ctx).await,
        "image" => set_image(&ctx).await,
        "author" => set_author_line(&ctx).await,
        "footer" => set_footer_line(&ctx).await,
        "color" => set_side_color(&ctx).await,
        "title-url" => set_title_link(&ctx).await,
        "preview" => preview_embed(&ctx).await,
        "publish" => publish_embed(&ctx).await,
        "update" => update_embed(&ctx).await,
        "list" => list_embeds(&ctx).await,
        "delete" => delete_embed(&ctx).await,
        "export" => export_embed(&ctx).await,
        "import" => import_embed(&ctx).await,
        "welcome-channel" => welcome_channel(&ctx).await,
        _ => return Ok(()),
    };
    let Err(err) = result else { return Ok(()) };
    if let Some(e) = err.downcast_ref::<InteractionError>()
        && is_interaction_gone(e)
    {
        bot_util::log_debug!(format!("embeds: {key}: interaction gone ({})", e.code().map(|c| c.to_string()).unwrap_or_else(|| "undefined".into())));
        return Ok(());
    }
    if let Some(e) = err.downcast_ref::<EmbedEditError>() {
        send_error(&ctx.responder, embed_for_error(e)).await;
        return Ok(());
    }
    bot_util::log_error!(format!("embeds: {key} failed"), bot_core::error::log_arg(&err));
    send_error(&ctx.responder, e_db()).await;
    Ok(())
}

/// A leaf option's text in an autocomplete (`interaction.options.getString(name)`).
fn autocomplete_string(ctx: &AutocompleteCtx, name: &str) -> Option<String> {
    use serenity::all::CommandDataOptionValue as V;
    bot_core::interaction::leaf_options(&ctx.interaction.data.options).iter().find(|o| o.name == name).and_then(|o| match &o.value {
        V::String(s) => Some(s.clone()),
        V::Autocomplete { value, .. } => Some(value.clone()),
        _ => None,
    })
}

/// The `index` choices: `n. title` for fields, `n. emoji label — url` for buttons, filtered by the typed text.
pub fn index_choices(tpl: &Value, group: Option<&str>, typed: &str) -> Vec<Choice> {
    let items: Vec<String> = if group == Some("button") {
        tpl["buttons"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, b)| {
                let emoji = b["emoji"].as_str().filter(|e| !e.is_empty()).map(|e| format!("{e} ")).unwrap_or_default();
                format!("{}. {emoji}{} — {}", i + 1, b["label"].as_str().unwrap_or(""), b["url"].as_str().unwrap_or(""))
            })
            .collect()
    } else {
        tpl["embed"]
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, f)| format!("{}. {}", i + 1, f["name"].as_str().unwrap_or("")))
            .collect()
    };
    let q = lower(trim(typed));
    items
        .into_iter()
        .enumerate()
        .filter(|(_, label)| q.is_empty() || lower(label).contains(&q))
        .take(25)
        .map(|(i, label)| Choice::new(label, json!(i + 1)))
        .collect()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (focused, typed) = ctx.focused().unwrap_or_default();
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let choices: Vec<Choice> = match focused.as_str() {
        "name" => svc::search_saved_embeds(ctx.app.db(), &g, &typed, 25)
            .await
            .into_iter()
            .map(|r| {
                let label = if r.message_id.as_deref().is_some_and(|m| !m.is_empty()) { format!("{} · published", r.name) } else { r.name.clone() };
                Choice::new(label, r.name)
            })
            .collect(),
        "index" => match svc::normalize_embed_name(&autocomplete_string(&ctx, "name").unwrap_or_default()) {
            None => Vec::new(),
            Some(n) => {
                let row = svc::get_saved_embed(ctx.app.db(), &g, &n).await.ok().flatten();
                let tpl = normalize_template(&row.map(|r| r.template).unwrap_or(Value::Null));
                index_choices(&tpl, ctx.subcommand_group().as_deref(), &typed)
            }
        },
        _ => Vec::new(),
    };
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
