//! Port of `src/commands/utility/invites.js`: `/invites show | inviter | leaderboard | link` (the MEE6 `/invites`,
//! `/inviter`, `/invites-leaderboard` and personal links). Gated on the Invites module, every member may run it.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{PermTarget, Payload, cached_channel, cached_guild, missing_bot_permissions};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::GuildMemberCache;
use bot_util::text::{clamp_text, format_number};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions};
use serenity::builder::CreateEmbed;

use crate::invite_tracker::{
    CodeLife, LINK_COOLDOWN_SECONDS, bot_can_track, delete_discord_invite, get_code_state, get_invite_leaderboard, get_invite_stats, get_join_record,
    get_personal_invite, invite_api, save_personal_invite,
};

const LINK_CHANNEL_TYPES: [u8; 4] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT, channel_type::GUILD_VOICE, channel_type::GUILD_STAGE_VOICE];
const LEADERBOARD_PAGE_SIZE: usize = 10;
/// The interaction expired or was already answered.
const GONE_CODES: [u64; 4] = [10062, 40060, 10015, 50027];
const UNKNOWN_INVITER: &str = "Unknown (joined before tracking, via Server Discovery, or the invite could not be determined)";

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("invites", "Invite Tracker: /invites, /inviter, /invites-leaderboard and personal links")
        .option(
            Opt::sub_command("show", "Invite stats (valid, left, fake) and who invited a member — /invites")
                .option(Opt::user("member", "Member to check (default: you)")),
        )
        .option(Opt::sub_command("inviter", "Who invited a member — /inviter").option(Opt::user("member", "Member to look up").required(true)))
        .option(
            Opt::sub_command("leaderboard", "Top inviters by valid invites — /invites-leaderboard")
                .option(Opt::integer("page", "Page (1–20)").min_value(1.0).max_value(20.0)),
        )
        .option(
            Opt::sub_command("link", "Get your personal, never-expiring invite link")
                .option(Opt::channel("channel", "Channel the link opens (default: this one)").channel_types(&LINK_CHANNEL_TYPES)),
        )
}

/// The command (`module 'invites'`, gated, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), Some(ModuleKey::Invites), execute).permission_level(PermissionLevel::Member)
}

/// Serialises `/invites link` per member (double clicks must not create orphan invites).
#[derive(Default)]
struct LinkMutex(bot_core::sync::KeyedMutex);

// ── Helpers ───────────────────────────────────────────────────────────────────

/// `edit(interaction, payload)`: an editReply that ignores an interaction that can no longer be answered.
async fn edit(ctx: &CommandCtx, payload: Payload) {
    if let Err(e) = ctx.edit_reply(payload.to_edit()).await
        && !e.code().is_some_and(|c| GONE_CODES.contains(&c))
    {
        bot_util::log_warn!(format!("/invites reply failed: {e}"));
    }
}

fn unavailable_embed() -> CreateEmbed {
    error_embed("Temporarily Unavailable", Some("Invite stats could not be loaded. Please try again in a moment."), None)
}

fn pl(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn fmt(n: i64) -> String {
    format_number(n as f64)
}

fn ts(d: &chrono::DateTime<chrono::Utc>) -> String {
    bot_util::time::discord_timestamp_ms(d.timestamp_millis() as f64, "R")
}

/// `describeJoinRecord(record, guild)`: the "Invited by" field.
pub fn describe_join_record(record: Option<&GuildMemberCache>, vanity_code: Option<&str>) -> String {
    let Some(r) = record.filter(|r| r.inviter_id.is_some() || r.invite_code.is_some()) else { return UNKNOWN_INVITER.into() };
    let when = ts(&r.joined_at);
    let code = r.invite_code.as_deref();
    let mut text = if let Some(inviter) = &r.inviter_id {
        format!("<@{inviter}>{} · joined {when}", code.map(|c| format!(" · `discord.gg/{c}`")).unwrap_or_default())
    } else if code == vanity_code {
        format!("Vanity URL `discord.gg/{}` · joined {when}", code.unwrap_or("null"))
    } else {
        format!("`discord.gg/{}` (creator unknown) · joined {when}", code.unwrap_or("null"))
    };
    if let Some(l) = &r.left_at {
        text.push_str(&format!(" · left {}", ts(l)));
    }
    clamp_text(&text, 1024)
}

/// `describeInviter(record, targetId, guild)`: the sentence for `/invites inviter`.
pub fn describe_inviter(record: Option<&GuildMemberCache>, target_id: &str, vanity_code: Option<&str>) -> String {
    let Some(r) = record.filter(|r| r.inviter_id.is_some() || r.invite_code.is_some()) else {
        return format!(
            "I don't know who invited <@{target_id}>. They may have joined before invite tracking started, through Server Discovery, or with an invite I couldn't determine."
        );
    };
    let when = ts(&r.joined_at);
    let code = r.invite_code.as_deref();
    let mut text = if let Some(inviter) = &r.inviter_id {
        format!("<@{target_id}> was invited by <@{inviter}>{} {when}.", code.map(|c| format!(" with `discord.gg/{c}`")).unwrap_or_default())
    } else if code == vanity_code {
        format!("<@{target_id}> joined through the vanity URL `discord.gg/{}` {when}.", code.unwrap_or("null"))
    } else {
        format!("<@{target_id}> joined with `discord.gg/{}` {when}, but I don't know who created that invite.", code.unwrap_or("null"))
    };
    if let Some(l) = &r.left_at {
        text.push_str(&format!("\nThey left {}.", ts(l)));
    }
    text
}

fn guild_vanity(app: &App, guild_id: GuildId) -> Option<String> {
    cached_guild(app, guild_id).and_then(|g| g.vanity_url_code)
}

// ── /invites show ─────────────────────────────────────────────────────────────

async fn show_stats(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    ctx.defer_reply(false).await?;
    let g = guild_id.to_string();
    let target = ctx.get_user("member").unwrap_or_else(|| ctx.user().clone());
    let target_member = ctx.get_member("member").or_else(|| if target.id == ctx.user().id { ctx.member().cloned() } else { None });
    let display = match &target_member {
        Some(m) => bot_core::adapters::member_display_name(m),
        None => target.global_name.clone().unwrap_or_else(|| target.name.clone()),
    };
    let name = clamp_text(&display, 100);
    let uid = target.id.to_string();
    let pool = ctx.app.db();
    let (stats, record, personal, settings) =
        tokio::join!(get_invite_stats(pool, &g, &uid), get_join_record(pool, &g, &uid), get_personal_invite(pool, &g, &uid), ctx.app.settings().get_settings_safe(&g));
    let (stats, record, personal) = match (stats, record, personal) {
        (Ok(s), Ok(r), Ok(p)) => (s, r, p),
        (s, r, p) => {
            let err = s.err().or(r.err()).or(p.err()).map(|e| e.to_string()).unwrap_or_default();
            bot_util::log_warn!(format!("/invites show failed in {g}: {err}"));
            edit(ctx, Payload::embed(unavailable_embed())).await;
            return Ok(());
        }
    };

    let mut description = format!("<@{uid}> has **{}** valid invite{}.", fmt(stats.valid), pl(stats.valid));
    if stats.recent > 0 {
        description.push_str(&format!("\n_{} of them joined less than 24 hours ago and become fake if they leave before then._", fmt(stats.recent)));
    }
    let footer = if bot_can_track(&ctx.app, guild_id) {
        "Valid = still here or stayed 24 h · Fake = left within 24 h"
    } else {
        "⚠️ Invite tracking is paused: I need the Manage Server permission."
    };
    let vanity = guild_vanity(&ctx.app, guild_id);
    let mut embed = guild_embed(settings.as_deref(), Some(&format!("📨 Invites — {name}")), Some(&description), Some(footer))
        .field("✅ Valid", fmt(stats.valid), true)
        .field("🏠 Still here", fmt(stats.active), true)
        .field("🚪 Left (after 24 h)", fmt(stats.left), true)
        .field("❌ Fake", fmt(stats.fake), true)
        .field("📥 Total joins", fmt(stats.total), true)
        .field("🙋 Invited by", describe_join_record(record.as_ref(), vanity.as_deref()), false);
    if let Some(p) = personal {
        let state = get_code_state(&ctx.app, guild_id, &p.code, false, Some(p.created_at.timestamp_millis())).await;
        let value = if state == CodeLife::Gone {
            "Expired — run `/invites link` to get a new one.".to_string()
        } else {
            format!("https://discord.gg/{} (opens <#{}>)", p.code, p.channel_id)
        };
        embed = embed.field("🔗 Personal link", clamp_text(&value, 1024), false);
    }
    edit(ctx, Payload::embed(embed)).await;
    Ok(())
}

// ── /invites inviter ──────────────────────────────────────────────────────────

async fn show_inviter(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    ctx.defer_reply(false).await?;
    let g = guild_id.to_string();
    let target = ctx.get_user_id("member").ok_or_else(|| anyhow::anyhow!("Required option \"member\" not found."))?;
    let tid = target.to_string();
    let (record, settings) = tokio::join!(get_join_record(ctx.app.db(), &g, &tid), ctx.app.settings().get_settings_safe(&g));
    let record = match record {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!(format!("/invites inviter failed in {g}: {e}"));
            edit(ctx, Payload::embed(unavailable_embed())).await;
            return Ok(());
        }
    };
    let vanity = guild_vanity(&ctx.app, guild_id);
    let text = describe_inviter(record.as_ref(), &target.to_string(), vanity.as_deref());
    edit(ctx, Payload::embed(guild_embed(settings.as_deref(), Some("🙋 Inviter"), Some(&text), None))).await;
    Ok(())
}

// ── /invites leaderboard ──────────────────────────────────────────────────────

fn medal(rank: usize) -> String {
    match rank {
        1 => "🥇".into(),
        2 => "🥈".into(),
        3 => "🥉".into(),
        r => format!("**#{r}**"),
    }
}

async fn show_leaderboard(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    ctx.defer_reply(false).await?;
    let g = guild_id.to_string();
    let exclude: Vec<String> = ctx.app.discord().map(|d| vec![d.cache.current_user().id.to_string()]).unwrap_or_default();
    let (rows, settings) = tokio::join!(get_invite_leaderboard(ctx.app.db(), &g, 200, &exclude), ctx.app.settings().get_settings_safe(&g));
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!(format!("/invites leaderboard failed in {g}: {e}"));
            edit(ctx, Payload::embed(unavailable_embed())).await;
            return Ok(());
        }
    };
    if rows.is_empty() {
        edit(ctx, Payload::embed(guild_embed(settings.as_deref(), Some("🏆 Invite Leaderboard"), Some("No invites have been tracked in this server yet."), None))).await;
        return Ok(());
    }
    let me = ctx.user().id.to_string();
    let my_index = rows.iter().position(|r| r.inviter_id == me);
    let footer = format!("Valid = still here or stayed 24 h{}", my_index.map(|i| format!(" · Your rank: #{}", i + 1)).unwrap_or_default());
    let guild_name = cached_guild(&ctx.app, guild_id).map(|g| g.name).unwrap_or_default();
    let title = format!("🏆 {} — Invite Leaderboard", clamp_text(&guild_name, 200));
    let mut pages = Vec::new();
    let mut i = 0;
    while i < rows.len() && pages.len() < 20 {
        let lines: Vec<String> = rows[i..rows.len().min(i + LEADERBOARD_PAGE_SIZE)]
            .iter()
            .enumerate()
            .map(|(j, r)| {
                let s = &r.stats;
                format!("{} <@{}> — **{}** invite{} ({} left · {} fake)", medal(i + j + 1), r.inviter_id, fmt(s.valid), pl(s.valid), fmt(s.left), fmt(s.fake))
            })
            .collect();
        pages.push(guild_embed(settings.as_deref(), Some(&title), Some(&lines.join("\n")), Some(&footer)));
        i += LEADERBOARD_PAGE_SIZE;
    }
    let page = ctx.get_integer("page").unwrap_or(1);
    let start = (page - 1).min(pages.len() as i64 - 1);
    paginate(ctx, &pages, PaginateOptions { start_page: start as f64, ..Default::default() }).await?;
    Ok(())
}

// ── /invites link ─────────────────────────────────────────────────────────────

/// `linkReply(settings, { code, channelId, reused, replacedCode })`.
pub fn link_reply(settings: Option<&bot_db::GuildSettings>, code: &str, channel_id: &str, reused: bool, replaced_code: Option<&str>) -> Payload {
    let url = format!("https://discord.gg/{code}");
    let footer = if reused {
        Some("This is your existing link.".to_string())
    } else {
        replaced_code.map(|r| format!("Your previous link discord.gg/{r} was deleted."))
    };
    let description = format!(
        "**{url}**\nOpens <#{channel_id}> · never expires · unlimited uses.\n\nEveryone who joins with this link is credited to you. An invite counts while the member stays, or once they have stayed 24 hours; leaving sooner makes it fake."
    );
    Payload::embed(guild_embed(settings, Some("🔗 Your personal invite link"), Some(&description), footer.as_deref())).content(url).no_mentions()
}

enum Check {
    Ok,
    Type,
    Bot(Vec<String>),
    Member,
}

/// `checkChannel(interaction, ch)`: the first failure for a candidate channel.
fn check_channel(ctx: &CommandCtx, guild_id: GuildId, ch: Option<&GuildChannel>) -> Check {
    let Some(ch) = ch.filter(|c| LINK_CHANNEL_TYPES.contains(&u8::from(c.kind))) else { return Check::Type };
    let missing = missing_bot_permissions(&ctx.app, PermTarget::Channel(guild_id, ch.id), &["ViewChannel", "CreateInstantInvite"]);
    if !missing.is_empty() {
        return Check::Bot(missing);
    }
    let member_ok = ctx.member().is_some_and(|m| {
        let Some(g) = cached_guild(&ctx.app, guild_id) else { return false };
        bot_core::permissions::member_permissions_in(&g, ch, m.user.id, &m.roles, true)
            .is_some_and(|p| bot_core::permissions::js_has(p, Permissions::VIEW_CHANNEL | Permissions::CREATE_INSTANT_INVITE))
    });
    if !member_ok {
        return Check::Member;
    }
    Check::Ok
}

/// `resolveTargetChannel(interaction, explicit)`.
#[allow(clippy::result_large_err)] // an error embed, built once per command
fn resolve_target_channel(ctx: &CommandCtx, guild_id: GuildId, explicit: Option<ChannelId>) -> Result<GuildChannel, CreateEmbed> {
    if let Some(id) = explicit {
        let ch = cached_channel(&ctx.app, guild_id, id);
        return match check_channel(ctx, guild_id, ch.as_ref()) {
            Check::Ok => ch.ok_or_else(|| error_embed("Unsupported Channel", Some("Invites can only open a text, announcement, voice or stage channel."), None)),
            Check::Type => Err(error_embed("Unsupported Channel", Some("Invites can only open a text, announcement, voice or stage channel."), None)),
            Check::Bot(missing) => Err(error_embed(
                "Missing Permissions",
                Some(&format!("I need **{}** in <#{id}>.", bot_core::permissions::permission_names(&missing).join(", "))),
                None,
            )),
            Check::Member => Err(error_embed("Not Allowed", Some(&format!("You don't have permission to create invites in <#{id}>.")), None)),
        };
    }
    let here = cached_channel(&ctx.app, guild_id, ctx.channel_id());
    let is_thread = here.as_ref().is_some_and(bot_core::permissions::is_thread);
    let parent = if is_thread { here.as_ref().and_then(|h| h.parent_id).and_then(|p| cached_channel(&ctx.app, guild_id, p)) } else { None };
    let system = cached_guild(&ctx.app, guild_id).and_then(|g| g.system_channel_id).and_then(|s| cached_channel(&ctx.app, guild_id, s));
    let candidates = [if is_thread { None } else { here }, parent, system];
    let mut seen = Vec::new();
    for ch in candidates.into_iter().flatten() {
        if seen.contains(&ch.id) {
            continue;
        }
        seen.push(ch.id);
        if matches!(check_channel(ctx, guild_id, Some(&ch)), Check::Ok) {
            return Ok(ch);
        }
    }
    Err(error_embed(
        "No Channel Available",
        Some("I can't create an invite in this channel or the server's system channel. Pick a channel with the `channel` option where both of us can create invites."),
        None,
    ))
}

/// `createInviteError(err, channel)`.
fn create_invite_error(code: Option<u64>, message: &str, channel_id: ChannelId) -> CreateEmbed {
    match code {
        Some(30016) => error_embed(
            "Invite Limit Reached",
            Some("This server has reached Discord's limit of 1,000 invites. Ask an admin to delete unused invites, then try again."),
            None,
        ),
        Some(50013) | Some(50001) => error_embed("Missing Permissions", Some(&format!("I need **View Channel** and **Create Invite** in <#{channel_id}>.")), None),
        Some(10003) => error_embed("Channel Not Found", Some("That channel no longer exists."), None),
        _ => {
            bot_util::log_error!("/invites link: createInvite failed".to_string(), bot_util::logger::LogArg::Error { message: message.to_string(), stack: None });
            error_embed("Something Went Wrong", Some("I could not create your invite link. Please try again later."), None)
        }
    }
}

/// `invites:link:cd:{guildId}:{userId}`: the `/invites link` cooldown key.
pub fn link_cooldown_key(guild_id: &str, user_id: &str) -> String {
    format!("invites:link:cd:{guild_id}:{user_id}")
}

/// `deleteDiscordInvite(guild, code, reason).catch(() => {})` without awaiting it: the reply does not wait for it.
fn spawn_delete(app: &App, guild_id: GuildId, code: &str, reason: &'static str) {
    let (app, code) = (app.clone(), code.to_string());
    tokio::spawn(async move { delete_discord_invite(&app, guild_id, &code, reason).await });
}

async fn create_link(ctx: CommandCtx, guild_id: GuildId) -> BotResult {
    ctx.defer_reply(true).await?;
    let g = guild_id.to_string();
    let user_id = ctx.user().id.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await; // colour only
    let explicit = ctx.get_channel_id("channel");
    let mutex = ctx.app.ext::<LinkMutex>();
    let key = format!("{g}:{user_id}");
    mutex
        .0
        .run(&key, move || async move {
            let existing = match get_personal_invite(ctx.app.db(), &g, &user_id).await {
                Ok(e) => e,
                Err(e) => {
                    bot_util::log_warn!(format!("/invites link lookup failed in {g}: {e}"));
                    edit(&ctx, Payload::embed(unavailable_embed())).await;
                    return;
                }
            };

            // Reuse: an unknown state counts as alive, so a network error never creates a duplicate.
            let mut old_state = None;
            if let Some(ex) = &existing {
                let state = get_code_state(&ctx.app, guild_id, &ex.code, true, Some(ex.created_at.timestamp_millis())).await;
                old_state = Some(state);
                if state != CodeLife::Gone && explicit.is_none_or(|c| c.to_string() == ex.channel_id) {
                    edit(&ctx, link_reply(settings.as_deref(), &ex.code, &ex.channel_id, true, None)).await;
                    return;
                }
            }

            // discord.js hands over the option's resolved channel even when it was not cached: fetch (and cache) it.
            if let Some(id) = explicit
                && cached_channel(&ctx.app, guild_id, id).is_none()
            {
                let _ = bot_core::interaction_helpers::resolve_channel(&ctx.app, guild_id, id).await;
            }
            let channel = match resolve_target_channel(&ctx, guild_id, explicit) {
                Ok(c) => c,
                Err(embed) => {
                    edit(&ctx, Payload::embed(embed)).await;
                    return;
                }
            };

            // Cooldown (anti-spam). Redis down (None) fails OPEN: Discord's own limits still apply.
            let cd_key = link_cooldown_key(&g, &user_id);
            if ctx.app.cache().set_nx(&cd_key, "1", LINK_COOLDOWN_SECONDS).await == Some(false) {
                edit(&ctx, Payload::embed(warn_embed("Slow Down", Some("You just created an invite link. Try again in a minute."), None))).await;
                return;
            }

            let reason = clamp_text(&format!("Personal invite link for {} (/invites link)", bot_core::adapters::user_tag(ctx.user())), 512);
            let code = match invite_api(&ctx.app).create_invite(channel.id, &reason).await {
                Ok(c) => c,
                Err(e) => {
                    ctx.app.cache().del_one(&cd_key).await; // a failed attempt must not lock the user out
                    edit(&ctx, Payload::embed(create_invite_error(e.code, &e.message, channel.id))).await;
                    return;
                }
            };

            if let Err(e) = save_personal_invite(&ctx.app, &g, &user_id, &code, &channel.id.to_string()).await {
                bot_util::log_warn!(format!("/invites link save failed in {g}: {e}"));
                spawn_delete(&ctx.app, guild_id, &code, "Personal invite could not be saved");
                ctx.app.cache().del_one(&cd_key).await;
                edit(&ctx, Payload::embed(unavailable_embed())).await;
                return;
            }

            let mut replaced = None;
            if let Some(ex) = &existing
                && ex.code != code
            {
                spawn_delete(&ctx.app, guild_id, &ex.code, "Replaced by a new personal invite link");
                if old_state != Some(CodeLife::Gone) {
                    replaced = Some(ex.code.clone());
                }
            }
            edit(&ctx, link_reply(settings.as_deref(), &code, &channel.id.to_string(), false, replaced.as_deref())).await;
        })
        .await;
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let sub = ctx.subcommand().unwrap_or_default();
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let responder = ctx.responder.clone();
    let result = match sub.as_str() {
        "show" => show_stats(&ctx, guild_id).await,
        "inviter" => show_inviter(&ctx, guild_id).await,
        "leaderboard" => show_leaderboard(&ctx, guild_id).await,
        "link" => create_link(ctx, guild_id).await,
        _ => Ok(()),
    };
    if let Err(e) = result {
        bot_util::log_error!(format!("/invites {sub} failed"), bot_core::error::log_arg(&e));
        if responder.is_acknowledged() {
            let p = Payload::embed(error_embed("Something Went Wrong", Some("That command failed. Please try again later."), None));
            if let Err(e) = responder.edit_reply(p.to_edit()).await
                && !e.code().is_some_and(|c| GONE_CODES.contains(&c))
            {
                bot_util::log_warn!(format!("/invites reply failed: {e}"));
            }
        }
    }
    Ok(())
}
