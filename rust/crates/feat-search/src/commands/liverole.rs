//! Port of `src/commands/admin/liverole.js`: `/liverole set | require | enable | disable | announce | status`. An admin
//! config command: replies are public and it keeps working while the Live Role module is off (`gated = false`). The
//! engine is [`crate::plugins::liverole`].

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{
    ConfirmOptions, Payload, RoleCheck, await_confirm, channel_usable_verdict, check_assignable_role, is_interaction_gone, pingable_role_ids, safe_defer,
    safe_reply,
};
use bot_core::new_targets::{NewOptions, new_role_option, resolve_new_options};
use bot_core::prelude::*;
use bot_util::placeholder_parser::{VarValue, render_placeholders_detailed};
use bot_util::text::{clamp_text, format_number, join_with_limit};
use serde_json::{Value, json};
use serenity::all::{ChannelId, Guild, GuildChannel, GuildId, Permissions, Role, RoleId, UserId};
use serenity::builder::CreateEmbed;

use crate::ops::{self, snowflake};
use crate::plugins::liverole::{
    DEFAULT_LIVE_ANNOUNCE_MESSAGE, Mode, ProblemInput, SyncResult, count_losing_holders, format_sync_result, is_streaming, limits, live_role_problems,
    load_members, stream_label, sync_guild,
};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("liverole", "Give a role while members stream (MEE6 Twitch Alerts live-streaming role).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("set", "Set the role members get while they are live.")
                .option(Opt::role("role", "A dedicated role, e.g. @Live."))
                .option(new_role_option("role")),
        )
        .option(
            Opt::sub_command("require", "Only give the live role to members with a role.").option(Opt::role("role", "Required role (leave empty for everyone).")),
        )
        .option(Opt::sub_command("enable", "Turn the live role on and re-sync it."))
        .option(
            Opt::sub_command("disable", "Turn the live role off.")
                .option(Opt::boolean("remove_roles", "Remove it from members who have it now (default: yes).")),
        )
        .option(
            Opt::sub_command("announce", "Post a stream link when a member goes live.")
                .option(
                    Opt::channel("channel", "Where to post. Leave every option empty to stop.")
                        .channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]),
                )
                .option(Opt::string("message", "Text with {url}, {platform}, {title}; \"default\" resets.").max_length(limits::ANNOUNCE_MAX_CHARS))
                .option(Opt::role("ping_role", "Role to ping in each post."))
                .option(Opt::boolean("clear_ping_role", "Stop pinging a role.")),
        )
        .option(Opt::sub_command("status", "Show the setup and who is live now."))
}

/// The command (`module 'liverole'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Liverole), execute).gated(false).permission_level(PermissionLevel::Admin)
}

// ── Shared pieces ─────────────────────────────────────────────────────────────

/// The `MSG` embeds.
pub mod msg {
    use super::*;

    pub fn everyone() -> CreateEmbed {
        error_embed("Invalid Role", Some("@everyone can't be the live role. Pick or create a dedicated role such as @Live."), None)
    }
    pub fn same_as_required() -> CreateEmbed {
        error_embed("Invalid Role", Some("The live role can't also be the required role. Change the requirement with `/liverole require`."), None)
    }
    pub fn required_is_live() -> CreateEmbed {
        error_embed("Invalid Role", Some("The required role can't be the live role itself."), None)
    }
    pub fn cannot_use(reason: &str) -> CreateEmbed {
        error_embed("Cannot Use That Role", Some(&format!("{reason} Pick another role, or fix my role position and permissions.")), None)
    }
    pub fn no_role() -> CreateEmbed {
        error_embed("No Live Role Set", Some("Pick the role first with `/liverole set role:@Live` (or create it with `new_role:Live`), then run `/liverole enable`."), None)
    }
    pub fn role_deleted() -> CreateEmbed {
        error_embed("Live Role Missing", Some("The live role was deleted. Pick a new one with `/liverole set`."), None)
    }
    pub fn too_many(n: usize, role_id: &str, partial: &str) -> CreateEmbed {
        error_embed(
            "Use a Dedicated Role",
            Some(&format!(
                "<@&{role_id}> is held by **{}** members who are not live{partial}. They would all lose it. Create a dedicated one with `/liverole set new_role:Live`.",
                format_number(n as f64)
            )),
            None,
        )
    }
    pub fn settings_unavailable() -> CreateEmbed {
        error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
    }
    pub fn could_not_save() -> CreateEmbed {
        error_embed("Could Not Save", Some("The setting could not be saved. Please try again in a moment."), None)
    }
    pub fn cannot_post(reason: &str) -> CreateEmbed {
        let reason = format!("{}.", reason.strip_suffix('.').unwrap_or(reason));
        error_embed("Cannot Post There", Some(&format!("{reason} Pick another channel or fix my permissions there.")), None)
    }
    pub fn empty_message() -> CreateEmbed {
        error_embed("Empty Message", Some("That text renders as an empty message."), None)
    }
    pub fn ping_everyone() -> CreateEmbed {
        error_embed("Invalid Role", Some("@everyone can't be the ping role. Use a role members can opt into, such as @Stream Alerts."), None)
    }
    pub fn ping_no_channel() -> CreateEmbed {
        error_embed("No Channel Set", Some("Pick the channel in the same command: `/liverole announce channel:#live-now ping_role:@Role`."), None)
    }
    pub fn ping_not_allowed(role_id: &str, channel_id: &str) -> CreateEmbed {
        error_embed(
            "Role Not Pingable",
            Some(&format!("<@&{role_id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{channel_id}>.")),
            None,
        )
    }
    pub fn no_presences() -> CreateEmbed {
        error_embed(
            "Live Role Unavailable",
            Some(
                "This bot runs without presence data (`DISABLE_PRESENCES=1`), so it cannot see who is streaming. Ask the bot owner to turn presences back on. `/liverole disable` still removes the role from members.",
            ),
            None,
        )
    }
}

pub const OFF_NOTE: &str = "ℹ️ The live role is **off**. Turn it on with `/liverole enable`.";

/// `finish(interaction, payload)`: the final answer; overwrites the deferred reply (which is also the confirm message
/// when one was shown). Never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    let p = Payload::embed(embed).components(Vec::new()).no_mentions();
    if let Err(e) = ctx.edit_reply(p.to_edit()).await
        && !is_interaction_gone(&e)
    {
        safe_reply(&ctx.responder, p).await;
    }
    Ok(())
}

async fn load_settings_or_finish(ctx: &CommandCtx, guild_id: GuildId) -> Option<Settings> {
    let s = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    if s.is_none() {
        let _ = finish(ctx, msg::settings_unavailable()).await;
    }
    s
}

/// `save(interaction, sub, data, field, oldValue)`: writes through the settings service; answers "Could Not Save" and
/// returns `None` on failure.
async fn save(ctx: &CommandCtx, guild_id: GuildId, sub: &str, data: Vec<(&str, Value)>, field: Option<(&str, Value)>) -> Option<Settings> {
    let mut patch = SettingsPatch::new();
    for (k, v) in &data {
        patch.insert(k, v.clone());
    }
    match ctx.app.settings().update_settings(&guild_id.to_string(), patch).await {
        Ok(row) => {
            let change = match field {
                Some((name, old)) => {
                    let new = data.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone()).unwrap_or(Value::Null);
                    format!("{name} {old} → {new}")
                }
                None => Value::Object(data.into_iter().map(|(k, v)| (k.to_string(), v)).collect()).to_string(),
            };
            bot_util::log_info!(format!("[liverole] {guild_id}: {change} by {}", ctx.user().id));
            Some(row)
        }
        Err(e) => {
            bot_util::log_error!(format!("[liverole] {sub} save failed"), bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
            let _ = finish(ctx, msg::could_not_save()).await;
            None
        }
    }
}

/// `boundedSync(guild, opts)`: waits for the sync, but not forever (`None` while it is still running).
pub async fn bounded_sync(app: &App, guild_id: GuildId, mode: Mode, reason: &str) -> Option<SyncResult> {
    let (app2, reason) = (app.clone(), reason.to_string());
    let fut = async move { Ok::<_, std::convert::Infallible>(sync_guild(&app2, guild_id, mode, &reason).await) };
    bot_util::async_util::with_timeout(fut, limits::COMMAND_SYNC_WAIT_MS, "liverole command sync").await.ok()
}

/// `describeRule(roleId, requiredId)`.
pub fn describe_rule(role_id: &str, required: Option<&str>) -> String {
    let tail = match required.filter(|r| !r.is_empty()) {
        Some(r) => format!(" and they have <@&{r}>."),
        None => ".".into(),
    };
    format!("Members get <@&{role_id}> while Discord shows them as **Streaming** (Twitch or YouTube){tail}")
}

fn cached_role(app: &App, guild_id: GuildId, id: Option<&str>) -> Option<Role> {
    let rid = RoleId::new(id.and_then(snowflake)?);
    ops::view(app, guild_id, |g| g.roles.get(&rid).cloned()).flatten()
}

/// `resolveRole(interaction, required)`: the guild's own role for a role option.
fn resolve_role(ctx: &CommandCtx, guild_id: GuildId, name: &str) -> Option<Role> {
    let id = ctx.get_role_id(name)?;
    ops::view(&ctx.app, guild_id, |g| g.roles.get(&id).cloned()).flatten()
}

fn role_check(ctx: &CommandCtx, guild_id: GuildId, role: &Role, executor: bool, allow_dangerous: bool) -> RoleCheck {
    let bot = ops::ops(&ctx.app).bot_id(&ctx.app);
    let member = if executor { ctx.member() } else { None };
    bot.and_then(|b| ops::view(&ctx.app, guild_id, |g| check_assignable_role(g, Some(role), b, member, allow_dangerous)))
        .unwrap_or(RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() })
}

fn has_role(g: &Guild, id: &str) -> bool {
    snowflake(id).is_some_and(|v| g.roles.contains_key(&RoleId::new(v)))
}

fn opt_json(s: Option<&str>) -> Value {
    s.map(|v| json!(v)).unwrap_or(Value::Null)
}

// ── /liverole set ─────────────────────────────────────────────────────────────

async fn run_set(mut ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(settings) = load_settings_or_finish(&ctx, guild_id).await else { return Ok(()) };
    if !resolve_new_options(&mut ctx, NewOptions { required: &["role"], private: &[] }).await {
        return Ok(());
    }
    let Some(role) = resolve_role(&ctx, guild_id, "role") else { return finish(&ctx, msg::cannot_use("Role not found.")).await };
    let rid = role.id.to_string();
    if role.id.get() == guild_id.get() {
        return finish(&ctx, msg::everyone()).await;
    }
    if settings.live_role_required_role_id.as_deref() == Some(rid.as_str()) {
        return finish(&ctx, msg::same_as_required()).await;
    }
    // Anyone who can show a Streaming status gets this role, so it must never carry dangerous permissions.
    let check = role_check(&ctx, guild_id, &role, true, false);
    if !check.ok {
        return finish(&ctx, msg::cannot_use(check.reason.as_deref().unwrap_or(""))).await;
    }
    if settings.live_role_id.as_deref() == Some(rid.as_str()) {
        return finish(&ctx, info_embed("No Change", Some(&format!("<@&{rid}> is already the live role. Use `/liverole enable` to re-sync it.")), None)).await;
    }

    // Dedicated-role guard: the sweep strips the role from every holder who is not streaming.
    let complete = load_members(&ctx.app, guild_id).await;
    let n = ops::view(&ctx.app, guild_id, |g| count_losing_holders(g, role.id, &settings)).unwrap_or(0);
    if n > limits::SET_MAX_STRIP {
        return finish(&ctx, msg::too_many(n, &rid, if complete { "" } else { " (counting cached members only)" })).await;
    }
    if n > 0 {
        let lacking = if settings.live_role_required_role_id.as_deref().is_some_and(|r| !r.is_empty()) { " or lack the required role" } else { "" };
        let ok = await_confirm(
            &ctx,
            ConfirmOptions {
                title: Some("Use this role as the live role?".into()),
                description: Some(format!(
                    "**{}** member(s) have <@&{rid}> but are not live{lacking}. While the live role is on, they will lose it. A dedicated role (for example a new @Live role) is recommended.",
                    format_number(n as f64)
                )),
                confirm_label: "Use this role".into(),
                cancel_label: "Cancel".into(),
                danger: true,
                ..ConfirmOptions::default()
            },
        )
        .await;
        if !ok {
            return Ok(()); // awaitConfirm already showed Cancelled / Timed out; nothing was written
        }
    }

    let previous = settings.live_role_id.clone().filter(|p| !p.is_empty());
    let Some(row) = save(&ctx, guild_id, "set", vec![("liveRoleId", json!(rid))], Some(("liveRoleId", opt_json(previous.as_deref())))).await else {
        return Ok(());
    };
    let enabled = row.live_role_enabled;
    let result = if enabled { bounded_sync(&ctx.app, guild_id, Mode::Sync, "set").await } else { None };

    // The previous role is never touched: the bot no longer manages it.
    let old_holders = match previous.as_deref().filter(|p| *p != rid) {
        Some(p) => ops::view(&ctx.app, guild_id, |g| {
            snowflake(p).map(RoleId::new).filter(|r| g.roles.contains_key(r)).map(|r| g.members.values().filter(|m| m.roles.contains(&r) && !m.user.bot).count()).unwrap_or(0)
        })
        .unwrap_or(0),
        None => 0,
    };

    let mut lines = vec![describe_rule(&rid, settings.live_role_required_role_id.as_deref())];
    if enabled {
        lines.push(format_sync_result(result.as_ref()));
    } else {
        let extra = if n > 0 { format!(" Members who have <@&{rid}> but are not live will lose it then.") } else { String::new() };
        lines.push(format!("{OFF_NOTE}{extra}"));
    }
    if old_holders > 0 {
        lines.push(format!(
            "ℹ️ **{}** member(s) still have the previous live role <@&{}>. I no longer manage it, so remove it from them or delete that role.",
            format_number(old_holders as f64),
            previous.unwrap_or_default()
        ));
    }
    finish(&ctx, success_embed("Live Role Set", Some(&lines.join("\n")), None)).await
}

// ── /liverole require ─────────────────────────────────────────────────────────

async fn run_require(ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(settings) = load_settings_or_finish(&ctx, guild_id).await else { return Ok(()) };
    let role = resolve_role(&ctx, guild_id, "role");
    let required = role.filter(|r| r.id.get() != guild_id.get()).map(|r| r.id.to_string());

    if required.is_some() && required == settings.live_role_id {
        return finish(&ctx, msg::required_is_live()).await;
    }
    if required == settings.live_role_required_role_id {
        let text = match &required {
            Some(r) => format!("<@&{r}> is already required."),
            None => "Everyone who streams already qualifies.".into(),
        };
        return finish(&ctx, info_embed("No Change", Some(&text), None)).await;
    }
    // No checkAssignableRole here: the bot never assigns the required role.
    let Some(row) = save(
        &ctx,
        guild_id,
        "require",
        vec![("liveRoleRequiredRoleId", opt_json(required.as_deref()))],
        Some(("liveRoleRequiredRoleId", opt_json(settings.live_role_required_role_id.as_deref()))),
    )
    .await
    else {
        return Ok(());
    };
    let sync = row.live_role_enabled && row.live_role_id.as_deref().is_some_and(|r| !r.is_empty());
    let result = if sync { bounded_sync(&ctx.app, guild_id, Mode::Sync, "require").await } else { None };
    let head = match &required {
        Some(r) => format!("Only members with <@&{r}> get the live role."),
        None => "Everyone who streams can get the live role.".into(),
    };
    let tail = if sync { format_sync_result(result.as_ref()) } else { OFF_NOTE.to_string() };
    finish(&ctx, success_embed("Live Role Requirement Updated", Some(&format!("{head}\n{tail}")), None)).await
}

// ── /liverole enable ──────────────────────────────────────────────────────────

async fn run_enable(ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(settings) = load_settings_or_finish(&ctx, guild_id).await else { return Ok(()) };
    let Some(role_id) = settings.live_role_id.clone().filter(|r| !r.is_empty()) else { return finish(&ctx, msg::no_role()).await };
    let Some(role) = cached_role(&ctx.app, guild_id, Some(&role_id)) else { return finish(&ctx, msg::role_deleted()).await };
    let check = role_check(&ctx, guild_id, &role, true, false);
    if !check.ok {
        return finish(&ctx, msg::cannot_use(check.reason.as_deref().unwrap_or(""))).await;
    }

    let was_on = settings.live_role_enabled;
    if !was_on {
        // Stale holders can exist after /module disable liverole or "disable remove_roles:false". No hard refusal here.
        load_members(&ctx.app, guild_id).await;
        let n = ops::view(&ctx.app, guild_id, |g| count_losing_holders(g, role.id, &settings)).unwrap_or(0);
        if n > 0 {
            let ok = await_confirm(
                &ctx,
                ConfirmOptions {
                    title: Some("Turn the live role on?".into()),
                    description: Some(format!(
                        "**{}** member(s) have <@&{}> but are not live. Turning the live role on removes it from them.",
                        format_number(n as f64),
                        role.id
                    )),
                    confirm_label: "Turn on".into(),
                    cancel_label: "Cancel".into(),
                    danger: true,
                    ..ConfirmOptions::default()
                },
            )
            .await;
            if !ok {
                return Ok(());
            }
        }
        if save(&ctx, guild_id, "enable", vec![("liveRoleEnabled", json!(true))], Some(("liveRoleEnabled", json!(false)))).await.is_none() {
            return Ok(());
        }
    }

    let result = bounded_sync(&ctx.app, guild_id, Mode::Sync, if was_on { "resync" } else { "enable" }).await;
    let mut lines = vec![describe_rule(&role.id.to_string(), settings.live_role_required_role_id.as_deref()), format_sync_result(result.as_ref())];
    if let Some(req) = settings.live_role_required_role_id.as_deref().filter(|r| !r.is_empty())
        && !ops::view(&ctx.app, guild_id, |g| has_role(g, req)).unwrap_or(false)
    {
        lines.push("⚠️ The required role was deleted, so nobody qualifies. Change it with `/liverole require`.".into());
    }
    let title = if was_on { "Live Role Already On — Resynced" } else { "Live Role Enabled" };
    finish(&ctx, success_embed(title, Some(&lines.join("\n")), None)).await
}

// ── /liverole disable ─────────────────────────────────────────────────────────

async fn run_disable(ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(settings) = load_settings_or_finish(&ctx, guild_id).await else { return Ok(()) };
    let remove_roles = ctx.get_boolean("remove_roles").unwrap_or(true);

    let was_on = settings.live_role_enabled;
    if was_on && save(&ctx, guild_id, "disable", vec![("liveRoleEnabled", json!(false))], Some(("liveRoleEnabled", json!(true)))).await.is_none() {
        return Ok(());
    }
    let live = settings.live_role_id.clone().filter(|r| !r.is_empty());
    let clear = remove_roles && live.is_some();
    // Clear mode removes the role from every non-bot holder, including members who are streaming right now.
    let result = if clear { bounded_sync(&ctx.app, guild_id, Mode::Clear, "disable").await } else { None };

    let mut lines = vec!["Nobody will get the live role when they go live. The live role and the required role stay saved.".to_string()];
    if clear {
        lines.push(format_sync_result(result.as_ref()));
    }
    if !remove_roles && let Some(r) = &live {
        lines.push(format!("Members who have <@&{r}> now keep it. Run `/liverole disable` again to remove it from them."));
    }
    let title = if was_on { "Live Role Disabled" } else { "Live Role Already Off" };
    finish(&ctx, success_embed(title, Some(&lines.join("\n")), None)).await
}

// ── /liverole announce ────────────────────────────────────────────────────────

/// `SAMPLE_STREAM`: the stand-in stream for the preview.
pub fn sample_stream() -> Vec<(String, VarValue)> {
    vec![
        ("url".into(), VarValue::from("https://twitch.tv/example")),
        ("platform".into(), VarValue::from("Twitch")),
        ("title".into(), VarValue::from("Ranked grind")),
        ("game".into(), VarValue::from("Valorant")),
    ]
}

fn usable(app: &App, guild_id: GuildId, channel: Option<ChannelId>, needs: &[&str]) -> bot_core::interaction_helpers::ChannelUsable {
    let bot = ops::ops(app).bot_id(app);
    ops::view(app, guild_id, |g| {
        let ch = channel.and_then(|c| bot_core::permissions::find_channel(g, c));
        let perms = ch.and_then(|c| bot.and_then(|b| bot_core::permissions::me(g, b).and_then(|me| bot_core::permissions::member_permissions_in(g, c, b, &me.roles, true))));
        channel_usable_verdict(ch, perms, needs)
    })
    .unwrap_or_else(|| channel_usable_verdict(None, None, needs))
}

fn cached_channel(app: &App, guild_id: GuildId, id: Option<&str>) -> Option<GuildChannel> {
    let cid = ChannelId::new(id.and_then(snowflake)?);
    ops::view(app, guild_id, |g| bot_core::permissions::find_channel(g, cid).cloned()).flatten()
}

/// `pingableRoleIds(channel, who, [role])` on the cached guild.
fn pingable(app: &App, guild_id: GuildId, channel: ChannelId, who: Option<(UserId, Vec<RoleId>)>, role: &str) -> bot_core::interaction_helpers::Pingable {
    ops::view(app, guild_id, |g| {
        let Some(ch) = bot_core::permissions::find_channel(g, channel) else { return bot_core::interaction_helpers::Pingable::default() };
        pingable_role_ids(g, ch, who.as_ref().map(|(u, r)| (*u, r.as_slice())), &[role])
    })
    .unwrap_or_default()
}

async fn run_announce(ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(settings) = load_settings_or_finish(&ctx, guild_id).await else { return Ok(()) };
    let app = ctx.app.clone();
    let channel_opt = ctx.get_channel_id("channel");
    let text = ctx.get_string("message").map(|t| bot_util::js::trim(&t).to_string()).filter(|t| !t.is_empty());
    let ping_opt = ctx.get_role_id("ping_role");
    let clear_ping = ctx.get_boolean("clear_ping_role") == Some(true);
    let saved_channel = settings.live_role_announce_channel_id.clone().filter(|c| !c.is_empty());

    // Every option empty: stop posting. The message and ping role stay saved for next time.
    if channel_opt.is_none() && text.is_none() && ping_opt.is_none() && !clear_ping {
        let Some(saved) = saved_channel else {
            return finish(&ctx, info_embed("No Change", Some("Go-live posts are already off. Turn them on with `/liverole announce channel:#live-now`."), None)).await;
        };
        let saved_row = save(
            &ctx,
            guild_id,
            "announce",
            vec![("liveRoleAnnounceChannelId", Value::Null)],
            Some(("liveRoleAnnounceChannelId", json!(saved))),
        )
        .await;
        if saved_row.is_none() {
            return Ok(());
        }
        return finish(&ctx, success_embed("Go-Live Posts Off", Some("Nothing is posted when members go live. The live role itself keeps working."), None)).await;
    }

    let mut data: Vec<(&str, Value)> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut channel = cached_channel(&app, guild_id, saved_channel.as_deref());
    if let Some(c) = channel_opt {
        channel = cached_channel(&app, guild_id, Some(&c.to_string()));
        let check = usable(&app, guild_id, channel.as_ref().map(|c| c.id), &["ViewChannel", "SendMessages"]);
        if !check.ok {
            return finish(&ctx, msg::cannot_post(check.reason.as_deref().unwrap_or(""))).await;
        }
        if let Some(ch) = &channel {
            data.push(("liveRoleAnnounceChannelId", json!(ch.id.to_string())));
        }
    }
    // A channel is still saved when it lacks Embed Links: the post works, only Discord's stream preview is missing.
    if let Some(ch) = &channel
        && !usable(&app, guild_id, Some(ch.id), &["EmbedLinks"]).ok
    {
        notes.push(format!("⚠️ I lack **Embed Links** in <#{}>, so posts show no stream preview.", ch.id));
    }

    let mut ping_id = settings.live_role_announce_ping_role_id.clone().filter(|p| !p.is_empty());
    if let Some(p) = ping_opt {
        let Some(ch) = &channel else { return finish(&ctx, msg::ping_no_channel()).await };
        let Some(role) = cached_role(&app, guild_id, Some(&p.to_string())) else { return finish(&ctx, msg::cannot_use("Role not found.")).await };
        if role.id.get() == guild_id.get() {
            return finish(&ctx, msg::ping_everyone()).await;
        }
        // The admin may only set up pings they could send themselves.
        let invoker = ctx.member().map(|m| (m.user.id, m.roles.clone()));
        if !pingable(&app, guild_id, ch.id, invoker, &role.id.to_string()).refused.is_empty() {
            return finish(&ctx, msg::ping_not_allowed(&role.id.to_string(), &ch.id.to_string())).await;
        }
        ping_id = Some(role.id.to_string());
        data.push(("liveRoleAnnouncePingRoleId", json!(role.id.to_string())));
    } else if clear_ping {
        ping_id = None;
        data.push(("liveRoleAnnouncePingRoleId", Value::Null));
    }
    let ping_role = cached_role(&app, guild_id, ping_id.as_deref());
    if ping_id.is_some() && ping_role.is_none() {
        notes.push("⚠️ The ping role was deleted, so nobody is pinged. Pick a new one with `ping_role:`.".into());
    }
    if let (Some(r), Some(ch)) = (&ping_role, &channel) {
        let bot = ops::ops(&app).bot_id(&app);
        let me = bot.and_then(|b| ops::view(&app, guild_id, |g| bot_core::permissions::me(g, b).map(|m| (b, m.roles.clone()))).flatten());
        if pingable(&app, guild_id, ch.id, me, &r.id.to_string()).allowed.is_empty() {
            notes.push(format!(
                "⚠️ <@&{}> is not mentionable and I lack **Mention @everyone, @here and All Roles** in <#{}>, so it won't actually ping.",
                r.id, ch.id
            ));
        }
    }

    let mut template = settings.live_role_announce_message.as_deref().map(bot_util::js::trim).filter(|t| !t.is_empty()).map(str::to_string);
    if let Some(t) = &text {
        template = if bot_util::js::lower(t) == "default" { None } else { Some(t.clone()) };
        data.push(("liveRoleAnnounceMessage", opt_json(template.as_deref())));
    }
    let mut rc = bot_core::adapters::render_context(&app, Some(guild_id), ctx.user(), ctx.member(), Some(channel.as_ref().map(|c| c.id).unwrap_or(ctx.channel_id())));
    rc.vars = Some(sample_stream());
    let d = render_placeholders_detailed(template.as_deref().unwrap_or(DEFAULT_LIVE_ANNOUNCE_MESSAGE), &rc, Some(2000));
    if bot_util::js::trim(&d.text).is_empty() {
        return finish(&ctx, msg::empty_message()).await;
    }
    if !d.unknown_tokens.is_empty() {
        let list: Vec<String> = d.unknown_tokens.iter().take(10).map(|t| format!("`{t}`")).collect();
        notes.push(format!("⚠️ Unknown variables (sent as typed): {}", list.join(", ")));
    }
    if template.as_deref().is_some_and(|t| !bot_util::js::lower(t).contains("{url}")) {
        notes.push("Tip: add `{url}` to link the stream.".into());
    }

    let Some(row) = save(&ctx, guild_id, "announce", data, None).await else { return Ok(()) };

    let role_text = match row.live_role_id.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => format!("<@&{r}>"),
        None => "the live role".into(),
    };
    let head = if let Some(ch) = &channel {
        let ping = ping_role.as_ref().map(|r| format!(" and ping <@&{}>", r.id)).unwrap_or_default();
        format!("When a member goes live and gets {role_text}, I post in <#{}>{ping} (at most once an hour per member).", ch.id)
    } else if row.live_role_announce_channel_id.as_deref().is_some_and(|c| !c.is_empty()) {
        "⚠️ The go-live channel was deleted. Pick a new one with `/liverole announce channel:`.".into()
    } else {
        "ℹ️ No channel is set, so nothing is posted yet. Add one with `/liverole announce channel:#live-now`.".into()
    };
    let mut lines = vec![head];
    if !row.live_role_enabled {
        lines.push(OFF_NOTE.into());
    }
    lines.extend(notes);
    lines.push(String::new());
    lines.push("**Preview**".into());
    lines.push(match &ping_role {
        Some(r) => format!("<@&{}> {}", r.id, d.text),
        None => d.text.clone(),
    });
    finish(&ctx, success_embed("Go-Live Posts Updated", Some(&clamp_text(&lines.join("\n"), 4096)), None)).await
}

// ── /liverole status ──────────────────────────────────────────────────────────

/// `announceValue(guild, settings)`: the "Go-live posts" field.
pub fn announce_value(guild: &Guild, settings: &bot_db::GuildSettings) -> String {
    let Some(id) = settings.live_role_announce_channel_id.as_deref().filter(|c| !c.is_empty()) else { return "Off — `/liverole announce`".into() };
    let exists = snowflake(id).is_some_and(|c| bot_core::permissions::find_channel(guild, ChannelId::new(c)).is_some());
    if !exists {
        return format!("⚠️ Channel deleted (`{id}`) — `/liverole announce`");
    }
    let ping = match settings.live_role_announce_ping_role_id.as_deref().filter(|p| !p.is_empty()) {
        None => "no ping".to_string(),
        Some(p) if has_role(guild, p) => format!("pings <@&{p}>"),
        Some(p) => format!("⚠️ ping role deleted (`{p}`)"),
    };
    let message = if settings.live_role_announce_message.as_deref().is_some_and(|m| !bot_util::js::trim(m).is_empty()) { "custom message" } else { "default message" };
    format!("<#{id}> · {message} · {ping}")
}

/// The `/liverole status` embed for a cached guild.
pub fn status_embed(app: &App, guild: &Guild, settings: &bot_db::GuildSettings) -> CreateEmbed {
    let bot = ops::ops(app).bot_id(app);
    let live_id = settings.live_role_id.clone().filter(|r| !r.is_empty());
    let role = live_id.as_deref().and_then(snowflake).and_then(|r| guild.roles.get(&RoleId::new(r)));
    let manage = role.map(|r| match bot {
        Some(b) => check_assignable_role(guild, Some(r), b, None, true),
        None => RoleCheck { ok: false, reason: Some("I need the Manage Roles permission.".into()), dangerous: Vec::new() },
    });
    let dangerous: Vec<&str> = manage.as_ref().map(|m| m.dangerous.clone()).unwrap_or_default();
    let req = settings.live_role_required_role_id.clone().filter(|r| !r.is_empty());
    let req_exists = req.as_deref().is_none_or(|r| has_role(guild, r));
    let mut holders: Vec<UserId> =
        role.map(|r| guild.members.values().filter(|m| m.roles.contains(&r.id) && !m.user.bot).map(|m| m.user.id).collect()).unwrap_or_default();
    holders.sort();
    let streamers = guild
        .presences
        .iter()
        .filter(|(id, p)| is_streaming(Some(p)) && !guild.members.get(id).is_some_and(|m| m.user.bot))
        .count();
    let enabled = settings.live_role_enabled;

    let mut embed = guild_embed(
        Some(settings),
        Some("🔴 Live Role"),
        Some("Gives a role to members while Discord shows them as **Streaming**, which happens when a Twitch or YouTube account they connected goes live."),
        Some("Counts use cached members · MEE6: Twitch Alerts › live-streaming role"),
    );

    let mut live_now = "—".to_string();
    if role.is_some() {
        let mut lines = vec![format!(
            "**{}** member(s) have the live role · **{}** member(s) are streaming",
            format_number(holders.len() as f64),
            format_number(streamers as f64)
        )];
        for id in holders.iter().take(limits::STATUS_LIST_MAX) {
            lines.push(format!("<@{id}> · {}", stream_label(guild.presences.get(id)).unwrap_or_else(|| "not streaming".into())));
        }
        live_now = join_with_limit(&lines, "\n", 1024, bot_util::text::default_more);
    }

    let live_value = match (&live_id, role) {
        (None, _) => "Not set — `/liverole set`".to_string(),
        (Some(_), Some(r)) => format!("<@&{}>", r.id),
        (Some(id), None) => format!("⚠️ Deleted (`{id}`)"),
    };
    let req_value = match &req {
        None => "None — everyone who streams".to_string(),
        Some(r) if req_exists => format!("<@&{r}>"),
        Some(r) => format!("⚠️ Deleted (`{r}`)"),
    };
    embed = embed
        .field("Status", if enabled { "✅ On" } else { "❌ Off — `/liverole enable`" }, true)
        .field("Live role", live_value, true)
        .field("Required role", req_value, true)
        .field("Go-live posts", announce_value(guild, settings), false)
        .field("Live now", live_now, false);

    let problems = live_role_problems(&ProblemInput {
        enabled,
        live_role_id: live_id.as_deref(),
        role_exists: role.is_some(),
        manage: manage.as_ref(),
        dangerous_names: bot_core::permissions::permission_names(&dangerous),
        required_role_id: req.as_deref(),
        required_role_exists: req_exists,
        holder_count: holders.len(),
    });
    if !problems.is_empty() {
        embed = embed.field("Problems", join_with_limit(&problems, "\n", 1024, bot_util::text::default_more), false);
    }
    embed
}

async fn run_status(ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(settings) = load_settings_or_finish(&ctx, guild_id).await else { return Ok(()) };
    // Caches only (no member fetch), so status is always fast.
    let embed = ops::view(&ctx.app, guild_id, |g| status_embed(&ctx.app, g, &settings)).ok_or_else(|| anyhow::anyhow!("/liverole status: guild {guild_id} is not cached"))?;
    finish(&ctx, embed).await
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/liverole outside a guild"))?;
    // Without presences every member looks offline; only the cleanup (disable) still makes sense.
    if sub != "disable" && !ops::presences_enabled(&ctx.app) {
        if !safe_defer(&ctx.responder, false).await {
            return Ok(());
        }
        return finish(&ctx, msg::no_presences()).await;
    }
    match sub.as_str() {
        "set" => run_set(ctx, guild_id).await,
        "require" => run_require(ctx, guild_id).await,
        "enable" => run_enable(ctx, guild_id).await,
        "disable" => run_disable(ctx, guild_id).await,
        "announce" => run_announce(ctx, guild_id).await,
        "status" => run_status(ctx, guild_id).await,
        _ => Ok(()),
    }
}
