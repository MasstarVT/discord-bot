//! Port of `src/commands/moderation/case.js`: `/case view | reason | proof | history | clear-history`. Every displayed
//! value is clamped (older rows may hold very long text); reason and proof edits are bounded to 1000 characters.

use bot_core::interaction_helpers::{ConfirmOptions, Payload, await_confirm, is_interaction_gone, safe_defer};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::{GuildSettings, Infraction, InfractionType, ModVisibility};
use bot_util::text::clamp_text;
use bot_util::time::discord_timestamp;
use serenity::all::{CreateEmbed, CreateEmbedFooter, Permissions, User, UserId};

use super::common::{deliver, edit_embed, guild_of, reason_opt, required_user, tag_of};
use crate::guild_ops::{UserRef, rest, snowflake};
use crate::moderation_service::{
    CreateOpts, action_meta, create_infraction, deliver_lookup, display_type, find_case, msg, post_mod_log,
};

/// `MAX_CASE_ID`: `caseId` is a 32-bit Int column.
pub const MAX_CASE_ID: i64 = 2_147_483_647;
/// Cases per history page.
pub const CASES_PER_PAGE: usize = 5;
/// At most this many rows are listed.
pub const MAX_ROWS: i64 = 250;

/// `TYPE_CHOICES`: (name, value).
pub const TYPE_CHOICES: [(&str, &str); 10] = [
    ("Active only", "active"),
    ("Warnings", "WARN"),
    ("Timeouts", "TIMEOUT"),
    ("Mutes", "MUTE"),
    ("Kicks", "KICK"),
    ("Softbans", "SOFTBAN"),
    ("Bans", "BAN"),
    ("Unbans", "UNBAN"),
    ("Unmutes", "UNMUTE"),
    ("Notes", "NOTE"),
];

/// `TYPE_LABEL[value]`.
pub fn type_label(value: &str) -> Option<&'static str> {
    TYPE_CHOICES.iter().find(|(_, v)| *v == value).map(|(n, _)| *n)
}

/// The command's data.
pub fn data() -> SlashCommand {
    let mut type_opt = CommandOption::string("type", "Only show one kind of case");
    for (name, value) in TYPE_CHOICES {
        type_opt = type_opt.choice(name, value);
    }
    SlashCommand::new("case", "View or edit a moderation case, or list or clear a member's infractions.")
        .default_member_permissions(Permissions::MODERATE_MEMBERS)
        .option(
            CommandOption::sub_command("view", "View a case.")
                .option(CommandOption::integer("id", "Case number").required(true).min_value(1)),
        )
        .option(
            CommandOption::sub_command("reason", "Change a case's reason.")
                .option(CommandOption::integer("id", "Case number").required(true).min_value(1))
                .option(CommandOption::string("reason", "New reason").required(true).max_length(1000)),
        )
        .option(
            CommandOption::sub_command("proof", "Set a case's proof link.")
                .option(CommandOption::integer("id", "Case number").required(true).min_value(1))
                .option(CommandOption::string("url", "Proof link (http/https)").required(true).max_length(1000)),
        )
        .option(
            CommandOption::sub_command("history", "List a member's moderation cases (MEE6 /infractions).")
                .option(CommandOption::user("user", "Member to look up").required(true))
                .option(type_opt),
        )
        .option(
            CommandOption::sub_command("clear-history", "Pardon all of a member's active warnings (MEE6 /clear-all-infractions).")
                .option(CommandOption::user("user", "Member whose warnings to clear").required(true))
                .option(reason_opt("Why the warnings are cleared")),
        )
}

/// `/case` (`module 'moderation'`, MODERATOR; `clear-history` is ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("moderation", data(), Some(ModuleKey::Moderation), execute)
        .permission_level(PermissionLevel::Moderator)
        .subcommand_permission("clear-history", PermissionLevel::Admin)
}

fn not_found(case_id: i64) -> CreateEmbed {
    error_embed("Not Found", Some(&format!("Case #{case_id} doesn't exist in this server.")), None)
}

/// `quote(text)`: every line of a block quote.
pub fn quote(text: &str) -> String {
    text.replace('\n', "\n> ")
}

/// `.replace(/\s*\n\s*/g, ' ')`: a whitespace run containing a newline becomes one space.
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.contains('\n') {
            out.push(' ');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if bot_util::js::is_js_space(c) || c == '\n' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn meta_of(t: &str) -> (String, String) {
    match action_meta(t) {
        Some(m) => (m.emoji.to_string(), m.label.to_string()),
        None => ("•".to_string(), t.to_string()),
    }
}

/// `personField(client, id)`: "<@id> `tag`", or "Unknown" when the user cannot be loaded.
async fn person_field(app: &App, id: &str) -> String {
    let cached = snowflake(id).and_then(|u| app.discord().and_then(|d| d.cache.user(UserId::new(u)).map(|u| u.clone())));
    let user = match cached {
        Some(u) => Some(u),
        None => match snowflake(id) {
            Some(u) => rest(app).get_user(UserId::new(u)).await.ok(),
            None => None,
        },
    };
    match user {
        Some(u) => format!("<@{id}> `{}`", clamp_text(&UserRef::of(&u).label().replace('`', "'"), 100)),
        None => format!("<@{id}> `Unknown`"),
    }
}

/// The `/case view` embed.
pub fn case_embed(inf: &Infraction, target: &str, moderator: &str) -> CreateEmbed {
    let dt = display_type(inf);
    let (emoji, label, color) = match action_meta(dt) {
        Some(m) => (m.emoji, m.label.to_string(), m.color),
        None => ("📋", inf.r#type.as_str().to_string(), bot_config::colors::NEUTRAL),
    };
    let mut embed = neutral_embed(Some(&clamp_text(&format!("{emoji} Case #{} | {label}", inf.case_id), 256)), None, None).color(color);
    let reason = inf.reason.as_deref().filter(|r| !r.is_empty()).unwrap_or("None");
    embed = embed.fields(vec![
        ("Target", target.to_string(), true),
        ("Moderator", moderator.to_string(), true),
        ("Reason", clamp_text(reason, 1024), false),
        ("Active", if inf.active { "Yes".into() } else { "No".into() }, true),
        ("Created", discord_timestamp(&inf.created_at, "F"), true),
    ]);
    if let Some(at) = inf.expires_at {
        embed = embed.field("Expires", format!("{} ({})", discord_timestamp(&at, "F"), discord_timestamp(&at, "R")), true);
    }
    if let Some(p) = inf.proof_url.as_deref().filter(|p| !p.is_empty()) {
        embed = embed.field("Proof", clamp_text(p, 1024), false);
    }
    embed.footer(CreateEmbedFooter::new(format!("User ID: {}", inf.target_user_id)))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(()); // errors stay private
    }
    let sub = ctx.subcommand().unwrap_or_default();
    let g = guild_of(&ctx)?;
    if sub == "history" || sub == "clear-history" {
        let settings = ctx.app.settings().get_settings_safe(&g).await;
        if sub == "clear-history" {
            return clear_warnings(&ctx, &g, settings.as_deref()).await;
        }
        return view_history(&ctx, &g, settings.as_deref()).await;
    }

    let case_id = ctx.get_integer("id").ok_or_else(|| anyhow::anyhow!("Required option \"id\" not found."))?;
    if !(1..=MAX_CASE_ID).contains(&case_id) {
        return edit_embed(&ctx, not_found(case_id)).await;
    }

    // ── view ──
    if sub == "view" {
        let settings = ctx.app.settings().get_settings_safe(&g).await;
        let Some(inf) = find_case(&ctx.app, &g, case_id).await? else { return edit_embed(&ctx, not_found(case_id)).await };
        let target = person_field(&ctx.app, &inf.target_user_id).await;
        let moderator = person_field(&ctx.app, &inf.moderator_id).await;
        let embed = case_embed(&inf, &target, &moderator);
        deliver_lookup(&ctx, settings.as_deref(), Payload::embed(embed).no_mentions()).await;
        return Ok(());
    }

    // ── reason / proof ──
    let (column, value, confirmation, log_text) = if sub == "reason" {
        let raw = ctx.get_string("reason").ok_or_else(|| anyhow::anyhow!("Required option \"reason\" not found."))?;
        let new_reason = clamp_text(bot_util::js::trim(&raw), 1000);
        if new_reason.is_empty() {
            return edit_embed(&ctx, error_embed("Invalid Input", Some("The reason can't be empty."), None)).await;
        }
        let confirmation = success_embed("Case Updated", Some(&format!("Case #{case_id} reason is now:\n> {}", quote(&clamp_text(&new_reason, 1000)))), None);
        let log_text = format!("Reason changed by <@{}>:\n> {}", ctx.user().id, quote(&new_reason));
        ("reason", new_reason, confirmation, log_text)
    } else {
        let raw = ctx.get_string("url").ok_or_else(|| anyhow::anyhow!("Required option \"url\" not found."))?;
        let url = bot_util::js::trim(&raw).to_string();
        if !bot_core::custom_embed::is_http_url(&url) || bot_util::js::utf16_len(&url) > 1000 {
            return edit_embed(&ctx, error_embed("Invalid Link", Some(msg::BAD_PROOF), None)).await;
        }
        let confirmation = success_embed("Case Updated", Some(&format!("Case #{case_id} proof link updated.")), None);
        let log_text = format!("Proof link changed by <@{}>:\n{url}", ctx.user().id);
        ("proofUrl", url, confirmation, log_text)
    };

    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let sql = format!(
        r#"UPDATE "Infraction" SET "{column}" = $1, "updatedAt" = {} WHERE "guildId" = $2 AND "caseId" = $3"#,
        bot_db::SQL_NOW
    );
    let changed = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&value)
        .bind(&g)
        .bind(case_id as i32)
        .execute(ctx.app.db())
        .await
        .map_err(|e| anyhow::Error::new(bot_db::DbError::from(e)))?
        .rows_affected();
    if changed == 0 {
        return edit_embed(&ctx, not_found(case_id)).await;
    }
    deliver_lookup(&ctx, settings.as_deref(), Payload::embed(confirmation).no_mentions()).await;
    // Best-effort note in the case-log channel.
    if let Some(gid) = ctx.guild_id() {
        let note = neutral_embed(Some(&format!("📝 Case #{case_id} edited")), Some(&clamp_text(&log_text, 4000)), None);
        post_mod_log(&ctx.app, gid, note).await;
    }
    Ok(())
}

// ── history ──

/// Live Discord state for a member who has no records at all (a hint; a failed lookup adds none).
async fn live_state_hints(app: &App, guild_id: &str, user_id: &str) -> Vec<String> {
    let (Some(g), Some(u)) = (snowflake(guild_id), snowflake(user_id)) else { return Vec::new() };
    let (gid, uid) = (serenity::all::GuildId::new(g), UserId::new(u));
    let r = rest(app);
    let ban = r.get_ban(gid, uid).await.ok().flatten();
    let member = match crate::guild_ops::load_guild(app, gid) {
        Some(lg) => lg.member_or_none(r.as_ref(), uid).await.ok().flatten(),
        None => None,
    };
    let mut lines = Vec::new();
    if let Some(b) = ban {
        lines.push("⚠️ **This user is currently banned**, but the ban is not in the records yet (it will be imported at the next ban sync).".to_string());
        if let Some(reason) = b.reason.filter(|r| !r.is_empty()) {
            lines.push(format!("Ban reason on record: {}", clamp_text(&reason.replace('`', "'"), 300)));
        }
    } else if let Some(m) = member.filter(|m| m.is_communication_disabled(app.clock().now_ms())) {
        lines.push("⚠️ **This member is currently timed out** (not by this bot).".to_string());
        if let Some(until) = m.communication_disabled_until {
            lines.push(format!("Timeout ends: {}", discord_timestamp(&until, "R")));
        }
    }
    lines
}

/// The `/case history` filter as SQL (`active`, a type, or none).
fn history_filter(kind: Option<&str>) -> (String, Option<String>) {
    match kind {
        Some("active") => (r#" AND "active" = true"#.to_string(), None),
        Some(t) => (r#" AND "type" = $3::"InfractionType""#.to_string(), Some(t.to_string())),
        None => (String::new(), None),
    }
}

/// The per-type summary line: `⚠️ 3× Warn · 🔨 1× Ban`, most frequent first, ties by type name.
pub fn history_summary(counts: &[(InfractionType, i64)], active_warns: i64) -> String {
    let mut sorted = counts.to_vec();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.as_str().cmp(b.0.as_str())));
    let line = sorted
        .iter()
        .map(|(t, n)| {
            let (emoji, label) = meta_of(t.as_str());
            format!("{emoji} {n}× {label}")
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let parts: Vec<String> = [line, format!("**Active warnings:** {active_warns}")].into_iter().filter(|s| !s.is_empty()).collect();
    clamp_text(&parts.join("\n"), 4096)
}

/// One history field: (name, value).
pub fn history_field(inf: &Infraction) -> (String, String) {
    let (emoji, label) = meta_of(display_type(inf));
    let reason = clamp_text(&one_line(inf.reason.as_deref().unwrap_or("No reason")), 200);
    let name = clamp_text(&format!("{emoji} Case #{} | {label}{}", inf.case_id, if inf.active { " 🔴 active" } else { "" }), 256);
    let ends = inf.expires_at.map(|at| format!(" · ends {}", discord_timestamp(&at, "R"))).unwrap_or_default();
    let value = clamp_text(&format!("{} by <@{}>{ends}\n> {reason}", discord_timestamp(&inf.created_at, "D"), inf.moderator_id), 1024);
    (name, value)
}

async fn view_history(ctx: &CommandCtx, g: &str, settings: Option<&GuildSettings>) -> BotResult {
    let user = required_user(ctx, "user")?;
    let kind = ctx.get_string("type");
    let uid = user.id.to_string();
    let db = |e: sqlx::Error| anyhow::Error::new(bot_db::DbError::from(e));
    let (filter, bind) = history_filter(kind.as_deref());

    let count_sql = format!(r#"SELECT COUNT(*) FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2{filter}"#);
    let mut q = sqlx::query_as::<_, (i64,)>(sqlx::AssertSqlSafe(count_sql)).bind(g).bind(&uid);
    if let Some(b) = &bind {
        q = q.bind(b.clone());
    }
    let total = q.fetch_one(ctx.app.db()).await.map_err(db)?.0;

    let rows_sql = format!(r#"SELECT * FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2{filter} ORDER BY "caseId" DESC LIMIT {MAX_ROWS}"#);
    let mut q = sqlx::query_as::<_, Infraction>(sqlx::AssertSqlSafe(rows_sql)).bind(g).bind(&uid);
    if let Some(b) = &bind {
        q = q.bind(b.clone());
    }
    let rows = q.fetch_all(ctx.app.db()).await.map_err(db)?;

    let counts: Vec<(InfractionType, i64)> =
        sqlx::query_as(r#"SELECT "type", COUNT(*) FROM "Infraction" WHERE "guildId" = $1 AND "targetUserId" = $2 GROUP BY "type""#)
            .bind(g)
            .bind(&uid)
            .fetch_all(ctx.app.db())
            .await
            .map_err(db)?;
    let active_warns = crate::moderation_service::count_active_warns(&ctx.app, g, &uid).await?;

    if rows.is_empty() {
        let filter_note = match &kind {
            Some(k) => format!(" (filter: {})", type_label(k).unwrap_or(k)),
            None => String::new(),
        };
        let mut lines = vec![format!("No moderation history for <@{uid}>{filter_note}.")];
        // "Not in the records yet" is only true when the member has no records at all.
        if counts.is_empty() {
            let hints = live_state_hints(&ctx.app, g, &uid).await;
            if !hints.is_empty() {
                lines.push(String::new());
                lines.extend(hints);
            }
        }
        deliver_lookup(ctx, settings, Payload::embed(info_embed("No Records", Some(&lines.join("\n")), None)).no_mentions()).await;
        return Ok(());
    }

    let summary = history_summary(&counts, active_warns);
    let more = if total > MAX_ROWS { format!(" · showing the {MAX_ROWS} most recent") } else { String::new() };
    let footer = format!("{total} record(s){more}");
    let pages = history_pages(settings, &user, &rows, &summary, &footer);

    let public = settings.map(|s| s.mod_visibility).unwrap_or(ModVisibility::Public) == ModVisibility::Public;
    let sent = if public {
        let _ = ctx.delete_reply().await;
        paginate(ctx, &pages, PaginateOptions { follow_up: true, ..Default::default() }).await
    } else {
        paginate(ctx, &pages, PaginateOptions { ephemeral: true, ..Default::default() }).await
    };
    if let Err(e) = sent {
        if is_interaction_gone(&e) {
            return Ok(());
        }
        bot_util::log_warn!(format!("/case history: could not send the pages: {e}"));
        let embed = error_embed("Action Failed", Some("I couldn't show the history here. Check my permissions in this channel and try again."), None);
        if public {
            let _ = ctx.follow_up(Payload::embed(embed).ephemeral(true).to_follow_up()).await;
        } else {
            let _ = ctx.edit_reply(Payload::embed(embed).to_edit()).await;
        }
    }
    Ok(())
}

/// The history pages: 5 cases per page, the summary and the footer on each.
pub fn history_pages(settings: Option<&GuildSettings>, user: &User, rows: &[Infraction], summary: &str, footer: &str) -> Vec<CreateEmbed> {
    let user_ref = UserRef::of(user);
    let title = clamp_text(&format!("📋 Infractions | {}", user_ref.label()), 256);
    rows.chunks(CASES_PER_PAGE)
        .map(|chunk| {
            let mut embed = guild_embed(settings, Some(&title), Some(summary), Some(footer));
            if let Some(a) = &user_ref.avatar_url {
                embed = embed.thumbnail(a);
            }
            for inf in chunk {
                let (name, value) = history_field(inf);
                embed = embed.field(name, value, false);
            }
            embed
        })
        .collect()
}

// ── clear-history ──

async fn clear_warnings(ctx: &CommandCtx, g: &str, settings: Option<&GuildSettings>) -> BotResult {
    let user = required_user(ctx, "user")?;
    let reason = ctx.get_string("reason");
    let uid = user.id.to_string();
    let n = crate::moderation_service::count_active_warns(&ctx.app, g, &uid).await?;
    if n == 0 {
        return edit_embed(ctx, info_embed("Nothing To Clear", Some(&format!("<@{uid}> has no active warnings.")), None)).await;
    }
    let confirmed = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Clear warnings?".into()),
            description: Some(format!(
                "This pardons **{n}** active warning(s) for <@{uid}>. Their cases stay in the history, and escalation counting starts over."
            )),
            confirm_label: "Clear warnings".into(),
            danger: true,
            ephemeral: true,
            ..Default::default()
        },
    )
    .await;
    if !confirmed {
        return Ok(()); // awaitConfirm already shows the outcome
    }
    let count = crate::moderation_service::resolve_infractions(&ctx.app, g, &uid, &[InfractionType::Warn]).await?;
    let why = reason.as_deref().map(bot_util::js::trim).filter(|r| !r.is_empty()).unwrap_or("No reason provided").to_string();
    let note_reason = clamp_text(&format!("[Pardon] Cleared {count} warning(s): {why}"), 1000);

    // The pardon itself is the update above; this only records it as a case (and posts the case log).
    let mut lines = vec![
        format!("**User:** <@{uid}> `{}`", tag_of(&user)),
        format!("**Warnings cleared:** {count}"),
        format!("**Reason:** {}", clamp_text(&why, 1000)),
    ];
    let mut warnings = Vec::new();
    let gid = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/case outside a guild"))?;
    let opts = CreateOpts { reason: Some(note_reason), silent: true, skip_escalation: true, ..Default::default() };
    match create_infraction(&ctx.app, gid, &UserRef::of(&user), &UserRef::of(ctx.user()), InfractionType::Note, opts).await {
        Ok(note) => lines.push(format!("**Case:** #{}", note.case_id)),
        Err(e) => {
            bot_util::log_error!(format!("/case clear-history: could not record the pardon note in {g}"), bot_core::error::log_arg(&e));
            warnings.push("The warnings were cleared, but the pardon could not be saved as a case.".to_string());
        }
    }
    deliver(ctx, settings, "Warnings Cleared", lines, warnings).await;
    Ok(())
}
