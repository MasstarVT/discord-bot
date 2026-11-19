//! Port of `src/commands/admin/captcha.js`: `/captcha setup | enable | disable | status | mode | role | require |
//! channel | timeout | message | lock | verify-existing`. An admin config command: replies are public and it works while
//! the module is off (`gated = false`). The rules are in [`crate::captcha_service`].

use std::sync::{Arc, Mutex};

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, PermTarget, Payload, await_confirm, cached_channel, cached_guild, check_assignable_role, check_channel_usable,
    missing_bot_permissions, respond_autocomplete, safe_reply,
};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_db::{CaptchaMode, GuildSettings};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildId, Permissions, Role, RoleId};
use serenity::builder::{CreateEmbed, EditInteractionResponse};

use crate::captcha_service::panel::{DiscordFailure, parse_snowflake};
use crate::captcha_service::*;

const TXT: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn modes(o: Opt) -> Opt {
    o.choice("Image code", "IMAGE").choice("Button click", "BUTTON")
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("captcha", "Captcha verification for new members (MEE6 Welcome › Captcha verification).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("setup", "Create or reuse #verification and the role, then post the panel.")
                .option(Opt::channel("channel", "Verification channel (default: create one)").channel_types(&TXT))
                .option(new_channel_option("channel", "channel"))
                .option(Opt::role("role", "Verified role (default: create one)"))
                .option(new_role_option("role"))
                .option(modes(Opt::string("mode", "How members verify"))),
        )
        .option(Opt::sub_command("enable", "Turn verification on."))
        .option(Opt::sub_command("disable", "Turn verification off (settings are kept)."))
        .option(Opt::sub_command("status", "Show the settings and any problems."))
        .option(Opt::sub_command("mode", "Image code or one-click button.").option(modes(Opt::string("type", "How members verify").required(true))))
        .option(Opt::sub_command("role", "Set the verified role.").option(Opt::role("role", "Verified role")).option(new_role_option("role")))
        .option(
            Opt::sub_command("require", "Roles needed to verify, and a form for those without (no options: show).")
                .option(Opt::role("role", "Role to require (any one is enough)"))
                .option(Opt::boolean("remove", "Remove that role instead"))
                .option(Opt::boolean("clear", "Remove every required role"))
                .option(Opt::string("form", "Fallback application form; none clears").autocomplete(true).max_length(100)),
        )
        .option(
            Opt::sub_command("channel", "Move the panel to another channel.")
                .option(Opt::channel("channel", "Verification channel").channel_types(&TXT))
                .option(new_channel_option("channel", "channel")),
        )
        .option(
            Opt::sub_command("timeout", "Kick members who do not verify in time.")
                .option(Opt::integer("minutes", "Minutes after joining, 5-10080; 0 = never").required(true).min_value(0).max_value(10080)),
        )
        .option(
            Opt::sub_command("message", "Edit the panel (no options opens a form).")
                .option(Opt::string("button_label", "Verify button text").max_length(80))
                .option(
                    Opt::string("button_color", "Verify button colour")
                        .choice("Green", "SUCCESS")
                        .choice("Blurple", "PRIMARY")
                        .choice("Grey", "SECONDARY")
                        .choice("Red", "DANGER"),
                )
                .option(Opt::boolean("reset", "Restore the default panel")),
        )
        .option(
            Opt::sub_command("lock", "Hide channels until members verify (MEE6 deny View Channels).")
                .option(Opt::string("state", "on or off").required(true).choice("on", "on").choice("off", "off")),
        )
        .option(
            Opt::sub_command("verify-existing", "Give the verified role to current members.")
                .option(Opt::boolean("include_pending", "Also verify members still waiting"))
                .option(Opt::boolean("stop", "Stop a running batch")),
        )
}

/// The command (`module 'captcha'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Captcha), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

const LOCK_TIP: &str = "• Unverified members can still see every channel @everyone can see. Hide them with `/captcha lock state:on`.";

fn unavailable() -> CreateEmbed {
    error_embed("Temporarily Unavailable", Some("Settings could not be loaded. Try again in a moment."), None)
}

fn config_err(message: impl Into<String>, title: Option<&str>) -> anyhow::Error {
    CaptchaConfigError::new(message, title).into()
}

async fn edit(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/captcha outside a guild"))
}

fn user_tag(ctx: &CommandCtx) -> String {
    bot_core::adapters::user_tag(ctx.user())
}

fn locked(app: &App, g: GuildId) -> bool {
    bot_core::interaction_helpers::with_cached_guild(app, g, is_locked).unwrap_or(false)
}

fn cached_role(app: &App, g: GuildId, id: Option<RoleId>) -> Option<Role> {
    let id = id?;
    bot_core::interaction_helpers::with_cached_guild(app, g, |guild| guild.roles.get(&id).cloned()).flatten()
}

fn role_of_settings(app: &App, g: GuildId, s: &GuildSettings) -> Option<Role> {
    cached_role(app, g, s.captcha_role_id.as_deref().and_then(parse_snowflake).map(RoleId::new))
}

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    match run(&mut ctx, &sub).await {
        Err(e) => match e.downcast::<CaptchaConfigError>() {
            Ok(c) => {
                let title = c.title.clone().unwrap_or_else(|| "Not Saved".into());
                safe_reply(&ctx.responder, Payload::embed(error_embed(&title, Some(&c.message), None))).await;
                Ok(())
            }
            // The router's generic handler.
            Err(e) => Err(e),
        },
        ok => ok,
    }
}

fn has_any_option(ctx: &CommandCtx) -> bool {
    ctx.get_string("button_label").is_some() || ctx.get_string("button_color").is_some() || ctx.get_boolean("reset").is_some()
}

async fn run(ctx: &mut CommandCtx, sub: &str) -> BotResult {
    // showModal is the first response.
    if sub == "message" && !has_any_option(ctx) {
        return open_panel_modal(ctx).await;
    }
    // awaitConfirm replies first.
    if sub == "lock" {
        return run_lock(ctx).await;
    }
    // Public (admin convention).
    ctx.defer_reply(false).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let required: &[&str] = if sub == "setup" { &[] } else { &["role", "channel"] };
    if !resolve_new_options(ctx, NewOptions { required, private: &[] }).await {
        return Ok(());
    }
    let g = guild_of(ctx)?;
    // A memo hit: the router loaded it.
    let s = ctx.app.settings().get_settings(&g.to_string()).await?;
    match sub {
        "setup" => do_setup(ctx, g, &s).await,
        "enable" => do_enable(ctx, g).await,
        "disable" => do_disable(ctx, g).await,
        "status" => {
            let embed = build_status_embed(&ctx.app, g, &s).await;
            edit(ctx, embed).await
        }
        "mode" => do_mode(ctx, g).await,
        "role" => do_role(ctx, g, &s).await,
        "require" => do_require(ctx, g, &s).await,
        "channel" => do_channel(ctx, g, &s).await,
        "timeout" => do_timeout(ctx, g).await,
        "message" => do_message(ctx, g).await,
        "verify-existing" => do_verify_existing(ctx, g, &s).await,
        _ => edit(ctx, error_embed("Unknown Subcommand", Some("That subcommand does not exist."), None)).await,
    }
}

// ── setup ─────────────────────────────────────────────────────────────────────

/// What the bot needs to create #verification.
pub const CHANNEL_CREATE_NEEDS: [&str; 8] =
    ["ManageChannels", "ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory", "AddReactions", "CreatePublicThreads", "CreatePrivateThreads"];

fn message_url(g: GuildId, c: ChannelId, m: serenity::all::MessageId) -> String {
    format!("https://discord.com/channels/{g}/{c}/{m}")
}

async fn do_setup(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    let app = &ctx.app;
    let role_opt = ctx.get_role_id("role");
    let channel_opt = ctx.get_channel_id("channel");
    let mode = ctx.get_string("mode").and_then(|m| CaptchaMode::parse(&m));

    let stored_channel = s.captcha_channel_id.as_deref().and_then(parse_snowflake).map(ChannelId::new).filter(|c| cached_channel(app, g, *c).is_some());
    let stored_usable = stored_channel.is_some_and(|c| check_channel_usable(app, g, Some(c), &PANEL_NEEDS).ok);
    let will_create_channel = channel_opt.is_none() && !stored_usable;
    let mut needs = vec!["ManageRoles"];
    if will_create_channel {
        needs.extend(CHANNEL_CREATE_NEEDS);
    }
    let missing = missing_bot_permissions(app, PermTarget::Guild(g), &needs);
    if !missing.is_empty() {
        return Err(config_err(format!("I need **{}** in this server to set up verification.", permission_names(&missing).join(", ")), Some("Missing Permissions")));
    }

    let mut created: Vec<String> = Vec::new();
    let reason = audit_reason(&user_tag(ctx), "captcha setup");

    // Role: the option, else the stored one, else a new `verified` role with no permissions.
    let role_id: RoleId = if let Some(r) = role_opt {
        let picked = ctx.get_role("role");
        let chk = check_verified_role_resolved(app, g, r, picked.as_ref(), Some(s), ctx.member());
        if !chk.ok {
            return Err(config_err(chk.reason.unwrap_or_default(), Some("Cannot Use This Role")));
        }
        adopt_resolved_role(app, g, picked.as_ref());
        r
    } else if let Some(stored) = role_of_settings(app, g, s) {
        let chk = check_verified_role_cached(app, g, stored.id, Some(s), None);
        if !chk.ok {
            return Err(config_err(format!("{} Pick another role with the `role` option.", chk.reason.unwrap_or_default()), Some("Cannot Use This Role")));
        }
        stored.id
    } else {
        let http = app.http().ok_or_else(|| anyhow::Error::from(DiscordFailure::offline()))?;
        let body = json!({ "name": DEFAULT_ROLE_NAME, "permissions": "0", "hoist": false, "mentionable": false });
        let role = http.create_role(g, &body, Some(&reason)).await.map_err(|e| anyhow::Error::from(DiscordFailure::of(&e)))?;
        bot_core::discord::cache_created_role(app, g, &role);
        created.push(format!("role <@&{}>", role.id));
        role.id
    };

    // Channel: the option, else the stored usable one, else a new #verification.
    let channel_id: ChannelId = if let Some(c) = channel_opt {
        // discord.js resolves the option channel even when the cache lacks it: fetched (and cached) here.
        let chk = bot_core::interaction_helpers::check_channel_usable_fetch(app, g, Some(c), &PANEL_NEEDS).await;
        if !chk.ok {
            return Err(config_err(chk.reason.unwrap_or_default(), Some("Cannot Use That Channel")));
        }
        c
    } else if let (true, Some(c)) = (stored_usable, stored_channel) {
        c
    } else {
        match create_verification_channel(app, g, &reason).await {
            Ok(ch) => {
                created.push(format!("channel <#{}>", ch.id));
                ch.id
            }
            Err(f) => {
                let lead = if created.is_empty() {
                    "I could not create the channel".to_string()
                } else {
                    format!("I created {} but could not create the channel", created.join(" and "))
                };
                return edit(ctx, error_embed("Setup Incomplete", Some(&format!("{lead}: {}", f.text())), None)).await;
            }
        }
    };

    let mut next = s.clone();
    next.captcha_role_id = Some(role_id.to_string());
    next.captcha_channel_id = Some(channel_id.to_string());
    if let Some(m) = mode {
        next.captcha_mode = m;
    }
    let msg = match publish_panel(app, g, channel_id, &next, s.captcha_channel_id.as_deref(), s.captcha_message_id.as_deref()).await {
        Ok(m) => m,
        Err(e) => {
            if let (Some(c), false) = (e.downcast_ref::<CaptchaConfigError>(), created.is_empty()) {
                return Err(config_err(format!("{}\n\nI created {} before this failed; nothing was saved.", c.message, created.join(" and ")), c.title.as_deref()));
            }
            return Err(e);
        }
    };

    let mut patch = SettingsPatch::new()
        .set("captchaEnabled", true)
        .set("captchaRoleId", role_id.to_string())
        .set("captchaChannelId", channel_id.to_string())
        .set("captchaMessageId", msg.id.to_string());
    if let Some(m) = mode {
        patch = patch.set("captchaMode", m.as_str());
    }
    let after = app.settings().update_settings(&g.to_string(), patch).await?;

    let mut lines = vec![
        format!("**Channel:** <#{channel_id}> ([panel]({}))", message_url(g, channel_id, msg.id)),
        format!("**Role:** <@&{role_id}>"),
        format!("**Mode:** {}", if after.captcha_mode == CaptchaMode::Button { "Button click" } else { "Image code" }),
    ];
    if !created.is_empty() {
        lines.push(format!("**Created:** {}", created.join(", ")));
    }
    lines.push(String::new());
    lines.push(format!("• New members must verify in <#{channel_id}> before they get <@&{role_id}> and their autoroles."));
    if !locked(app, g) {
        lines.push(LOCK_TIP.into());
    }
    lines.push(format!("• Give <@&{role_id}> to members who are already here with `/captcha verify-existing`."));
    if after.captcha_timeout_minutes == 0 {
        lines.push("• Optionally kick members who never verify: `/captcha timeout`.".into());
    }
    for p in diagnose_captcha(app, g, &after, false, None).await {
        lines.push(format!("⚠️ {p}"));
    }
    edit(ctx, success_embed("Verification Ready", Some(&lines.join("\n")), None)).await
}

// ── enable / disable ──────────────────────────────────────────────────────────

async fn do_enable(ctx: &CommandCtx, g: GuildId) -> BotResult {
    let app = &ctx.app;
    let after = app.settings().update_settings(&g.to_string(), SettingsPatch::new().set("captchaEnabled", true)).await?;
    let problems = diagnose_captcha(app, g, &after, true, None).await;
    let role = role_of_settings(app, g, &after);
    let mut text = format!(
        "New members must verify before they get {} and their autoroles.",
        role.map(|r| format!("<@&{}>", r.id)).unwrap_or_else(|| "the verified role".into())
    );
    if !problems.is_empty() {
        let list: Vec<String> = problems.iter().map(|p| format!("• {p}")).collect();
        text.push_str(&format!("\n\n**Fix these first:**\n{}", list.join("\n")));
    } else if !locked(app, g) {
        text.push_str(&format!("\n\n{LOCK_TIP}"));
    }
    edit(ctx, success_embed("Verification Enabled", Some(&text), None)).await
}

async fn do_disable(ctx: &CommandCtx, g: GuildId) -> BotResult {
    let app = &ctx.app;
    let pending = count_pending(app.db(), &g.to_string()).await?;
    let after = app.settings().update_settings(&g.to_string(), SettingsPatch::new().set("captchaEnabled", false)).await?;
    let mut text = "New members no longer need to verify. The panel, role and settings are kept.".to_string();
    if pending > 0 {
        text.push_str(&format!(
            "\n\n**{}** member(s) were still waiting to verify. They will not be kicked, but they have not received their autoroles. To let them in now, run `/captcha verify-existing include_pending:true`.",
            bot_util::text::format_number(pending as f64)
        ));
    }
    if let (true, Some(role)) = (locked(app, g), role_of_settings(app, g, &after)) {
        text.push_str(&format!(
            "\n\n⚠️ The lock-down is still on: members without <@&{}> only see <#{}>. Undo it with `/captcha lock state:off`.",
            role.id,
            after.captcha_channel_id.clone().unwrap_or_else(|| "null".into())
        ));
    }
    edit(ctx, success_embed("Verification Disabled", Some(&text), None)).await
}

// ── mode / role / channel / timeout ───────────────────────────────────────────

async fn do_mode(ctx: &CommandCtx, g: GuildId) -> BotResult {
    let t = ctx.get_string("type").ok_or_else(|| anyhow::anyhow!("Required option \"type\" not found."))?;
    ctx.app.settings().update_settings(&g.to_string(), SettingsPatch::new().set("captchaMode", t.clone())).await?;
    let mut text = if t == "IMAGE" {
        "Members click **Verify** and type the 6-character code from an image (3 tries per image; codes expire after 10 minutes).".to_string()
    } else {
        "Members click **Verify** once. This stops accidental joins and simple bots, but not determined ones.".to_string()
    };
    if t == "IMAGE" && !is_image_captcha_available() {
        text.push_str(&format!("\n⚠️ {}", msg::FONT_MISSING));
    }
    edit(ctx, success_embed("Verification Mode Set", Some(&text), None)).await
}

async fn do_role(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    let app = &ctx.app;
    let role_id = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    let picked = ctx.get_role("role");
    let chk = check_verified_role_resolved(app, g, role_id, picked.as_ref(), Some(s), ctx.member());
    if !chk.ok {
        return Err(config_err(chk.reason.unwrap_or_default(), Some("Cannot Use This Role")));
    }
    adopt_resolved_role(app, g, picked.as_ref());
    let rid = role_id.to_string();
    if s.captcha_role_id.as_deref() == Some(rid.as_str()) {
        return edit(ctx, info_embed("No Change", Some(&format!("<@&{rid}> is already the verified role.")), None)).await;
    }
    app.settings().update_settings(&g.to_string(), SettingsPatch::new().set("captchaRoleId", rid.clone())).await?;
    let mut lines = vec![format!("<@&{rid}> is given to members after they verify.")];
    if let Some(old) = role_of_settings(app, g, s) {
        lines.push(format!(
            "Members who verified with <@&{}> don't have this role. Run `/captcha verify-existing` to give it to everyone already here.",
            old.id
        ));
    }
    if s.autorole_ids.contains(&rid) {
        lines.push("This role is also an autorole. Autoroles are given after verification, so members get it then.".into());
    }
    let role_views = cached_role(app, g, Some(role_id))
        .or(picked)
        .is_some_and(|r| bot_core::permissions::js_has(r.permissions, Permissions::VIEW_CHANNEL));
    if locked(app, g) && !role_views {
        lines.push("⚠️ The server is locked: run `/captcha lock state:on` again so this role gets View Channels.".into());
    }
    edit(ctx, success_embed("Verified Role Set", Some(&lines.join("\n")), None)).await
}

// ── require (required-role gate and fallback form) ────────────────────────────

/// The `form` choices: the guild's application forms (any state: the status explains a closed one) and `none`.
pub async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let Some((name, value)) = ctx.focused() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    if name != "form" {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let g = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let query = bot_util::js::trim(&value).to_lowercase();
    match feat_welcome::application_service::find_forms_for_autocomplete(ctx.app.db(), &g, &value, false).await {
        Ok(forms) => {
            let mut choices: Vec<Choice> =
                forms.into_iter().map(|f| Choice::new(format!("{}{}", if f.enabled { "" } else { "⏸️ " }, f.name), f.id)).collect();
            if query.is_empty() || "none".contains(&query) {
                choices.insert(0, Choice::new("none (no form)", "none"));
            }
            respond_autocomplete(&ctx.responder, &choices).await;
        }
        Err(e) => {
            bot_util::log_debug!(format!("[captcha] autocomplete failed: {e}"));
            respond_autocomplete(&ctx.responder, &[]).await;
        }
    }
    Ok(())
}

fn require_outcome_text(id: &str, outcome: RequireOutcome) -> Option<String> {
    match outcome {
        RequireOutcome::Added => Some(format!("<@&{id}> is now required.")),
        RequireOutcome::Already => Some(format!("<@&{id}> is already required.")),
        RequireOutcome::Removed => Some(format!("<@&{id}> is no longer required.")),
        RequireOutcome::Absent => Some(format!("<@&{id}> was not required.")),
        _ => None,
    }
}

async fn do_require(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    let app = &ctx.app;
    let gid = g.to_string();
    // The resolved option role (the cached one when Discord sent no resolved data).
    let role = ctx.get_role("role").or_else(|| cached_role(app, g, ctx.get_role_id("role")));
    let remove = ctx.get_boolean("remove") == Some(true);
    let clear = ctx.get_boolean("clear") == Some(true);
    let form_text = ctx.get_string("form").map(|f| bot_util::js::trim(&f).to_string()).filter(|f| !f.is_empty());

    if clear && (role.is_some() || remove) {
        return Err(config_err("Use `clear` on its own (it can go with `form`).", None));
    }
    if remove && role.is_none() {
        return Err(config_err("Pick the role to remove in `role`.", None));
    }
    if let (Some(r), false) = (&role, remove) {
        let chk = check_required_role(&r.id.to_string(), r.managed, r.tags.guild_connections, &gid, s.captcha_role_id.as_deref());
        if !chk.ok {
            return Err(config_err(chk.reason.unwrap_or_default(), Some("Cannot Require This Role")));
        }
    }

    // None = leave it, Some(None) = clear it.
    let mut form_id: Option<Option<String>> = None;
    if let Some(text) = &form_text {
        if text.eq_ignore_ascii_case("none") {
            form_id = Some(None);
        } else {
            let form = feat_welcome::application_service::find_form(app.db(), &gid, text).await?;
            let Some(form) = form else {
                return Err(config_err(
                    format!(
                        "No application form is named \"{}\". Pick one from the list, or create it with `/application form create`.",
                        bot_util::text::clamp_text(text, 80)
                    ),
                    Some("Form Not Found"),
                ));
            };
            form_id = Some(Some(form.id));
        }
    }

    let asked = role.is_some() || clear || form_id.is_some();
    let mut plan = RolePlan { role_ids: s.captcha_required_role_ids.clone(), outcome: RequireOutcome::Unchanged, changed: false, dropped: 0 };
    let mut form_changed = false;
    let mut after: Settings = Arc::new(s.clone());
    if asked {
        let guild_role_ids: Vec<String> = cached_guild(app, g).map(|gd| gd.roles.keys().map(ToString::to_string).collect()).unwrap_or_default();
        let out: Arc<Mutex<Option<(RolePlan, bool)>>> = Arc::new(Mutex::new(None));
        let out2 = out.clone();
        let add = role.as_ref().filter(|_| !remove).map(|r| r.id.to_string());
        let rm = role.as_ref().filter(|_| remove).map(|r| r.id.to_string());
        let fid = form_id.clone();
        after = app
            .settings()
            .update_settings_with(&gid, move |cur| {
                // Judged on the locked row: the cap and duplicates.
                let plan = plan_required_roles(&RequireChange {
                    current: &cur.captcha_required_role_ids,
                    guild_role_ids: &guild_role_ids,
                    add_id: add.as_deref(),
                    remove_id: rm.as_deref(),
                    clear,
                    max: None,
                })
                .map_err(|reason| anyhow::Error::from(CaptchaConfigError::new(reason, Some("Not Saved"))))?;
                let changed_form = fid.as_ref().is_some_and(|f| cur.captcha_fallback_form_id.as_deref().filter(|c| !c.is_empty()) != f.as_deref());
                let mut patch = SettingsPatch::new();
                if plan.changed {
                    patch = patch.set("captchaRequiredRoleIds", json!(plan.role_ids));
                }
                if changed_form {
                    patch = patch.set("captchaFallbackFormId", fid.clone().flatten().map(Value::from).unwrap_or(Value::Null));
                }
                if let Ok(mut o) = out2.lock() {
                    *o = Some((plan, changed_form));
                }
                Ok(patch)
            })
            .await?;
        if let Some((p, f)) = out.lock().ok().and_then(|mut o| o.take()) {
            plan = p;
            form_changed = f;
        }
    }

    let mut lines: Vec<String> = Vec::new();
    if let Some(r) = &role
        && let Some(t) = require_outcome_text(&r.id.to_string(), plan.outcome)
    {
        lines.push(t);
    }
    if clear {
        lines.push(
            if plan.outcome == RequireOutcome::Cleared && plan.changed { "Every required role was removed: anyone can verify again." } else { "No role was required." }
                .into(),
        );
    }
    if let Some(f) = &form_id {
        let line = match (form_changed, f.is_none()) {
            (false, true) => "No fallback form was set.",
            (false, false) => "That form was already the fallback form.",
            (true, true) => "The fallback form was cleared.",
            (true, false) => "Members without a required role are offered that form.",
        };
        lines.push(line.into());
    }
    if plan.dropped > 0 {
        lines.push(format!("Dropped {} deleted role(s) from the list.", plan.dropped));
    }
    if !lines.is_empty() {
        lines.push(String::new());
    }

    let fallback = inspect_fallback_form(app, g, &after, &[]).await;
    let guild = cached_guild(app, g);
    let exists = |id: &str| guild.as_ref().is_some_and(|gd| parse_snowflake(id).is_some_and(|r| gd.roles.contains_key(&RoleId::new(r))));
    let (roles, form) = describe_requirements(exists, &after, &fallback);
    lines.push(format!("**Required roles** (any one of them): {roles}"));
    lines.push(format!("**Fallback form:** {form}"));

    let live = after.captcha_required_role_ids.iter().filter(|id| exists(id) && Some(id.as_str()) != after.captcha_role_id.as_deref()).count();
    if live > 0 {
        lines.push(String::new());
        lines.push("Members who already have the verified role are not checked, and roles given by staff or `/captcha verify-existing` skip the check.".into());
        if after.captcha_timeout_minutes > 0 {
            lines.push(format!(
                "⚠️ Unverified members are kicked after {} (`/captcha timeout`), including those who never get a required role.",
                bot_util::parse_duration::format_duration_long(f64::from(after.captcha_timeout_minutes) * 60_000.0)
            ));
        }
    } else if after.captcha_fallback_form_id.as_deref().is_some_and(|f| !f.is_empty()) {
        lines.push(String::new());
        lines.push("⚠️ The form only appears for members who lack a required role: add one with `role`.".into());
    }
    let text = bot_util::text::clamp_text(&lines.join("\n"), 4000);
    let embed = if asked && (plan.changed || form_changed) {
        success_embed("Requirements Updated", Some(&text), None)
    } else {
        info_embed(if asked { "No Change" } else { "Verification Requirements" }, Some(&text), None)
    };
    edit(ctx, embed).await
}

async fn do_channel(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    let app = &ctx.app;
    let channel = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    // discord.js resolves the option channel even when the cache lacks it: fetched (and cached) here.
    let chk = bot_core::interaction_helpers::check_channel_usable_fetch(app, g, Some(channel), &PANEL_NEEDS).await;
    if !chk.ok {
        return Err(config_err(chk.reason.unwrap_or_default(), Some("Cannot Use That Channel")));
    }
    let msg = publish_panel(app, g, channel, s, s.captcha_channel_id.as_deref(), s.captcha_message_id.as_deref()).await?;
    app.settings()
        .update_settings(&g.to_string(), SettingsPatch::new().set("captchaChannelId", channel.to_string()).set("captchaMessageId", msg.id.to_string()))
        .await?;
    let mut lines = vec![format!("The verification panel is now in <#{channel}> ([jump]({})).", message_url(g, channel, msg.id))];
    let hidden = bot_core::interaction_helpers::with_cached_guild(app, g, |gd| {
        is_locked(gd) && bot_core::permissions::find_channel(gd, channel).is_some_and(|c| !everyone_can_view(gd, c))
    })
    .unwrap_or(false);
    if hidden {
        lines.push("⚠️ The server is locked but @everyone can't see this channel. Run `/captcha lock state:on` again to fix it.".into());
    }
    if s.captcha_role_id.as_deref().is_none_or(str::is_empty) {
        lines.push("Set the verified role with `/captcha setup` or `/captcha role`.".into());
    }
    edit(ctx, success_embed("Panel Moved", Some(&lines.join("\n")), None)).await
}

async fn do_timeout(ctx: &CommandCtx, g: GuildId) -> BotResult {
    let app = &ctx.app;
    let n = ctx.get_integer("minutes").ok_or_else(|| anyhow::anyhow!("Required option \"minutes\" not found."))?;
    let minutes = validate_timeout_minutes(n).map_err(|e| config_err(e, None))?;
    app.settings().update_settings(&g.to_string(), SettingsPatch::new().set("captchaTimeoutMinutes", minutes)).await?;
    if minutes == 0 {
        let n = app
            .scheduler()
            .cancel_jobs(CancelJobs { guild_id: Some(g.to_string()), job_type: Some(JobType::CaptchaTimeout.as_str().to_string()), ..Default::default() })
            .await;
        let extra = if n > 0 { format!(" Cancelled {n} scheduled kick(s).") } else { String::new() };
        return edit(ctx, success_embed("Timeout Off", Some(&format!("Unverified members are never kicked.{extra}")), None)).await;
    }
    let mut text = format!(
        "Members who haven't verified **{}** after joining are kicked (they can rejoin). Members already waiting keep the deadline they got when they joined.",
        bot_util::parse_duration::format_duration_long(minutes as f64 * 60_000.0)
    );
    if !missing_bot_permissions(app, PermTarget::Guild(g), &["KickMembers"]).is_empty() {
        text.push_str("\n⚠️ I need **Kick Members** to remove unverified members.");
    }
    edit(ctx, success_embed("Timeout Set", Some(&text), None)).await
}

// ── message ───────────────────────────────────────────────────────────────────

async fn open_panel_modal(ctx: &CommandCtx) -> BotResult {
    let g = guild_of(ctx)?;
    // The memo, else a lookup bounded by the acknowledgement budget.
    let Some(s) = settings_for_ack(&ctx.app, &g.to_string(), ctx.ack_budget_ms()).await else {
        safe_reply(&ctx.responder, Payload::embed(unavailable()).ephemeral(true)).await;
        return Ok(());
    };
    let modal = bot_core::custom_embed::build_template_modal(custom_ids::MODAL_PANEL, Some("Verification panel"), &panel_template(Some(&s)), false)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    ctx.show_modal(modal).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}

async fn do_message(ctx: &CommandCtx, g: GuildId) -> BotResult {
    let app = &ctx.app;
    let label = ctx.get_string("button_label").map(|l| bot_util::js::trim(&l).to_string());
    let color = ctx.get_string("button_color");
    let reset = ctx.get_boolean("reset");

    if reset.is_some() && (label.is_some() || color.is_some()) {
        return Err(config_err("Use reset on its own.", None));
    }
    if reset == Some(true) {
        let after = reset_panel_template(app, &g.to_string()).await?;
        let extra = match refresh_panel(app, g, &after).await {
            Ok(()) => String::new(),
            Err(r) => format!("\n{r}"),
        };
        return edit(ctx, success_embed("Panel Reset", Some(&format!("The default verification panel is back.{extra}")), None)).await;
    }
    if reset == Some(false) && label.is_none() && color.is_none() {
        return Err(config_err("Choose an option, or run `/captcha message` with no options to open the form.", None));
    }
    if label.as_deref() == Some("") {
        return Err(config_err("The button label cannot be empty.", None));
    }

    let (l2, c2) = (label.clone(), color.clone());
    let after = app
        .settings()
        .update_settings_with(&g.to_string(), move |cur| {
            let mut tpl = panel_template(Some(&cur));
            let mut btn = parse_verify_button(cur.captcha_template.as_ref().and_then(|t| t.get("verifyButton")));
            if let Some(l) = l2.filter(|l| !l.is_empty()) {
                btn.label = bot_util::text::clamp_text(&l, 80);
            }
            if let Some(c) = c2.filter(|c| !c.is_empty()) {
                btn.style = c;
            }
            if let Some(o) = tpl.as_object_mut() {
                o.insert("verifyButton".into(), btn.to_json());
            }
            Ok(SettingsPatch::new().set("captchaTemplate", tpl))
        })
        .await?;
    let r = refresh_panel(app, g, &after).await;
    let b = parse_verify_button(after.captcha_template.as_ref().and_then(|t| t.get("verifyButton")));
    let extra = match r {
        Ok(()) => String::new(),
        Err(reason) => format!("\n{reason}"),
    };
    edit(ctx, success_embed("Verify Button Updated", Some(&format!("Label **{}**, colour **{}**.{extra}", b.label, b.style_name())), None)).await
}

// ── lock ──────────────────────────────────────────────────────────────────────

async fn run_lock(ctx: &CommandCtx) -> BotResult {
    let state = ctx.get_string("state").ok_or_else(|| anyhow::anyhow!("Required option \"state\" not found."))?;
    let g = guild_of(ctx)?;
    let Some(s) = settings_for_ack(&ctx.app, &g.to_string(), ctx.ack_budget_ms()).await else {
        safe_reply(&ctx.responder, Payload::embed(unavailable()).ephemeral(true)).await;
        return Ok(());
    };
    if state == "on" { lock_on(ctx, g, &s).await } else { lock_off(ctx, g, &s).await }
}

async fn reply_public(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.reply(serenity::builder::CreateInteractionResponseMessage::new().embed(embed)).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
    Ok(())
}

async fn final_edit(ctx: &CommandCtx, embed: CreateEmbed, what: &str) -> BotResult {
    if let Err(e) = ctx.edit_reply(EditInteractionResponse::new().embed(embed).components(Vec::new())).await {
        bot_util::log_debug!(format!("captcha {what} reply: {e}"));
    }
    Ok(())
}

async fn lock_on(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    let app = &ctx.app;
    let guild = cached_guild(app, g);
    let role = guild.as_ref().and_then(|gd| s.captcha_role_id.as_deref().and_then(parse_snowflake).and_then(|r| gd.roles.get(&RoleId::new(r)).cloned()));
    let channel = s.captcha_channel_id.as_deref().and_then(parse_snowflake).and_then(|c| cached_channel(app, g, ChannelId::new(c)));
    let (Some(guild), Some(role), Some(channel)) = (guild, role, channel) else {
        return reply_public(ctx, error_embed("Set Up First", Some("Run `/captcha setup` first: the lock needs a verified role and a verification channel."), None)).await;
    };
    let bot = bot_id(app).unwrap_or(serenity::all::UserId::new(1));
    let assignable = check_assignable_role(&guild, Some(&role), bot, None, false);
    if !assignable.ok {
        let text = format!("{} I need to edit <@&{}>'s permissions.", assignable.reason.unwrap_or_default(), role.id);
        return reply_public(ctx, error_embed("Cannot Lock", Some(&text), None)).await;
    }
    if !bot_sees_without_everyone(&guild, bot) {
        return reply_public(
            ctx,
            error_embed(
                "Cannot Lock",
                Some("My own role doesn't have **View Channels**, so locking would hide the server from me too. Give my role View Channels in **Server Settings → Roles**, then try again."),
                None,
            ),
        )
        .await;
    }
    let missing = missing_bot_permissions(app, PermTarget::Channel(g, channel.id), &["ViewChannel", "ManageRoles"]);
    if !missing.is_empty() {
        let names: Vec<String> = permission_names(&missing).into_iter().map(|n| if n == "Manage Roles" { "Manage Permissions".to_string() } else { n }).collect();
        let text = format!("I need **{}** in <#{}> so unverified members can still see it.", names.join(", "), channel.id);
        return reply_public(ctx, error_embed("Cannot Lock", Some(&text), None)).await;
    }

    let plan = lock_plan(&guild, &role, &channel);
    if !plan.channel && !plan.role && !plan.everyone {
        let text = format!("Members without <@&{}> only see <#{}> and channels that explicitly allow @everyone.", role.id, channel.id);
        return reply_public(ctx, info_embed("Already Locked", Some(&text), None)).await;
    }

    let mut lines = vec!["This will:".to_string()];
    if plan.channel {
        lines.push(format!("• let @everyone see <#{}>, so new members can verify", channel.id));
    }
    if plan.role {
        lines.push(format!("• give <@&{}> the **View Channels** permission", role.id));
    }
    lines.push(if plan.everyone { "• remove **View Channels** from @everyone, server-wide".into() } else { "• (@everyone already can't view channels)".into() });
    lines.push(String::new());
    lines.push(format!(
        "Members without <@&{}> will then only see <#{}> and channels that explicitly allow @everyone. **Give the role to members already here first** with `/captcha verify-existing`, or they lose access too.",
        role.id, channel.id
    ));
    if guild.members.len() as u64 >= guild.member_count {
        let n = guild.members.values().filter(|m| !m.roles.contains(&role.id)).count();
        lines.push(format!("**{}** current member(s) don't have <@&{}> yet.", bot_util::text::format_number(n as f64), role.id));
    }
    if get_bulk_run(app, &g.to_string()).is_some() {
        lines.push("A verify-existing batch is still running.".into());
    }
    let everyone_perms = guild.roles.get(&RoleId::new(g.get())).map(|r| r.permissions).unwrap_or_else(Permissions::empty);
    if bot_core::permissions::has_exact(everyone_perms, Permissions::ADMINISTRATOR) {
        lines.push(String::new());
        lines.push("⚠️ @everyone has **Administrator**, so everyone still sees every channel: the lock has no effect until you remove it.".into());
    }

    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("🔒 Lock the server until members verify?".into()),
            description: Some(lines.join("\n")),
            confirm_label: "Lock".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            timeout_ms: 60_000.0,
            ephemeral: false,
        },
    )
    .await;
    // awaitConfirm already showed "Cancelled" or "Timed out".
    if !ok {
        return Ok(());
    }

    // Remember whether @everyone could view channels before the lock, so `lock off` never opens a private server.
    let prior = bot_core::interaction_helpers::with_cached_guild(app, g, |gd| !is_locked(gd)).unwrap_or(!plan.everyone);
    let res = apply_lock(app, g, role.id, channel.id, plan, &audit_reason(&user_tag(ctx), "captcha lock-down")).await;
    let embed = if let Some(err) = res.error {
        let done = if res.done.is_empty() { "nothing".to_string() } else { res.done.join(", ") };
        error_embed("Lock Incomplete", Some(&format!("{err}\n\nDone: {done}.")), None)
    } else {
        let saved = app
            .settings()
            .update_settings_with(&g.to_string(), move |cur| {
                Ok(if cur.captcha_lockdown_prior.is_none() { SettingsPatch::new().set("captchaLockdownPrior", prior) } else { SettingsPatch::new() })
            })
            .await;
        if let Err(e) = saved {
            bot_util::log_warn!(format!("Captcha: could not store the lock-down prior state in {g}"), bot_core::error::log_arg(&e));
        }
        success_embed(
            "Server Locked",
            Some(&format!(
                "Members without <@&{}> now only see <#{}> (and channels that explicitly allow @everyone). Undo with `/captcha lock state:off`.",
                role.id, channel.id
            )),
            None,
        )
    };
    final_edit(ctx, embed, "lock").await
}

async fn lock_off(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    let app = &ctx.app;
    let gid = g.to_string();
    if !locked(app, g) {
        if s.captcha_lockdown_prior.is_some() {
            let _ = app.settings().update_settings(&gid, SettingsPatch::new().set("captchaLockdownPrior", Value::Null)).await;
        }
        return reply_public(ctx, info_embed("Not Locked", Some("@everyone can already see channels."), None)).await;
    }
    if !missing_bot_permissions(app, PermTarget::Guild(g), &["ManageRoles"]).is_empty() {
        return reply_public(ctx, error_embed("Cannot Unlock", Some("I need **Manage Roles** to change @everyone."), None)).await;
    }
    if s.captcha_lockdown_prior == Some(false) {
        app.settings().update_settings(&gid, SettingsPatch::new().set("captchaLockdownPrior", Value::Null)).await?;
        return reply_public(
            ctx,
            info_embed("Lock-down Cleared", Some("The server was private before the lock-down, so @everyone stays without View Channels."), None),
        )
        .await;
    }
    let mut description =
        "This gives **View Channels** back to @everyone, so everyone can see channels whether or not they verified. Verification keeps running.".to_string();
    if s.captcha_lockdown_prior.is_none() {
        description.push_str("\n\nIf @everyone was not allowed to view channels before you locked the server, do not unlock.");
    }
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("🔓 Unlock the server?".into()),
            description: Some(description),
            confirm_label: "Unlock".into(),
            danger: false,
            timeout_ms: 60_000.0,
            ephemeral: false,
            ..Default::default()
        },
    )
    .await;
    if !ok {
        return Ok(());
    }
    let res = remove_lock(app, g, &audit_reason(&user_tag(ctx), "captcha unlock")).await;
    if res.is_ok()
        && let Err(e) = app.settings().update_settings(&gid, SettingsPatch::new().set("captchaLockdownPrior", Value::Null)).await
    {
        bot_util::log_warn!(format!("Captcha: could not clear the lock-down prior state in {gid}"), e.to_string());
    }
    let embed = match res {
        Err(e) => error_embed("Unlock Failed", Some(&e), None),
        Ok(()) => success_embed(
            "Server Unlocked",
            Some("@everyone can see channels again. The verified role keeps **View Channels**, and the verification channel overwrite stays."),
            None,
        ),
    };
    final_edit(ctx, embed, "unlock").await
}

// ── verify-existing ───────────────────────────────────────────────────────────

async fn do_verify_existing(ctx: &CommandCtx, g: GuildId, s: &GuildSettings) -> BotResult {
    if ctx.get_boolean("stop") == Some(true) {
        return match stop_bulk_run(&ctx.app, &g.to_string()) {
            None => edit(ctx, info_embed("Nothing Running", Some("No verify-existing batch is running."), None)).await,
            Some(progress) => edit(ctx, info_embed("Stopping", Some(&format!("Stopping after the current member ({progress}).")), None)).await,
        };
    }
    // Edits the deferred reply itself and returns once the batch is started (it is not awaited).
    let rep = BatchReporter {
        app: ctx.app.clone(),
        responder: ctx.responder.clone(),
        guild_id: g,
        channel_id: ctx.channel_id(),
        created_ms: bot_core::interaction::created_ms(ctx.interaction.id) as i64,
    };
    start_verify_existing(rep, s, ctx.member(), ctx.get_boolean("include_pending") == Some(true)).await?;
    Ok(())
}
