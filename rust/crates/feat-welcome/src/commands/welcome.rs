//! Port of `src/commands/admin/welcome.js`: `/welcome` with its `dm`, `autorole` and `goodbye` groups. An ADMIN config
//! command (`gated = false`: configuration works while the module is off); replies are public. The logic is in
//! [`crate::welcome_service`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, Payload, cached_channel, cached_guild, respond_autocomplete};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use serenity::all::{GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::welcome_service::{
    CardStyleOptions, Diagnose, EmbedOptions, Kind, WelcomeConfigError, add_autorole_resolved, autorole_list_lines, autorole_notes, build_setup_modal,
    build_status_embed, build_test_preview, card_channel_warning, card_summary, check_config_channel, delay_text, describe_embed_settings,
    diagnose_welcome, guild_features, message_info_embed, message_update_embed, prune_missing_autoroles, remove_autorole, render_ctx,
    resolve_autoroles, resolve_template, save_settings, set_autorole_delay, settings_for_ack, update_card_style, update_embed_options,
    update_message, welcome_channel_needs, with_card_preview,
};

const TXT: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn thumb() -> Opt {
    Opt::string("thumbnail", "Small corner image").choice("Member avatar", "avatar").choice("Server icon", "server").choice("None", "none")
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("welcome", "Welcome & Goodbye: join message, card, DM, autoroles and goodbye (MEE6 Welcome & Goodbye).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("enable", "Turn the Welcome & Goodbye module on."))
        .option(Opt::sub_command("disable", "Turn the Welcome & Goodbye module off (settings are kept)."))
        .option(Opt::sub_command("status", "Show all welcome, DM, goodbye and autorole settings."))
        .option(
            Opt::sub_command("channel", "Set the channel for welcome messages.")
                .option(Opt::channel("channel", "Welcome channel").channel_types(&TXT))
                .option(new_channel_option("channel", "channel")),
        )
        .option(
            Opt::sub_command("message", "Set the welcome text; {user} pings the new member.")
                .option(Opt::string("text", "Text with variables such as {user} and {server}").max_length(2000))
                .option(Opt::boolean("enabled", "Post the welcome message in the channel"))
                .option(Opt::boolean("reset", "Use the default text")),
        )
        .option(
            Opt::sub_command("embed", "Send text only or text + embed; set embed images.")
                .option(Opt::boolean("enabled", "true = text + embed, false = text only"))
                .option(thumb())
                .option(Opt::string("image_url", "Large image URL, {user.avatar}, or none").max_length(2048))
                .option(Opt::boolean("reset", "Use the default embed")),
        )
        .option(Opt::sub_command("setup", "Edit the welcome text and embed in a form."))
        .option(
            Opt::sub_command("card", "Welcome card image: on/off and style.")
                .option(Opt::boolean("enabled", "Attach the welcome card"))
                .option(Opt::string("background", "Background colour, e.g. #23272A").max_length(16))
                .option(Opt::string("background_url", "Background image URL, or none").max_length(2048))
                .option(Opt::string("text_color", "Text colour, e.g. #FFFFFF").max_length(16))
                .option(Opt::string("accent_color", "Accent colour, e.g. #5865F2").max_length(16))
                .option(Opt::number("overlay", "Darkening over the background image, 0 to 1").min_value(0).max_value(1))
                .option(Opt::boolean("reset", "Reset the card style")),
        )
        .option(
            Opt::sub_command("test", "Preview the welcome as if you just joined.")
                .option(Opt::string("type", "What to preview").choice("Channel message", "channel").choice("Welcome DM", "dm")),
        )
        .option(
            Opt::sub_command_group("dm", "Welcome DM sent to new members.")
                .option(Opt::sub_command("enable", "DM new members when they join."))
                .option(Opt::sub_command("disable", "Stop the welcome DM."))
                .option(
                    Opt::sub_command("message", "Set the DM text.")
                        .option(Opt::string("text", "Text with variables such as {server}").max_length(2000))
                        .option(Opt::boolean("reset", "Use the default text")),
                )
                .option(Opt::sub_command("setup", "Edit the DM text and embed in a form."))
                .option(Opt::sub_command("card", "Attach the welcome card to the DM.").option(Opt::boolean("enabled", "Attach the card").required(true))),
        )
        .option(
            Opt::sub_command_group("autorole", "Roles given to new members (MEE6 welcome role).")
                .option(
                    Opt::sub_command("add", "Add a role given to new members.")
                        .option(Opt::role("role", "Role to give"))
                        .option(new_role_option("role"))
                        .option(Opt::string("delay", "Delay for all autoroles, e.g. 10m (0 = at once, max 7d)").max_length(32)),
                )
                .option(
                    Opt::sub_command("remove", "Remove an autorole, including deleted roles.")
                        .option(Opt::string("role", "Autorole to remove").required(true).autocomplete(true)),
                )
                .option(
                    Opt::sub_command("delay", "Set the delay before autoroles are given.")
                        .option(Opt::string("duration", "e.g. 10m or 2h; 0 = at once; max 7d").required(true).max_length(32)),
                )
                .option(Opt::sub_command("list", "List the autoroles and the delay.")),
        )
        .option(
            Opt::sub_command_group("goodbye", "Goodbye message when a member leaves (MEE6 Goodbye message).")
                .option(Opt::sub_command("enable", "Post a goodbye message when members leave."))
                .option(Opt::sub_command("disable", "Stop goodbye messages."))
                .option(
                    Opt::sub_command("channel", "Set the goodbye channel.")
                        .option(Opt::channel("channel", "Goodbye channel").channel_types(&TXT))
                        .option(new_channel_option("channel", "channel")),
                )
                .option(
                    Opt::sub_command("message", "Set the goodbye text (mentions do not ping).")
                        .option(Opt::string("text", "Text, e.g. **{user.idname}** just left the server").max_length(2000))
                        .option(Opt::boolean("reset", "Use the default text")),
                )
                .option(
                    Opt::sub_command("embed", "Send the goodbye as text or as an embed.")
                        .option(Opt::boolean("enabled", "true = embed, false = text only"))
                        .option(thumb())
                        .option(Opt::boolean("reset", "Use the default embed")),
                )
                .option(Opt::sub_command("setup", "Edit the goodbye text and embed in a form."))
                .option(Opt::sub_command("test", "Preview the goodbye message here.")),
        )
}

/// The command (`module 'welcome'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Welcome), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

fn unavailable() -> CreateEmbed {
    error_embed("Temporarily Unavailable", Some("Settings could not be loaded. Try again in a moment."), None)
}

/// Lines about the goodbye channel only: the module and sub-toggle lines have their own wording here.
fn channel_warnings(app: &App, guild_id: GuildId, s: &GuildSettings) -> Vec<String> {
    diagnose_welcome(app, guild_id, s, Diagnose::Goodbye).into_iter().filter(|l| !(l.contains("module is off") || l.contains("goodbye message is off"))).collect()
}

// ── Autocomplete ───────────────────────────────────────────────────────────────

/// `/welcome autorole remove role`: the configured autoroles, deleted ones included.
pub fn autorole_choices(guild: Option<&serenity::all::Guild>, autorole_ids: &[String], query: &str) -> Vec<Choice> {
    let q = query.to_lowercase();
    autorole_ids
        .iter()
        .map(|id| {
            let role = guild.and_then(|g| id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|r| g.roles.get(&serenity::all::RoleId::new(r))));
            let name = match role {
                Some(r) => format!("@{}", r.name),
                None => format!("Deleted role ({id})"),
            };
            (name, id.clone())
        })
        .filter(|(name, value)| name.to_lowercase().contains(&q) || value.contains(&q))
        .take(25)
        .map(|(name, value)| Choice::new(name, value))
        .collect()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    if name != "role" || ctx.subcommand().as_deref() != Some("remove") {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
    let settings = settings_for_ack(&ctx.app, &guild_id.to_string(), budget).await;
    let guild = cached_guild(&ctx.app, guild_id);
    let ids = settings.map(|s| s.autorole_ids.clone()).unwrap_or_default();
    respond_autocomplete(&ctx.responder, &autorole_choices(guild.as_ref(), &ids, &value)).await;
    Ok(())
}

// ── Execute ────────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let key = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub,
    };
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();

    // Forms and previews are acknowledged by showModal / buildTestPreview themselves, so settings come from the memo
    // or a bounded lookup before any defer.
    if matches!(key.as_str(), "setup" | "dm setup" | "goodbye setup" | "test" | "goodbye test") {
        let Some(settings) = settings_for_ack(&ctx.app, &g, ctx.ack_budget_ms()).await else {
            ctx.reply(Payload::embed(unavailable()).ephemeral(true).to_reply()).await?;
            return Ok(());
        };
        return match key.as_str() {
            "goodbye test" => build_test_preview(&ctx, &settings, Kind::Goodbye).await,
            "test" => {
                let kind = if ctx.get_string("type").as_deref() == Some("dm") { Kind::Dm } else { Kind::Welcome };
                build_test_preview(&ctx, &settings, kind).await
            }
            other => {
                let kind = match other {
                    "dm setup" => Kind::Dm,
                    "goodbye setup" => Kind::Goodbye,
                    _ => Kind::Welcome,
                };
                ctx.show_modal(build_setup_modal(kind, Some(&settings))).await?;
                Ok(())
            }
        };
    }

    ctx.defer_reply(false).await?;
    let settings = match ctx.app.settings().get_settings(&g).await {
        Ok(s) => s,
        Err(e) => {
            bot_util::log_warn!(format!("/welcome could not load settings for {g}: {e}"));
            ctx.edit_reply(Payload::embed(unavailable()).to_edit()).await?;
            return Ok(());
        }
    };
    let reply_ctx = ctx.clone();
    match run(ctx, &key, &settings).await {
        Ok(()) => Ok(()),
        Err(e) => match e.downcast_ref::<WelcomeConfigError>() {
            Some(c) => {
                let embed = error_embed(c.title.as_deref().unwrap_or("Not Saved"), Some(&c.message), None);
                reply_ctx.edit_reply(Payload::embed(embed).to_edit()).await?;
                Ok(())
            }
            None => Err(e),
        },
    }
}

/// `enabledParts(s)`: what `/welcome enable` turned on (the DM and the goodbye have their own switches, so only the
/// ones that are on are named).
pub fn enabled_parts(dm_enabled: bool, goodbye_enabled: bool) -> String {
    let mut on = vec!["Welcome messages"];
    let mut off = Vec::new();
    if dm_enabled {
        on.push("welcome DMs");
    } else {
        off.push("the welcome DM (`/welcome dm enable`)");
    }
    if goodbye_enabled {
        on.push("goodbye messages");
    } else {
        off.push("goodbye messages (`/welcome goodbye enable`)");
    }
    let list = |items: &[&str]| -> String {
        if items.len() > 1 { format!("{} and {}", items[..items.len() - 1].join(", "), items[items.len() - 1]) } else { items.first().map(|s| s.to_string()).unwrap_or_default() }
    };
    let tail = if off.is_empty() { String::new() } else { format!(" Still off: {}.", list(&off)) };
    format!("{} are on.{tail}", list(&on))
}

async fn reply(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.edit_reply(Payload::embed(embed).to_edit()).await?;
    Ok(())
}

const MODULE_OFF: &str = "⚠️ The Welcome & Goodbye module is off (`/welcome enable`).";

async fn run(mut ctx: CommandCtx, key: &str, s: &GuildSettings) -> BotResult {
    if !resolve_new_options(&mut ctx, NewOptions { required: &["channel", "role"], private: &[] }).await {
        return Ok(());
    }
    let app = ctx.app.clone();
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/welcome outside a guild"))?;
    let g = guild_id.to_string();
    let render = || {
        let guild = cached_guild(&app, guild_id);
        let channel = cached_channel(&app, guild_id, ctx.channel_id());
        render_ctx(guild.as_ref(), ctx.member(), Some(ctx.user()), channel.as_ref())
    };

    match key {
        "enable" => {
            let after = save_settings(&app, &g, SettingsPatch::new().set("welcomeEnabled", true)).await?;
            let problems = diagnose_welcome(&app, guild_id, &after, Diagnose::Welcome);
            let tail = if problems.is_empty() { "Preview it with `/welcome test`.".to_string() } else { problems.join("\n") };
            reply(&ctx, success_embed("Welcome & Goodbye Enabled", Some(&format!("{}\n{tail}", enabled_parts(after.welcome_dm_enabled, after.goodbye_enabled))), None)).await
        }
        "disable" => {
            save_settings(&app, &g, SettingsPatch::new().set("welcomeEnabled", false)).await?;
            reply(
                &ctx,
                success_embed(
                    "Welcome & Goodbye Disabled",
                    Some("Welcome messages, welcome DMs and goodbye messages are off. Autoroles keep working; remove them with `/welcome autorole remove`."),
                    None,
                ),
            )
            .await
        }
        "status" => reply(&ctx, build_status_embed(&app, guild_id, s)).await,
        "channel" => {
            let picked = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
            if let Err(m) = check_config_channel(&app, guild_id, picked, &welcome_channel_needs(Some(s))).await {
                return reply(&ctx, error_embed("Cannot Use That Channel", Some(&m), None)).await;
            }
            let after = save_settings(&app, &g, SettingsPatch::new().set("welcomeChannelId", picked.to_string())).await?;
            let mut lines = vec![format!("Welcome messages will be posted in <#{picked}>.")];
            if !after.welcome_enabled {
                lines.push(MODULE_OFF.into());
            }
            reply(&ctx, success_embed("Welcome Channel Set", Some(&lines.join("\n")), None)).await
        }
        "message" | "dm message" => {
            let kind = if key == "message" { Kind::Welcome } else { Kind::Dm };
            let text = ctx.get_string("text");
            let reset = ctx.get_boolean("reset") == Some(true);
            let enabled = if kind == Kind::Welcome { ctx.get_boolean("enabled") } else { None };
            let title = if kind == Kind::Welcome { "Welcome Text" } else { "Welcome DM Text" };
            if text.is_none() && !reset && enabled.is_none() {
                return reply(&ctx, message_info_embed(title, kind, s)).await;
            }
            let after = update_message(&app, &g, kind, text.as_deref(), reset, enabled).await?.unwrap_or_else(|| std::sync::Arc::new(s.clone()));
            let updated = if kind == Kind::Welcome { "Welcome Text Updated" } else { "Welcome DM Text Updated" };
            reply(&ctx, message_update_embed(updated, kind, &after, text.as_deref(), reset, enabled, &render())).await
        }
        "embed" => {
            let opts = EmbedOptions {
                enabled: ctx.get_boolean("enabled"),
                thumbnail: ctx.get_string("thumbnail"),
                image_url: ctx.get_string("image_url"),
                reset: ctx.get_boolean("reset") == Some(true),
            };
            let after = update_embed_options(&app, &g, Kind::Welcome, opts).await?;
            reply(&ctx, success_embed("Welcome Embed Updated", Some(&describe_embed_settings(Kind::Welcome, &after)), None)).await
        }
        "card" => {
            let opts = CardStyleOptions {
                enabled: ctx.get_boolean("enabled"),
                background: ctx.get_string("background"),
                background_url: ctx.get_string("background_url"),
                text_color: ctx.get_string("text_color"),
                accent_color: ctx.get_string("accent_color"),
                overlay: ctx.get_number("overlay"),
                reset: ctx.get_boolean("reset") == Some(true),
            };
            if !opts.given() {
                let embed = info_embed("Welcome Card", Some(&card_summary(s)), None);
                let p = with_card_preview(&app, embed, ctx.member(), s).await;
                ctx.edit_reply(p.to_edit()).await?;
                return Ok(());
            }
            let after = update_card_style(&app, &g, opts).await?;
            let mut lines = vec![card_summary(&after)];
            let warning = card_channel_warning(&app, guild_id, &after);
            if !warning.is_empty() {
                lines.push(String::new());
                lines.push(warning);
            }
            let p = with_card_preview(&app, success_embed("Welcome Card Updated", Some(&lines.join("\n")), None), ctx.member(), &after).await;
            ctx.edit_reply(p.to_edit()).await?;
            Ok(())
        }
        "dm enable" => {
            let after = save_settings(&app, &g, SettingsPatch::new().set("welcomeDmEnabled", true)).await?;
            let mut lines = vec!["New members will get a DM. Members with closed DMs are skipped silently.".to_string()];
            if !after.welcome_enabled {
                lines.push(MODULE_OFF.into());
            }
            let r = resolve_template(Kind::Dm, Some(&after), false);
            if r.content.is_none() && r.embed.is_none() && !after.welcome_dm_card {
                lines.push("⚠️ Nothing would be sent: the DM has no text, embed or card.".into());
            }
            reply(&ctx, success_embed("Welcome DM Enabled", Some(&lines.join("\n")), None)).await
        }
        "dm disable" => {
            save_settings(&app, &g, SettingsPatch::new().set("welcomeDmEnabled", false)).await?;
            reply(&ctx, success_embed("Welcome DM Disabled", Some("New members will no longer get a welcome DM."), None)).await
        }
        "dm card" => {
            let enabled = ctx.get_boolean("enabled").ok_or_else(|| anyhow::anyhow!("Required option \"enabled\" not found."))?;
            save_settings(&app, &g, SettingsPatch::new().set("welcomeDmCard", enabled)).await?;
            let (title, text) = if enabled {
                ("Welcome DM Card On", "The welcome card is attached to the DM.")
            } else {
                ("Welcome DM Card Off", "The welcome DM is sent without the card.")
            };
            reply(&ctx, success_embed(title, Some(text), None)).await
        }
        "autorole add" => {
            let role = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
            let delay = ctx.get_string("delay");
            let picked = ctx.get_role("role");
            crate::welcome_service::adopt_resolved_role(&app, guild_id, picked.as_ref());
            let (after, already) = add_autorole_resolved(&app, guild_id, ctx.member(), role, picked.as_ref(), delay.as_deref()).await?;
            let mut lines = vec![format!("<@&{role}> is given to new members {}.", delay_text(i64::from(after.autorole_delay)))];
            if already {
                lines.insert(0, "Already an autorole; the delay was updated.".into());
            }
            lines.extend(autorole_notes(&guild_features(&app, guild_id), Some(&after)));
            reply(&ctx, success_embed(if already { "Autorole Delay Updated" } else { "Autorole Added" }, Some(&lines.join("\n")), None)).await
        }
        "autorole remove" => {
            let input = ctx.get_string("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
            let (_, id) = remove_autorole(&app, guild_id, s, &input).await?;
            let Some(id) = id else {
                return reply(&ctx, error_embed("Not an Autorole", Some("That role is not in the autorole list. Use `/welcome autorole list`."), None)).await;
            };
            let exists = cached_guild(&app, guild_id).is_some_and(|gd| id.parse::<u64>().ok().is_some_and(|r| gd.roles.contains_key(&serenity::all::RoleId::new(r))));
            let text = if exists { format!("<@&{id}> is no longer given to new members.") } else { format!("Removed the deleted role `{id}` from the list.") };
            reply(&ctx, success_embed("Autorole Removed", Some(&text), None)).await
        }
        "autorole delay" => {
            let input = ctx.get_string("duration").ok_or_else(|| anyhow::anyhow!("Required option \"duration\" not found."))?;
            let (_, seconds) = set_autorole_delay(&app, &g, &input).await?;
            let text = format!("Autoroles are given {}. Members already waiting keep the schedule they got when they joined.", delay_text(seconds));
            reply(&ctx, success_embed("Autorole Delay Set", Some(&text), None)).await
        }
        "autorole list" => {
            let guild = cached_guild(&app, guild_id);
            let (_, missing) = resolve_autoroles(guild.as_ref(), Some(s));
            let mut current = s.clone();
            let mut pruned = 0;
            if let Some(gd) = guild.as_ref()
                && !missing.is_empty()
            {
                let existing: Vec<String> = gd.roles.keys().map(|r| r.to_string()).collect();
                pruned = prune_missing_autoroles(&app, &g, &missing, existing.clone()).await?;
                current.autorole_ids.retain(|id| existing.contains(id));
            }
            if current.autorole_ids.is_empty() {
                return reply(&ctx, info_embed("No Autoroles", Some("Add one with `/welcome autorole add`."), None)).await;
            }
            let lines = autorole_list_lines(&app, guild_id, &current, pruned);
            let text = bot_util::text::join_with_limit(&lines, "\n", 4000, bot_util::text::default_more);
            reply(&ctx, apply_guild_color(info_embed("Autoroles", Some(&text), None), Some(&current))).await
        }
        "goodbye enable" => {
            let after = save_settings(&app, &g, SettingsPatch::new().set("goodbyeEnabled", true)).await?;
            let mut lines = vec!["A message is posted when a member leaves.".to_string()];
            if !after.welcome_enabled {
                lines.push("⚠️ The Welcome & Goodbye module is off. Nothing is posted until an admin runs `/welcome enable`.".into());
            }
            lines.extend(channel_warnings(&app, guild_id, &after));
            reply(&ctx, success_embed("Goodbye Enabled", Some(&lines.join("\n")), None)).await
        }
        "goodbye disable" => {
            save_settings(&app, &g, SettingsPatch::new().set("goodbyeEnabled", false)).await?;
            reply(&ctx, success_embed("Goodbye Disabled", Some("No goodbye messages will be posted."), None)).await
        }
        "goodbye channel" => {
            let picked = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
            let mut needs = vec!["ViewChannel", "SendMessages"];
            if s.goodbye_use_embed {
                needs.push("EmbedLinks");
            }
            if let Err(m) = check_config_channel(&app, guild_id, picked, &needs).await {
                return reply(&ctx, error_embed("Cannot Use That Channel", Some(&m), None)).await;
            }
            let after = save_settings(&app, &g, SettingsPatch::new().set("goodbyeChannelId", picked.to_string())).await?;
            let mut lines = vec![format!("Goodbye messages will be posted in <#{picked}>.")];
            if !after.welcome_enabled {
                lines.push(MODULE_OFF.into());
            }
            if !after.goodbye_enabled {
                lines.push("⚠️ The goodbye message is off (`/welcome goodbye enable`).".into());
            }
            reply(&ctx, success_embed("Goodbye Channel Set", Some(&lines.join("\n")), None)).await
        }
        "goodbye message" => {
            let text = ctx.get_string("text");
            let reset = ctx.get_boolean("reset") == Some(true);
            if text.is_none() && !reset {
                return reply(&ctx, message_info_embed("Goodbye Text", Kind::Goodbye, s)).await;
            }
            let after = update_message(&app, &g, Kind::Goodbye, text.as_deref(), reset, None).await?.unwrap_or_else(|| std::sync::Arc::new(s.clone()));
            reply(&ctx, message_update_embed("Goodbye Text Updated", Kind::Goodbye, &after, text.as_deref(), reset, None, &render())).await
        }
        "goodbye embed" => {
            let opts = EmbedOptions {
                enabled: ctx.get_boolean("enabled"),
                thumbnail: ctx.get_string("thumbnail"),
                image_url: None,
                reset: ctx.get_boolean("reset") == Some(true),
            };
            let after = update_embed_options(&app, &g, Kind::Goodbye, opts).await?;
            reply(&ctx, success_embed("Goodbye Embed Updated", Some(&describe_embed_settings(Kind::Goodbye, &after)), None)).await
        }
        _ => reply(&ctx, error_embed("Unknown Subcommand", Some("That subcommand is not available."), None)).await,
    }
}
