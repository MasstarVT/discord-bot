//! Port of `src/commands/admin/ticket-panel.js`: `/ticket-panel` (MEE6 Ticketing setup). Thin: option parsing,
//! validation and replies; the rest lives in [`crate::ticket_service`]. Replies are public (admin convention) except
//! `message`, which opens a form and therefore must not defer first.

use std::sync::Arc;

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::custom_embed::{build_template_modal, normalize_template};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, Payload, PermTarget, await_confirm, cached_guild, check_channel_usable, check_channel_usable_for, is_interaction_gone, missing_bot_permissions,
    resolve_channel,
    pingable_role_ids, respond_autocomplete, safe_defer, safe_reply,
};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_db::{GuildSettings, TicketPanel, TicketPanelOption, TicketPanelStyle};
use bot_util::text::{chunk_lines, clamp_text};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::ticket_service::{
    CATEGORY_PERMS, CategoryKind, GuildEmoji, MANAGERS_WARNING, NewOption, NewPanel, OPEN_REQUIRED_PERMS, PanelWithOptions, add_option, bot_has_emoji,
    build_categories_embed, build_panel_details_embed, build_transcripts_embed, categories_text, clamp_template_for_modal, create_panel, default_intro_template,
    default_panel_template, delete_panel, ensure_category, find_options_for_autocomplete, find_panel, find_panels_for_autocomplete, list_panels_view,
    live_managers, load_settings, normalize_panel_name, option_line, panel_error_embed, panel_error_payload, panel_list_line, parse_option_emoji, peek_panel,
    publish_panel, refresh_note, refresh_panel_message, remove_option, render_channel_name, sync_manager_overwrites, template_uses_token, transcripts_text,
    update_managers, update_option, update_panel_fields, validate_name_format,
};

// ── Builder ───────────────────────────────────────────────────────────────────

fn panel_opt() -> Opt {
    Opt::string("panel", "Ticket panel").required(true).autocomplete(true)
}

fn style_opt() -> Opt {
    Opt::string("style", "Open with buttons or a dropdown (default buttons)").choice("Buttons", "BUTTONS").choice("Dropdown", "SELECT")
}

const TEXT_CH: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn color_opt() -> Opt {
    Opt::integer("color", "Button colour (default Blue)").num_choice("Blue", 1).num_choice("Grey", 2).num_choice("Green", 3).num_choice("Red", 4)
}

fn category_opt(name: &str, description: &str) -> Opt {
    Opt::channel(name, description).channel_types(&[channel_type::GUILD_CATEGORY])
}

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("ticket-panel", "Set up ticket panels: options, managers, categories, transcripts (MEE6 Ticketing).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("create", "Create a panel with a default \"Open Ticket\" button.")
                .option(Opt::string("name", "Panel name (unique on this server)").required(true).min_length(1).max_length(50))
                .option(Opt::channel("channel", "Publish it here right away").channel_types(&TEXT_CH))
                .option(new_channel_option("channel", "channel"))
                .option(style_opt())
                .option(Opt::boolean("ask_reason", "Ask for a reason in a form before opening"))
                .option(Opt::integer("max_open", "Open tickets per member on this panel (default 1)").min_value(1).max_value(10))
                .option(Opt::string("name_format", "Channel name, e.g. ticket-{number} or {user.name}-{number}").max_length(90)),
        )
        .option(
            Opt::sub_command("edit", "Change panel settings; with no options, show them.")
                .option(panel_opt())
                .option(Opt::string("name", "New panel name").min_length(1).max_length(50))
                .option(style_opt())
                .option(Opt::boolean("ask_reason", "Ask for a reason in a form before opening"))
                .option(Opt::integer("max_open", "Open tickets per member on this panel").min_value(1).max_value(10))
                .option(Opt::string("name_format", "Channel name format ({number} {user.name} {user.id} {topic})").max_length(90)),
        )
        .option(Opt::sub_command("delete", "Delete a panel and its message (open tickets stay).").option(panel_opt()))
        .option(Opt::sub_command("list", "List the ticket panels on this server."))
        .option(
            Opt::sub_command("publish", "Post or update the panel message in a channel.")
                .option(panel_opt())
                .option(Opt::channel("channel", "Channel to post the panel in").channel_types(&TEXT_CH))
                .option(new_channel_option("channel", "channel")),
        )
        .option(
            Opt::sub_command("message", "Edit the panel message or the ticket intro message in a form.")
                .option(panel_opt())
                .option(Opt::string("type", "Which message (default panel)").choice("Panel message", "panel").choice("Ticket intro", "intro"))
                .option(Opt::string("option", "Intro for one option only (type intro)").autocomplete(true)),
        )
        .option(
            Opt::sub_command("categories", "Set the open/claimed/closed categories; with no options, show them.")
                .option(panel_opt())
                .option(category_opt("open", "Category for new tickets"))
                .option(new_channel_option("open", "category"))
                .option(category_opt("claimed", "Category for claimed tickets"))
                .option(new_channel_option("claimed", "category"))
                .option(category_opt("closed", "Category for closed tickets"))
                .option(new_channel_option("closed", "category"))
                .option(Opt::boolean("create_missing", "Create any category that is not set (MEE6 \"Create one for me\")")),
        )
        .option(
            Opt::sub_command("transcripts", "Where transcripts go when a ticket is deleted; no options = show.")
                .option(panel_opt())
                .option(Opt::channel("channel", "Channel that receives transcripts").channel_types(&TEXT_CH))
                .option(new_channel_option("channel", "channel"))
                .option(Opt::boolean("dm_owner", "Also DM the transcript to the ticket opener"))
                .option(Opt::boolean("disable", "Stop posting transcripts to a channel")),
        )
        .option(
            Opt::sub_command_group("option", "Panel buttons / dropdown options")
                .option(
                    Opt::sub_command("add", "Add a button or dropdown option (max 25).")
                        .option(panel_opt())
                        .option(Opt::string("label", "Label, e.g. Open Ticket").required(true).min_length(1).max_length(80))
                        .option(Opt::string("emoji", "Emoji, e.g. ✉️ or a server emoji").max_length(64))
                        .option(Opt::string("description", "Dropdown description").max_length(100))
                        .option(color_opt())
                        .option(category_opt("category", "Open tickets of this option here"))
                        .option(new_channel_option("category", "category")),
                )
                .option(
                    Opt::sub_command("edit", "Change an option.")
                        .option(panel_opt())
                        .option(Opt::string("option", "Option to change").required(true).autocomplete(true))
                        .option(Opt::string("label", "New label").min_length(1).max_length(80))
                        .option(Opt::string("emoji", "New emoji (\"none\" removes it)").max_length(64))
                        .option(Opt::string("description", "New dropdown description (\"none\" removes it)").max_length(100))
                        .option(color_opt())
                        .option(category_opt("category", "Open tickets of this option here"))
                        .option(new_channel_option("category", "category"))
                        .option(Opt::boolean("clear_category", "Use the panel open category again")),
                )
                .option(
                    Opt::sub_command("remove", "Remove an option (a panel keeps at least one).")
                        .option(panel_opt())
                        .option(Opt::string("option", "Option to remove").required(true).autocomplete(true)),
                )
                .option(Opt::sub_command("list", "List the options of a panel.").option(panel_opt())),
        )
        .option(
            Opt::sub_command_group("managers", "Ticket Manager roles (max 10 per panel)")
                .option(
                    Opt::sub_command("add", "Let a role see and handle this panel's tickets.")
                        .option(panel_opt())
                        .option(Opt::role("role", "Role"))
                        .option(new_role_option("role")),
                )
                .option(Opt::sub_command("remove", "Remove a Ticket Manager role.").option(panel_opt()).option(Opt::role("role", "Role").required(true))),
        )
}

/// The command (`module 'tickets'`, `gated = false`: configuration keeps working while the module is off; ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Tickets), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn style_label(style: TicketPanelStyle) -> &'static str {
    if style == TicketPanelStyle::Select { "Dropdown" } else { "Buttons" }
}

fn yes_no(v: bool) -> &'static str {
    if v { "Yes" } else { "No" }
}

struct Ctx {
    cmd: CommandCtx,
    guild_id: GuildId,
    settings: Option<Arc<GuildSettings>>,
}

impl Ctx {
    fn app(&self) -> &App {
        &self.cmd.app
    }

    fn g(&self) -> String {
        self.guild_id.to_string()
    }

    fn s(&self) -> Option<&GuildSettings> {
        self.settings.as_deref()
    }

    async fn reply(&self, p: Payload) {
        safe_reply(&self.cmd.responder, p).await;
    }

    async fn reply_embed(&self, embed: CreateEmbed) {
        self.reply(Payload::embed(embed).no_mentions()).await;
    }

    /// `cached(guild, picked)`: the picked channel, from the cache, else fetched over REST (and cached), as discord.js
    /// resolves a channel option into a channel object.
    async fn picked(&self, name: &str) -> Option<GuildChannel> {
        let c = self.cmd.get_channel_id(name)?;
        resolve_channel(self.app(), self.guild_id, c).await
    }

    fn picked_id(&self, name: &str) -> Option<ChannelId> {
        self.cmd.get_channel_id(name)
    }

    /// `resolvePanel(ctx)`.
    async fn resolve_panel(&self) -> anyhow::Result<Option<PanelWithOptions>> {
        let r = self.cmd.get_string("panel").unwrap_or_default();
        let p = find_panel(self.app().db(), &self.g(), &r).await?;
        if p.is_none() {
            self.reply(panel_error_payload("PANEL_NOT_FOUND", &Value::Null)).await;
        }
        Ok(p)
    }
}

/// `categoryProblem(category)`: the bot's permissions in a category, as a P18 payload (or `None` when fine).
fn category_problem(app: &App, guild_id: GuildId, category: &GuildChannel) -> Option<Payload> {
    let missing = crate::ticket_service::missing_in_channel(app, guild_id, category, &CATEGORY_PERMS);
    if missing.is_empty() {
        return None;
    }
    Some(panel_error_payload("CATEGORY_PERMS", &json!({ "names": permission_names(&missing).join(", "), "category": category.name })))
}

/// `findOption(panel, ref)`: by id or (case-insensitive) label.
fn find_option<'a>(panel: &'a PanelWithOptions, r: &str) -> Option<&'a TicketPanelOption> {
    let lower = r.to_lowercase();
    panel.options.iter().find(|o| o.id == r || o.label.to_lowercase() == lower)
}

/// `publishChecks(guild, picked)`: the channel, and the server-wide permissions every ticket needs. The channel or the
/// refusal (code and data).
async fn publish_checks(app: &App, guild_id: GuildId, picked: ChannelId) -> Result<GuildChannel, (String, Value)> {
    let channel = resolve_channel(app, guild_id, picked).await;
    let u = match &channel {
        Some(c) => check_channel_usable_for(app, guild_id, c, &["ViewChannel", "SendMessages", "EmbedLinks"]),
        None => check_channel_usable(app, guild_id, Some(picked), &["ViewChannel", "SendMessages", "EmbedLinks"]),
    };
    if !u.ok {
        return Err(("CHANNEL_UNUSABLE".into(), json!({ "missing": u.missing, "reason": u.reason, "id": picked.to_string() })));
    }
    let missing = missing_bot_permissions(app, PermTarget::Guild(guild_id), &OPEN_REQUIRED_PERMS);
    if !missing.is_empty() {
        return Err(("PERMS_SERVER".into(), json!({ "missing": missing })));
    }
    channel.ok_or_else(|| ("CHANNEL_UNUSABLE".into(), json!({ "missing": [], "reason": "Channel not found", "id": picked.to_string() })))
}

fn embed_description(embed: &CreateEmbed) -> String {
    serde_json::to_value(embed).ok().and_then(|v| v.get("description").and_then(Value::as_str).map(str::to_string)).unwrap_or_default()
}

/// `publishWarnings(guild, panel)`: no manager roles, and an open category the bot cannot use.
fn publish_warnings(app: &App, guild_id: GuildId, panel: &TicketPanel) -> Vec<String> {
    let mut lines = Vec::new();
    if live_managers(app, guild_id, panel).is_empty() {
        lines.push(MANAGERS_WARNING.to_string());
    }
    if let Some(open) = panel.open_category_id.as_deref().and_then(|id| crate::ticket_service::channel_cached(app, guild_id, id))
        && !missing_bot_permissions(app, PermTarget::Channel(guild_id, open.id), &CATEGORY_PERMS).is_empty()
    {
        lines.push(format!("⚠️ I can't manage channels in the **{}** category. Tickets would fail to open there.", open.name));
    }
    lines
}

fn guild_emojis(app: &App, guild_id: GuildId) -> Vec<GuildEmoji> {
    cached_guild(app, guild_id)
        .map(|g| g.emojis.values().map(|e| GuildEmoji { id: e.id.to_string(), name: e.name.clone(), animated: e.animated }).collect())
        .unwrap_or_default()
}

fn parse_emoji(app: &App, guild_id: GuildId, input: &str) -> Result<Option<String>, String> {
    parse_option_emoji(input, &|id| bot_has_emoji(app, id), &guild_emojis(app, guild_id))
}

// ── execute ───────────────────────────────────────────────────────────────────

async fn execute(mut cmd: CommandCtx) -> BotResult {
    let group = cmd.subcommand_group();
    let sub = cmd.subcommand().unwrap_or_default();
    if group.is_none() && sub == "message" {
        return message(cmd).await;
    }
    if !safe_defer(&cmd.responder, false).await {
        return Ok(());
    }
    let key = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    let targets = match key.as_str() {
        "publish" => NewOptions { required: &["channel"], private: &[] },
        "transcripts" => NewOptions { required: &[], private: &["channel"] },
        "managers add" => NewOptions { required: &["role"], private: &[] },
        _ => NewOptions::default(),
    };
    if !resolve_new_options(&mut cmd, targets).await {
        return Ok(());
    }
    let guild_id = cmd.guild_id().ok_or_else(|| anyhow::anyhow!("/ticket-panel outside a guild"))?;
    let settings = load_settings(&cmd.app, &guild_id.to_string()).await;
    let ctx = Ctx { cmd, guild_id, settings };
    match key.as_str() {
        "create" => create(&ctx).await,
        "edit" => edit(&ctx).await,
        "delete" => remove(&ctx).await,
        "list" => list(&ctx).await,
        "publish" => publish(&ctx).await,
        "categories" => categories(&ctx).await,
        "transcripts" => transcripts(&ctx).await,
        "option add" => option_add(&ctx).await,
        "option edit" => option_edit(&ctx).await,
        "option remove" => option_remove(&ctx).await,
        "option list" => option_list(&ctx).await,
        "managers add" => managers_change(&ctx, true).await,
        "managers remove" => managers_change(&ctx, false).await,
        _ => Ok(()),
    }
}

// ── create / edit / delete / list / publish ───────────────────────────────────

async fn create(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let raw = ctx.cmd.get_string("name").unwrap_or_default();
    let Some(name) = normalize_panel_name(&raw) else {
        ctx.reply(panel_error_payload("BAD_PANEL_NAME", &Value::Null)).await;
        return Ok(());
    };
    let mut format = None;
    if let Some(input) = ctx.cmd.get_string("name_format") {
        match validate_name_format(&input) {
            Ok(f) => format = Some(f),
            Err(error) => {
                ctx.reply(panel_error_payload("BAD_FORMAT", &json!({ "error": error }))).await;
                return Ok(());
            }
        }
    }
    let mut input = NewPanel::new(&ctx.g(), &name);
    if ctx.cmd.get_string("style").as_deref() == Some("SELECT") {
        input.style = TicketPanelStyle::Select;
    }
    input.ask_reason = ctx.cmd.get_boolean("ask_reason").unwrap_or(false);
    input.max_open_per_user = ctx.cmd.get_integer("max_open").unwrap_or(1) as i32;
    if let Some(f) = format {
        input.channel_name_format = f;
    }
    let panel = match create_panel(app, input).await? {
        Ok(p) => p,
        Err(f) => {
            ctx.reply(panel_error_payload(&f.code, &f.data)).await;
            return Ok(());
        }
    };

    let mut steps = vec![
        format!("Created **{name}** ({}) with an **Open Ticket** ✉️ option.", style_label(panel.panel.style)),
        "Next steps:".to_string(),
        "• `/ticket-panel managers add` — who handles the tickets".to_string(),
        "• `/ticket-panel option add` — more buttons or dropdown options".to_string(),
        "• `/ticket-panel message` — the panel and intro texts".to_string(),
    ];
    if let Some(picked) = ctx.picked_id("channel") {
        let line = match publish_checks(app, ctx.guild_id, picked).await {
            Err((code, data)) => format!(
                "⚠️ The panel was created but not published: {}\n• Publish it later with `/ticket-panel publish`",
                embed_description(&panel_error_embed(&code, &data))
            ),
            Ok(channel) => match publish_panel(app, ctx.guild_id, &panel.panel.id, &channel, ctx.s().cloned()).await {
                Ok(p) => format!("Published it in <#{}>. [Jump to panel]({})", channel.id, p.message().url),
                Err(e) => format!(
                    "⚠️ The panel was created but I couldn't post it in <#{}> ({}).\n• Publish it later with `/ticket-panel publish`",
                    channel.id,
                    e.reason()
                ),
            },
        };
        steps.push(line);
    } else {
        steps.push("• `/ticket-panel publish` — post the panel".into());
    }
    steps.push(MANAGERS_WARNING.into()); // a new panel never has manager roles
    ctx.reply_embed(success_embed("Panel Created", Some(&steps.join("\n")), None)).await;
    Ok(())
}

async fn edit(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let name_in = ctx.cmd.get_string("name");
    let style = ctx.cmd.get_string("style");
    let ask_reason = ctx.cmd.get_boolean("ask_reason");
    let max_open = ctx.cmd.get_integer("max_open");
    let format_in = ctx.cmd.get_string("name_format");
    if name_in.is_none() && style.is_none() && ask_reason.is_none() && max_open.is_none() && format_in.is_none() {
        ctx.reply_embed(build_panel_details_embed(app, ctx.guild_id, &panel.panel, panel.options.len(), ctx.s())).await;
        return Ok(());
    }
    let mut data: Vec<(&str, Value)> = Vec::new();
    let mut lines = Vec::new();
    if let Some(n) = name_in {
        let Some(name) = normalize_panel_name(&n) else {
            ctx.reply(panel_error_payload("BAD_PANEL_NAME", &Value::Null)).await;
            return Ok(());
        };
        lines.push(format!("Name: **{}** → **{name}**", panel.panel.name));
        data.push(("name", json!(name)));
    }
    let new_style = style.as_deref().map(|s| if s == "SELECT" { TicketPanelStyle::Select } else { TicketPanelStyle::Buttons });
    if let Some(s) = new_style {
        data.push(("style", json!(s.as_str())));
        lines.push(format!("Style: **{}**", style_label(s)));
    }
    if let Some(a) = ask_reason {
        data.push(("askReason", json!(a)));
        lines.push(format!("Ask for a reason: **{}**", yes_no(a)));
    }
    if let Some(m) = max_open {
        data.push(("maxOpenPerUser", json!(m)));
        lines.push(format!("Open tickets per member: **{m}**"));
    }
    if let Some(f) = format_in {
        let fmt = match validate_name_format(&f) {
            Ok(v) => v,
            Err(error) => {
                ctx.reply(panel_error_payload("BAD_FORMAT", &json!({ "error": error }))).await;
                return Ok(());
            }
        };
        let guild = cached_guild(app, ctx.guild_id);
        let preview = render_channel_name(
            &fmt,
            1,
            Some(bot_core::adapters::placeholder_user(ctx.cmd.user())),
            ctx.cmd.member().map(|m| bot_core::adapters::placeholder_member(m, guild.as_ref())),
            Some(panel.options.first().map(|o| o.label.as_str()).unwrap_or("")),
        );
        lines.push(format!("Channel name: `{fmt}` — preview `{preview}`"));
        data.push(("channelNameFormat", json!(fmt)));
    }
    if let Err(f) = update_panel_fields(app, &panel.panel.id, &ctx.g(), &data).await? {
        let code = if f.code == "PANEL_NOT_FOUND" { "PANEL_NOT_FOUND" } else { "NAME_TAKEN" };
        ctx.reply(panel_error_payload(code, &f.data)).await;
        return Ok(());
    }
    let refresh = match new_style {
        Some(s) if s != panel.panel.style => Some(refresh_panel_message(app, ctx.guild_id, &panel.panel.id).await),
        _ => None,
    };
    ctx.reply_embed(success_embed("Panel Updated", Some(&format!("{}{}", lines.join("\n"), refresh_note(refresh.as_ref()))), None)).await;
    Ok(())
}

async fn remove(ctx: &Ctx) -> BotResult {
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let name = clamp_text(&panel.panel.name, 80);
    let ok = await_confirm(
        &ctx.cmd,
        ConfirmOptions {
            title: Some("Delete Panel?".into()),
            description: Some(format!("Delete **{name}** and its published message? Open tickets keep their channels.")),
            confirm_label: "Delete".into(),
            ephemeral: false,
            ..Default::default()
        },
    )
    .await;
    if !ok {
        ctx.reply(Payload::embed(info_embed("Cancelled", Some("The panel was not deleted."), None)).components(Vec::new())).await;
        return Ok(());
    }
    let open = delete_panel(ctx.app(), ctx.guild_id, &panel.panel).await?;
    let extra = if open > 0 { format!(" {open} ticket(s) keep their channels; only moderators and admins can manage them now.") } else { String::new() };
    ctx.reply(Payload::embed(success_embed("Panel Deleted", Some(&format!("Deleted **{name}**.{extra}")), None)).components(Vec::new()).no_mentions()).await;
    Ok(())
}

async fn list(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let rows = list_panels_view(app, ctx.guild_id).await?;
    if rows.is_empty() {
        ctx.reply(Payload::embed(info_embed("No Ticket Panels", Some("Create one with `/ticket-panel create`."), None))).await;
        return Ok(());
    }
    let lines: Vec<String> = rows.iter().map(|(p, active)| panel_list_line(app, ctx.guild_id, &p.panel, p.options.len(), *active)).collect();
    let title = format!("🎫 Ticket panels ({}/{})", rows.len(), bot_config::limits::TICKET_PANELS);
    let pages: Vec<CreateEmbed> = chunk_lines(&lines, 4000).iter().map(|chunk| guild_embed(ctx.s(), Some(&title), Some(chunk), None)).collect();
    paginate(&ctx.cmd, &pages, PaginateOptions::default()).await?;
    Ok(())
}

async fn publish(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let picked = ctx.picked_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let channel = match publish_checks(app, ctx.guild_id, picked).await {
        Ok(c) => c,
        Err((code, data)) => {
            ctx.reply(panel_error_payload(&code, &data)).await;
            return Ok(());
        }
    };
    let r = match publish_panel(app, ctx.guild_id, &panel.panel.id, &channel, ctx.s().cloned()).await {
        Ok(r) => r,
        Err(e) => {
            ctx.reply(panel_error_payload("PUBLISH_FAILED", &json!({ "reason": e.reason() }))).await;
            return Ok(());
        }
    };
    let edited = r.edited();
    let mut lines = vec![format!(
        "{} **{}** in <#{}>. [Jump to panel]({})",
        if edited { "Updated" } else { "Posted" },
        clamp_text(&panel.panel.name, 80),
        channel.id,
        r.message().url
    )];
    lines.extend(publish_warnings(app, ctx.guild_id, &panel.panel));
    ctx.reply_embed(success_embed(if edited { "Panel Updated" } else { "Panel Published" }, Some(&lines.join("\n")), None)).await;
    Ok(())
}

// ── message (the template form; no defer: showModal must be the first response) ──

async fn message(cmd: CommandCtx) -> BotResult {
    let deny = |p: Payload| {
        let r = cmd.responder.clone();
        async move {
            safe_reply(&r, p.ephemeral(true)).await;
        }
    };
    let type_ = cmd.get_string("type").unwrap_or_else(|| "panel".into());
    let option_ref = cmd.get_string("option");
    if type_ == "panel" && option_ref.is_some() {
        deny(panel_error_payload("BAD_COMBO", &Value::Null)).await;
        return Ok(());
    }
    let guild_id = cmd.guild_id().ok_or_else(|| anyhow::anyhow!("/ticket-panel outside a guild"))?;
    let g = guild_id.to_string();
    // The lookup shares the 3 s acknowledgement window: memo first, then a bounded read.
    let r = cmd.get_string("panel").unwrap_or_default();
    let budget = cmd.ack_budget_ms();
    let mut panel: Option<Option<PanelWithOptions>> = peek_panel(&cmd.app, &g, &r).map(|p| Some((*p).clone()));
    if panel.is_none() && budget > 0 {
        let (a, gg, rr) = (cmd.app.clone(), g.clone(), r.clone());
        panel = bot_util::async_util::with_timeout(async move { find_panel(a.db(), &gg, &rr).await }, budget, "ticket-panel message lookup").await.ok();
    }
    let Some(panel) = panel else {
        deny(panel_error_payload("SLOW", &Value::Null)).await;
        return Ok(());
    };
    let Some(panel) = panel else {
        deny(panel_error_payload("PANEL_NOT_FOUND", &Value::Null)).await;
        return Ok(());
    };
    let mut option = None;
    if let Some(o) = &option_ref {
        option = find_option(&panel, o).cloned();
        if option.is_none() {
            deny(panel_error_payload("OPTION_NOT_FOUND", &Value::Null)).await;
            return Ok(());
        }
    }
    let is_panel = type_ == "panel";
    let base = if is_panel {
        match &panel.panel.template {
            Some(t) if !t.is_null() => normalize_template(t),
            _ => default_panel_template(panel.panel.style == TicketPanelStyle::Select),
        }
    } else {
        let src = option
            .as_ref()
            .and_then(|o| o.intro_template.clone())
            .filter(|v| !v.is_null())
            .or_else(|| panel.panel.intro_template.clone().filter(|v| !v.is_null()))
            .unwrap_or_else(default_intro_template);
        normalize_template(&src)
    };
    let custom_id = if is_panel {
        bot_config::custom_id!("ticket-modal", "tpl", panel.panel.id, "panel")?
    } else {
        match &option {
            Some(o) => bot_config::custom_id!("ticket-modal", "tpl", panel.panel.id, "intro", o.id)?,
            None => bot_config::custom_id!("ticket-modal", "tpl", panel.panel.id, "intro")?,
        }
    };
    let title = if is_panel {
        format!("Panel message: {}", panel.panel.name)
    } else {
        match &option {
            Some(o) => format!("Intro: {}", o.label),
            None => format!("Ticket intro: {}", panel.panel.name),
        }
    };
    match build_template_modal(&custom_id, Some(&clamp_text(&title, 45)), &clamp_template_for_modal(&base), false) {
        Ok(modal) => {
            if let Err(e) = cmd.show_modal(modal).await
                && !is_interaction_gone(&e)
            {
                bot_util::log_error!(format!("ticket-panel message: showModal failed: {e}"));
            }
        }
        Err(e) => bot_util::log_error!(format!("ticket-panel message: showModal failed: {e}")),
    }
    Ok(())
}

// ── categories / transcripts ──────────────────────────────────────────────────

fn set_category(p: &mut TicketPanel, kind: CategoryKind, id: Option<String>) {
    match kind {
        CategoryKind::Open => p.open_category_id = id,
        CategoryKind::Claimed => p.claimed_category_id = id,
        CategoryKind::Closed => p.closed_category_id = id,
    }
}

fn category_of(p: &TicketPanel, kind: CategoryKind) -> Option<&String> {
    match kind {
        CategoryKind::Open => p.open_category_id.as_ref(),
        CategoryKind::Claimed => p.claimed_category_id.as_ref(),
        CategoryKind::Closed => p.closed_category_id.as_ref(),
    }
}

async fn categories(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let mut picked: Vec<(CategoryKind, GuildChannel)> = Vec::new();
    for (name, kind) in [("open", CategoryKind::Open), ("claimed", CategoryKind::Claimed), ("closed", CategoryKind::Closed)] {
        if let Some(c) = ctx.picked(name).await {
            picked.push((kind, c));
        }
    }
    let create_missing = ctx.cmd.get_boolean("create_missing").unwrap_or(false);
    if picked.is_empty() && !create_missing {
        ctx.reply_embed(build_categories_embed(app, ctx.guild_id, &panel.panel, ctx.s())).await;
        return Ok(());
    }
    for (_, c) in &picked {
        if let Some(p) = category_problem(app, ctx.guild_id, c) {
            ctx.reply(p).await;
            return Ok(());
        }
    }
    let mut fresh = panel.panel.clone();
    if !picked.is_empty() {
        let data: Vec<(&str, Value)> = picked.iter().map(|(k, c)| (k.field(), json!(c.id.to_string()))).collect();
        if update_panel_fields(app, &panel.panel.id, &ctx.g(), &data).await?.is_err() {
            ctx.reply(panel_error_payload("PANEL_NOT_FOUND", &Value::Null)).await;
            return Ok(());
        }
        for (k, c) in &picked {
            set_category(&mut fresh, *k, Some(c.id.to_string()));
        }
    }
    let mut extra = Vec::new();
    if create_missing {
        for kind in [CategoryKind::Open, CategoryKind::Claimed, CategoryKind::Closed] {
            if category_of(&fresh, kind).is_some_and(|id| crate::ticket_service::channel_cached(app, ctx.guild_id, id).is_some()) {
                continue;
            }
            let e = ensure_category(app, ctx.guild_id, &panel.panel.id, kind).await;
            match &e.category {
                Some(c) if e.created => extra.push(format!("Created **{}**.", c.name)),
                Some(c) if e.reused => extra.push(format!("Using the existing **{}** category.", c.name)),
                _ => {}
            }
            if let Some(n) = &e.note {
                extra.push(format!("⚠️ {n}"));
            }
        }
        if let Some(p) = find_panel(app.db(), &ctx.g(), &panel.panel.id).await? {
            fresh = p.panel;
        }
    }
    let full: Vec<String> = picked
        .iter()
        .filter(|(_, c)| crate::ticket_service::child_count(app, ctx.guild_id, c.id) >= 50)
        .map(|(_, c)| format!("⚠️ **{}** is full (50 channels). New tickets will be created outside any category until there is room.", c.name))
        .collect();
    let mut lines = vec![categories_text(app, ctx.guild_id, &fresh)];
    lines.extend(extra);
    lines.extend(full);
    lines.push("Existing tickets are not moved.".into());
    ctx.reply_embed(success_embed("Categories Updated", Some(&lines.join("\n")), None)).await;
    Ok(())
}

async fn transcripts(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let picked_id = ctx.picked_id("channel");
    let dm_owner = ctx.cmd.get_boolean("dm_owner");
    let disable = ctx.cmd.get_boolean("disable");
    if picked_id.is_none() && dm_owner.is_none() && disable.is_none() {
        ctx.reply_embed(build_transcripts_embed(&panel.panel, ctx.s())).await;
        return Ok(());
    }
    if picked_id.is_some() && disable == Some(true) {
        ctx.reply(panel_error_payload("CONFLICT_CHANNEL", &Value::Null)).await;
        return Ok(());
    }
    if let Some(c) = picked_id {
        let needs = ["ViewChannel", "SendMessages", "EmbedLinks", "AttachFiles"];
        let u = match resolve_channel(app, ctx.guild_id, c).await {
            Some(ch) => check_channel_usable_for(app, ctx.guild_id, &ch, &needs),
            None => check_channel_usable(app, ctx.guild_id, Some(c), &needs),
        };
        if !u.ok {
            ctx.reply(panel_error_payload("TRANSCRIPT_CHANNEL_UNUSABLE", &json!({ "missing": u.missing, "reason": u.reason, "id": c.to_string() }))).await;
            return Ok(());
        }
    }
    let mut data: Vec<(&str, Value)> = Vec::new();
    let mut channel = panel.panel.transcript_channel_id.clone();
    let mut dm = panel.panel.dm_transcript;
    if let Some(c) = picked_id {
        data.push(("transcriptChannelId", json!(c.to_string())));
        channel = Some(c.to_string());
    }
    if disable == Some(true) {
        data.push(("transcriptChannelId", Value::Null));
        channel = None;
    }
    if let Some(d) = dm_owner {
        data.push(("dmTranscript", json!(d)));
        dm = d;
    }
    if data.is_empty() {
        ctx.reply_embed(build_transcripts_embed(&panel.panel, ctx.s())).await;
        return Ok(());
    }
    if update_panel_fields(app, &panel.panel.id, &ctx.g(), &data).await?.is_err() {
        ctx.reply(panel_error_payload("PANEL_NOT_FOUND", &Value::Null)).await;
        return Ok(());
    }
    ctx.reply_embed(success_embed("Transcripts Updated", Some(&transcripts_text(channel.as_deref(), dm)), None)).await;
    Ok(())
}

// ── option add | edit | remove | list ─────────────────────────────────────────

fn label_ok(label: &str) -> bool {
    let n = bot_util::js::utf16_len(label);
    (1..=80).contains(&n)
}

async fn option_add(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let label = bot_util::js::trim(&ctx.cmd.get_string("label").unwrap_or_default()).to_string();
    if !label_ok(&label) {
        ctx.reply(panel_error_payload("BAD_LABEL", &Value::Null)).await;
        return Ok(());
    }
    let emoji = match parse_emoji(app, ctx.guild_id, &ctx.cmd.get_string("emoji").unwrap_or_default()) {
        Ok(e) => e,
        Err(error) => {
            ctx.reply(panel_error_payload("BAD_EMOJI", &json!({ "error": error }))).await;
            return Ok(());
        }
    };
    let category = ctx.picked("category").await;
    if let Some(c) = &category
        && let Some(p) = category_problem(app, ctx.guild_id, c)
    {
        ctx.reply(p).await;
        return Ok(());
    }
    let description = ctx.cmd.get_string("description").map(|d| bot_util::js::trim(&d).to_string()).filter(|d| !d.is_empty());
    let data = NewOption {
        label: label.clone(),
        emoji: emoji.clone(),
        description,
        button_style: ctx.cmd.get_integer("color").unwrap_or(1) as i32,
        category_id: category.map(|c| c.id.to_string()),
    };
    let (_, count) = match add_option(app, &panel.panel.id, &ctx.g(), data).await? {
        Ok(v) => v,
        Err(f) => {
            ctx.reply(panel_error_payload(&f.code, &f.data)).await;
            return Ok(());
        }
    };
    let refresh = refresh_panel_message(app, ctx.guild_id, &panel.panel.id).await;
    let text = format!(
        "Added {}**{}** to **{}** ({count}/{}).",
        emoji.map(|e| format!("{e} ")).unwrap_or_default(),
        clamp_text(&label, 80),
        clamp_text(&panel.panel.name, 80),
        bot_config::limits::TICKET_PANEL_OPTIONS
    );
    ctx.reply_embed(success_embed("Option Added", Some(&format!("{text}{}", refresh_note(Some(&refresh)))), None)).await;
    Ok(())
}

async fn option_edit(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let Some(option) = find_option(&panel, &ctx.cmd.get_string("option").unwrap_or_default()).cloned() else {
        ctx.reply(panel_error_payload("OPTION_NOT_FOUND", &Value::Null)).await;
        return Ok(());
    };
    let mut data: Vec<(&'static str, Value)> = Vec::new();
    let mut updated = option.clone();
    if let Some(l) = ctx.cmd.get_string("label") {
        let label = bot_util::js::trim(&l).to_string();
        if !label_ok(&label) {
            ctx.reply(panel_error_payload("BAD_LABEL", &Value::Null)).await;
            return Ok(());
        }
        data.push(("label", json!(label)));
        updated.label = label;
    }
    if let Some(input) = ctx.cmd.get_string("emoji") {
        match parse_emoji(app, ctx.guild_id, &input) {
            Ok(e) => {
                data.push(("emoji", json!(e)));
                updated.emoji = e;
            }
            Err(error) => {
                ctx.reply(panel_error_payload("BAD_EMOJI", &json!({ "error": error }))).await;
                return Ok(());
            }
        }
    }
    if let Some(d) = ctx.cmd.get_string("description") {
        let t = bot_util::js::trim(&d);
        let v = if t.to_lowercase() == "none" || t.is_empty() { None } else { Some(t.to_string()) };
        data.push(("description", json!(v)));
        updated.description = v;
    }
    if let Some(c) = ctx.cmd.get_integer("color") {
        data.push(("buttonStyle", json!(c)));
        updated.button_style = c as i32;
    }
    let category = ctx.picked("category").await;
    let clear = ctx.cmd.get_boolean("clear_category");
    if category.is_some() && clear == Some(true) {
        ctx.reply(panel_error_payload("CONFLICT_CATEGORY", &Value::Null)).await;
        return Ok(());
    }
    if let Some(c) = &category {
        if let Some(p) = category_problem(app, ctx.guild_id, c) {
            ctx.reply(p).await;
            return Ok(());
        }
        data.push(("categoryId", json!(c.id.to_string())));
        updated.category_id = Some(c.id.to_string());
    }
    if clear == Some(true) {
        data.push(("categoryId", Value::Null));
        updated.category_id = None;
    }
    let index = panel.options.iter().position(|x| x.id == option.id).unwrap_or(0);
    if data.is_empty() {
        let embed = guild_embed(ctx.s(), Some(&format!("🎫 {} — option", clamp_text(&panel.panel.name, 100))), Some(&option_line(&option, index, panel.panel.style)), None);
        ctx.reply_embed(embed).await;
        return Ok(());
    }
    if let Err(f) = update_option(app, &panel.panel.id, &ctx.g(), &option.id, data).await? {
        ctx.reply(panel_error_payload(&f.code, &f.data)).await;
        return Ok(());
    }
    let refresh = refresh_panel_message(app, ctx.guild_id, &panel.panel.id).await;
    ctx.reply_embed(success_embed("Option Updated", Some(&format!("{}{}", option_line(&updated, index, panel.panel.style), refresh_note(Some(&refresh)))), None)).await;
    Ok(())
}

async fn option_remove(ctx: &Ctx) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let Some(option) = find_option(&panel, &ctx.cmd.get_string("option").unwrap_or_default()).cloned() else {
        ctx.reply(panel_error_payload("OPTION_NOT_FOUND", &Value::Null)).await;
        return Ok(());
    };
    if let Err(f) = remove_option(app, &panel.panel.id, &ctx.g(), &option.id).await? {
        ctx.reply(panel_error_payload(&f.code, &f.data)).await;
        return Ok(());
    }
    let refresh = refresh_panel_message(app, ctx.guild_id, &panel.panel.id).await;
    ctx.reply_embed(success_embed("Option Removed", Some(&format!("Removed **{}**.{}", clamp_text(&option.label, 80), refresh_note(Some(&refresh)))), None)).await;
    Ok(())
}

async fn option_list(ctx: &Ctx) -> BotResult {
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let lines: Vec<String> = panel.options.iter().enumerate().map(|(i, o)| option_line(o, i, panel.panel.style)).collect();
    let title = format!("🎫 {} — options ({}/{})", clamp_text(&panel.panel.name, 100), panel.options.len(), bot_config::limits::TICKET_PANEL_OPTIONS);
    ctx.reply_embed(guild_embed(ctx.s(), Some(&title), Some(&lines.join("\n")), None)).await;
    Ok(())
}

// ── managers add | remove ─────────────────────────────────────────────────────

async fn managers_change(ctx: &Ctx, add: bool) -> BotResult {
    let app = ctx.app();
    let Some(panel) = ctx.resolve_panel().await? else { return Ok(()) };
    let role_id = ctx.cmd.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?.to_string();
    if add && role_id == ctx.g() {
        ctx.reply(panel_error_payload("EVERYONE_ROLE", &Value::Null)).await;
        return Ok(());
    }
    // Configuration time: when an intro pings {managers}, the new role must be pingable by the admin.
    if add {
        let intros: Vec<&Value> =
            std::iter::once(panel.panel.intro_template.as_ref()).chain(panel.options.iter().map(|o| o.intro_template.as_ref())).flatten().filter(|v| !v.is_null()).collect();
        let guild = cached_guild(app, ctx.guild_id);
        // `guild.channels.cache.get(interaction.channelId) ?? interaction.channel`.
        let where_ = match guild.as_ref().and_then(|g| g.channels.get(&ctx.cmd.channel_id()).cloned()) {
            Some(c) => Some(c),
            None => resolve_channel(app, ctx.guild_id, ctx.cmd.channel_id()).await,
        };
        if let (Some(g), Some(w)) = (&guild, &where_)
            && intros.iter().any(|t| template_uses_token(t, "managers"))
        {
            let roles = ctx.cmd.member().map(|m| m.roles.clone()).unwrap_or_default();
            let refused = pingable_role_ids(g, w, Some((ctx.cmd.user().id, &roles)), std::slice::from_ref(&role_id)).refused;
            if !refused.is_empty() {
                let text = format!(
                    "<@&{role_id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{}>. A ticket intro of this panel pings `{{managers}}`.",
                    w.id
                );
                ctx.reply_embed(error_embed("Role Not Pingable", Some(&text), None)).await;
                return Ok(());
            }
        }
    }
    let (rid, pname) = (role_id.clone(), panel.panel.name.clone());
    let r = update_managers(app, &panel.panel.id, ctx.guild_id, move |current| {
        if add {
            if current.contains(&rid) {
                return Err(crate::ticket_service::Fail::with("ALREADY_MANAGER", json!({ "roleId": rid, "panel": pname })));
            }
            if current.len() >= bot_config::limits::TICKET_MANAGER_ROLES {
                return Err(crate::ticket_service::Fail::new("MANAGER_LIMIT"));
            }
            let mut next = current;
            next.push(rid);
            return Ok(next);
        }
        if !current.contains(&rid) {
            return Err(crate::ticket_service::Fail::with("NOT_MANAGER", json!({ "roleId": rid, "panel": pname })));
        }
        Ok(current.into_iter().filter(|id| *id != rid).collect())
    })
    .await?;
    let roles = match r {
        Ok(v) => v,
        Err(f) => {
            ctx.reply(panel_error_payload(&f.code, &f.data)).await;
            return Ok(());
        }
    };
    let sync = sync_manager_overwrites(app, ctx.guild_id, &panel.panel.id, &role_id, add).await?;
    let list = if roles.is_empty() { "none".to_string() } else { roles.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" ") };
    let lines = [
        format!(
            "<@&{role_id}> {} see and handle tickets from **{}**.",
            if add { "can now" } else { "can no longer" },
            clamp_text(&panel.panel.name, 80)
        ),
        format!(
            "Updated {} existing ticket(s){}{}.",
            sync.updated,
            if sync.failed > 0 { format!(", {} failed (missing permissions)", sync.failed) } else { String::new() },
            if sync.skipped > 0 { format!(", {} not updated (too many — only the 50 newest are changed)", sync.skipped) } else { String::new() }
        ),
        format!("Manager roles: {list} ({}/{})", roles.len(), bot_config::limits::TICKET_MANAGER_ROLES),
    ];
    ctx.reply_embed(success_embed("Ticket Managers Updated", Some(&lines.join("\n")), None)).await;
    Ok(())
}

// ── autocomplete ──────────────────────────────────────────────────────────────

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    let q = bot_util::js::trim(&value).to_string();
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let r: anyhow::Result<Vec<Choice>> = async {
        if name == "panel" {
            let rows = find_panels_for_autocomplete(ctx.app.db(), &g, &q).await?;
            return Ok(rows
                .into_iter()
                .map(|p| Choice::new(format!("{} · {}", p.name, if p.style == TicketPanelStyle::Select { "dropdown" } else { "buttons" }), p.id))
                .collect());
        }
        if name == "option" {
            let panel_ref = option_string(&ctx.interaction, "panel").unwrap_or_default();
            if !panel_ref.is_empty() {
                let rows = find_options_for_autocomplete(ctx.app.db(), &g, &panel_ref, &q).await?;
                return Ok(rows
                    .into_iter()
                    .map(|x| {
                        let prefix = x.emoji.as_deref().filter(|e| !e.is_empty() && !e.starts_with('<')).map(|e| format!("{e} ")).unwrap_or_default();
                        Choice::new(format!("{prefix}{}", x.label), x.id)
                    })
                    .collect());
            }
        }
        Ok(Vec::new())
    }
    .await;
    match r {
        Ok(choices) => respond_autocomplete(&ctx.responder, &choices).await,
        Err(e) => {
            bot_util::log_warn!("ticket-panel autocomplete failed", bot_core::error::log_arg(&e));
            respond_autocomplete(&ctx.responder, &[]).await;
        }
    }
    Ok(())
}

/// `interaction.options.getString(name)` in an autocomplete: a non-focused string option.
fn option_string(interaction: &serenity::all::CommandInteraction, name: &str) -> Option<String> {
    bot_core::interaction::leaf_options(&interaction.data.options).iter().find(|o| o.name == name).and_then(|o| o.value.as_str().map(str::to_string))
}
