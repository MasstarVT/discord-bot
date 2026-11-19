//! Port of `src/commands/community/birthday.js`: `/birthday set | forget | show | next` (MEE6 /remember-birthdays,
//! /forget-birthday, /birthday, /next-birthdays) and the admin `config` group (channel, role, hour, message,
//! message-age, require-year, set-user, unset-user, show, test).
//!
//! `show` and `next` are public and show month and day only; `set` and `forget` answer privately. `gated = false`: the
//! `config` group works while the module is off (its replies then carry a note), so the member subcommands check the
//! module themselves and refuse with the router's wording. `config` replies are public except `test`.

use std::sync::Arc;

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Payload, check_assignable_role_cached, check_channel_usable, safe_defer, safe_reply};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::permissions::permission_names;
use bot_core::prelude::*;
use bot_core::settings::Settings;
use bot_db::GuildSettings;
use bot_util::text::{clamp_text, format_number};
use bot_util::time::{discord_timestamp, zoned_parts, zoned_to_utc};
use serenity::all::{ChannelId, GuildId, RoleId};
use serenity::builder::CreateEmbed;

use crate::birthday_service::{
    self as svc, BirthdayDiscord, DEFAULT_BIRTHDAY_MESSAGE, DEFAULT_BIRTHDAY_MESSAGE_AGE, LocalDate, NEXT_LIMIT, SerenityBirthdays, clamp_hour, code_block,
    format_birthday, format_hour, next_occurrence, parse_birthday_input, pick_template, render_birthday_line, resolve_time_zone, tick_key,
};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("birthday", "Share and see birthdays (MEE6 /remember-birthdays, /birthday, /next-birthdays).")
        .option(
            Opt::sub_command("set", "Remember your birthday on this server (MEE6 /remember-birthdays).")
                .option(Opt::string("date", "MM-DD or YYYY-MM-DD, e.g. 03-14 or 1998-03-14").required(true).min_length(3).max_length(10)),
        )
        .option(Opt::sub_command("forget", "Forget your birthday on this server (MEE6 /forget-birthday)."))
        .option(Opt::sub_command("show", "Show your or a member's birthday (MEE6 /birthday).").option(Opt::user("member", "Member to look up (default: you)")))
        .option(Opt::sub_command("next", "List the next 10 birthdays on this server (MEE6 /next-birthdays)."))
        .option(
            Opt::sub_command_group("config", "Configure birthday announcements and the birthday role (MEE6 Birthdays).")
                .option(
                    Opt::sub_command("channel", "Set the birthday announcement channel; leave empty to turn announcements off.")
                        .option(Opt::channel("channel", "Text or announcement channel (empty = off)").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                        .option(new_channel_option("channel", "channel")),
                )
                .option(
                    Opt::sub_command("role", "Set the role members get on their birthday; leave empty to turn it off.")
                        .option(Opt::role("role", "Birthday role (empty = off)"))
                        .option(new_role_option("role")),
                )
                .option(
                    Opt::sub_command("hour", "Set the hour (server timezone) when birthdays are announced.")
                        .option(Opt::integer("hour", "0–23, e.g. 9 = 09:00 (default 9)").required(true).min_value(0).max_value(23)),
                )
                .option(
                    Opt::sub_command("message", "Set the message used when the age is unknown; leave empty for the default.")
                        .option(Opt::string("text", "Supports {user}, {user.name}, {server} and more").max_length(1000)),
                )
                .option(
                    Opt::sub_command("message-age", "Set the message used when the age is known; leave empty for the default.")
                        .option(Opt::string("text", "Supports {user}, {age}, {server} and more").max_length(1000)),
                )
                .option(
                    Opt::sub_command("require-year", "Require members to include their birth year.")
                        .option(Opt::boolean("enabled", "true = members must use YYYY-MM-DD").required(true)),
                )
                .option(
                    Opt::sub_command("set-user", "Set a member's birthday (MEE6 /set-user-birthday).")
                        .option(Opt::user("member", "The member").required(true))
                        .option(Opt::string("date", "MM-DD or YYYY-MM-DD").required(true).min_length(3).max_length(10)),
                )
                .option(Opt::sub_command("unset-user", "Remove a member's birthday (MEE6 /unset-user-birthday).").option(Opt::user("member", "The member").required(true)))
                .option(Opt::sub_command("show", "Show the birthday settings and any problems."))
                .option(Opt::sub_command("test", "Preview both birthday messages with you as the birthday member.")),
        )
}

/// `/birthday` (`module 'birthdays'`, `gated = false`, MEMBER; every `config` subcommand ADMIN).
pub fn command() -> CommandDef {
    let mut def = CommandDef::new("community", data(), Some(ModuleKey::Birthdays), execute).gated(false).permission_level(PermissionLevel::Member);
    for sub in ["channel", "role", "hour", "message", "message-age", "require-year", "set-user", "unset-user", "show", "test"] {
        def = def.subcommand_permission(format!("config {sub}"), PermissionLevel::Admin);
    }
    def
}

// ── Messages ──────────────────────────────────────────────────────────────────

fn u1() -> CreateEmbed {
    error_embed("Temporarily Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}
fn g1() -> CreateEmbed {
    error_embed("Something Went Wrong", Some("That did not work. Please try again in a moment."), None)
}
fn g2() -> CreateEmbed {
    error_embed("Could Not Save", Some("Something went wrong while saving. Please try again in a moment."), None)
}
fn e1() -> CreateEmbed {
    error_embed("Not a Member", Some("That user is not a member of this server."), None)
}
fn e2() -> CreateEmbed {
    error_embed("Not Allowed", Some("Bots don't have birthdays."), None)
}
/// `N1`: the module-off note of the config replies.
pub const N1: &str = "\n\n⚠️ The **Birthdays** module is off. Turn it on with `/module enable birthdays`.";
/// The router's wording for a gated command whose module is off.
fn off() -> CreateEmbed {
    warn_embed("Module Disabled", Some("The **Birthdays** module is turned off on this server. An admin can enable it with `/module enable birthdays`."), None)
}

/// Every reply: embeds only, and nothing pings.
async fn send(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
}

fn guild_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/birthday outside a guild"))
}

fn discord_of(ctx: &CommandCtx, g: GuildId) -> Arc<dyn BirthdayDiscord> {
    Arc::new(SerenityBirthdays::new(&ctx.app, g))
}

fn hour_f(s: &GuildSettings) -> Option<f64> {
    Some(f64::from(s.birthday_hour))
}

fn local_of(now: &chrono::DateTime<chrono::Utc>, tz: &str) -> LocalDate {
    LocalDate::from(zoned_parts(now, tz))
}

// ── Member subcommands: set | forget | show | next ────────────────────────────

async fn handle_set(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let g = guild_of(ctx)?.to_string();
    let tz = resolve_time_zone(Some(settings));
    let now = ctx.app.clock().now();
    let r = match parse_birthday_input(&ctx.get_string("date").unwrap_or_default(), settings.birthday_require_year, &now, &tz) {
        Ok(r) => r,
        Err(f) => {
            send(ctx, error_embed("Invalid Date", Some(&f.message), None)).await;
            return Ok(());
        }
    };
    let res = svc::save_birthday(&ctx.app, &g, &ctx.user().id.to_string(), r, Some(settings), &now).await?;
    let long = format_birthday(r.month, r.day, r.year, false, true);
    let hh = format_hour(Some(clamp_hour(hour_f(settings)) as f64));
    let mut lines: Vec<String> = Vec::new();
    if res.late {
        lines.push(format!("Happy birthday! 🎉 Today's birthdays were celebrated at **{hh}**, so I'll celebrate you from next year."));
    } else if res.already_celebrated {
        lines.push("You were already celebrated this year, so the new date counts from next year.".into());
    }
    match svc::set_id(&settings.birthday_channel_id) {
        None => lines.push("Birthday announcements aren't set up on this server yet.".into()),
        Some(c) if !res.late && !res.already_celebrated => lines.push(format!("It will be announced in <#{c}> at **{hh}** ({tz}).")),
        _ => {}
    }
    if r.year.is_some() && svc::set_id(&settings.birthday_channel_id).is_some() {
        lines.push("Your age will be included in the announcement.".into());
    }
    let head = if res.created { format!("I'll celebrate your birthday on **{long}**.") } else { format!("Your birthday is now **{long}**.") };
    let body = if lines.is_empty() { head } else { format!("{head}\n\n{}", lines.join("\n")) };
    send(ctx, success_embed(if res.created { "Birthday Saved" } else { "Birthday Updated" }, Some(&body), None)).await;
    Ok(())
}

async fn handle_forget(ctx: &CommandCtx, _: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let g = gid.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await; // None is fine: the role removal is skipped
    let removed = svc::remove_birthday(&ctx.app, discord_of(ctx, gid).as_ref(), &g, &ctx.user().id.to_string(), settings.as_deref()).await?;
    send(
        ctx,
        if removed {
            success_embed("Birthday Forgotten", Some("I've removed your birthday from this server."), None)
        } else {
            info_embed("Nothing to Forget", Some("You haven't shared your birthday on this server."), None)
        },
    )
    .await;
    Ok(())
}

async fn handle_show(ctx: &CommandCtx, _: &Settings) -> anyhow::Result<()> {
    let g = guild_of(ctx)?.to_string();
    let user = ctx.get_user("member").unwrap_or_else(|| ctx.user().clone());
    let is_self = user.id == ctx.user().id;
    let member = ctx.get_member("member").or_else(|| if is_self { ctx.member().cloned() } else { None });
    let Some(member) = member else {
        send(ctx, e1()).await;
        return Ok(());
    };
    let row = if user.bot { None } else { svc::get_birthday(ctx.app.db(), &g, &user.id.to_string()).await? };
    let Some(row) = row else {
        send(
            ctx,
            if is_self {
                info_embed("No Birthday Saved", Some("You haven't shared your birthday yet. Add it with `/birthday set`."), None)
            } else {
                info_embed("No Birthday Saved", Some(&format!("<@{}> hasn't shared their birthday.", member.user.id)), None)
            },
        )
        .await;
        return Ok(());
    };
    let settings = ctx.app.settings().get_settings_safe(&g).await; // None: UTC and the default colour
    let tz = resolve_time_zone(settings.as_deref());
    let now = ctx.app.clock().now();
    let local = local_of(&now, &tz);
    let occ = next_occurrence(row.month as u32, row.day as u32, local);
    let at = zoned_to_utc(occ.year, i64::from(occ.month), i64::from(occ.day), 0, 0, &tz).unwrap_or(now);
    // Never the year or the age (privacy): only the announcement may reveal them.
    let mut desc = format!("<@{}>'s birthday is **{}**.", member.user.id, format_birthday(row.month as u32, row.day as u32, None, false, false));
    if occ == local {
        desc.push_str("\n🎉 It's today!");
    } else {
        desc.push_str(&format!("\nNext: {} ({})", discord_timestamp(&at, "D"), discord_timestamp(&at, "R")));
    }
    send(ctx, guild_embed(settings.as_deref(), Some("🎂 Birthday"), Some(&desc), Some(&format!("Server time zone: {tz}")))).await;
    Ok(())
}

/// The `/birthday next` lines.
pub fn next_lines(list: &[svc::Upcoming]) -> Vec<String> {
    list.iter()
        .map(|e| {
            if e.is_today {
                format!("🎉 **Today** — <@{}>", e.user_id)
            } else {
                format!("**{}** — <@{}> · {}", format_birthday(e.month, e.day, None, true, false), e.user_id, discord_timestamp(&e.at, "R"))
            }
        })
        .collect()
}

async fn handle_next(ctx: &CommandCtx, _: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let g = gid.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let tz = resolve_time_zone(settings.as_deref());
    let list = svc::get_upcoming_birthdays(&ctx.app, discord_of(ctx, gid).as_ref(), &g, settings.as_deref(), NEXT_LIMIT, &ctx.app.clock().now()).await?;
    if list.is_empty() {
        send(ctx, info_embed("No Birthdays Yet", Some("Nobody has shared a birthday yet. Add yours with `/birthday set`."), None)).await;
        return Ok(());
    }
    let body = clamp_text(&next_lines(&list).join("\n"), 4096);
    send(ctx, guild_embed(settings.as_deref(), Some("🎂 Upcoming Birthdays"), Some(&body), Some(&format!("Server time zone: {tz}")))).await;
    Ok(())
}

// ── Config subcommands ────────────────────────────────────────────────────────

/// A database write failed (answered with G2); keeps the original error for the log.
#[derive(Debug)]
struct WriteError(anyhow::Error);

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for WriteError {}

async fn guarded<T>(fut: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    fut.await.map_err(|e| anyhow::Error::new(WriteError(e)))
}

async fn update(ctx: &CommandCtx, patch: SettingsPatch) -> anyhow::Result<Settings> {
    let g = guild_of(ctx)?.to_string();
    guarded(async { ctx.app.settings().update_settings(&g, patch).await.map_err(|e| anyhow::anyhow!("{e}")) }).await
}

fn module_note(s: &GuildSettings) -> &'static str {
    if is_module_enabled(Some(s), Some(ModuleKey::Birthdays)) { "" } else { N1 }
}

fn is_configured(s: &GuildSettings) -> bool {
    svc::set_id(&s.birthday_channel_id).is_some() || svc::set_id(&s.birthday_role_id).is_some()
}

async fn handle_channel(ctx: &CommandCtx, _: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let g = gid.to_string();
    let Some(channel) = ctx.get_channel_id("channel") else {
        let next = update(ctx, SettingsPatch::new().set("birthdayChannelId", serde_json::Value::Null)).await?;
        svc::ensure_tick_job(&ctx.app, &g, Some(&next), true).await; // may cancel the job when the role is unset too
        send(ctx, success_embed("Birthday Announcements Off", Some(&format!("Birthdays will no longer be announced. Saved birthdays are kept.{}", module_note(&next))), None)).await;
        return Ok(());
    };
    let u = check_channel_usable(&ctx.app, gid, Some(channel), &["ViewChannel", "SendMessages"]);
    if !u.ok {
        let text = if u.missing.is_empty() {
            format!("I can't use <#{channel}>: {}.", u.reason.unwrap_or_default())
        } else {
            format!("I can't post in <#{channel}>: missing **{}**.", permission_names(&u.missing).join(", "))
        };
        send(ctx, error_embed("Cannot Use Channel", Some(&text), None)).await;
        return Ok(());
    }
    let next = update(ctx, SettingsPatch::new().set("birthdayChannelId", channel.to_string())).await?;
    svc::ensure_tick_job(&ctx.app, &g, Some(&next), true).await;
    let text = format!(
        "Birthdays will be announced in <#{channel}> at **{}** ({}).{}",
        format_hour(hour_f(&next)),
        resolve_time_zone(Some(&next)),
        module_note(&next)
    );
    send(ctx, success_embed("Birthday Channel Set", Some(&text), None)).await;
    Ok(())
}

async fn handle_role(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let g = gid.to_string();
    let role: Option<RoleId> = ctx.get_role_id("role");
    let old_id = svc::set_id(&settings.birthday_role_id).map(str::to_string);
    let new_id = role.map(|r| r.to_string());
    if let Some(r) = role {
        if Some(r.to_string()) == old_id {
            send(ctx, info_embed("No Change", Some(&format!("<@&{r}> is already the birthday role.")), None)).await;
            return Ok(());
        }
        let chk = check_assignable_role_cached(&ctx.app, gid, r, ctx.member(), false);
        if !chk.ok {
            send(ctx, error_embed("Cannot Use Role", Some(&chk.reason.unwrap_or_default()), None)).await;
            return Ok(());
        }
    }
    let next = update(ctx, SettingsPatch::new().set("birthdayRoleId", new_id.clone())).await?;
    // From now on the tick only removes the current role, so the previous one is taken off today's celebrants here.
    let t = match old_id.as_deref() {
        Some(old) if Some(old) != new_id.as_deref() => Some(svc::transfer_birthday_role(&ctx.app, discord_of(ctx, gid), &g, old, new_id.as_deref()).await),
        _ => None,
    };
    svc::ensure_tick_job(&ctx.app, &g, Some(&next), true).await;
    let mut desc = match role {
        Some(r) => format!(
            "Members get <@&{r}> on their birthday from **{}** until the end of that day ({}).",
            format_hour(hour_f(&next)),
            resolve_time_zone(Some(&next))
        ),
        None => "No role will be given on birthdays.".into(),
    };
    if let Some((removed, failed)) = t {
        if removed > 0 {
            desc.push_str(&format!("\nI removed the previous birthday role from {removed} member(s) who had it today."));
        }
        if failed > 0 {
            desc.push_str(&format!("\n⚠️ I couldn't remove the previous birthday role from {failed} member(s). Please remove it manually."));
        }
    }
    desc.push_str(module_note(&next));
    send(ctx, success_embed(if role.is_some() { "Birthday Role Set" } else { "Birthday Role Off" }, Some(&desc), None)).await;
    Ok(())
}

async fn handle_hour(ctx: &CommandCtx, _: &Settings) -> anyhow::Result<()> {
    let g = guild_of(ctx)?.to_string();
    let hour = ctx.get_integer("hour").unwrap_or(9);
    let next = update(ctx, SettingsPatch::new().set("birthdayHour", hour)).await?;
    svc::ensure_tick_job(&ctx.app, &g, Some(&next), true).await; // only matters when a channel or role is set
    let tz = resolve_time_zone(Some(&next));
    let mut desc = format!("Birthdays will be announced at **{}** ({tz}).", format_hour(Some(hour as f64)));
    if i64::from(zoned_parts(&ctx.app.clock().now(), &tz).hour) >= hour && is_configured(&next) {
        desc.push_str("\nToday's birthdays that haven't been celebrated yet will be celebrated in a moment.");
    }
    desc.push_str(module_note(&next));
    send(ctx, success_embed("Announcement Hour Set", Some(&desc), None)).await;
    Ok(())
}

fn mentions_age(text: &str) -> bool {
    bot_util::js::lower(text).contains("{age}")
}

async fn handle_message(ctx: &CommandCtx, with_age: bool) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let field = if with_age { "birthdayMessageAge" } else { "birthdayMessage" };
    let text = bot_util::js::trim(&ctx.get_string("text").unwrap_or_default()).to_string();
    if text.is_empty() {
        let next = update(ctx, SettingsPatch::new().set(field, serde_json::Value::Null)).await?;
        let def = if with_age { DEFAULT_BIRTHDAY_MESSAGE_AGE } else { DEFAULT_BIRTHDAY_MESSAGE };
        send(ctx, success_embed("Birthday Message Reset", Some(&format!("Back to the default: `{def}`{}", module_note(&next))), None)).await;
        return Ok(());
    }
    let mut rctx = bot_core::adapters::render_context(&ctx.app, Some(gid), ctx.user(), ctx.member(), Some(ctx.channel_id()));
    rctx.user = None; // the JS context has the member, the guild and the channel only
    rctx.vars = Some(vec![("age".into(), bot_util::placeholder_parser::VarValue::Text(if with_age { "25".into() } else { String::new() }))]);
    let d = bot_util::placeholder_parser::render_placeholders_detailed(&text, &rctx, Some(2000));
    let next = update(ctx, SettingsPatch::new().set(field, text.clone())).await?;
    // Warnings never block saving.
    let mut desc = format!("Preview:\n{}", d.text);
    if !d.unknown_tokens.is_empty() {
        let toks: Vec<String> = d.unknown_tokens.iter().take(10).map(|t| format!("`{t}`")).collect();
        desc.push_str(&format!("\n⚠️ Unknown variables (sent as typed): {}", toks.join(", ")));
    }
    if !with_age && mentions_age(&text) {
        desc.push_str("\n⚠️ `{age}` is always empty in this message: it is used when the birth year is unknown.");
    }
    if with_age && !mentions_age(&text) {
        desc.push_str("\nTip: add `{age}` to show the new age.");
    }
    let title = if with_age { "Birthday Message (With Age) Updated" } else { "Birthday Message Updated" };
    send(ctx, success_embed(title, Some(&clamp_text(&format!("{desc}{}", module_note(&next)), 4096)), None)).await;
    Ok(())
}

async fn handle_require_year(ctx: &CommandCtx, _: &Settings) -> anyhow::Result<()> {
    let g = guild_of(ctx)?.to_string();
    let enabled = ctx.get_boolean("enabled").unwrap_or(false);
    let next = update(ctx, SettingsPatch::new().set("birthdayRequireYear", enabled)).await?;
    // Existing rows are never changed.
    if !enabled {
        send(ctx, success_embed("Birth Year Optional", Some(&format!("Members can use **MM-DD** or **YYYY-MM-DD**.{}", module_note(&next))), None)).await;
        return Ok(());
    }
    let without = svc::count_birthdays(ctx.app.db(), &g).await.map(|c| c.1).unwrap_or(0);
    let mut desc = "Members must now use **YYYY-MM-DD**.".to_string();
    if without > 0 {
        desc.push_str(&format!(" {without} saved birthday(s) without a year are kept and use the message without age."));
    }
    desc.push_str(module_note(&next));
    send(ctx, success_embed("Birth Year Required", Some(&desc), None)).await;
    Ok(())
}

async fn handle_set_user(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let g = guild_of(ctx)?.to_string();
    let Some(member) = ctx.get_member("member") else {
        send(ctx, e1()).await;
        return Ok(());
    };
    if member.user.bot {
        send(ctx, e2()).await;
        return Ok(());
    }
    let now = ctx.app.clock().now();
    // An admin may enter MM-DD even when members must give a year.
    let r = match parse_birthday_input(&ctx.get_string("date").unwrap_or_default(), false, &now, &resolve_time_zone(Some(settings))) {
        Ok(r) => r,
        Err(f) => {
            send(ctx, error_embed("Invalid Date", Some(&f.message), None)).await;
            return Ok(());
        }
    };
    let res = guarded(svc::save_birthday(&ctx.app, &g, &member.user.id.to_string(), r, Some(settings), &now)).await?;
    let mut desc = format!("<@{}>'s birthday is now **{}**.", member.user.id, format_birthday(r.month, r.day, None, false, false)); // no year: the reply is public
    if res.late {
        desc.push_str("\nIt is today and today's celebration time has passed, so it counts from next year.");
    } else if res.already_celebrated {
        desc.push_str("\nThey were already celebrated this year, so the new date counts from next year.");
    }
    if let (Some(_), Some(c)) = (r.year, svc::set_id(&settings.birthday_channel_id))
        && mentions_age(&pick_template(Some(settings), Some(1)))
    {
        desc.push_str(&format!("\nBecause a birth year was given, their age will be shown in the public announcement in <#{c}>."));
    }
    desc.push_str(module_note(settings));
    send(ctx, success_embed("Birthday Saved", Some(&desc), None)).await;
    Ok(())
}

async fn handle_unset_user(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    // The user need not be a member any more, so a departed member's data can be removed too.
    let Some(user) = ctx.get_user_id("member") else { anyhow::bail!("Required option \"member\" not found.") };
    let d = discord_of(ctx, gid);
    let removed = guarded(svc::remove_birthday(&ctx.app, d.as_ref(), &gid.to_string(), &user.to_string(), Some(settings))).await?;
    send(
        ctx,
        if removed {
            success_embed("Birthday Removed", Some(&format!("<@{user}>'s birthday was removed.")), None)
        } else {
            info_embed("Nothing to Remove", Some(&format!("<@{user}> has no saved birthday.")), None)
        },
    )
    .await;
    Ok(())
}

/// `channelField(guild, settings)`.
fn channel_field(app: &App, gid: GuildId, s: &GuildSettings) -> String {
    let Some(id) = svc::set_id(&s.birthday_channel_id) else { return "Not set (no announcements)".into() };
    let ch = id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new);
    if ch.and_then(|c| bot_core::interaction_helpers::cached_channel(app, gid, c)).is_none() {
        return "⚠️ Deleted channel — set a new one".into();
    }
    let u = check_channel_usable(app, gid, ch, &["ViewChannel", "SendMessages"]);
    if u.ok {
        return format!("<#{id}>");
    }
    let names = if u.missing.is_empty() { String::new() } else { permission_names(&u.missing).join(", ") };
    format!("<#{id}> ⚠️ missing {}", if names.is_empty() { u.reason.unwrap_or_default() } else { names })
}

/// `roleField(guild, settings)`.
fn role_field(app: &App, gid: GuildId, s: &GuildSettings) -> String {
    let Some(id) = svc::set_id(&s.birthday_role_id) else { return "Not set".into() };
    let role = id.parse::<u64>().ok().filter(|v| *v != 0).map(RoleId::new);
    let exists = role.is_some_and(|r| bot_core::interaction_helpers::cached_guild(app, gid).is_some_and(|g| g.roles.contains_key(&r)));
    let Some(role) = role.filter(|_| exists) else { return "⚠️ Deleted role — set a new one".into() };
    let chk = check_assignable_role_cached(app, gid, role, None, false);
    if chk.ok { format!("<@&{id}>") } else { format!("<@&{id}> ⚠️ {}", chk.reason.unwrap_or_default()) }
}

async fn handle_config_show(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let g = gid.to_string();
    let configured = is_configured(settings);
    if configured {
        svc::ensure_tick_job(&ctx.app, &g, Some(settings), false).await; // self-heal (never fails)
    }
    let key = tick_key(&g);
    let (job, counts) = tokio::join!(ctx.app.scheduler().get_job(&key), svc::count_birthdays(ctx.app.db(), &g));
    let (total, without) = counts?;
    let tz = resolve_time_zone(Some(settings));
    let next = if !configured {
        "Off (no channel or role set)".to_string()
    } else if let Some(j) = job.filter(|j| matches!(j.status, bot_db::JobStatus::Pending | bot_db::JobStatus::Running)) {
        discord_timestamp(&j.run_at, "R")
    } else {
        "⚠️ Not scheduled — run this again in a minute".to_string()
    };
    let message_field = |value: Option<&String>, def: &str| match value {
        Some(v) => code_block(v, 1000),
        None => format!("{}\n(default)", code_block(def, 1000)),
    };
    let mut embed = guild_embed(
        Some(settings),
        Some("🎂 Birthday Settings"),
        Some("Variables: `{user}` `{user.name}` `{user.display_name}` `{server}` `{server.member_count}` `{age}` and the other MEE6 placeholders."),
        None,
    );
    let module = if is_module_enabled(Some(settings), Some(ModuleKey::Birthdays)) { "✅ On" } else { "❌ Off — `/module enable birthdays`" };
    let saved = format!("{}{}", format_number(total as f64), if without > 0 { format!(" ({} without a year)", format_number(without as f64)) } else { String::new() });
    let fields: Vec<(&str, String, bool)> = vec![
        ("Module", module.into(), true),
        ("Channel", channel_field(&ctx.app, gid, settings), true),
        ("Role", role_field(&ctx.app, gid, settings), true),
        ("Announcement hour", format!("**{}** ({tz})", format_hour(hour_f(settings))), true),
        ("Birth year", if settings.birthday_require_year { "Required".into() } else { "Optional".into() }, true),
        ("Saved birthdays", saved, true),
        ("Next check", next, true),
        ("Message (age unknown)", message_field(settings.birthday_message.as_ref(), DEFAULT_BIRTHDAY_MESSAGE), false),
        ("Message (age known)", message_field(settings.birthday_message_age.as_ref(), DEFAULT_BIRTHDAY_MESSAGE_AGE), false),
    ];
    for (name, value, inline) in fields {
        embed = embed.field(name, clamp_text(&value, 1024), inline);
    }
    send(ctx, embed).await;
    Ok(())
}

async fn handle_test(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let gid = guild_of(ctx)?;
    let tz = resolve_time_zone(Some(settings));
    let configured_channel = settings
        .birthday_channel_id
        .as_deref()
        .filter(|c| !c.is_empty())
        .and_then(|c| c.parse::<u64>().ok())
        .filter(|v| *v != 0)
        .map(ChannelId::new)
        .filter(|c| bot_core::interaction_helpers::cached_channel(&ctx.app, gid, *c).is_some());
    let channel = configured_channel.unwrap_or_else(|| ctx.channel_id());
    let line = |age: Option<i64>| {
        let rctx = bot_core::adapters::render_context(&ctx.app, Some(gid), ctx.user(), ctx.member(), Some(channel));
        render_birthday_line(&pick_template(Some(settings), age), rctx, age)
    };
    let mut desc = format!("**Without a birth year**\n{}", line(None));
    desc.push_str(&format!("\n\n**With a birth year** (example age: 25)\n{}", line(Some(25))));
    desc.push_str(&match svc::set_id(&settings.birthday_channel_id) {
        Some(c) => format!("\n\nPosted in <#{c}> at **{}** ({tz}).", format_hour(hour_f(settings))),
        None => "\n\nAnnouncements are off: no channel is set.".into(),
    });
    send(ctx, guild_embed(Some(settings), Some("🎂 Birthday Preview"), Some(&clamp_text(&desc, 4096)), None)).await;
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

async fn execute_member(ctx: CommandCtx, sub: &str) -> BotResult {
    if !["set", "forget", "show", "next"].contains(&sub) {
        return Ok(());
    }
    // set and forget answer privately; show and next are public.
    if !safe_defer(&ctx.responder, sub == "set" || sub == "forget").await {
        return Ok(());
    }
    let r: anyhow::Result<()> = async {
        let g = guild_of(&ctx)?.to_string();
        // The router does not gate this command (the `config` group must work while the module is off).
        let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
            send(&ctx, u1()).await;
            return Ok(());
        };
        if !is_module_enabled(Some(&settings), Some(ModuleKey::Birthdays)) {
            send(&ctx, off()).await;
            return Ok(());
        }
        match sub {
            "set" => handle_set(&ctx, &settings).await,
            "forget" => handle_forget(&ctx, &settings).await,
            "show" => handle_show(&ctx, &settings).await,
            _ => handle_next(&ctx, &settings).await,
        }
    }
    .await;
    if let Err(e) = r {
        bot_util::log_error!(format!("birthday {sub} failed"), bot_core::error::log_arg(&e));
        send(&ctx, g1()).await;
    }
    Ok(())
}

const CONFIG_SUBS: [&str; 10] = ["channel", "role", "hour", "message", "message-age", "require-year", "set-user", "unset-user", "show", "test"];

async fn execute_config(mut ctx: CommandCtx, sub: &str) -> BotResult {
    if !CONFIG_SUBS.contains(&sub) {
        return Ok(());
    }
    // `test` is the one private reply.
    if !safe_defer(&ctx.responder, sub == "test").await {
        return Ok(());
    }
    let r: anyhow::Result<()> = async {
        if !resolve_new_options(&mut ctx, NewOptions::default()).await {
            return Ok(());
        }
        let g = guild_of(&ctx)?.to_string();
        let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
            send(&ctx, u1()).await;
            return Ok(());
        };
        match sub {
            "channel" => handle_channel(&ctx, &settings).await,
            "role" => handle_role(&ctx, &settings).await,
            "hour" => handle_hour(&ctx, &settings).await,
            "message" => handle_message(&ctx, false).await,
            "message-age" => handle_message(&ctx, true).await,
            "require-year" => handle_require_year(&ctx, &settings).await,
            "set-user" => handle_set_user(&ctx, &settings).await,
            "unset-user" => handle_unset_user(&ctx, &settings).await,
            "show" => handle_config_show(&ctx, &settings).await,
            _ => handle_test(&ctx, &settings).await,
        }
    }
    .await;
    if let Err(e) = r {
        match e.downcast_ref::<WriteError>() {
            Some(w) => {
                bot_util::log_error!(format!("birthday config {sub} failed"), bot_core::error::log_arg(&w.0));
                send(&ctx, g2()).await;
            }
            None => {
                bot_util::log_error!(format!("birthday config {sub} failed"), bot_core::error::log_arg(&e));
                send(&ctx, g1()).await;
            }
        }
    }
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    if ctx.subcommand_group().as_deref() == Some("config") {
        return execute_config(ctx, &sub).await;
    }
    execute_member(ctx, &sub).await
}
