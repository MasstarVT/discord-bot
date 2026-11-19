//! Port of `src/commands/community/poll.js`: MEE6 Polls (`/poll`, `/poll-end`). Thin: option parsing, validation and
//! replies; everything else (the form, voting, live results, ending) is [`crate::poll_service`]. `create` opens a
//! modal and does no I/O before `show_modal`; the poll is posted when the form is submitted.

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Choice, Payload, cached_channel, is_interaction_gone, respond_autocomplete, safe_defer, safe_reply};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_config::limits;
use bot_db::PollStatus;
use bot_util::text::{chunk_lines, clamp_text};
use serenity::all::Permissions;
use serenity::builder::CreateEmbed;

use crate::poll_service::{
    DurationError, EndPollError, EndReason, PendingPoll, PendingPolls, build_create_modal, build_results_embed, channel_problem, end_poll,
    find_poll_in, find_polls_for_autocomplete, get_tally, list_active_polls, parse_duration_option, poll_list_lines,
};

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    SlashCommand::new("poll", "Create and manage polls (MEE6 /poll, /poll-end).")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(
            Opt::sub_command("create", "Create a poll with 2–20 options; a form opens for the question and options.")
                .option(Opt::string("duration", "How long it runs, e.g. 30m, 2h, 3d (max 30d). Empty = until /poll end").max_length(32))
                .option(Opt::integer("max_choices", "Options each member may pick (default 1)").min_value(1).max_value(limits::POLL_OPTIONS as i64))
                .option(Opt::role("allowed_role", "Only members with this role can vote"))
                .option(Opt::role("denied_role", "Members with this role cannot vote"))
                .option(Opt::boolean("hide_results", "Hide results until the poll ends (default: live)"))
                .option(Opt::channel("channel", "Where to post it (default: this channel)").channel_types(&[
                    channel_type::GUILD_TEXT,
                    channel_type::GUILD_ANNOUNCEMENT,
                    channel_type::GUILD_VOICE,
                    channel_type::PUBLIC_THREAD,
                    channel_type::PRIVATE_THREAD,
                    channel_type::ANNOUNCEMENT_THREAD,
                ])),
        )
        .option(
            Opt::sub_command("end", "End a poll in this channel and post the results (MEE6 /poll-end).")
                .option(Opt::string("poll", "Active poll in this channel").required(true).autocomplete(true)),
        )
        .option(
            Opt::sub_command("results", "Show a poll's results privately, even while hidden.")
                .option(Opt::string("poll", "Poll on this server").required(true).autocomplete(true)),
        )
        .option(Opt::sub_command("list", "List the active polls on this server."))
}

/// The command (`module 'polls'`, gated, MODERATOR, servers only).
pub fn command() -> CommandDef {
    CommandDef::new("community", data(), Some(ModuleKey::Polls), execute)
        .permission_level(PermissionLevel::Moderator)
        .autocomplete(autocomplete)
}

/// `DURATION_ERRORS`.
pub fn duration_error_text(e: DurationError) -> &'static str {
    match e {
        DurationError::Invalid => "Use a duration like `30m`, `2h`, `1d` or `1w`.",
        DurationError::TooShort => "Polls must run for at least 1 minute.",
        DurationError::TooLong => "Polls can run for at most 30 days.",
    }
}

fn not_found() -> CreateEmbed {
    error_embed("Poll Not Found", Some("Pick a poll from the list."), None)
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("create") => create(ctx).await,
        Some("end") => end(ctx).await,
        Some("results") => results(ctx).await,
        Some("list") => list(ctx).await,
        _ => Ok(()),
    }
}

// ── create ────────────────────────────────────────────────────────────────────

/// Every check here is synchronous: `show_modal` must be the first response.
async fn create(ctx: CommandCtx) -> BotResult {
    let deny = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true));
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let g = guild_id.to_string();

    let duration_ms = match parse_duration_option(ctx.get_string("duration").as_deref()) {
        Ok(d) => d,
        Err(e) => {
            deny(error_embed("Invalid Duration", Some(duration_error_text(e)), None)).await;
            return Ok(());
        }
    };
    let max_choices = ctx.get_integer("max_choices").unwrap_or(1);
    let allowed = ctx.get_role_id("allowed_role").map(|r| r.to_string());
    let denied = ctx.get_role_id("denied_role").map(|r| r.to_string());
    let hide_results = ctx.get_boolean("hide_results").unwrap_or(false);
    if denied.as_deref() == Some(g.as_str()) {
        deny(error_embed("Invalid Roles", Some("You can't deny @everyone, or nobody could vote."), None)).await;
        return Ok(());
    }
    if allowed.is_some() && allowed == denied {
        deny(error_embed("Invalid Roles", Some("The allowed role and the denied role must be different."), None)).await;
        return Ok(());
    }
    // @everyone = no restriction.
    let allowed_role_ids: Vec<String> = allowed.into_iter().filter(|a| *a != g).collect();

    let picked = ctx.get_channel_id("channel");
    let channel_id = picked.unwrap_or_else(|| ctx.channel_id());
    let mut channel = cached_channel(&ctx.app, guild_id, channel_id);
    if channel.is_none() && picked.is_some() {
        // discord.js builds a picked channel the cache lacks (an archived thread, say) from the interaction's resolved
        // data. That data has no permission overwrites, so Discord is asked, within half the answer window: the modal
        // must still be the first response.
        let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id) / 2;
        let cid = channel_id.to_string();
        let fetch = crate::guild_view::fetch_guild_channel(&ctx.app, &g, &cid);
        channel = tokio::time::timeout(std::time::Duration::from_millis(budget), fetch).await.ok().and_then(Result::ok).flatten();
    }
    if let Some(problem) = channel_problem(&ctx.app, channel.as_ref(), ctx.member()) {
        deny(problem).await;
        return Ok(());
    }
    let Some(channel) = channel else { return Ok(()) };

    let nonce = ctx.interaction.id.to_string();
    let pending = PendingPoll {
        guild_id: g,
        channel_id: channel.id.to_string(),
        user_id: ctx.user().id.to_string(),
        max_choices,
        allowed_role_ids,
        denied_role_ids: denied.into_iter().collect(),
        hide_results,
        duration_ms,
    };
    let store = ctx.app.ext::<PendingPolls>();
    store.0.put(&nonce, pending, ctx.app.clock().now_ms());
    if let Err(e) = ctx.show_modal(build_create_modal(&nonce)).await {
        store.0.take(&nonce, ctx.app.clock().now_ms());
        if !is_interaction_gone(&e) {
            bot_util::log_error!("poll create: showModal failed", crate::giveaway_service::err_arg(&e));
        }
    }
    Ok(())
}

// ── end / results / list ──────────────────────────────────────────────────────

async fn end(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let reply = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed));
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let id = ctx.get_string("poll").ok_or_else(|| anyhow::anyhow!("Required option \"poll\" not found."))?;
    let Some(p) = find_poll_in(ctx.app.db(), &id, &guild_id).await? else {
        reply(not_found()).await;
        return Ok(());
    };
    let r = end_poll(&ctx.app, &p.id, EndReason::Manual, Some(&guild_id)).await?;
    let embed = match r {
        Err(EndPollError::AlreadyEnded) => warn_embed("Already Ended", Some("That poll has already ended."), None),
        Err(EndPollError::NotFound) => not_found(),
        Ok(done) => success_embed(
            "Poll Ended",
            Some(&if done.posted {
                format!("The results were posted in <#{}>.", p.channel_id)
            } else {
                format!("The poll is closed, but I couldn't post the results in <#{}>.", p.channel_id)
            }),
            None,
        ),
    };
    reply(embed).await;
    Ok(())
}

async fn results(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let reply = |embed: CreateEmbed| safe_reply(&ctx.responder, Payload::embed(embed));
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let id = ctx.get_string("poll").ok_or_else(|| anyhow::anyhow!("Required option \"poll\" not found."))?;
    let Some(mut p) = find_poll_in(ctx.app.db(), &id, &guild_id).await? else {
        reply(not_found()).await;
        return Ok(());
    };
    // Self-heal: the end job is late or missing.
    if p.status == PollStatus::Active && p.ends_at.is_some_and(|e| e.timestamp_millis() < ctx.app.clock().now_ms() - 60_000) {
        let _ = end_poll(&ctx.app, &p.id, EndReason::Expired, Some(&guild_id)).await?;
        if let Some(fresh) = find_poll_in(ctx.app.db(), &id, &guild_id).await? {
            p = fresh;
        }
    }
    let (tally, settings) = tokio::join!(get_tally(ctx.app.db(), &p), ctx.app.settings().get_settings_safe(&guild_id));
    let tally = tally?;
    reply(build_results_embed(&p, &tally, settings.as_deref(), p.status == PollStatus::Active, true)).await;
    Ok(())
}

async fn list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().map(|g| g.to_string()).unwrap_or_default();
    let settings = ctx.app.settings().get_settings_safe(&guild_id).await;
    let (polls, votes) = list_active_polls(ctx.app.db(), &guild_id).await?;
    if polls.is_empty() {
        safe_reply(&ctx.responder, Payload::embed(info_embed("No Active Polls", Some("There are no active polls. Start one with `/poll create`."), None)))
            .await;
        return Ok(());
    }
    let title = format!("📊 Active polls ({}/{})", polls.len(), limits::ACTIVE_POLLS);
    let lines = poll_list_lines(&polls, &votes, ctx.app.clock().now_ms());
    let pages: Vec<CreateEmbed> = chunk_lines(&lines, 4000).iter().map(|c| guild_embed(settings.as_deref(), Some(&title), Some(c), None)).collect();
    paginate(&ctx, &pages, PaginateOptions { ephemeral: true, ..Default::default() }).await?;
    Ok(())
}

// ── autocomplete ──────────────────────────────────────────────────────────────

/// The name of an autocomplete choice.
pub fn choice_name(sub: &str, question: &str, created_ms: i64, now_ms: i64, status: Option<PollStatus>, channel_name: Option<&str>) -> String {
    if sub == "end" {
        let age = bot_util::parse_duration::format_duration(((now_ms - created_ms).max(1000)) as f64);
        let first = age.split(' ').next().unwrap_or("").to_string();
        format!("{} · {first} ago", clamp_text(question, 75))
    } else {
        format!(
            "{} {} · #{}",
            if status == Some(PollStatus::Active) { "🟢" } else { "🏁" },
            clamp_text(question, 60),
            channel_name.unwrap_or("deleted-channel")
        )
    }
}

/// Read-only: nothing is ended, edited or written. Answers within the 3 s window or with no choices.
async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (name, value) = ctx.focused().unwrap_or_default();
    let sub = ctx.subcommand().unwrap_or_default();
    if name != "poll" || (sub != "end" && sub != "results") {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else {
        respond_autocomplete(&ctx.responder, &[]).await;
        return Ok(());
    };
    let q = bot_util::js::trim(&value).to_string();
    let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
    let (db, g, ch, s, qq) = (ctx.app.db().clone(), guild_id.to_string(), ctx.interaction.channel_id.to_string(), sub.clone(), q.clone());
    let rows = bot_util::async_util::with_timeout(
        async move { find_polls_for_autocomplete(&db, &g, &ch, &s, &qq).await.map_err(|e| e.to_string()) },
        budget,
        "poll autocomplete",
    )
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_warn!("poll autocomplete failed", crate::giveaway_service::err_arg(&e));
            respond_autocomplete(&ctx.responder, &[]).await;
            return Ok(());
        }
    };
    let now = ctx.app.clock().now_ms();
    let guild = bot_core::interaction_helpers::cached_guild(&ctx.app, guild_id);
    let choices: Vec<Choice> = rows
        .iter()
        .map(|p| {
            let channel_name = p.channel_id.as_deref().and_then(|c| {
                let cid = serenity::all::ChannelId::new(crate::guild_view::snowflake(c)?);
                guild.as_ref().and_then(|g| bot_core::permissions::find_channel(g, cid)).map(|c| c.name.clone())
            });
            Choice::new(choice_name(&sub, &p.question, p.created_at.timestamp_millis(), now, p.status, channel_name.as_deref()), p.id.clone())
        })
        .collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
