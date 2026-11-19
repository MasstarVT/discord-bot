//! Port of `src/commands/admin/reactionrole.js`: `/reactionrole create | attach | add | remove | edit | repost | delete
//! | list`. An admin config command: replies are public (the admin convention) except the delete prompt, and it keeps
//! working while the Reaction Roles module is off (`gated = false`). Every query goes through the service.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{
    ChannelUsable, Choice, ConfirmOptions, RoleCheck, await_confirm, cached_guild, check_assignable_role, check_channel_usable_fetch,
    respond_autocomplete, safe_defer,
};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_util::js::trim;
use bot_util::text::clamp_text;
use serde_json::Value;
use serenity::all::{ChannelId, CommandDataOptionValue, GuildId, Permissions, Role, RoleId};
use serenity::builder::{CreateEmbed, CreateInteractionResponseFollowup, EditInteractionResponse};

use crate::reaction_role_service::{
    self as rr, EmojiInput, Fetched, GuildEmojis, MessageRef, NewItem, Panel, PanelArg, PanelDraft, PanelListRow, PanelPatch, SyncResult,
    discord_ops,
};
use crate::role_template_setup::{RoleTemplateError, describe_reused, describe_setup, setup_role_template};
use crate::role_templates::{get_role_template, role_templates};

const TYPE_CHOICES: [(&str, &str); 4] = [
    ("Normal (get and remove)", "NORMAL"),
    ("Unique (one at a time)", "UNIQUE"),
    ("Verify (get only)", "VERIFY"),
    ("Reverse (removes it)", "REVERSE"),
];
const LAYOUT_CHOICES: [(&str, &str); 3] = [("Buttons", "BUTTONS"), ("Dropdown", "SELECT"), ("Emoji reactions", "REACTIONS")];
const STYLE_CHOICES: [(&str, &str); 4] = [("Blue", "Primary"), ("Grey", "Secondary"), ("Green", "Success"), ("Red", "Danger")];
const PANEL_CHANNEL_TYPES: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

/// `TEMPLATE_CHOICES`: the two presets, then every role template.
pub fn template_choices() -> Vec<(String, String)> {
    let mut v = vec![("Verify".to_string(), "verify".to_string()), ("Notifications".to_string(), "notifications".to_string())];
    v.extend(role_templates().into_iter().map(|t| (t.label.to_string(), t.key.to_string())));
    v
}

fn choices(mut o: Opt, list: &[(&str, &str)]) -> Opt {
    for (n, v) in list {
        o = o.choice(*n, *v);
    }
    o
}

fn panel_option() -> Opt {
    Opt::string("panel", "Panel (type to search)").required(true).autocomplete(true).max_length(100)
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    let mut template = Opt::string("template", "Preset panel; the role sets also create their roles");
    for (n, v) in template_choices() {
        template = template.choice(n, v);
    }
    SlashCommand::new("reactionrole", "Role panels with buttons, a dropdown or emoji reactions (MEE6 Reaction Roles)")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("create", "Post a new role panel")
                .option(Opt::channel("channel", "Where to post it").channel_types(&PANEL_CHANNEL_TYPES))
                .option(new_channel_option("channel", "channel"))
                .option(Opt::string("title", "Panel title").max_length(256))
                .option(choices(Opt::string("type", "Role mode (default Normal)"), &TYPE_CHOICES))
                .option(choices(Opt::string("layout", "Default Buttons"), &LAYOUT_CHOICES))
                .option(template)
                .option(Opt::string("description", "Embed text under the title").max_length(4000))
                .option(Opt::string("message", "Plain text sent with the panel").max_length(2000))
                .option(Opt::boolean("plain_text", "Send text only, no embed (default False)"))
                .option(Opt::integer("max_roles", "Most roles a member can hold (Normal only)").min_value(1).max_value(25))
                .option(Opt::role("role", "First role to add (the verified role for Verify)"))
                .option(new_role_option("role"))
                .option(Opt::string("emoji", "Emoji for that first role").max_length(100))
                .option(Opt::string("placeholder", "Dropdown placeholder text").max_length(150)),
        )
        .option(
            Opt::sub_command("attach", "Turn an existing message into an emoji-reaction panel")
                .option(Opt::string("message_link", "Message link, or ID of a message in this channel").required(true).max_length(200))
                .option(choices(Opt::string("type", "Role mode (default Normal)"), &TYPE_CHOICES))
                .option(Opt::string("title", "Name shown in lists").max_length(100))
                .option(Opt::integer("max_roles", "Most roles a member can hold (Normal only)").min_value(1).max_value(20)),
        )
        .option(
            Opt::sub_command("add", "Add a role to a panel")
                .option(panel_option())
                .option(Opt::role("role", "Role to offer"))
                .option(new_role_option("role"))
                .option(Opt::string("emoji", "Emoji (required on emoji panels)").max_length(100))
                .option(Opt::string("label", "Text (default: role name)").max_length(80))
                .option(choices(Opt::string("style", "Button colour (default Blue)"), &STYLE_CHOICES))
                .option(Opt::string("description", "Dropdown option description").max_length(100)),
        )
        .option(
            Opt::sub_command("remove", "Remove a role from a panel (also deleted roles)")
                .option(panel_option())
                .option(Opt::string("item", "Role to remove").required(true).autocomplete(true).max_length(100)),
        )
        .option(
            Opt::sub_command("edit", "Edit a panel")
                .option(panel_option())
                .option(Opt::string("title", "New title").max_length(256))
                .option(Opt::string("description", "New embed text (- resets)").max_length(4000))
                .option(Opt::string("message", "New plain text (- removes)").max_length(2000))
                .option(Opt::boolean("plain_text", "Text only, no embed"))
                .option(choices(Opt::string("type", "Role mode"), &TYPE_CHOICES))
                .option(Opt::integer("max_roles", "Most roles a member can hold (0 = no limit)").min_value(0).max_value(25))
                .option(Opt::string("placeholder", "New dropdown placeholder (- resets)").max_length(150)),
        )
        .option(
            Opt::sub_command("repost", "Post a panel again (deleted message) or move it")
                .option(panel_option())
                .option(Opt::channel("channel", "Default: its current channel").channel_types(&PANEL_CHANNEL_TYPES))
                .option(new_channel_option("channel", "channel")),
        )
        .option(Opt::sub_command("delete", "Delete a panel").option(panel_option()))
        .option(Opt::sub_command("list", "List role panels"))
}

/// The command (`module 'reactionroles'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Reactionroles), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Messages ──────────────────────────────────────────────────────────────────

const ROLE_HINT: &str = "Pick a role below my highest role (and below yours), or move my role up in Server Settings → Roles.";

fn not_found() -> CreateEmbed {
    error_embed("Panel Not Found", Some("I can't find that panel. Pick one from the list as you type."), None)
}

/// The warning lines (`W`).
pub mod w {
    use bot_util::text::clamp_text;

    pub const MODULE: &str = "⚠️ The Reaction Roles module is off, so panels don't respond. Turn it on with `/module enable reactionroles`.";
    pub fn manage_messages(channel_id: &str) -> String {
        format!("⚠️ I don't have **Manage Messages** in <#{channel_id}>, so I can't remove a member's old reaction when they pick a new role.")
    }
    pub fn missing(title: &str) -> String {
        format!("⚠️ The panel message no longer exists. Run `/reactionrole repost panel:{}` to post it again.", clamp_text(title, 60))
    }
    pub fn sync(reason: &str) -> String {
        format!("⚠️ I couldn't update the panel message: {reason}.")
    }
    pub fn react(emoji: &str, reason: &str) -> String {
        format!("⚠️ I couldn't add the {emoji} reaction: {reason}.")
    }
    pub const MAX_ROLES: &str = "ℹ️ `max_roles` only applies to Normal panels, so it was ignored.";
    pub const STYLE: &str = "ℹ️ `style` only applies to button panels, so it was ignored.";
    pub const DESCRIPTION: &str = "ℹ️ `description` only applies to dropdown panels, so it was ignored.";
    pub const PLACEHOLDER: &str = "ℹ️ `placeholder` only applies to dropdown panels, so it was ignored.";
}

/// `withWarnings(text, warnings)`: appends the warning lines after a blank line.
pub fn with_warnings(text: &str, warnings: &[Option<String>]) -> String {
    let lines: Vec<&str> = warnings.iter().filter_map(|w| w.as_deref()).filter(|w| !w.is_empty()).collect();
    if lines.is_empty() { text.to_string() } else { format!("{text}\n\n{}", lines.join("\n")) }
}

/// `statusWarning(result, title)`: the warning for a sync or fetch result that is not ok.
fn status_warning(status: &str, reason: Option<&str>, title: &str) -> Option<String> {
    match status {
        "missing" => Some(w::missing(title)),
        "failed" => Some(w::sync(reason.unwrap_or_default())),
        _ => None,
    }
}

fn sync_warning(r: &SyncResult, title: &str) -> Option<String> {
    status_warning(r.status, r.reason.as_deref(), title)
}

fn fetch_warning(f: &Fetched, title: &str) -> Option<String> {
    match f {
        Fetched::Missing { reason, .. } | Fetched::Failed { reason, .. } => status_warning(f.status(), Some(reason), title),
        Fetched::Ok { .. } => None,
    }
}

fn module_warning(settings: Option<&Settings>) -> Option<String> {
    match settings {
        Some(s) if !is_module_enabled(Some(s), Some(ModuleKey::Reactionroles)) => Some(w::MODULE.to_string()),
        _ => None,
    }
}

/// The command's context (`{ interaction, client, guild, guildId, settings }`).
struct C {
    ctx: CommandCtx,
    guild: GuildId,
    g: String,
    settings: Option<Settings>,
}

impl C {
    fn color(&self) -> Option<i64> {
        self.settings.as_ref().and_then(|s| s.embed_color).map(i64::from)
    }

    fn lacks_manage_messages(&self, channel_id: &str) -> bool {
        !discord_ops(&self.ctx.app).bot_has_manage_messages(&self.g, channel_id)
    }
}

/// `respond(interaction, embed)`: edits the (deferred) reply with one embed and no components; falls back to a
/// follow-up when the original was deleted. Never fails.
async fn respond(ctx: &CommandCtx, embed: CreateEmbed) {
    let edit = EditInteractionResponse::new().embeds(vec![embed.clone()]).components(Vec::new());
    if let Err(e) = ctx.edit_reply(edit).await {
        if e.code() == Some(10008) {
            if let Err(e2) = ctx.follow_up(CreateInteractionResponseFollowup::new().embeds(vec![embed]).components(Vec::new())).await
                && !bot_core::interaction_helpers::is_interaction_gone(&e2)
            {
                bot_util::log_warn!("reactionrole reply failed", e2.to_string());
            }
            return;
        }
        if !bot_core::interaction_helpers::is_interaction_gone(&e) {
            bot_util::log_warn!("reactionrole reply failed", e.to_string());
        }
    }
}

async fn missing_permissions(c: &C, channel_id: &str, check: &bot_core::interaction_helpers::ChannelUsable) {
    let embed = if !check.missing.is_empty() {
        error_embed("Missing Permissions", Some(&format!("I need **{}** in <#{channel_id}>.", permission_names(&check.missing).join(", "))), None)
    } else {
        error_embed("Invalid Channel", check.reason.as_deref(), None)
    };
    respond(&c.ctx, embed).await;
}

async fn lookup_panel(c: &C, panel_id: &str) -> anyhow::Result<Option<Panel>> {
    let group = rr::get_panel(c.ctx.app.db(), &c.g, panel_id).await?;
    if group.is_none() {
        respond(&c.ctx, not_found()).await;
    }
    Ok(group)
}

fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `checkRole(c, role)`: checks a role the admin wants on a panel (bot and executor hierarchy, managed roles) and asks
/// before a role with dangerous permissions is offered. Replies and returns `false` when the role cannot be used.
async fn check_role(c: &C, role: Option<&Role>) -> bool {
    let r = assignable(c, role);
    if !r.ok {
        respond(&c.ctx, error_embed("Role Not Allowed", Some(&format!("{} {ROLE_HINT}", r.reason.unwrap_or_default())), None)).await;
        return false;
    }
    if !r.dangerous.is_empty() {
        let id = role.map(|r| r.id.to_string()).unwrap_or_default();
        let ok = await_confirm(
            &c.ctx,
            ConfirmOptions {
                title: Some("Dangerous role".into()),
                description: Some(format!(
                    "<@&{id}> has **{}**. Anyone who uses this panel gets those permissions. Add it anyway?",
                    permission_names(&r.dangerous).join(", ")
                )),
                confirm_label: "Add anyway".into(),
                ephemeral: false,
                ..Default::default()
            },
        )
        .await;
        if !ok {
            respond(&c.ctx, info_embed("Cancelled", Some("Nothing was changed."), None)).await;
            return false;
        }
    }
    true
}

fn opt_trim(v: Option<String>) -> Option<String> {
    v.map(|s| trim(&s).to_string()).filter(|s| !s.is_empty())
}

fn needs_for(plain_text: bool, layout: &str) -> Vec<&'static str> {
    let mut needs = vec!["ViewChannel", "SendMessages"];
    if !plain_text {
        needs.push("EmbedLinks");
    }
    if layout == "REACTIONS" {
        needs.extend(["AddReactions", "ReadMessageHistory"]);
    }
    needs
}

fn panel_limit() -> CreateEmbed {
    let n = bot_config::constants::limits::REACTION_ROLE_PANELS;
    error_embed(
        "Panel Limit Reached",
        Some(&format!("This server already has {n} reaction-role panels. Delete one with `/reactionrole delete` first.")),
        None,
    )
}


/// The role the admin picked (`guild.roles.cache.get(id)`): the cached role, or the one `new_role` just created,
/// which serenity only caches once the gateway event arrives (discord.js caches it on create).
fn picked_role(c: &C, role_id: RoleId) -> Option<Role> {
    cached_guild(&c.ctx.app, c.guild)
        .and_then(|g| g.roles.get(&role_id).cloned())
        .or_else(|| c.ctx.get_role("role").filter(|r| r.id == role_id))
}

/// `checkAssignableRole(guild, role, { executor, allowDangerous: true })` on the cached guild, with a just-created role
/// added to it.
fn assignable(c: &C, role: Option<&Role>) -> RoleCheck {
    let not_found = || RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() };
    let (Some(d), Some(mut guild)) = (c.ctx.app.discord(), cached_guild(&c.ctx.app, c.guild)) else { return not_found() };
    if let Some(r) = role {
        guild.roles.entry(r.id).or_insert_with(|| r.clone());
    }
    check_assignable_role(&guild, role, d.cache.current_user().id, c.ctx.member(), true)
}

/// `checkChannelUsable(channel, { needs })`: on the cached channel, or on the channel read over REST (and cached) when
/// the cache does not have it yet (a channel `new_channel` just created, or one only `guild.channels.fetch` found).
async fn channel_check(c: &C, channel_id: &str, needs: &[&str]) -> ChannelUsable {
    check_channel_usable_fetch(&c.ctx.app, c.guild, parse_id(channel_id).map(ChannelId::new), needs).await
}

// ── /reactionrole create ──────────────────────────────────────────────────────

async fn create(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let ops = discord_ops(app);
    let channel_opt = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let type_opt = ctx.get_string("type");
    let layout_opt = ctx.get_string("layout");
    let template_key = ctx.get_string("template");
    let description_opt = ctx.get_string("description");
    let message_opt = opt_trim(ctx.get_string("message"));
    let plain_text = ctx.get_boolean("plain_text").unwrap_or(false);
    let max_roles_opt = ctx.get_integer("max_roles");
    let role_opt = ctx.get_role_id("role");
    let emoji_opt = ctx.get_string("emoji");
    let placeholder_opt = opt_trim(ctx.get_string("placeholder"));
    let mut warnings: Vec<Option<String>> = Vec::new();

    let tpl = template_key.as_deref().and_then(rr::rr_template);
    let kind = type_opt.clone().or_else(|| tpl.as_ref().map(|t| t.kind.to_string())).unwrap_or_else(|| "NORMAL".into());
    let layout = layout_opt.clone().or_else(|| tpl.as_ref().map(|t| t.layout.to_string())).unwrap_or_else(|| "BUTTONS".into());
    let title = opt_trim(ctx.get_string("title")).or_else(|| tpl.as_ref().map(|t| t.title.to_string())).unwrap_or_else(|| rr::DEFAULT_TITLE.into());
    let description = opt_trim(description_opt.clone()).or_else(|| tpl.as_ref().and_then(|t| t.description_for(&layout)).map(str::to_string));
    let max_roles = if kind == "NORMAL" { max_roles_opt.map(|m| m as i32) } else { None };
    if max_roles_opt.is_some() && kind != "NORMAL" {
        warnings.push(Some(w::MAX_ROLES.into()));
    }
    let placeholder = if layout == "SELECT" { placeholder_opt.clone() } else { None };
    if placeholder_opt.is_some() && layout != "SELECT" {
        warnings.push(Some(w::PLACEHOLDER.into()));
    }

    let channel_id = channel_opt.to_string();
    let check = channel_check(c, &channel_id, &needs_for(plain_text, &layout)).await;
    if !check.ok {
        missing_permissions(c, &channel_id, &check).await;
        return Ok(());
    }

    if rr::count_panels(app.db(), &c.g).await? >= bot_config::constants::limits::REACTION_ROLE_PANELS as i64 {
        respond(ctx, panel_limit()).await;
        return Ok(());
    }

    if let Some(key) = template_key.as_deref().filter(|k| get_role_template(k).is_some()) {
        return create_template_panel(
            c,
            TemplateArgs {
                key: key.to_string(),
                channel_id,
                layout_opt,
                message_opt,
                plain_text,
                placeholder_opt,
                type_opt,
                title: opt_trim(ctx.get_string("title")),
                description: opt_trim(description_opt),
                extras: role_opt.is_some() || emoji_opt.is_some() || max_roles_opt.is_some(),
            },
        )
        .await;
    }

    let mut item: Option<NewItem> = None;
    if emoji_opt.is_some() && role_opt.is_none() {
        respond(ctx, error_embed("Missing Role", Some("The `emoji` option sets the emoji for `role`. Pick a role too, or leave `emoji` empty."), None)).await;
        return Ok(());
    }
    if let Some(role_id) = role_opt {
        let role = picked_role(c, role_id);
        if !check_role(c, role.as_ref()).await {
            return Ok(());
        }
        let stored = emoji_opt.clone().or_else(|| tpl.as_ref().map(|t| t.item.emoji.to_string()));
        let (emoji, _key) = match rr::parse_emoji_input(stored.as_deref(), &GuildEmojis { ops: ops.as_ref(), guild_id: &c.g }) {
            EmojiInput::Ok { emoji, key } => (emoji, key),
            EmojiInput::Err(e) => {
                respond(ctx, error_embed("Invalid Emoji", Some(&e), None)).await;
                return Ok(());
            }
        };
        if layout == "REACTIONS" && emoji.is_none() {
            respond(ctx, error_embed("Emoji Needed", Some("Emoji-reaction panels need an emoji for each role. Add `emoji:`."), None)).await;
            return Ok(());
        }
        let rid = role_id.to_string();
        let name = tpl.as_ref().and_then(|t| t.item.label.map(str::to_string)).or_else(|| role.map(|r| r.name)).unwrap_or_default();
        item = Some(NewItem {
            role_id: rid,
            label: clamp_text(&name, 80),
            emoji,
            style: Some(if layout == "BUTTONS" { tpl.as_ref().map(|t| t.item.style).unwrap_or(1) } else { 1 }),
            description: None,
        });
    }

    app.settings().ensure_guild_settings(&c.g).await?;
    let id = bot_db::new_uuid();
    let mut draft = PanelDraft {
        id: id.clone(),
        guild_id: c.g.clone(),
        channel_id: channel_id.clone(),
        message_id: "0".into(),
        title: title.clone(),
        description,
        kind: kind.clone(),
        layout: layout.clone(),
        max_roles,
        plain_text,
        message_content: message_opt,
        attached: false,
        placeholder,
        items: match &item {
            Some(i) => vec![rr::draft_item(&id, i, 0)?],
            None => Vec::new(),
        },
    };

    // The JS builds the payload inside the send's try: a payload discord.js refuses is "Could Not Post".
    let sent = if rr::panel_payload_throws(&draft.view()) {
        Err(rr::DErr::other("empty embed description"))
    } else {
        ops.send_message(&c.g, &channel_id, &rr::build_panel_payload(&draft.view(), c.color()).to_api_body()).await
    };
    let sent = match sent {
        Ok(m) => m,
        Err(e) => {
            respond(
                ctx,
                error_embed("Could Not Post", Some(&format!("I couldn't post in <#{channel_id}>: {}.", rr::describe_discord_error(e.code))), None),
            )
            .await;
            return Ok(());
        }
    };
    draft.message_id = sent.id.clone();
    if let Err(e) = rr::create_panel(app.db(), &draft).await {
        bot_core::deletion_tracker::mark_bot_deletion(app, &[sent.id.as_str()], Some("reaction-roles:rollback"), None);
        let _ = ops.delete_message(&sent.channel_id, &sent.id).await;
        bot_util::log_error!("reactionrole create failed to save the panel", e.to_string());
        respond(ctx, error_embed("Something Went Wrong", Some("I could not save the panel, so I removed the message. Please try again."), None)).await;
        return Ok(());
    }
    rr::invalidate_panel_index(app, &c.g).await;

    if layout == "REACTIONS"
        && let Some(i) = &item
    {
        let emoji = i.emoji.clone().unwrap_or_default();
        let r = rr::add_panel_reaction(app, &sent, &emoji).await;
        if !r.ok {
            warnings.push(Some(w::react(&emoji, r.reason.as_deref().unwrap_or_default())));
        }
    }
    if kind == "UNIQUE" && layout == "REACTIONS" && c.lacks_manage_messages(&channel_id) {
        warnings.push(Some(w::manage_messages(&channel_id)));
    }

    let mut lines = vec![format!("Panel **{}** posted in <#{channel_id}> · [Jump]({}).", clamp_text(&title, 100), sent.url())];
    if let Some(i) = &item {
        lines.push(format!("First role: <@&{}>{}.", i.role_id, i.emoji.as_ref().map(|e| format!(" {e}")).unwrap_or_default()));
    }
    lines.push(format!("Add {}roles with `/reactionrole add`.", if item.is_some() { "more " } else { "" }));
    warnings.push(module_warning(c.settings.as_ref()));
    respond(ctx, success_embed("Panel Created", Some(&with_warnings(&lines.join("\n"), &warnings)), None)).await;
    Ok(())
}

struct TemplateArgs {
    key: String,
    channel_id: String,
    layout_opt: Option<String>,
    message_opt: Option<String>,
    plain_text: bool,
    placeholder_opt: Option<String>,
    type_opt: Option<String>,
    title: Option<String>,
    description: Option<String>,
    extras: bool,
}

/// `/reactionrole create template:<role set>`: creates (or reuses) the template's roles (and links a timezone
/// template's roles to their zones), then posts them as the template's panel (a dropdown or buttons, never emoji
/// reactions).
async fn create_template_panel(c: &C, a: TemplateArgs) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let ops = discord_ops(app);
    let Some(template) = get_role_template(&a.key) else { return Ok(()) };
    let panel = &template.panel;
    if a.layout_opt.as_deref() == Some("REACTIONS") {
        respond(ctx, error_embed("Pick Another Layout", Some("Template panels use a dropdown or buttons. Leave `layout` empty for the template's own."), None))
            .await;
        return Ok(());
    }
    let reason = format!("Role template by {}", ctx.user().tag());
    let setup = match setup_role_template(app, &c.g, &a.key, ctx.member(), &reason).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(t) = e.downcast_ref::<RoleTemplateError>() {
                respond(ctx, error_embed("Roles Not Created", Some(&t.0), None)).await;
                return Ok(());
            }
            return Err(e);
        }
    };
    let layout = a.layout_opt.clone().unwrap_or_else(|| panel.layout.to_string());
    // A timezone panel is always Unique (one zone at a time); a role set's mode can be changed.
    let kind = if template.links_zones { panel.kind.to_string() } else { a.type_opt.clone().unwrap_or_else(|| panel.kind.to_string()) };
    let id = bot_db::new_uuid();
    let mut items = Vec::new();
    for (i, r) in setup.roles.iter().enumerate() {
        items.push(rr::draft_item(
            &id,
            &NewItem {
                role_id: r.role_id.clone(),
                label: r.label.clone(),
                emoji: r.emoji.clone(),
                style: Some(2),
                description: if layout == "SELECT" { r.description.clone() } else { None },
            },
            i as i32,
        )?);
    }
    let mut draft = PanelDraft {
        id: id.clone(),
        guild_id: c.g.clone(),
        channel_id: a.channel_id.clone(),
        message_id: "0".into(),
        title: a.title.clone().unwrap_or_else(|| panel.title.to_string()),
        description: Some(a.description.clone().unwrap_or_else(|| panel.description.to_string())),
        kind,
        layout: layout.clone(),
        max_roles: None,
        plain_text: a.plain_text,
        message_content: a.message_opt.clone(),
        attached: false,
        placeholder: if layout == "SELECT" { a.placeholder_opt.clone().or_else(|| panel.placeholder.map(str::to_string)) } else { None },
        items,
    };
    let done = describe_setup(setup.created, setup.reused);
    let reused = describe_reused(&setup.roles);

    app.settings().ensure_guild_settings(&c.g).await?;
    let body = rr::build_panel_payload(&draft.view(), c.color()).to_api_body();
    let sent = match ops.send_message(&c.g, &a.channel_id, &body).await {
        Ok(m) => m,
        Err(e) => {
            let text = format!(
                "{done}\nBut I couldn't post the panel in <#{}>: {}. Run the command again to retry; it reuses the roles.",
                a.channel_id,
                rr::describe_discord_error(e.code)
            );
            respond(ctx, error_embed("Could Not Post", Some(&text), None)).await;
            return Ok(());
        }
    };
    draft.message_id = sent.id.clone();
    if let Err(e) = rr::create_panel(app.db(), &draft).await {
        bot_core::deletion_tracker::mark_bot_deletion(app, &[sent.id.as_str()], Some("reaction-roles:rollback"), None);
        let _ = ops.delete_message(&sent.channel_id, &sent.id).await;
        bot_util::log_error!("reactionrole create (role template) failed to save the panel", e.to_string());
        let text = format!("{done}\nBut I could not save the panel, so I removed the message. Please try again.");
        respond(ctx, error_embed("Something Went Wrong", Some(&text), None)).await;
        return Ok(());
    }
    rr::invalidate_panel_index(app, &c.g).await;

    let mut warnings: Vec<Option<String>> = Vec::new();
    if template.links_zones && a.type_opt.as_deref().is_some_and(|t| t != "UNIQUE") {
        warnings.push(Some("Timezone panels are always **Unique**, so members hold one timezone role at a time.".into()));
    }
    if a.extras {
        warnings.push(Some("`role`, `emoji` and `max_roles` are ignored with a role template.".into()));
    }
    let mut lines = vec![format!("Panel **{}** posted in <#{}> · [Jump]({}).", clamp_text(&draft.title, 100), a.channel_id, sent.url()), done];
    if let Some(r) = reused {
        lines.push(r);
    }
    if let Some(n) = template.note {
        lines.push(n.to_string());
    }
    lines.push("Remove the roles you do not need with `/reactionrole remove`.".into());
    warnings.push(module_warning(c.settings.as_ref()));
    respond(ctx, success_embed("Template Panel Created", Some(&with_warnings(&lines.join("\n"), &warnings)), None)).await;
    Ok(())
}

// ── /reactionrole attach ──────────────────────────────────────────────────────

async fn attach(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let ops = discord_ops(app);
    let link = ctx.get_string("message_link").unwrap_or_default();
    let (channel_id, message_id) = match rr::parse_message_reference(&link, &c.g, &ctx.channel_id().to_string()) {
        MessageRef::Ok { channel_id, message_id } => (channel_id, message_id),
        MessageRef::Err(e) => {
            respond(ctx, error_embed("Invalid Message", Some(&e), None)).await;
            return Ok(());
        }
    };

    let channel = match ops.cached_channel(&c.g, &channel_id) {
        Some(ch) => Some(ch),
        None => ops.fetch_channel(&c.g, &channel_id).await.ok(),
    };
    let Some(channel) = channel.filter(|ch| ch.text_based) else {
        respond(ctx, error_embed("Channel Not Found", Some("I can't find that channel, or it isn't a text channel."), None)).await;
        return Ok(());
    };
    let check = channel_check(c, &channel.id, &["ViewChannel", "ReadMessageHistory", "AddReactions"]).await;
    if !check.ok {
        missing_permissions(c, &channel.id, &check).await;
        return Ok(());
    }

    let message = match ops.fetch_message(&c.g, &channel.id, &message_id).await {
        Ok(m) => m,
        Err(e) => {
            if e.code == Some(10008) {
                respond(ctx, error_embed("Message Not Found", Some("I can't find that message. Copy its link again (right-click → Copy Message Link)."), None)).await;
                return Ok(());
            }
            if e.code == Some(50001) || e.code == Some(50013) {
                respond(ctx, error_embed("Missing Permissions", Some(&format!("I can't read messages in <#{}>.", channel.id)), None)).await;
                return Ok(());
            }
            return Err(e.into());
        }
    };

    if let Some(existing) = rr::find_panel_by_message(app.db(), &c.g, &message.id).await? {
        let text = format!("That message is already the panel **{}**. Add roles to it with `/reactionrole add`.", clamp_text(&existing.title, 100));
        respond(ctx, error_embed("Already a Panel", Some(&text), None)).await;
        return Ok(());
    }
    if rr::count_panels(app.db(), &c.g).await? >= bot_config::constants::limits::REACTION_ROLE_PANELS as i64 {
        respond(ctx, panel_limit()).await;
        return Ok(());
    }
    if message.reactions.len() >= rr::max_items("REACTIONS").unwrap_or(20) {
        respond(
            ctx,
            error_embed("Too Many Reactions", Some("That message already has 20 different reactions, so I can't add role emojis to it. Clear some reactions first."), None),
        )
        .await;
        return Ok(());
    }

    let mut warnings: Vec<Option<String>> = Vec::new();
    let kind = ctx.get_string("type").unwrap_or_else(|| "NORMAL".into());
    let max_roles_opt = ctx.get_integer("max_roles");
    let max_roles = if kind == "NORMAL" { max_roles_opt.map(|m| m as i32) } else { None };
    if max_roles_opt.is_some() && kind != "NORMAL" {
        warnings.push(Some(w::MAX_ROLES.into()));
    }
    let title = opt_trim(ctx.get_string("title"))
        .unwrap_or_else(|| rr::default_attach_title(&message.content, message.first_embed_title.as_deref(), &channel.name));

    app.settings().ensure_guild_settings(&c.g).await?;
    rr::create_panel(
        app.db(),
        &PanelDraft {
            id: bot_db::new_uuid(),
            guild_id: c.g.clone(),
            channel_id: channel.id.clone(),
            message_id: message.id.clone(),
            title: title.clone(),
            description: None,
            kind: kind.clone(),
            layout: "REACTIONS".into(),
            max_roles,
            plain_text: false,
            message_content: None,
            attached: true,
            placeholder: None,
            items: Vec::new(),
        },
    )
    .await?;
    rr::invalidate_panel_index(app, &c.g).await;

    if kind == "UNIQUE" && c.lacks_manage_messages(&channel.id) {
        warnings.push(Some(w::manage_messages(&channel.id)));
    }
    let text = format!(
        "**{}** is now an emoji-reaction panel on [this message]({}). I won't change the message itself.\nAdd roles with `/reactionrole add panel:{} role:… emoji:…` and I'll add each reaction.",
        clamp_text(&title, 100),
        message.url(),
        clamp_text(&title, 60)
    );
    warnings.push(module_warning(c.settings.as_ref()));
    respond(ctx, success_embed("Panel Attached", Some(&with_warnings(&text, &warnings)), None)).await;
    Ok(())
}

// ── /reactionrole add ─────────────────────────────────────────────────────────

async fn add(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let ops = discord_ops(app);
    let Some(group) = lookup_panel(c, &ctx.get_string("panel").unwrap_or_default()).await? else { return Ok(()) };
    let role_opt = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let role_id = role_opt.to_string();
    let role_cached = picked_role(c, role_opt);
    let title = clamp_text(&group.group.title, 100);
    let layout = group.layout();

    if role_cached.is_some() && group.items.iter().any(|i| i.role_id == role_id) {
        respond(ctx, error_embed("Already Added", Some(&format!("<@&{role_id}> is already on **{title}**.")), None)).await;
        return Ok(());
    }
    let max = rr::max_items(layout).unwrap_or(25);
    if group.items.len() >= max {
        let text = format!("A {} panel can hold at most {max} roles.", rr::layout_label(layout).unwrap_or(layout).to_lowercase());
        respond(ctx, error_embed("Panel Full", Some(&text), None)).await;
        return Ok(());
    }
    if !check_role(c, role_cached.as_ref()).await {
        return Ok(());
    }

    let (emoji, key) = match rr::parse_emoji_input(ctx.get_string("emoji").as_deref(), &GuildEmojis { ops: ops.as_ref(), guild_id: &c.g }) {
        EmojiInput::Ok { emoji, key } => (emoji, key),
        EmojiInput::Err(e) => {
            respond(ctx, error_embed("Invalid Emoji", Some(&e), None)).await;
            return Ok(());
        }
    };
    if layout == "REACTIONS" {
        let Some(e) = emoji.as_deref() else {
            respond(ctx, error_embed("Emoji Needed", Some("Emoji-reaction panels need an emoji for each role. Add `emoji:`."), None)).await;
            return Ok(());
        };
        if let Some(other) = group.items.iter().find(|i| rr::emoji_key_of_stored(i.emoji.as_deref()) == key) {
            respond(ctx, error_embed("Emoji In Use", Some(&format!("{e} is already used for <@&{}> on this panel.", other.role_id)), None)).await;
            return Ok(());
        }
    }

    let mut warnings: Vec<Option<String>> = Vec::new();
    let role_name = role_cached.as_ref().map(|r| r.name.clone()).unwrap_or_default();
    let label = clamp_text(&opt_trim(ctx.get_string("label")).unwrap_or(role_name), 80);
    let style_opt = ctx.get_string("style");
    let style = if layout == "BUTTONS" { style_opt.as_deref().and_then(rr::style_map).unwrap_or(1) } else { 1 };
    if style_opt.is_some() && layout != "BUTTONS" {
        warnings.push(Some(w::STYLE.into()));
    }
    let description_opt = opt_trim(ctx.get_string("description"));
    let description = description_opt.as_deref().map(|d| clamp_text(d, 100));
    if description_opt.is_some() && layout != "SELECT" {
        warnings.push(Some(w::DESCRIPTION.into()));
    }

    let new_item = NewItem { role_id: role_id.clone(), label, emoji: emoji.clone(), style: Some(style), description };
    let item = match rr::add_item(app.db(), &group.group.id, &group.items, &new_item).await {
        Ok(i) => i,
        Err(e) => {
            if e.downcast_ref::<bot_db::DbError>().is_some_and(|d| d.is_unique_violation()) {
                respond(ctx, error_embed("Already Added", Some(&format!("<@&{role_id}> is already on **{title}**.")), None)).await;
                return Ok(());
            }
            return Err(e);
        }
    };

    if !group.group.attached {
        let sync = rr::sync_panel(app, &c.g, PanelArg::Id(group.group.id.clone()), c.color()).await;
        if sync.status == "failed" && sync.code == Some(50035) {
            rr::remove_item(app.db(), &group.group.id, &item.id).await?;
            rr::sync_panel(app, &c.g, PanelArg::Id(group.group.id.clone()), c.color()).await;
            rr::invalidate_panel_index(app, &c.g).await;
            let text = format!("Discord rejected the updated panel ({}), so I undid the change.", sync.reason.unwrap_or_default());
            respond(ctx, error_embed("Panel Update Failed", Some(&text), None)).await;
            return Ok(());
        }
        warnings.push(sync_warning(&sync, &group.group.title));
    }
    rr::invalidate_panel_index(app, &c.g).await;

    if layout == "REACTIONS" {
        let fm = rr::fetch_panel(app, &c.g, &group).await;
        match &fm {
            Fetched::Ok { message, .. } => {
                let e = emoji.clone().unwrap_or_default();
                let r = rr::add_panel_reaction(app, message, &e).await;
                if !r.ok {
                    if r.code == Some(10014) || r.code == Some(30010) {
                        rr::remove_item(app.db(), &group.group.id, &item.id).await?;
                        if !group.group.attached {
                            rr::sync_panel(app, &c.g, PanelArg::Id(group.group.id.clone()), c.color()).await;
                        }
                        rr::invalidate_panel_index(app, &c.g).await;
                        let text = format!("{}, so I undid the change.", rr::describe_discord_error(r.code));
                        respond(ctx, error_embed("Could Not Add Reaction", Some(&text), None)).await;
                        return Ok(());
                    }
                    warnings.push(Some(w::react(&e, r.reason.as_deref().unwrap_or_default())));
                }
            }
            _ if group.group.attached => warnings.push(fetch_warning(&fm, &group.group.title)),
            _ => {}
        }
    }

    if group.kind() == "UNIQUE" && layout == "REACTIONS" && c.lacks_manage_messages(&group.group.channel_id) {
        warnings.push(Some(w::manage_messages(&group.group.channel_id)));
    }
    warnings.push(module_warning(c.settings.as_ref()));
    let with = emoji.as_ref().map(|e| format!(" with {e}")).unwrap_or_default();
    respond(ctx, success_embed("Role Added", Some(&with_warnings(&format!("<@&{role_id}> added to **{title}**{with}."), &warnings)), None)).await;
    Ok(())
}

// ── /reactionrole remove ──────────────────────────────────────────────────────

async fn remove(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let Some(group) = lookup_panel(c, &ctx.get_string("panel").unwrap_or_default()).await? else { return Ok(()) };
    let Some(item) = rr::resolve_item(&group.items, &ctx.get_string("item").unwrap_or_default()).cloned() else {
        respond(ctx, error_embed("Not On Panel", Some("That role isn't on this panel. Pick one from the list as you type."), None)).await;
        return Ok(());
    };

    let mut warnings: Vec<Option<String>> = Vec::new();
    let is_reactions = group.layout() == "REACTIONS";
    if is_reactions {
        rr::invalidate_panel_index(app, &c.g).await; // before the item goes, so our own hooks never see a half-removed one
    }
    rr::remove_item(app.db(), &group.group.id, &item.id).await?;
    if !group.group.attached {
        let s = rr::sync_panel(app, &c.g, PanelArg::Id(group.group.id.clone()), c.color()).await;
        warnings.push(sync_warning(&s, &group.group.title));
    }
    if is_reactions {
        let fm = rr::fetch_panel(app, &c.g, &group).await;
        match &fm {
            Fetched::Ok { message, .. } => {
                if let Some(e) = item.emoji.as_deref().filter(|e| !e.is_empty()) {
                    rr::remove_panel_reaction(app, &c.g, message, e).await; // cosmetic: failures are ignored
                }
            }
            _ if group.group.attached => warnings.push(fetch_warning(&fm, &group.group.title)),
            _ => {}
        }
    }
    rr::invalidate_panel_index(app, &c.g).await; // the index may have been reloaded between the two steps

    let who = if discord_ops(app).cached_role(&c.g, &item.role_id).is_some() {
        format!("<@&{}>", item.role_id)
    } else {
        format!("**{}** (deleted role)", clamp_text(&item.label, 80))
    };
    warnings.push(module_warning(c.settings.as_ref()));
    let text = format!("{who} removed from **{}**. Members who already have the role keep it.", clamp_text(&group.group.title, 100));
    respond(ctx, success_embed("Role Removed", Some(&with_warnings(&text, &warnings)), None)).await;
    Ok(())
}

// ── /reactionrole edit ────────────────────────────────────────────────────────

/// `textOrNull(value)`: '-' or an empty value resets a text field to null.
pub fn text_or_null(value: &str) -> Option<String> {
    let t = trim(value);
    if t == "-" || t.is_empty() { None } else { Some(t.to_string()) }
}

async fn edit(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let Some(group) = lookup_panel(c, &ctx.get_string("panel").unwrap_or_default()).await? else { return Ok(()) };

    let title_opt = ctx.get_string("title");
    let description_opt = ctx.get_string("description");
    let message_opt = ctx.get_string("message");
    let plain_opt = ctx.get_boolean("plain_text");
    let type_opt = ctx.get_string("type");
    let max_roles_opt = ctx.get_integer("max_roles");
    let placeholder_opt = ctx.get_string("placeholder");
    if title_opt.is_none()
        && description_opt.is_none()
        && message_opt.is_none()
        && plain_opt.is_none()
        && type_opt.is_none()
        && max_roles_opt.is_none()
        && placeholder_opt.is_none()
    {
        respond(ctx, error_embed("Nothing To Change", Some("Give at least one option to change."), None)).await;
        return Ok(());
    }
    if group.group.attached && (description_opt.is_some() || message_opt.is_some() || plain_opt.is_some()) {
        respond(
            ctx,
            error_embed("Attached Panel", Some("This panel uses an existing message that I never edit, so only `title`, `type` and `max_roles` can change."), None),
        )
        .await;
        return Ok(());
    }

    let mut warnings: Vec<Option<String>> = Vec::new();
    let mut changed: Vec<&str> = Vec::new();
    let mut patch = PanelPatch::default();
    if let Some(t) = &title_opt {
        let t = trim(t);
        patch.title = Some(if t.is_empty() { rr::DEFAULT_TITLE.to_string() } else { t.to_string() });
        changed.push("title");
    }
    if let Some(d) = &description_opt {
        patch.description = Some(text_or_null(d));
        changed.push("description");
    }
    if let Some(m) = &message_opt {
        patch.message_content = Some(text_or_null(m));
        changed.push("message");
    }
    if let Some(p) = plain_opt {
        patch.plain_text = Some(p);
        changed.push("plain_text");
    }
    if let Some(t) = &type_opt {
        patch.kind = Some(t.clone());
        changed.push("type");
    }
    if let Some(p) = &placeholder_opt {
        if group.layout() == "SELECT" {
            patch.placeholder = Some(text_or_null(p));
            changed.push("placeholder");
        } else {
            warnings.push(Some(w::PLACEHOLDER.into()));
        }
    }
    let new_kind = patch.kind.clone().unwrap_or_else(|| group.kind().to_string());
    if new_kind != "NORMAL" {
        patch.max_roles = Some(None);
        if max_roles_opt.is_some_and(|m| m != 0) {
            warnings.push(Some(w::MAX_ROLES.into()));
        }
    } else if let Some(m) = max_roles_opt {
        patch.max_roles = Some(if m == 0 { None } else { Some(m as i32) });
        changed.push("max_roles");
    }
    if changed.is_empty() {
        respond(ctx, error_embed("Nothing To Change", Some(&with_warnings("None of those options apply to this panel.", &warnings)), None)).await;
        return Ok(());
    }

    let Some(updated) = rr::update_panel(app.db(), &c.g, &group.group.id, &patch).await? else {
        respond(ctx, not_found()).await;
        return Ok(());
    };
    rr::invalidate_panel_index(app, &c.g).await;
    if !updated.group.attached {
        let title = updated.group.title.clone();
        let s = rr::sync_panel(app, &c.g, PanelArg::Row(Box::new(updated.clone())), c.color()).await;
        warnings.push(sync_warning(&s, &title));
    }
    if updated.kind() == "UNIQUE" && updated.layout() == "REACTIONS" && c.lacks_manage_messages(&updated.group.channel_id) {
        warnings.push(Some(w::manage_messages(&updated.group.channel_id)));
    }
    warnings.push(module_warning(c.settings.as_ref()));
    let text = format!("**{}** updated: {}.", clamp_text(&updated.group.title, 100), changed.join(", "));
    respond(ctx, success_embed("Panel Updated", Some(&with_warnings(&text, &warnings)), None)).await;
    Ok(())
}

// ── /reactionrole repost ──────────────────────────────────────────────────────

async fn repost(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let ops = discord_ops(app);
    let Some(group) = lookup_panel(c, &ctx.get_string("panel").unwrap_or_default()).await? else { return Ok(()) };
    let channel_opt = ctx.get_channel_id("channel").map(|c| c.to_string());
    let title = clamp_text(&group.group.title, 100);
    let mut warnings: Vec<Option<String>> = Vec::new();

    let picked = match channel_opt.as_deref() {
        Some(id) => match ops.cached_channel(&c.g, id) {
            Some(t) => Some(t),
            None => ops.fetch_channel(&c.g, id).await.ok(), // a channel `new_channel` just created
        },
        None => None,
    };
    let target = match picked {
        Some(t) => Some(t),
        None => match ops.cached_channel(&c.g, &group.group.channel_id) {
            Some(t) => Some(t),
            None => ops.fetch_channel(&c.g, &group.group.channel_id).await.ok(),
        },
    };
    let Some(target) = target else {
        respond(ctx, error_embed("Channel Needed", Some("The panel's channel no longer exists. Run this again with `channel:`."), None)).await;
        return Ok(());
    };

    let restore_only = group.group.attached && channel_opt.is_none();
    let mut old_message: Option<Fetched> = None;
    if restore_only {
        let fm = rr::fetch_panel(app, &c.g, &group).await;
        match &fm {
            Fetched::Ok { message, .. } => {
                let res = rr::ensure_panel_reactions(app, message, &rr::index_items(&group.items)).await;
                let mut lines = vec![format!("I re-added the role reactions on [the attached message]({}).", message.url())];
                for f in res.failed.iter().take(5) {
                    lines.push(w::react(&f.emoji, &f.reason));
                }
                respond(ctx, success_embed("Reactions Restored", Some(&lines.join("\n")), None)).await;
                return Ok(());
            }
            Fetched::Failed { reason, .. } => {
                respond(ctx, error_embed("Could Not Reach Panel", Some(&format!("I couldn't reach the attached message: {reason}.")), None)).await;
                return Ok(());
            }
            // missing: the panel becomes a message the bot posts below
            Fetched::Missing { .. } => {}
        }
    }

    let check = channel_check(c, &target.id, &needs_for(group.group.plain_text, group.layout())).await;
    if !check.ok {
        missing_permissions(c, &target.id, &check).await;
        return Ok(());
    }

    // Look the old message up before the panel moves: a fetch that finds nothing flags the panel as missing.
    if !restore_only {
        old_message = Some(rr::fetch_panel(app, &c.g, &group).await);
    }

    let sent = if rr::panel_payload_throws(&group.view()) {
        Err(rr::DErr::other("empty embed description"))
    } else {
        ops.send_message(&c.g, &target.id, &rr::build_panel_payload(&group.view(), c.color()).to_api_body()).await
    };
    let sent = match sent {
        Ok(m) => m,
        Err(e) => {
            let text = format!("I couldn't post in <#{}>: {}.", target.id, rr::describe_discord_error(e.code));
            respond(ctx, error_embed("Could Not Post", Some(&text), None)).await;
            return Ok(());
        }
    };
    if let Err(e) = rr::move_panel(app.db(), &c.g, &group.group.id, &target.id, &sent.id, false).await {
        bot_core::deletion_tracker::mark_bot_deletion(app, &[sent.id.as_str()], Some("reaction-roles:rollback"), None);
        let _ = ops.delete_message(&sent.channel_id, &sent.id).await;
        bot_util::log_error!("reactionrole repost failed to save the panel", e.to_string());
        respond(ctx, error_embed("Something Went Wrong", Some("I could not save the new message, so I removed it. Please try again."), None)).await;
        return Ok(());
    }
    rr::invalidate_panel_index(app, &c.g).await;
    if group.layout() == "REACTIONS" {
        let res = rr::ensure_panel_reactions(app, &sent, &rr::index_items(&group.items)).await;
        for f in res.failed.iter().take(5) {
            warnings.push(Some(w::react(&f.emoji, &f.reason)));
        }
    }

    // Clean up the old message (only when it is a different one).
    if let Some(Fetched::Ok { message, .. }) = &old_message
        && message.id != sent.id
    {
        if !group.group.attached {
            bot_core::deletion_tracker::mark_bot_deletion(app, &[message.id.as_str()], Some("reaction-roles:repost"), None);
            let _ = ops.delete_message(&message.channel_id, &message.id).await;
        } else {
            rr::remove_own_reactions(app, message, &rr::index_items(&group.items)).await; // its content is never touched
        }
    }

    let mut text = format!("**{title}** is now at [this message]({}) in <#{}>.", sent.url(), target.id);
    if group.group.attached {
        text.push_str(" It's now a message I posted, so `/reactionrole edit` can change its text. The original message was left alone.");
    }
    if group.kind() == "UNIQUE" && group.layout() == "REACTIONS" && c.lacks_manage_messages(&target.id) {
        warnings.push(Some(w::manage_messages(&target.id)));
    }
    warnings.push(module_warning(c.settings.as_ref()));
    respond(ctx, success_embed("Panel Reposted", Some(&with_warnings(&text, &warnings)), None)).await;
    Ok(())
}

// ── /reactionrole delete ──────────────────────────────────────────────────────

async fn del(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let app = &ctx.app;
    let ops = discord_ops(app);
    let Some(group) = lookup_panel(c, &ctx.get_string("panel").unwrap_or_default()).await? else { return Ok(()) };
    let title = clamp_text(&group.group.title, 100);

    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete this panel?".into()),
            description: Some(format!(
                "**{title}** and its {} role(s) will be removed{}. Members keep the roles they already have.",
                group.items.len(),
                if group.group.attached { "" } else { ", and I'll delete the panel message" }
            )),
            confirm_label: "Delete".into(),
            ..Default::default()
        },
    )
    .await;
    if !ok {
        respond(ctx, info_embed("Cancelled", Some("The panel was not deleted."), None)).await;
        return Ok(());
    }

    rr::delete_panel(app.db(), &c.g, &group.group.id).await?;
    rr::invalidate_panel_index(app, &c.g).await;

    let mut message_deleted = false;
    if let Fetched::Ok { message, .. } = rr::fetch_panel(app, &c.g, &group).await {
        if !group.group.attached {
            bot_core::deletion_tracker::mark_bot_deletion(app, &[message.id.as_str()], Some("reaction-roles:delete"), None);
            message_deleted = ops.delete_message(&message.channel_id, &message.id).await.is_ok();
        } else {
            rr::remove_own_reactions(app, &message, &rr::index_items(&group.items)).await; // never delete or edit an attached message
        }
    }

    let text = if group.group.attached {
        format!("**{title}** was deleted. The original message was left alone.")
    } else if message_deleted {
        format!("**{title}** was deleted, along with its message.")
    } else {
        format!("**{title}** was deleted. Its message was already gone, or I couldn't delete it.")
    };
    if let Err(e) = ctx.follow_up(CreateInteractionResponseFollowup::new().embed(success_embed("Panel Deleted", Some(&text), None))).await
        && !bot_core::interaction_helpers::is_interaction_gone(&e)
    {
        bot_util::log_warn!("reactionrole delete follow-up failed", e.to_string());
    }
    Ok(())
}

// ── /reactionrole list ────────────────────────────────────────────────────────

/// `buildListPages(panels, { guildId, total, settings })`: one embed per page of 10 panels. Pure; every field is
/// clamped so a page stays far below Discord's 6000-character limit. Returns `(title, fields)` per page plus the
/// embeds.
pub fn list_page_fields(panels: &[PanelListRow], guild_id: &str) -> Vec<Vec<(String, String)>> {
    let mut pages = Vec::new();
    for (start, chunk) in panels.chunks(rr::LIST_PAGE_SIZE).enumerate().map(|(i, c)| (i * rr::LIST_PAGE_SIZE, c)) {
        let mut fields = Vec::new();
        for (i, p) in chunk.iter().enumerate() {
            let mut lines = vec![
                format!(
                    "{} · {} · {} role(s){}",
                    rr::type_label(&p.kind).unwrap_or(&p.kind),
                    rr::layout_label(&p.layout).unwrap_or(&p.layout),
                    p.item_count,
                    p.max_roles.filter(|m| *m != 0).map(|m| format!(" · max {m}")).unwrap_or_default()
                ),
                format!(
                    "<#{}> · [Jump](https://discord.com/channels/{guild_id}/{}/{}){}{}",
                    p.channel_id,
                    p.channel_id,
                    p.message_id,
                    if p.attached { " · attached" } else { "" },
                    if p.plain_text { " · text only" } else { "" }
                ),
            ];
            if p.message_missing_at.is_some() {
                lines.push("⚠️ message deleted — run /reactionrole repost".into());
            }
            fields.push((format!("{}. {}", start + i + 1, clamp_text(&p.title, 200)), clamp_text(&lines.join("\n"), 1024)));
        }
        pages.push(fields);
    }
    pages
}

/// `buildListPages(panels, { guildId, total, settings })`.
pub fn build_list_pages(panels: &[PanelListRow], guild_id: &str, total: usize, settings: Option<&Settings>) -> Vec<CreateEmbed> {
    list_page_fields(panels, guild_id)
        .into_iter()
        .map(|fields| {
            let mut embed = apply_guild_color(info_embed(&format!("Reaction Role Panels ({total})"), None, None), settings.map(|s| s.as_ref()));
            for (name, value) in fields {
                embed = embed.field(name, value, false);
            }
            embed
        })
        .collect()
}

async fn list(c: &C) -> anyhow::Result<()> {
    let ctx = &c.ctx;
    let panels = rr::list_panels(ctx.app.db(), &c.g).await?;
    if panels.is_empty() {
        respond(
            ctx,
            info_embed(
                "No Panels",
                Some("This server has no reaction-role panels yet. Create one with `/reactionrole create`, or use `/reactionrole attach` on an existing message."),
                None,
            ),
        )
        .await;
        return Ok(());
    }
    let pages = build_list_pages(&panels, &c.g, panels.len(), c.settings.as_ref());
    paginate(ctx, &pages, PaginateOptions { timeout_ms: 120_000.0, ..Default::default() }).await?;
    Ok(())
}

// ── Entry points ──────────────────────────────────────────────────────────────

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    if !["create", "attach", "add", "remove", "edit", "repost", "delete", "list"].contains(&sub.as_str()) {
        return Ok(());
    }
    // Public replies (admin convention); the delete prompt is private and its result is a public follow-up.
    if !safe_defer(&ctx.responder, sub == "delete").await {
        return Ok(());
    }
    let required: &[&str] = match sub.as_str() {
        "create" => &["channel"],
        "add" => &["role"],
        _ => &[],
    };
    if !resolve_new_options(&mut ctx, NewOptions { required, private: &[] }).await {
        return Ok(());
    }
    let Some(guild) = ctx.guild_id() else { return Ok(()) };
    let g = guild.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await; // may be None: the command still works
    let c = C { ctx, guild, g, settings };
    let res = match sub.as_str() {
        "create" => create(&c).await,
        "attach" => attach(&c).await,
        "add" => add(&c).await,
        "remove" => remove(&c).await,
        "edit" => edit(&c).await,
        "repost" => repost(&c).await,
        "delete" => del(&c).await,
        _ => list(&c).await,
    };
    if let Err(e) = res {
        bot_util::log_error!(format!("reactionrole {sub} failed"), bot_core::error::log_arg(&e));
        respond(&c.ctx, error_embed("Something Went Wrong", Some("I could not save that change. Please try again in a moment."), None)).await;
    }
    Ok(())
}

/// The value of a non-focused string option of the autocomplete (`interaction.options.getString(name)`).
fn autocomplete_string(ctx: &AutocompleteCtx, name: &str) -> Option<String> {
    fn find(opts: &[serenity::all::CommandDataOption], name: &str) -> Option<String> {
        for o in opts {
            match &o.value {
                CommandDataOptionValue::SubCommand(inner) | CommandDataOptionValue::SubCommandGroup(inner) => {
                    if let Some(v) = find(inner, name) {
                        return Some(v);
                    }
                }
                CommandDataOptionValue::String(s) if o.name == name => return Some(s.clone()),
                CommandDataOptionValue::Autocomplete { value, .. } if o.name == name => return Some(value.clone()),
                _ => {}
            }
        }
        None
    }
    find(&ctx.interaction.data.options, name)
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let res: anyhow::Result<()> = async {
        let (name, value) = ctx.focused().unwrap_or_default();
        let q: String = bot_util::js::utf16_prefix(trim(&value), 100).to_string();
        let Some(guild) = ctx.guild_id() else {
            respond_autocomplete(&ctx.responder, &[]).await;
            return Ok(());
        };
        let g = guild.to_string();
        if name == "panel" {
            let rows = rr::search_panels(ctx.app.db(), &g, &q).await?;
            let list: Vec<Choice> = rows.iter().map(|r| Choice::new(rr::panel_choice_name(&r.title, &r.layout, r.item_count), Value::String(r.id.clone()))).collect();
            respond_autocomplete(&ctx.responder, &list).await;
            return Ok(());
        }
        if name == "item" {
            let Some(panel_id) = autocomplete_string(&ctx, "panel") else {
                respond_autocomplete(&ctx.responder, &[]).await;
                return Ok(());
            };
            let ops = discord_ops(&ctx.app);
            let role_ids: Vec<String> = if q.is_empty() {
                Vec::new()
            } else {
                let lower = q.to_lowercase();
                ops.guild_roles(&g).into_iter().filter(|r| r.name.to_lowercase().contains(&lower)).map(|r| r.id).take(100).collect()
            };
            let items = rr::search_items(ctx.app.db(), &g, &panel_id, &q, &role_ids).await?;
            let list: Vec<Choice> = items
                .iter()
                .map(|i| {
                    let role = ops.cached_role(&g, &i.role_id).map(|r| r.name);
                    Choice::new(rr::item_choice_name(&i.label, i.emoji.as_deref(), role.as_deref()), Value::String(i.id.clone()))
                })
                .collect();
            respond_autocomplete(&ctx.responder, &list).await;
            return Ok(());
        }
        respond_autocomplete(&ctx.responder, &[]).await;
        Ok(())
    }
    .await;
    if let Err(e) = res {
        bot_util::log_debug!(format!("reactionrole autocomplete failed: {e}"));
        respond_autocomplete(&ctx.responder, &[]).await;
    }
    Ok(())
}
