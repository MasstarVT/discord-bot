//! Opening a ticket (§6.7): the live-ticket check, `openTicket` (a numbered row reserved under an advisory lock, the
//! private channel, the intro) and `postIntro` / `postNotice`.

use bot_core::App;
use bot_core::adapters::{placeholder_channel, placeholder_guild, placeholder_member, placeholder_user, user_tag};
use bot_core::custom_embed::{RenderOptions, normalize_template, render_template};
use bot_core::interaction_helpers::{Payload, PermTarget, cached_guild, missing_bot_permissions, pingable_role_ids};
use bot_core::permissions::{me, permission_names};
use bot_db::sqlx::{self, AssertSqlSafe};
use bot_db::{DbError, GuildSettings, InsertBuilder, SQL_NOW, TICKET_INFO, Ticket, TicketPanelOption, TxOptions};
use bot_util::placeholder_parser::{RenderContext, VarValue};
use bot_util::text::clamp_text;
use serde_json::{Value, json};
use serenity::all::{GuildChannel, GuildId, Member, RoleId, User, UserId};
use serenity::builder::CreateAllowedMentions;

use super::components::build_intro_row;
use super::db::{PanelWithOptions, live_rows, mark_deleted, row_refs};
use super::io::{NewChannel, allow_open, channel_cached, confirm_gone_channels, io, sf};
use super::messages::live_managers;
use super::panels::{FULL_NOTE, default_color, resolve_open_parent};
use super::rules::{
    Fail, OPEN_REQUIRED_PERMS, Outcome, OverwriteInput, PENDING_PREFIX, RowRef, build_ticket_overwrites, classify_ticket_rows, default_intro_template,
    describe_discord_error, embed_length, format_ticket_number, is_category_full_error, render_channel_name, template_uses_token,
};

/// `listLiveTickets(guild, guildId, panelId, ownerId)`: the member's live tickets on a panel. Cache-based: a ticket
/// whose channel is not in the cache does not count here (the authoritative check is inside [`open_ticket`]).
pub async fn list_live_tickets(app: &App, guild_id: GuildId, panel_id: &str, owner_id: &str) -> anyhow::Result<Vec<RowRef>> {
    let rows = live_rows(app.db(), &guild_id.to_string(), panel_id, owner_id).await?;
    Ok(classify_ticket_rows(&rows, &|id| channel_cached(app, guild_id, id).is_some(), app.clock().now_ms()).live)
}

/// Who is opening a ticket (the parts of the interaction `openTicket` reads).
#[derive(Debug, Clone)]
pub struct Opener {
    /// `interaction.id`: the placeholder channel id is `pending:<id>`.
    pub interaction_id: String,
    pub user: User,
    pub member: Option<Member>,
}

/// A ticket that was opened.
#[derive(Debug, Clone)]
pub struct Opened {
    pub ticket: Ticket,
    pub channel: GuildChannel,
    pub notes: Vec<String>,
}

fn q(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

async fn delete_row(app: &App, id: &str) {
    if let Err(e) = sqlx::query(r#"DELETE FROM "Ticket" WHERE "id" = $1"#).bind(id).execute(app.db()).await {
        bot_util::log_warn!("tickets: could not remove a reserved ticket row", bot_core::error::log_arg(&anyhow::Error::from(e)));
    }
}

enum Reserved {
    Limit(Vec<RowRef>),
    Ticket(Box<Ticket>),
}

/// `openTicket({ interaction, client, guild, panel, option, reason, settings })`: reserves a numbered row under an
/// advisory lock, creates the private channel and posts the intro.
pub async fn open_ticket(
    app: &App,
    guild_id: GuildId,
    opener: &Opener,
    panel: &PanelWithOptions,
    option: Option<&TicketPanelOption>,
    reason: Option<String>,
    settings: Option<&GuildSettings>,
) -> Outcome<Opened> {
    let g = guild_id.to_string();
    let user_id = opener.user.id.to_string();
    if !allow_open(app, &format!("{g}:{user_id}")) {
        return Err(Fail::new("THROTTLED"));
    }
    let missing = missing_bot_permissions(app, PermTarget::Guild(guild_id), &OPEN_REQUIRED_PERMS);
    if !missing.is_empty() {
        return Err(Fail::with("BOT_PERMS", json!({ "names": permission_names(&missing).join(", "), "where": null })));
    }

    // Reserve the number and the row. The cap check and the insert share one per-guild advisory lock; no Discord call
    // happens inside the transaction.
    let reserved = match reserve(app, guild_id, opener, panel, option, reason.clone()).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!("tickets: reserving a ticket failed", bot_core::error::log_arg(&e));
            return Err(Fail::new("DB_ERROR"));
        }
    };
    let ticket = match reserved {
        Reserved::Limit(live) => {
            return Err(Fail::with("OPEN_LIMIT", json!({ "ids": live.iter().map(|r| r.channel_id.clone()).collect::<Vec<_>>(), "max": panel.panel.max_open_per_user })));
        }
        Reserved::Ticket(t) => *t,
    };

    let parent = resolve_open_parent(app, guild_id, Some(&panel.panel.id), option.map(|o| (o.id.as_str(), o.category_id.as_deref())), None).await;
    if let Some(m) = &parent.missing {
        delete_row(app, &ticket.id).await;
        let where_ = parent.category.as_ref().map(|c| c.name.clone());
        return Err(Fail::with("BOT_PERMS", json!({ "names": permission_names(m).join(", "), "where": where_ })));
    }
    let mut notes: Vec<String> = parent.note.iter().cloned().collect();

    let num = format_ticket_number(i64::from(ticket.number));
    let guild = cached_guild(app, guild_id);
    let tag = user_tag(&opener.user);
    let label = option.map(|o| o.label.clone());
    let managers = live_managers(app, guild_id, &panel.panel);
    let bot = super::io::bot_id(app).map(|b| b.to_string()).unwrap_or_default();
    let create = |parent_id: Option<serenity::all::ChannelId>| NewChannel {
        name: render_channel_name(
            &panel.panel.channel_name_format,
            i64::from(ticket.number),
            Some(placeholder_user(&opener.user)),
            opener.member.as_ref().map(|m| placeholder_member(m, guild.as_ref())),
            label.as_deref(),
        ),
        kind: 0,
        parent: parent_id,
        topic: Some(clamp_text(
            &format!("Ticket #{num} · opened by {tag} ({user_id}){}", label.as_ref().filter(|l| !l.is_empty()).map(|l| format!(" · {l}")).unwrap_or_default()),
            1024,
        )),
        overwrites: build_ticket_overwrites(&OverwriteInput {
            guild_id: &g,
            bot_id: &bot,
            owner_id: &user_id,
            manager_role_ids: &managers,
            added_user_ids: &[],
            closed: false,
        }),
        reason: clamp_text(&format!("Ticket #{num} opened by {tag}"), 512),
    };

    let io = io(app);
    let mut failure = None;
    let mut channel = match io.create_channel(guild_id, create(parent.category.as_ref().map(|c| c.id))).await {
        Ok(c) => Some(c),
        Err(e) => {
            failure = Some(e);
            None
        }
    };
    if channel.is_none() && failure.as_ref().is_some_and(is_category_full_error) && parent.category.is_some() {
        notes.push(FULL_NOTE.to_string());
        match io.create_channel(guild_id, create(None)).await {
            Ok(c) => {
                channel = Some(c);
                failure = None;
            }
            Err(e) => failure = Some(e),
        }
    }
    let Some(channel) = channel else {
        delete_row(app, &ticket.id).await;
        let f = failure.unwrap_or_default();
        bot_util::log_warn!(format!("tickets: channel create failed in guild {g}: {}", f.message));
        if f.is(30013) {
            return Err(Fail::new("GUILD_FULL"));
        }
        if f.is_perms() {
            return Err(Fail::with("BOT_PERMS", json!({ "names": permission_names(&["ManageChannels", "ManageRoles"]).join(", "), "where": null })));
        }
        return Err(Fail::with("CREATE_FAILED", json!({ "reason": describe_discord_error(&f) })));
    };

    let attached = sqlx::query(q(format!(r#"UPDATE "Ticket" SET "channelId" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2"#)))
        .bind(channel.id.to_string())
        .bind(&ticket.id)
        .execute(app.db())
        .await;
    if let Err(e) = attached {
        bot_util::log_error!("tickets: attaching the channel to its ticket failed", bot_core::error::log_arg(&anyhow::Error::from(e)));
        let _ = io.delete_channel(channel.id, "Ticket could not be saved".into()).await;
        delete_row(app, &ticket.id).await;
        return Err(Fail::new("DB_ERROR"));
    }

    let mut ticket = ticket;
    ticket.channel_id = channel.id.to_string();
    let intro_ok = post_intro(app, guild_id, &channel, &ticket, Some(panel), option, opener.member.as_ref(), Some(&opener.user), settings).await;
    if !intro_ok {
        notes.push("I couldn't post the welcome message in the ticket.".into());
    }
    Ok(Opened { ticket, channel, notes })
}

async fn reserve(
    app: &App,
    guild_id: GuildId,
    opener: &Opener,
    panel: &PanelWithOptions,
    option: Option<&TicketPanelOption>,
    reason: Option<String>,
) -> anyhow::Result<Reserved> {
    let g = guild_id.to_string();
    app.settings().ensure_guild_settings(&g).await?;
    let user_id = opener.user.id.to_string();
    let pre = live_rows(app.db(), &g, &panel.panel.id, &user_id).await?;
    let ids: Vec<String> = pre.iter().map(|r| r.channel_id.clone()).filter(|id| !id.starts_with(PENDING_PREFIX)).collect();
    // Rule 20: only a channel Discord confirmed is gone (10003) may be written off; everything else counts as live.
    let gone = confirm_gone_channels(app, guild_id, &ids).await;
    let now = app.clock().now_ms();
    let panel_id = panel.panel.id.clone();
    let max = panel.panel.max_open_per_user;
    let option_id = option.map(|o| o.id.clone());
    let pending = format!("{PENDING_PREFIX}{}", opener.interaction_id);
    let r = bot_db::transaction(app.db(), TxOptions::new(5_000, 10_000), move |tx| {
        Box::pin(async move {
            sqlx::query(super::db::NUMBER_LOCK_SQL).bind(format!("ticket-number:{g}")).execute(&mut *tx).await?;
            let rows = sqlx::query(
                r#"SELECT "id", "channelId", "createdAt" FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND "ownerId" = $3 AND "status" IN ('OPEN', 'CLAIMED')"#,
            )
            .bind(&g)
            .bind(&panel_id)
            .bind(&user_id)
            .fetch_all(&mut *tx)
            .await?;
            let rows = row_refs(&rows)?;
            let c = classify_ticket_rows(&rows, &|id| !gone.contains(id), now);
            if !c.stale_pending.is_empty() {
                sqlx::query(r#"DELETE FROM "Ticket" WHERE "id" = ANY($1)"#).bind(&c.stale_pending).execute(&mut *tx).await?;
            }
            if !c.stale_gone.is_empty() {
                sqlx::query(q(format!(r#"UPDATE "Ticket" SET "status" = 'DELETED', "updatedAt" = {SQL_NOW} WHERE "id" = ANY($1)"#)))
                    .bind(&c.stale_gone)
                    .execute(&mut *tx)
                    .await?;
            }
            if c.live.len() as i64 >= i64::from(max) {
                return Ok::<_, DbError>(Reserved::Limit(c.live));
            }
            let next: i32 = sqlx::query_scalar(super::db::NEXT_NUMBER_SQL).bind(&g).fetch_one(&mut *tx).await?;
            let ticket: Ticket = InsertBuilder::new(&TICKET_INFO)
                .values([
                    ("guildId", json!(g)),
                    ("panelId", json!(panel_id)),
                    ("optionId", json!(option_id)),
                    ("number", json!(next)),
                    ("channelId", json!(pending)),
                    ("ownerId", json!(user_id)),
                    ("reason", json!(reason)),
                ])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            Ok(Reserved::Ticket(Box::new(ticket)))
        })
    })
    .await?;
    Ok(r)
}

/// `postIntro({ channel, ticket, panel, option, member, settings })`: the intro message with Close / Claim. Pings the
/// opener, and the manager roles only when the template contains `{managers}` (and the bot may mention them in the
/// channel). Never fails; whether the message was sent.
#[allow(clippy::too_many_arguments)]
pub async fn post_intro(
    app: &App,
    guild_id: GuildId,
    channel: &GuildChannel,
    ticket: &Ticket,
    panel: Option<&PanelWithOptions>,
    option: Option<&TicketPanelOption>,
    member: Option<&Member>,
    user: Option<&User>,
    settings: Option<&GuildSettings>,
) -> bool {
    let tpl_src = option
        .and_then(|o| o.intro_template.clone())
        .filter(|v| !v.is_null())
        .or_else(|| panel.and_then(|p| p.panel.intro_template.clone()).filter(|v| !v.is_null()))
        .unwrap_or_else(default_intro_template);
    let tpl = normalize_template(&tpl_src);
    let guild = cached_guild(app, guild_id);
    let managers = panel.map(|p| live_managers(app, guild_id, &p.panel)).unwrap_or_default();
    let roles: Vec<String> = if template_uses_token(&tpl, "managers") && !managers.is_empty() {
        match (&guild, super::io::bot_id(app)) {
            (Some(gd), Some(bot)) => {
                let bot_roles: Vec<RoleId> = me(gd, bot).map(|m| m.roles.clone()).unwrap_or_default();
                pingable_role_ids(gd, channel, Some((bot, &bot_roles)), &managers).allowed
            }
            _ => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let vars = || {
        Some(vec![
            ("number".to_string(), VarValue::Text(format_ticket_number(i64::from(ticket.number)))),
            ("reason".to_string(), VarValue::Text(ticket.reason.clone().unwrap_or_default())),
            ("topic".to_string(), VarValue::Text(option.map(|o| o.label.clone()).unwrap_or_default())),
            ("panel".to_string(), VarValue::Text(panel.map(|p| p.panel.name.clone()).unwrap_or_default())),
            ("managers".to_string(), VarValue::Text(managers.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" "))),
        ])
    };
    let ctx = RenderContext {
        member: member.map(|m| placeholder_member(m, guild.as_ref())),
        user: member.map(|m| placeholder_user(&m.user)).or_else(|| user.map(placeholder_user)),
        guild: guild.as_ref().map(placeholder_guild),
        channel: Some(placeholder_channel(channel)),
        vars: vars(),
        args: None,
    };
    let owner: Vec<UserId> = sf(&ticket.owner_id).map(UserId::new).into_iter().collect();
    let role_ids: Vec<RoleId> = roles.iter().filter_map(|r| sf(r)).map(RoleId::new).collect();
    let opts = || RenderOptions {
        default_color: Some(default_color(settings)),
        allowed_mentions: Some(CreateAllowedMentions::new().users(owner.clone()).roles(role_ids.clone())),
        extra_components: vec![build_intro_row(&ticket.id, false)],
    };
    let Some(mut rendered) = render_template(&tpl, &ctx, opts()).or_else(|| render_template(&default_intro_template(), &ctx, opts())) else {
        bot_util::log_warn!("tickets: could not post the intro message: nothing to render");
        return false;
    };

    if let Some(reason) = ticket.reason.as_deref().filter(|r| !r.is_empty())
        && !template_uses_token(&tpl, "reason")
    {
        let mut added = false;
        if let Some(embed) = rendered.embeds.first_mut() {
            let fields = embed.get("fields").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
            if fields < 25 {
                let budget = 6000_i64 - embed_length(embed) as i64 - "Reason".len() as i64;
                if budget >= 20 {
                    let value = clamp_text(reason, budget.min(1024) as usize);
                    match embed.get_mut("fields").and_then(Value::as_array_mut) {
                        Some(f) => f.push(json!({ "name": "Reason", "value": value })),
                        None => embed["fields"] = json!([{ "name": "Reason", "value": value }]),
                    }
                    added = true;
                }
            }
        }
        if !added {
            let text = format!("{}\n\n**Reason:** {reason}", rendered.content.clone().unwrap_or_default());
            rendered.content = Some(clamp_text(bot_util::js::trim(&text), 2000));
        }
    }
    match io(app).send_message(channel.id, rendered.to_payload()).await {
        Ok(_) => true,
        Err(e) => {
            bot_util::log_warn!(format!("tickets: could not post the intro message: {}", e.message));
            false
        }
    }
}

/// `postNotice(channel, payload)`: a public notice in a ticket channel. Never fails; whether it was sent.
pub async fn post_notice(app: &App, channel: &GuildChannel, payload: Payload) -> bool {
    match io(app).send_message(channel.id, payload).await {
        Ok(_) => true,
        Err(e) => {
            bot_util::log_debug!(format!("tickets: notice failed in {}: {}", channel.id, e.message));
            false
        }
    }
}

/// Marks a ticket whose channel is confirmed gone as DELETED.
pub async fn write_off(app: &App, ticket_id: &str) {
    if let Err(e) = mark_deleted(app.db(), ticket_id).await {
        bot_util::log_warn!("tickets: could not mark a ticket deleted", bot_core::error::log_arg(&anyhow::Error::from(e)));
    }
}
