//! Port of `src/commands/admin/feed.js`: `/feed add rss|youtube|reddit|bluesky|podcast`, `list`, `remove`, `test`,
//! `edit`, `enable`, `disable`, with the feed autocomplete. Thin: parsing, fetching, message building, the poll job
//! and the database helpers live in [`crate::feed_service`] and [`crate::feed_parsers`]. An ADMIN config command:
//! public answers, manageable while the module is off.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Choice, Payload, bot_permissions, cached_channel, cached_guild, check_channel_usable, PermTarget, pingable_role_ids, reply_error, respond_autocomplete, safe_reply};
use bot_core::markdown::escape_markdown;
use bot_core::prelude::*;
use bot_db::{FEED_INFO, Feed, FeedType, SqlValue, UpdateBuilder};
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use bot_util::text::clamp_text;
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions, RoleId};
use serenity::builder::CreateEmbed;

use crate::feed_parsers::{normalize_feed_url, parse_bluesky_actor_input, parse_subreddit_input, parse_youtube_channel_input};
use crate::feed_service::{
    self as svc, FEED_CHANNEL_TYPES, FeedUserError, NewFeed, Probe, SourceInput, SourceResult, base_interval_ms, describe_fetch_error, feed_job_key, feed_status,
    type_emoji, type_label,
};

const D_CHANNEL: &str = "Channel to post new items in";
const D_PING_ROLE: &str = "Role to ping with each new item";
const D_MESSAGE: &str = "Custom text with {title} {link} etc.; \\n = new line";
const D_FEED: &str = "Feed to use (pick from the list)";
const TEXT_CHANNELS: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn common(mut s: Opt, reposts: bool) -> Opt {
    s = s
        .option(Opt::channel("channel", D_CHANNEL).required(true).channel_types(&TEXT_CHANNELS))
        .option(Opt::role("ping_role", D_PING_ROLE))
        .option(Opt::string("message", D_MESSAGE).max_length(1000));
    if reposts {
        s = s.option(Opt::boolean("include_reposts", "Also post reposts (default: off)"));
    }
    s
}

fn source(name: &str, desc: &str, max: u16) -> Opt {
    Opt::string(name, desc).required(true).max_length(max)
}

fn feed_opt() -> Opt {
    Opt::string("feed", D_FEED).required(true).autocomplete(true).max_length(100)
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("feed", "RSS, YouTube, Reddit, Bluesky and podcast alerts (MEE6 social alerts)")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command_group("add", "Follow a new feed")
                .option(common(Opt::sub_command("rss", "An RSS or Atom feed (MEE6 RSS Feeds)").option(source("url", "Feed URL, or a site that links to its feed", 500)), false))
                .option(common(
                    Opt::sub_command("youtube", "New uploads from a YouTube channel (MEE6 YouTube Alerts)")
                        .option(source("youtube_channel", "UC… channel ID or youtube.com/channel/UC… link (@handles not supported)", 200)),
                    false,
                ))
                .option(common(
                    Opt::sub_command("reddit", "New posts in a subreddit (MEE6 Reddit Alerts, best-effort)").option(source("subreddit", "Subreddit, e.g. r/aww", 100)),
                    false,
                ))
                .option(common(
                    Opt::sub_command("bluesky", "New posts from a Bluesky account (MEE6 Bluesky Alerts)")
                        .option(source("handle", "Handle (alice.bsky.social), profile link or DID", 300)),
                    true,
                ))
                .option(common(
                    Opt::sub_command("podcast", "New episodes of a podcast (MEE6 Podcast Alerts)").option(source("podcast", "Podcast name to search, or its RSS feed URL", 300)),
                    false,
                )),
        )
        .option(Opt::sub_command("list", "List this server's feeds and their status"))
        .option(Opt::sub_command("remove", "Stop following a feed").option(feed_opt()))
        .option(Opt::sub_command("test", "Fetch a feed now and preview its newest item (changes nothing)").option(feed_opt()))
        .option(
            Opt::sub_command("edit", "Change a feed: channel, ping role, message, reposts")
                .option(feed_opt())
                .option(Opt::channel("channel", "New channel").channel_types(&TEXT_CHANNELS))
                .option(Opt::role("ping_role", D_PING_ROLE))
                .option(Opt::boolean("clear_ping_role", "Stop pinging a role"))
                .option(Opt::string("message", D_MESSAGE).max_length(1000))
                .option(Opt::boolean("reset_message", "Go back to the default message"))
                .option(Opt::boolean("include_reposts", "Bluesky only: also post reposts")),
        )
        .option(Opt::sub_command("enable", "Turn a feed back on and reset its failures").option(feed_opt()))
        .option(Opt::sub_command("disable", "Pause a feed (its settings are kept)").option(feed_opt()))
}

/// The command (`module 'feeds'`, `gated = false`, ADMIN for every subcommand).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Feeds), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

const UNAVAILABLE_TITLE: &str = "Temporarily Unavailable";
const NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];
/// `MODULE_OFF_NOTE`.
pub const MODULE_OFF_NOTE: &str = "⚠️ The **Social Feeds** module is off, so nothing is posted until an admin runs `/module enable feeds`.";

fn module_off_note(settings: &Settings) -> Option<&'static str> {
    if is_module_enabled(Some(settings), Some(ModuleKey::Feeds)) { None } else { Some(MODULE_OFF_NOTE) }
}

async fn err(ctx: &CommandCtx, title: &str, text: &str) {
    reply_error(&ctx.responder, title, Some(text), true).await;
}

async fn edit_embed(ctx: &CommandCtx, embed: CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
}

async fn edit_error(ctx: &CommandCtx, title: &str, text: &str) {
    edit_embed(ctx, error_embed(title, Some(text), None)).await;
}

fn gid_of(ctx: &CommandCtx) -> anyhow::Result<GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/feed outside a guild"))
}

/// `loadSettings(interaction)`: the memo first, a cold load bounded by the ack budget.
async fn load_settings(ctx: &CommandCtx) -> Option<Settings> {
    let g = ctx.guild_id()?.to_string();
    if let Some(s) = ctx.app.settings().peek_settings(&g) {
        return Some(s);
    }
    let app = ctx.app.clone();
    let fut = async move { Ok::<_, std::convert::Infallible>(app.settings().get_settings_safe(&g).await) };
    match bot_util::async_util::with_timeout(fut, ctx.ack_budget_ms(), "feed settings").await {
        Ok(Some(s)) => Some(s),
        _ => {
            err(ctx, UNAVAILABLE_TITLE, "Server settings could not be loaded. Please try again in a moment.").await;
            None
        }
    }
}

/// `lookupFeed(interaction)`: the feed named by `feed`, or `None` after replying why.
async fn lookup_feed(ctx: &CommandCtx) -> anyhow::Result<Option<Feed>> {
    let raw = ctx.get_string("feed").unwrap_or_default();
    let (pool, g, r) = (ctx.app.db().clone(), gid_of(ctx)?.to_string(), raw.clone());
    let found = bot_util::async_util::with_timeout(async move { svc::find_feed_for_guild(&pool, &g, &r).await }, ctx.ack_budget_ms(), "feed lookup").await;
    match found {
        Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
            err(ctx, UNAVAILABLE_TITLE, "That took too long. Please try again.").await;
            Ok(None)
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => Err(e),
        Ok(None) => {
            err(ctx, "Feed Not Found", &format!("No feed matches **{}**. Pick one from the list, or see `/feed list`.", clamp_text(&raw, 60))).await;
            Ok(None)
        }
        Ok(Some(f)) => Ok(Some(f)),
    }
}

/// `checkTargetChannel(interaction, option)`: the channel, checked for what a post needs.
fn check_target_channel(ctx: &CommandCtx, gid: GuildId, channel_id: ChannelId) -> Result<GuildChannel, (&'static str, String)> {
    let channel = cached_channel(&ctx.app, gid, channel_id);
    if let Some(ch) = &channel
        && !FEED_CHANNEL_TYPES.contains(&u8::from(ch.kind))
    {
        return Err(("Invalid Channel", "Feeds can only post in text and announcement channels.".into()));
    }
    let u = check_channel_usable(&ctx.app, gid, Some(channel_id), &NEEDS);
    match channel {
        Some(ch) if u.ok => Ok(ch),
        _ => Err(("Missing Permissions", u.reason.unwrap_or_else(|| "I can't post in that channel.".into()))),
    }
}

/// `botPingWarning(guild, channel, role)`: a warning when the bot cannot ping the role in that channel.
fn bot_ping_warning(ctx: &CommandCtx, gid: GuildId, channel: &GuildChannel, role_id: RoleId, mentionable: bool) -> Option<String> {
    if mentionable {
        return None;
    }
    if bot_permissions(&ctx.app, PermTarget::Channel(gid, channel.id)).is_some_and(|p| p.contains(Permissions::MENTION_EVERYONE) || p.contains(Permissions::ADMINISTRATOR)) {
        return None;
    }
    Some(format!("⚠️ <@&{role_id}> is not mentionable and I lack **Mention @everyone, @here and All Roles** in <#{}>, so it won't actually ping.", channel.id))
}

/// The `ping_role` option: its id and whether it is mentionable (the guild cache first, else the resolved role).
pub(crate) fn ping_role_opt(ctx: &CommandCtx, gid: GuildId, name: &str) -> Option<(RoleId, bool)> {
    let id = ctx.get_role_id(name)?;
    let cached = cached_guild(&ctx.app, gid).and_then(|g| g.roles.get(&id).map(|r| r.mentionable));
    Some((id, cached.or_else(|| ctx.get_role(name).map(|r| r.mentionable)).unwrap_or(false)))
}

/// `checkPingRole(interaction, role, channel)`: refused for @everyone and for a role the member may not ping;
/// otherwise maybe a bot-side warning.
fn check_ping_role(ctx: &CommandCtx, gid: GuildId, role: (RoleId, bool), channel: &GuildChannel) -> Result<Option<String>, (&'static str, String)> {
    let (role_id, mentionable) = role;
    if role_id.get() == gid.get() {
        return Err(("Invalid Role", "@everyone can't be used as a ping role. Use a role members can opt into.".into()));
    }
    let refused = match cached_guild(&ctx.app, gid) {
        Some(g) => {
            let roles: Vec<RoleId> = ctx.member().map(|m| m.roles.clone()).unwrap_or_default();
            !pingable_role_ids(&g, channel, Some((ctx.user().id, roles.as_slice())), &[role_id.to_string()]).refused.is_empty()
        }
        None => true,
    };
    if refused {
        return Err((
            "Role Not Pingable",
            format!("<@&{role_id}> is not mentionable and you lack **Mention @everyone, @here and All Roles** in <#{}>.", channel.id),
        ));
    }
    Ok(bot_ping_warning(ctx, gid, channel, role_id, mentionable))
}

/// Lines 1-3 of the add / edit confirmation (`feedSummaryLines`).
pub fn feed_summary_lines(feed: &Feed, interval: bool) -> Vec<String> {
    let mut lines = vec![format!("{} **{}** → <#{}>", type_emoji(feed.r#type), escape_markdown(&feed.display_name), feed.channel_id)];
    if interval {
        lines.push(format!(
            "{} · checked every {} · {}",
            type_label(feed.r#type),
            bot_util::parse_duration::format_duration_long(base_interval_ms(feed.r#type) as f64),
            svc::source_link(feed.r#type, &feed.source, &feed.fetch_url)
        ));
    }
    lines.push(match feed.ping_role_id.as_deref().filter(|p| !p.is_empty()) {
        Some(p) => format!("Ping: <@&{p}>"),
        None => "Ping: none".into(),
    });
    lines
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let result: anyhow::Result<()> = async {
        let Some(settings) = load_settings(&ctx).await else { return Ok(()) };
        if group.as_deref() == Some("add") {
            return handle_add(&ctx, &sub, &settings).await;
        }
        match sub.as_str() {
            "list" => handle_list(&ctx, &settings).await,
            "remove" => handle_remove(&ctx).await,
            "test" => handle_test(&ctx).await,
            "edit" => handle_edit(&ctx, &settings).await,
            "enable" => handle_enable(&ctx, &settings).await,
            "disable" => handle_disable(&ctx).await,
            _ => {
                err(&ctx, "Unknown Subcommand", "That subcommand does not exist.").await;
                Ok(())
            }
        }
    }
    .await;
    if let Err(e) = result {
        // `if (isInteractionGone(err)) return;`
        if e.downcast_ref::<bot_core::interaction::InteractionError>().is_some_and(bot_core::interaction_helpers::is_interaction_gone) {
            return Ok(());
        }
        let (title, text) = match e.downcast_ref::<FeedUserError>() {
            Some(u) => (u.title.clone(), u.message.clone()),
            None => {
                bot_util::log_error!(format!("feed {} {sub} failed", group.as_deref().unwrap_or("")), bot_core::error::log_arg(&e));
                ("Something went wrong".to_string(), "I could not complete that. Please try again.".to_string())
            }
        };
        if ctx.is_acknowledged() {
            edit_error(&ctx, &title, &text).await;
        } else {
            err(&ctx, &title, &text).await;
        }
    }
    Ok(())
}

// ── add ────────────────────────────────────────────────────────────────────────

static PODCAST_URL_RE: LazyRe = lazy_re!(r"^(https?:\/\/|feed:)|^[\w-]+(\.[\w-]+)+\/\S*", "i");

/// `readAddSource(sub, options)`: the type and input of an `add` subcommand, or the error to show.
pub fn read_add_source(sub: &str, value: &str) -> Result<(FeedType, SourceInput), (&'static str, String)> {
    match sub {
        "rss" => normalize_feed_url(value).map(|u| (FeedType::Rss, SourceInput::Url(u))).map_err(|r| ("Invalid URL", r)),
        "youtube" => {
            parse_youtube_channel_input(value).map(|c| (FeedType::Youtube, SourceInput::ChannelId(c))).map_err(|r| (if r.handle { "Channel ID Needed" } else { "Invalid Channel" }, r.reason))
        }
        "reddit" => parse_subreddit_input(value).map(|n| (FeedType::Reddit, SourceInput::Name(n))).map_err(|r| ("Invalid Subreddit", r)),
        "bluesky" => parse_bluesky_actor_input(value).map(|a| (FeedType::Bluesky, SourceInput::Actor(a))).map_err(|r| ("Invalid Handle", r)),
        "podcast" => {
            let raw = bot_util::js::trim(value);
            if PODCAST_URL_RE.test(raw) {
                return normalize_feed_url(raw).map(|u| (FeedType::Podcast, SourceInput::Url(u))).map_err(|r| ("Invalid URL", r));
            }
            if bot_util::js::utf16_len(raw) < 2 {
                return Err(("Invalid Search", "Type at least 2 characters of the podcast's name, or paste its RSS feed URL.".into()));
            }
            Ok((FeedType::Podcast, SourceInput::Query(raw.to_string())))
        }
        _ => Err(("Unknown Feed Type", "That kind of feed is not supported.".into())),
    }
}

fn source_option(sub: &str) -> &'static str {
    match sub {
        "rss" => "url",
        "youtube" => "youtube_channel",
        "reddit" => "subreddit",
        "bluesky" => "handle",
        _ => "podcast",
    }
}

async fn handle_add(ctx: &CommandCtx, sub: &str, settings: &Settings) -> anyhow::Result<()> {
    let gid = gid_of(ctx)?;
    let g = gid.to_string();

    // Step A: cheap checks, before any defer (each failure is ephemeral).
    let (feed_type, input) = match read_add_source(sub, &ctx.get_string(source_option(sub)).unwrap_or_default()) {
        Ok(v) => v,
        Err((t, m)) => { err(ctx, t, &m).await; return Ok(()); },
    };
    let channel_id = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
    let channel = match check_target_channel(ctx, gid, channel_id) {
        Ok(c) => c,
        Err((t, m)) => { err(ctx, t, &m).await; return Ok(()); },
    };
    let mut warnings: Vec<String> = Vec::new();
    let role = ping_role_opt(ctx, gid, "ping_role");
    if let Some(r) = role {
        match check_ping_role(ctx, gid, r, &channel) {
            Ok(w) => warnings.extend(w),
            Err((t, m)) => { err(ctx, t, &m).await; return Ok(()); },
        }
    }
    let template = match svc::parse_message_option(ctx.get_string("message").as_deref()) {
        Ok(t) => t,
        Err(m) => { err(ctx, "Invalid Message", &m).await; return Ok(()); },
    };
    // Fast cap pre-check (create_feed_row checks again under the lock). Skipped when the ack budget ran out.
    let (pool, gg) = (ctx.app.db().clone(), g.clone());
    let existing = bot_util::async_util::with_timeout(async move { svc::count_feeds(&pool, &gg).await }, ctx.ack_budget_ms(), "feed cap").await;
    let limit = bot_config::limits::FEEDS as i64;
    match existing {
        Ok(n) if n >= limit => {
            err(ctx, "Feed Limit Reached", &format!("This server already follows {limit} feeds (the maximum). Remove one with `/feed remove` first.")).await;
            return Ok(());
        }
        Err(bot_util::async_util::WithTimeoutError::Inner(e)) => return Err(e),
        _ => {}
    }
    let include_reposts = feed_type == FeedType::Bluesky && ctx.get_boolean("include_reposts").unwrap_or(false);

    // Step B: acknowledge publicly; the rest may take a while (network).
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }

    // Step C: resolve and probe the source.
    let r = match svc::resolve_feed_source(&ctx.app, feed_type, &input).await {
        Ok(r) => r,
        Err(refusal) => { edit_error(ctx, &refusal.title, &refusal.message).await; return Ok(()); },
    };

    // Step D: duplicate.
    let dup = bot_db::BuiltQuery {
        sql: format!(
            r#"SELECT {} FROM {} WHERE "guildId" = $1 AND "type" = $2::"FeedType" AND "source" = $3 AND "channelId" = $4 LIMIT 1"#,
            FEED_INFO.select_list(),
            FEED_INFO.quoted_table()
        ),
        args: vec![SqlValue::Text(Some(g.clone())), SqlValue::Text(Some(feed_type.as_str().into())), SqlValue::Text(Some(r.source.clone())), SqlValue::Text(Some(channel.id.to_string()))],
    }
    .fetch_optional::<Feed, _>(ctx.app.db())
    .await?;
    if let Some(d) = dup {
        edit_error(ctx, "Already Following", &format!("**{}** is already posted in <#{}>.", escape_markdown(&d.display_name), channel.id)).await;
        return Ok(());
    }

    // Step E: create (a FeedUserError for the cap and a duplicate).
    let probe_items = match &r.probe {
        Probe::Ok { items, .. } => items.len(),
        Probe::Failed(_) => 0,
    };
    let row = svc::create_feed_row(
        &ctx.app,
        NewFeed {
            guild_id: g.clone(),
            feed_type,
            source: r.source.clone(),
            display_name: r.display_name.clone(),
            fetch_url: r.fetch_url.clone(),
            channel_id: channel.id.to_string(),
            ping_role_id: role.map(|x| x.0.to_string()),
            message_template: template.clone(),
            include_reposts,
            created_by: ctx.user().id.to_string(),
            probe: Some(r.probe.clone()),
        },
    )
    .await?;

    // Step F: schedule the first check.
    let mut schedule_warnings = Vec::new();
    let delay = if row.initialized { base_interval_ms(feed_type) } else { 2 * 60_000 };
    let run_at = ctx.app.clock().now() + chrono::Duration::milliseconds(delay);
    if let Err(e) = svc::schedule_feed_job(&ctx.app, &row.guild_id, &row.id, run_at).await {
        bot_util::log_error!(format!("feed add: could not schedule feed {}", row.id), bot_core::error::log_arg(&e));
        schedule_warnings.push(format!("⚠️ I couldn't schedule the first check (database busy). Run `/feed enable feed:{}` to retry.", row.display_name));
    }

    // Step G: reply.
    let mut lines = feed_summary_lines(&row, true);
    lines.push(if row.initialized {
        format!("✅ {probe_items} existing item(s) recorded — only new ones will be posted.")
    } else {
        format!("⚠️ I couldn't fetch it yet: {}. I'll keep trying; see `/feed list`.", row.last_error.as_deref().unwrap_or("null"))
    });
    lines.push(format!("Variables: {}", svc::var_help(feed_type)));
    lines.extend(r.warnings.iter().cloned());
    lines.extend(warnings);
    lines.extend(svc::template_warnings(feed_type, template.as_deref()));
    lines.extend(schedule_warnings);
    if let Some(off) = module_off_note(settings) {
        lines.push(off.into());
    }
    edit_embed(ctx, success_embed("Feed Added", Some(&clamp_text(&lines.join("\n"), 4000)), None)).await;
    Ok(())
}

// ── list ───────────────────────────────────────────────────────────────────────

/// One `/feed list` entry.
pub fn list_entry(i: usize, r: &Feed, next: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let (icon, text) = feed_status(r);
    let mut extras = Vec::new();
    if r.enabled && let Some(n) = next {
        extras.push(format!("next check {}", bot_util::time::discord_timestamp(&n, "R")));
    }
    if let Some(p) = r.last_posted_at {
        extras.push(format!("last post {}", bot_util::time::discord_timestamp(&p, "R")));
    } else if let Some(c) = r.last_checked_at {
        extras.push(format!("checked {}", bot_util::time::discord_timestamp(&c, "R")));
    }
    let ping = r.ping_role_id.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" · <@&{p}>")).unwrap_or_default();
    format!(
        "`{}.` {} **{}** → <#{}>{ping}\n↳ {icon} {}{}",
        i + 1,
        type_emoji(r.r#type),
        escape_markdown(&r.display_name),
        r.channel_id,
        clamp_text(&text, 120),
        if extras.is_empty() { String::new() } else { format!(" · {}", extras.join(" · ")) }
    )
}

async fn handle_list(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let g = gid_of(ctx)?.to_string();
    let rows = svc::list_feeds(ctx.app.db(), &g).await?;
    if rows.is_empty() {
        edit_embed(ctx, info_embed("Feeds", Some("No feeds yet. Add one with `/feed add rss`, `youtube`, `reddit`, `bluesky` or `podcast`."), None)).await;
        return Ok(());
    }
    let mut entries = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let next = ctx.app.scheduler().get_job(&feed_job_key(&r.id)).await.filter(|j| j.status == bot_db::JobStatus::Pending).map(|j| j.run_at);
        entries.push(list_entry(i, r, next));
    }
    let off = module_off_note(settings);
    let footer = format!("{}/{} feeds", rows.len(), bot_config::limits::FEEDS);
    let pages: Vec<CreateEmbed> = bot_util::text::chunk_lines(&entries, 3500)
        .into_iter()
        .map(|chunk| {
            let desc = match off {
                Some(o) => format!("{o}\n\n{chunk}"),
                None => chunk,
            };
            guild_embed(Some(settings), Some("📰 Social Feeds"), Some(&desc), Some(&footer))
        })
        .collect();
    if pages.len() == 1 {
        edit_embed(ctx, pages.into_iter().next().unwrap_or_default()).await;
        return Ok(());
    }
    bot_core::paginator::paginate(ctx, &pages, bot_core::paginator::PaginateOptions::default()).await?;
    Ok(())
}

// ── remove ─────────────────────────────────────────────────────────────────────

async fn handle_remove(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(feed) = lookup_feed(ctx).await? else { return Ok(()) };
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    bot_db::delete_where(&FEED_INFO, [("id", json!(feed.id)), ("guildId", json!(feed.guild_id))])?.execute(ctx.app.db()).await?;
    ctx.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(feed_job_key(&feed.id)), ..Default::default() }).await;
    let text = format!("**{}** will no longer be posted in <#{}>.", escape_markdown(&feed.display_name), feed.channel_id);
    edit_embed(ctx, success_embed("Feed Removed", Some(&text), None)).await;
    Ok(())
}

// ── test ───────────────────────────────────────────────────────────────────────

async fn handle_test(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(feed) = lookup_feed(ctx).await? else { return Ok(()) };
    if ctx.defer_reply(true).await.is_err() {
        return Ok(());
    }
    if !svc::feed_test_allowed(&ctx.app, &feed.id) {
        edit_embed(ctx, warn_embed("Slow Down", Some("This feed was just tested. Try again in a few seconds."), None)).await;
        return Ok(());
    }
    let t0 = ctx.app.clock().now_ms();
    // A fresh, unconditional fetch (a 304 would leave nothing to preview). It never writes the Feed row.
    let (items, image) = match svc::fetch_feed_source(&ctx.app, &feed, false, false).await {
        SourceResult::Err { error, .. } => {
            edit_error(ctx, "Fetch Failed", &format!("{}\nNothing was changed.", describe_fetch_error(feed.r#type, &error))).await;
            return Ok(());
        }
        SourceResult::Ok { items, image, .. } => (items, image),
    };
    let Some(newest) = items.iter().find(|i| feed.include_reposts || !i.is_repost) else {
        edit_embed(ctx, info_embed("No Items", Some("The feed answered but has no items to show yet."), None)).await;
        return Ok(());
    };
    let gid = gid_of(ctx)?;
    let guild = cached_guild(&ctx.app, gid);
    let channel = feed.channel_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| cached_channel(&ctx.app, gid, ChannelId::new(c)));
    let payload = svc::build_post_payload(
        &ctx.app,
        &feed,
        newest,
        &svc::PostCtx { guild: guild.as_ref(), channel: channel.as_ref(), feed_image: image.as_deref(), ping: false, now: ctx.app.clock().now() },
    );
    let (icon, text) = feed_status(&feed);
    let lines = format!(
        "Fetched {} item(s) in {} ms. Above is how the newest one would look in <#{}>.\nStatus: {icon} {}",
        items.len(),
        ctx.app.clock().now_ms() - t0,
        feed.channel_id,
        clamp_text(&text, 200)
    );
    let mut embeds = payload.embeds.clone();
    embeds.push(info_embed("Feed Test", Some(&lines), None));
    let mut p = Payload::new().embeds(embeds).no_mentions();
    if let Some(c) = payload.content {
        p = p.content(c);
    }
    safe_reply(&ctx.responder, p).await;
    Ok(())
}

// ── edit ───────────────────────────────────────────────────────────────────────

async fn handle_edit(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let Some(feed) = lookup_feed(ctx).await? else { return Ok(()) };
    let gid = gid_of(ctx)?;
    let channel_opt = ctx.get_channel_id("channel");
    let role_opt = ctx.guild_id().and_then(|g| ping_role_opt(ctx, g, "ping_role"));
    let clear_role = ctx.get_boolean("clear_ping_role") == Some(true);
    let message = ctx.get_string("message");
    let reset_message = ctx.get_boolean("reset_message") == Some(true);
    let reposts = ctx.get_boolean("include_reposts");

    if role_opt.is_some() && clear_role {
        { err(ctx, "Conflicting Options", "Use either `ping_role` or `clear_ping_role`, not both.").await; return Ok(()); }
    }
    if message.is_some() && reset_message {
        { err(ctx, "Conflicting Options", "Use either `message` or `reset_message`, not both.").await; return Ok(()); }
    }
    if reposts.is_some() && feed.r#type != FeedType::Bluesky {
        { err(ctx, "Not a Bluesky Feed", "Only Bluesky feeds can include reposts.").await; return Ok(()); }
    }

    let mut warnings = Vec::new();
    let mut data: Vec<(&str, Value)> = Vec::new();
    let mut channel = feed.channel_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| cached_channel(&ctx.app, gid, ChannelId::new(c)));
    if let Some(cid) = channel_opt {
        match check_target_channel(ctx, gid, cid) {
            Ok(c) => {
                if c.id.to_string() != feed.channel_id {
                    data.push(("channelId", json!(c.id.to_string())));
                    if feed.failure_count == 0 {
                        data.push(("lastError", Value::Null));
                    }
                }
                channel = Some(c);
            }
            Err((t, m)) => { err(ctx, t, &m).await; return Ok(()); },
        }
    }
    let guild = cached_guild(&ctx.app, gid);
    let mut final_role: Option<(RoleId, bool)> = feed
        .ping_role_id
        .as_deref()
        .and_then(|p| p.parse::<u64>().ok())
        .and_then(|p| guild.as_ref().and_then(|g| g.roles.get(&RoleId::new(p)).map(|r| (r.id, r.mentionable))));
    if let Some(role) = role_opt {
        let Some(ch) = &channel else {
            { err(ctx, "Channel Problem", "Its channel no longer exists. Pick a new one with `channel:` in the same command.").await; return Ok(()); }
        };
        if let Err((t, m)) = check_ping_role(ctx, gid, role, ch) {
            { err(ctx, t, &m).await; return Ok(()); }
        }
        data.push(("pingRoleId", json!(role.0.to_string())));
        final_role = Some(role);
    } else if clear_role {
        data.push(("pingRoleId", Value::Null));
        final_role = None;
    }
    if let (Some((rid, mentionable)), Some(ch)) = (final_role, &channel)
        && (channel_opt.is_some() || role_opt.is_some())
        && let Some(w) = bot_ping_warning(ctx, gid, ch, rid, mentionable)
    {
        warnings.push(w);
    }
    let mut template = None;
    if let Some(m) = &message {
        match svc::parse_message_option(Some(m)) {
            Err(e) => { err(ctx, "Invalid Message", &e).await; return Ok(()); },
            Ok(None) => { err(ctx, "Invalid Message", "The message can't be empty. Use reset_message to go back to the default.").await; return Ok(()); },
            Ok(Some(t)) => {
                data.push(("messageTemplate", json!(t)));
                template = Some(t);
            }
        }
    } else if reset_message {
        data.push(("messageTemplate", Value::Null));
    }
    if let Some(r) = reposts
        && r != feed.include_reposts
    {
        data.push(("includeReposts", json!(r)));
    }
    if data.is_empty() {
        { err(ctx, "Nothing to Change", "Give at least one option to change.").await; return Ok(()); }
    }

    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let q = UpdateBuilder::new(&FEED_INFO).set_all(data)?.filter("id", json!(feed.id))?.build()?;
    let updated = match q.fetch_optional::<Feed, _>(ctx.app.db()).await {
        Ok(Some(u)) => u,
        Ok(None) => anyhow::bail!("Record to update not found."),
        Err(e) if e.is_unique_violation() => { edit_error(ctx, "Already Following", "That channel already has this feed.").await; return Ok(()); },
        Err(e) => return Err(e.into()),
    };
    let mut lines = feed_summary_lines(&updated, false);
    lines.extend(warnings);
    lines.extend(svc::template_warnings(updated.r#type, template.as_deref()));
    if !updated.enabled {
        lines.push(format!("⏸️ This feed is disabled — run `/feed enable feed:{}` to turn it on.", updated.display_name));
    }
    if let Some(off) = module_off_note(settings) {
        lines.push(off.into());
    }
    edit_embed(ctx, success_embed("Feed Updated", Some(&clamp_text(&lines.join("\n"), 4000)), None)).await;
    Ok(())
}

// ── enable / disable ───────────────────────────────────────────────────────────

async fn handle_enable(ctx: &CommandCtx, settings: &Settings) -> anyhow::Result<()> {
    let Some(feed) = lookup_feed(ctx).await? else { return Ok(()) };
    let gid = gid_of(ctx)?;
    let cid = feed.channel_id.parse::<u64>().ok().filter(|v| *v != 0).map(ChannelId::new);
    let u = check_channel_usable(&ctx.app, gid, cid, &NEEDS);
    if !u.ok {
        let text = format!("{} Pick a new one with `/feed edit channel:` first.", u.reason.as_deref().unwrap_or("Its channel no longer exists."));
        { err(ctx, "Channel Problem", &text).await; return Ok(()); }
    }
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    let was_disabled = !feed.enabled;
    // Re-enabling resets `initialized`: the first check re-seeds, so items published while it was off are skipped.
    let mut q = UpdateBuilder::new(&FEED_INFO).set("failureCount", json!(0))?.set("lastError", Value::Null)?;
    if was_disabled {
        q = q.set("enabled", json!(true))?.set("disabledReason", Value::Null)?.set("initialized", json!(false))?;
    }
    let updated = q.filter("id", json!(feed.id))?.build()?.fetch_optional::<Feed, _>(ctx.app.db()).await?.ok_or_else(|| anyhow::anyhow!("Record to update not found."))?;
    svc::schedule_feed_job(&ctx.app, &updated.guild_id, &updated.id, ctx.app.clock().now() + chrono::Duration::milliseconds(10_000)).await?;
    let mut lines = vec![format!(
        "I'll check **{}** in a few seconds.{}",
        escape_markdown(&updated.display_name),
        if was_disabled { " Items published while it was off are skipped; only new ones will be posted." } else { "" }
    )];
    if let Some(off) = module_off_note(settings) {
        lines.push(off.into());
    }
    edit_embed(ctx, success_embed("Feed Enabled", Some(&lines.join("\n")), None)).await;
    Ok(())
}

async fn handle_disable(ctx: &CommandCtx) -> anyhow::Result<()> {
    let Some(feed) = lookup_feed(ctx).await? else { return Ok(()) };
    let name = escape_markdown(&feed.display_name);
    if !feed.enabled {
        let text = format!("**{name}** is already disabled: {}.", feed.disabled_reason.as_deref().unwrap_or("paused"));
        { err(ctx, "Already Paused", &text).await; return Ok(()); }
    }
    if ctx.defer_reply(false).await.is_err() {
        return Ok(());
    }
    UpdateBuilder::new(&FEED_INFO)
        .set("enabled", json!(false))?
        .set("disabledReason", json!("Paused by an admin"))?
        .filter("id", json!(feed.id))?
        .without_returning()
        .build()?
        .execute(ctx.app.db())
        .await?;
    ctx.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(feed_job_key(&feed.id)), ..Default::default() }).await;
    let text = format!("**{name}** is paused. Its settings are kept; turn it back on with `/feed enable`.");
    edit_embed(ctx, success_embed("Feed Paused", Some(&text), None)).await;
    Ok(())
}

// ── Autocomplete ───────────────────────────────────────────────────────────────

/// The autocomplete rows: the feeds whose name or source contains `q` (Prisma's unescaped `ILIKE '%q%'`).
pub async fn search_feeds(pool: &bot_db::PgPool, guild_id: &str, q: &str) -> anyhow::Result<Vec<Feed>> {
    let mut args = vec![SqlValue::Text(Some(guild_id.to_string()))];
    let mut where_sql = String::from(r#""guildId" = $1"#);
    if !q.is_empty() {
        args.push(SqlValue::Text(Some(format!("%{q}%"))));
        args.push(SqlValue::Text(Some(format!("%{q}%"))));
        where_sql.push_str(r#" AND ("displayName" ILIKE $2 OR "source" ILIKE $3)"#);
    }
    where_sql.push_str(r#" ORDER BY "createdAt" ASC LIMIT 25"#);
    let sql = format!("SELECT {} FROM {} WHERE {where_sql}", FEED_INFO.select_list(), FEED_INFO.quoted_table());
    Ok(bot_db::BuiltQuery { sql, args }.fetch_all::<Feed, _>(pool).await?)
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let allowed = ctx.interaction.member.as_ref().and_then(|m| m.permissions).is_some_and(|p| p.contains(Permissions::MANAGE_GUILD) || p.contains(Permissions::ADMINISTRATOR));
    let Some(gid) = ctx.guild_id().filter(|_| allowed) else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let q = bot_util::js::utf16_prefix(bot_util::js::trim(&ctx.focused_value()), 100).to_string();
    let choices = match search_feeds(ctx.app.db(), &gid.to_string(), &q).await {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                let channel = r.channel_id.parse::<u64>().ok().filter(|v| *v != 0).and_then(|c| cached_channel(&ctx.app, gid, ChannelId::new(c))).map(|c| c.name);
                let name = format!(
                    "{}{} {} → #{}",
                    if r.enabled { "" } else { "⏸️ " },
                    type_emoji(r.r#type),
                    r.display_name,
                    channel.unwrap_or_else(|| "deleted-channel".into())
                );
                Choice::new(name, r.id.clone())
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
