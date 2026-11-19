//! Lifecycle actions (§6.8): claim, close, reopen, add, remove, rename and delete, each under the ticket's keyed mutex,
//! plus the on-demand transcript. Every action re-reads the ticket, checks who may act, the state machine, the
//! channel and the bot's permissions, then writes the status conditionally (a concurrent change is `CONFLICT`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bot_core::App;
use bot_core::adapters::user_tag;
use bot_core::error::BoxFuture;
use bot_core::interaction_helpers::Payload;
use bot_core::permission_checker::{MemberAccess, resolve_permission_level};
use bot_core::permissions::permission_names;
use bot_core::settings::Settings;
use bot_config::PermissionLevel;
use bot_db::sqlx::{self, AssertSqlSafe};
use bot_db::{DbError, GuildSettings, SQL_NOW, TicketPanel};
use bot_util::text::clamp_text;
use serde_json::json;
use serenity::all::{GuildChannel, GuildId, Member, User, UserId};
use serenity::builder::CreateEmbedFooter;

use super::db::{TicketFull, get_ticket_by_id};
use super::io::{
    ChannelState, OverwriteEdit, allow_transcript, io, is_deleting, missing_in_channel, record_rename, rename_window, resolve_channel, set_deleting, sf, state,
    usable_channel,
};
use super::messages::{Notice, build_notice, delete_notice_text};
use super::open::{post_notice, write_off};
use super::panels::{full_category_note, move_channel, resolve_kind_parent, resolve_open_parent};
use super::rules::{
    Action, CategoryKind, Fail, MAX_ADDED_USERS, Outcome, Source, TRANSCRIPT_POST_PERMS, Transition, action_perms, can_perform, describe_discord_error,
    format_ticket_number, sanitize_channel_name, transition_for,
};
use super::transcript::{Person, build_transcript};

/// `loadSettings(guildId)`: the memo, else a safe load.
pub async fn load_settings(app: &App, guild_id: &str) -> Option<Settings> {
    match app.settings().peek_settings(guild_id) {
        Some(s) => Some(s),
        None => app.settings().get_settings_safe(guild_id).await,
    }
}

/// The member acting on a ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub id: String,
    /// `actor.user.tag`.
    pub tag: String,
    /// The permission view (`None`: not a member, e.g. the dashboard acting for nobody).
    pub access: Option<MemberAccess>,
}

impl Actor {
    /// The actor of an interaction member.
    pub fn of(app: &App, guild_id: GuildId, member: &Member) -> Self {
        let cache = app.discord().map(|d| d.cache.clone());
        Self { id: member.user.id.to_string(), tag: user_tag(&member.user), access: Some(MemberAccess::from_cache(cache.as_deref(), guild_id, member)) }
    }

    pub fn person(&self) -> Person {
        Person { id: self.id.clone(), tag: self.tag.clone() }
    }

    /// `resolvePermissionLevel(actor, settings)`.
    pub fn level(&self, app: &App, settings: Option<&GuildSettings>) -> PermissionLevel {
        resolve_permission_level(self.access.as_ref(), settings, app.config().bot_owner_id.as_deref())
    }
}

/// `isTicketStaff(member, panel, settings)`: moderators and up, or a member with one of the panel's manager roles
/// (`panel` `None` after the panel was deleted: moderators and up only).
pub fn is_ticket_staff(app: &App, actor: Option<&Actor>, panel: Option<&TicketPanel>, settings: Option<&GuildSettings>) -> bool {
    let Some(actor) = actor else { return false };
    if actor.level(app, settings) >= PermissionLevel::Moderator {
        return true;
    }
    let roles = actor.access.as_ref().map(|a| a.role_ids.clone()).unwrap_or_default();
    panel.is_some_and(|p| p.manager_role_ids.iter().any(|id| roles.contains(id)))
}

/// The context of a lifecycle action.
#[derive(Clone)]
pub struct ActionCtx {
    pub guild_id: GuildId,
    pub ticket_id: String,
    pub actor: Actor,
    pub settings: Option<Settings>,
    pub source: Source,
}

/// What the checks found (`prepareAction`).
pub struct Prepared {
    pub t: TicketFull,
    pub channel: GuildChannel,
    pub tr: Transition,
    pub is_owner: bool,
    pub is_staff: bool,
    pub is_admin: bool,
}

/// A finished action: the notes to show the actor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Done {
    pub notes: Vec<String>,
}

fn q(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

/// `reasonFor(action, actor, t)`: the audit-log reason.
pub fn reason_for(action: &str, actor: &Actor, number: i32) -> String {
    let who = if actor.tag.is_empty() { actor.id.clone() } else { actor.tag.clone() };
    clamp_text(&format!("Ticket #{}: {action} by {who}", format_ticket_number(i64::from(number))), 512)
}

/// `prepareAction(action, ctx)`: steps 1-4 of the skeleton (re-read, who, state, channel and the bot's permissions).
/// Nothing is changed except writing off a ticket whose channel Discord confirmed is gone.
pub async fn prepare_action(app: &App, action: Action, ctx: &ActionCtx) -> anyhow::Result<Outcome<Prepared>> {
    let g = ctx.guild_id.to_string();
    let Some(t) = get_ticket_by_id(app.db(), &g, &ctx.ticket_id).await? else { return Ok(Err(Fail::new("TICKET_DELETED"))) };
    if t.status() == "DELETED" {
        return Ok(Err(Fail::new("TICKET_DELETED")));
    }
    if is_deleting(app, &t.ticket.id) {
        return Ok(Err(Fail::new("DELETING")));
    }
    let settings = ctx.settings.as_deref();
    let is_owner = ctx.actor.id == t.ticket.owner_id;
    let is_staff = is_ticket_staff(app, Some(&ctx.actor), t.panel.as_ref(), settings);
    if let Err(f) = can_perform(action, is_owner, is_staff) {
        return Ok(Err(f));
    }
    let is_admin = ctx.actor.level(app, settings) >= PermissionLevel::Admin;
    let tr = match transition_for(action, t.status(), t.ticket.claimed_by_id.as_deref(), &ctx.actor.id, is_admin, ctx.source) {
        Ok(tr) => tr,
        Err(f) => return Ok(Err(f)),
    };
    let (found, st) = resolve_channel(app, ctx.guild_id, &t.ticket.channel_id).await;
    if st == ChannelState::Gone {
        write_off(app, &t.ticket.id).await;
        return Ok(Err(Fail::new("TICKET_DELETED")));
    }
    let Some(channel) = found else { return Ok(Err(Fail::new("UNKNOWN"))) };
    if let Some(perms) = action_perms(action) {
        let lacking = missing_in_channel(app, ctx.guild_id, &channel, perms);
        if !lacking.is_empty() {
            return Ok(Err(Fail::with("BOT_PERMS", json!({ "names": permission_names(&lacking).join(", "), "channelId": channel.id.to_string() }))));
        }
    }
    Ok(Ok(Prepared { t, channel, tr, is_owner, is_staff, is_admin }))
}

/// `runAction(ctx, action, body)`: runs an action inside the ticket's mutex; an unexpected error is logged and becomes
/// `UNKNOWN`.
pub async fn run_action<F, Fut>(app: &App, action: Action, ctx: ActionCtx, body: F) -> Outcome<Done>
where
    F: FnOnce(App, ActionCtx, Prepared) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<Outcome<Done>>> + Send + 'static,
{
    let a = app.clone();
    let key = ctx.ticket_id.clone();
    state(app)
        .ticket_mutex
        .run(&key, move || async move {
            let r = async {
                match prepare_action(&a, action, &ctx).await? {
                    Err(f) => Ok(Err(f)),
                    Ok(p) => body(a.clone(), ctx.clone(), p).await,
                }
            }
            .await;
            match r {
                Ok(v) => v,
                Err(e) => {
                    bot_util::log_error!(format!("tickets: {} failed", action.as_str()), bot_core::error::log_arg(&e));
                    Err(Fail::new("UNKNOWN"))
                }
            }
        })
        .await
}

/// `writeStatus(t, data, extraWhere)`: a conditional status write; `false` when someone changed it meanwhile.
async fn write_status(app: &App, t: &TicketFull, set: &str, binds: Vec<Option<String>>, closed_at: Option<chrono::NaiveDateTime>, extra_claimed: Option<Option<String>>) -> Result<bool, DbError> {
    let mut sql = format!(r#"UPDATE "Ticket" SET {set}, "updatedAt" = {SQL_NOW} WHERE "id" = $1 AND "status" = $2::"TicketStatus""#);
    let n = binds.len() + 2 + usize::from(closed_at.is_some());
    if let Some(c) = &extra_claimed {
        sql.push_str(&if c.is_some() { format!(r#" AND "claimedById" = ${}"#, n + 1) } else { r#" AND "claimedById" IS NULL"#.to_string() });
    }
    let mut query = sqlx::query(q(sql)).bind(&t.ticket.id).bind(t.status());
    for b in binds {
        query = query.bind(b);
    }
    if let Some(c) = closed_at {
        query = query.bind(c);
    }
    if let Some(Some(c)) = extra_claimed {
        query = query.bind(c);
    }
    Ok(query.execute(app.db()).await?.rows_affected() > 0)
}

/// `editMemberAccess(channel, t, perms, reason)`: the opener's and the added members' overwrites; one aggregated note
/// when any edit fails.
async fn edit_member_access(app: &App, guild_id: GuildId, channel: &GuildChannel, t: &TicketFull, send: bool, reason: &str) -> Option<String> {
    let mut failed = 0;
    let ids: Vec<String> = std::iter::once(t.ticket.owner_id.clone()).chain(t.ticket.added_user_ids.iter().cloned()).collect();
    for id in ids {
        let edit = OverwriteEdit {
            id,
            kind: 1,
            edits: vec![("ViewChannel", true), ("SendMessages", send), ("ReadMessageHistory", true)],
            reason: reason.to_string(),
        };
        if let Err(e) = io(app).edit_overwrite(guild_id, channel.id, edit).await {
            failed += 1;
            bot_util::log_debug!(format!("tickets: overwrite edit failed in {}: {}", channel.id, e.message));
        }
    }
    (failed > 0).then(|| format!("I couldn't update the permissions of {failed} member(s) in the ticket."))
}

/// `finish(channel, payload, notes)`.
async fn finish(app: &App, channel: &GuildChannel, payload: Payload, mut notes: Vec<String>) -> Outcome<Done> {
    if !post_notice(app, channel, payload).await {
        notes.push("I couldn't post the notice in the ticket.".into());
    }
    Ok(Done { notes })
}

/// `claimTicket(ctx)`: claim (or, for admins, take over) a ticket.
pub async fn claim_ticket(app: &App, ctx: ActionCtx) -> Outcome<Done> {
    run_action(app, Action::Claim, ctx, |app, ctx, p| async move {
        let previous = p.t.ticket.claimed_by_id.clone();
        let extra = if p.tr.takeover { Some(previous.clone()) } else { None };
        if !write_status(&app, &p.t, r#""status" = 'CLAIMED', "claimedById" = $3"#, vec![Some(ctx.actor.id.clone())], None, extra).await? {
            return Ok(Err(Fail::new("CONFLICT")));
        }
        let mut notes = Vec::new();
        let parent = resolve_kind_parent(&app, ctx.guild_id, p.t.ticket.panel_id.as_deref(), CategoryKind::Claimed).await;
        let (_, note) = move_channel(&app, &p.channel, &parent, &reason_for("claim", &ctx.actor, p.t.ticket.number)).await;
        notes.extend(note);
        let notice = Notice::Claim { actor_id: ctx.actor.id.clone(), previous_id: if p.tr.takeover { previous } else { None } };
        Ok(finish(&app, &p.channel, build_notice(&notice, ctx.settings.as_deref()), notes).await)
    })
    .await
}

/// `closeTicket(ctx)`: the opener and added members can read but not write. `reason` is shown in the notice only.
pub async fn close_ticket(app: &App, ctx: ActionCtx, reason: Option<String>) -> Outcome<Done> {
    run_action(app, Action::Close, ctx, move |app, ctx, p| async move {
        let now = app.clock().now().naive_utc();
        if !write_status(&app, &p.t, r#""status" = 'CLOSED', "closedById" = $3, "closedAt" = $4"#, vec![Some(ctx.actor.id.clone())], Some(now), None).await? {
            return Ok(Err(Fail::new("CONFLICT")));
        }
        let mut notes = Vec::new();
        let why = reason_for("close", &ctx.actor, p.t.ticket.number);
        notes.extend(edit_member_access(&app, ctx.guild_id, &p.channel, &p.t, false, &why).await);
        let parent = resolve_kind_parent(&app, ctx.guild_id, p.t.ticket.panel_id.as_deref(), CategoryKind::Closed).await;
        let (_, note) = move_channel(&app, &p.channel, &parent, &why).await;
        notes.extend(note);
        let notice = Notice::Close { actor_id: ctx.actor.id.clone(), ticket_id: p.t.ticket.id.clone(), reason };
        Ok(finish(&app, &p.channel, build_notice(&notice, ctx.settings.as_deref()), notes).await)
    })
    .await
}

/// `reopenTicket(ctx)`: back to CLAIMED when it had been claimed, else OPEN.
pub async fn reopen_ticket(app: &App, ctx: ActionCtx) -> Outcome<Done> {
    run_action(app, Action::Reopen, ctx, |app, ctx, p| async move {
        let to = p.tr.to.unwrap_or("OPEN");
        let set = format!(r#""status" = '{to}', "closedById" = NULL, "closedAt" = NULL"#);
        if !write_status(&app, &p.t, &set, vec![], None, None).await? {
            return Ok(Err(Fail::new("CONFLICT")));
        }
        let mut notes = Vec::new();
        let why = reason_for("reopen", &ctx.actor, p.t.ticket.number);
        notes.extend(edit_member_access(&app, ctx.guild_id, &p.channel, &p.t, true, &why).await);
        let parent = if to == "CLAIMED" {
            resolve_kind_parent(&app, ctx.guild_id, p.t.ticket.panel_id.as_deref(), CategoryKind::Claimed).await
        } else {
            let option = p.t.option.as_ref().map(|o| (o.id.as_str(), o.category_id.as_deref()));
            resolve_open_parent(&app, ctx.guild_id, p.t.panel.as_ref().map(|x| x.id.as_str()), option, Some(full_category_note)).await
        };
        let (_, note) = move_channel(&app, &p.channel, &parent, &why).await;
        notes.extend(note);
        let notice = Notice::Reopen { actor_id: ctx.actor.id.clone(), ticket_id: p.t.ticket.id.clone(), claimed: to == "CLAIMED" };
        Ok(finish(&app, &p.channel, build_notice(&notice, ctx.settings.as_deref()), notes).await)
    })
    .await
}

async fn set_added(app: &App, ticket_id: &str, ids: &[String]) -> Result<(), DbError> {
    sqlx::query(q(format!(r#"UPDATE "Ticket" SET "addedUserIds" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2"#)))
        .bind(ids)
        .bind(ticket_id)
        .execute(app.db())
        .await?;
    Ok(())
}

fn perms_fail(perm: &str, channel: &GuildChannel) -> Fail {
    Fail::with("BOT_PERMS", json!({ "names": permission_names(&[perm]).join(", "), "channelId": channel.id.to_string() }))
}

/// `addMember(ctx)`: give a member access. `member` is `None` when the user is not in the server.
pub async fn add_member(app: &App, ctx: ActionCtx, user: User, is_member: bool) -> Outcome<Done> {
    run_action(app, Action::Add, ctx, move |app, ctx, p| async move {
        let uid = user.id.to_string();
        let t = &p.t.ticket;
        if !is_member {
            return Ok(Err(Fail::new("NOT_MEMBER")));
        }
        if user.bot {
            return Ok(Err(Fail::new("ADD_BOT")));
        }
        if uid == t.owner_id {
            return Ok(Err(Fail::with("IS_OWNER", json!({ "op": "add" }))));
        }
        if t.added_user_ids.contains(&uid) {
            return Ok(Err(Fail::with("ALREADY_ADDED", json!({ "userId": uid }))));
        }
        if t.added_user_ids.len() >= MAX_ADDED_USERS {
            return Ok(Err(Fail::new("ADDED_LIMIT")));
        }
        let edit = OverwriteEdit {
            id: uid.clone(),
            kind: 1,
            edits: vec![("ViewChannel", true), ("SendMessages", t.status.as_str() != "CLOSED"), ("ReadMessageHistory", true), ("AttachFiles", true), ("EmbedLinks", true)],
            reason: reason_for("add", &ctx.actor, t.number),
        };
        if let Err(e) = io(&app).edit_overwrite(ctx.guild_id, p.channel.id, edit).await {
            bot_util::log_debug!(format!("tickets: add member failed in {}: {}", p.channel.id, e.message));
            return Ok(Err(if e.is_perms() { perms_fail("ManageRoles", &p.channel) } else { Fail::new("UNKNOWN") }));
        }
        let mut ids = t.added_user_ids.clone();
        ids.push(uid.clone());
        set_added(&app, &t.id, &ids).await?;
        let notice = Notice::Add { actor_id: ctx.actor.id.clone(), user_id: uid };
        Ok(finish(&app, &p.channel, build_notice(&notice, ctx.settings.as_deref()), Vec::new()).await)
    })
    .await
}

/// `removeMember(ctx)`: remove a member that was added with `add`.
pub async fn remove_member(app: &App, ctx: ActionCtx, user_id: UserId) -> Outcome<Done> {
    run_action(app, Action::Remove, ctx, move |app, ctx, p| async move {
        let uid = user_id.to_string();
        let t = &p.t.ticket;
        if uid == t.owner_id {
            return Ok(Err(Fail::with("IS_OWNER", json!({ "op": "remove" }))));
        }
        if !t.added_user_ids.contains(&uid) {
            return Ok(Err(Fail::with("NOT_ADDED", json!({ "userId": uid }))));
        }
        if let Err(e) = io(&app).delete_overwrite(p.channel.id, uid.clone(), reason_for("remove", &ctx.actor, t.number)).await
            && !e.is(10009)
        {
            bot_util::log_debug!(format!("tickets: remove member failed in {}: {}", p.channel.id, e.message));
            return Ok(Err(if e.is_perms() { perms_fail("ManageRoles", &p.channel) } else { Fail::new("UNKNOWN") }));
        }
        let ids: Vec<String> = t.added_user_ids.iter().filter(|id| **id != uid).cloned().collect();
        set_added(&app, &t.id, &ids).await?;
        let notice = Notice::Remove { actor_id: ctx.actor.id.clone(), user_id: uid };
        Ok(finish(&app, &p.channel, build_notice(&notice, ctx.settings.as_deref()), Vec::new()).await)
    })
    .await
}

/// `renameTicket(ctx)`: Discord allows 2 renames per 10 minutes, so a third is refused up front.
pub async fn rename_ticket(app: &App, ctx: ActionCtx, name: String) -> Outcome<Done> {
    run_action(app, Action::Rename, ctx, move |app, ctx, p| async move {
        let mut notes = Vec::new();
        let clean = sanitize_channel_name(&name);
        if clean.is_empty() {
            return Ok(Err(Fail::new("BAD_NAME")));
        }
        let now = app.clock().now_ms() as f64;
        let cid = p.channel.id.to_string();
        let (allowed, retry_at) = rename_window(&app, &cid, now);
        if !allowed {
            return Ok(Err(Fail::with("RENAME_LIMIT", json!({ "retryAt": retry_at }))));
        }
        let fut = io(&app).set_name(p.channel.id, clean, reason_for("rename", &ctx.actor, p.t.ticket.number));
        // `withTimeout` only stops waiting: the rename runs on (discord.js sends it after the rate-limit reset), so the
        // note's promise holds. Dropping the request future instead would cancel it.
        let wait = state(&app).rename_timeout_ms.load(Ordering::SeqCst);
        match bot_util::async_util::with_timeout(fut, wait, "ticket rename").await {
            Ok(()) => record_rename(&app, &cid, now),
            Err(bot_util::async_util::WithTimeoutError::Timeout(_)) => {
                record_rename(&app, &cid, now);
                notes.push("Discord is rate limiting renames; the new name will appear shortly.".to_string());
            }
            Err(bot_util::async_util::WithTimeoutError::Inner(e)) => {
                bot_util::log_debug!(format!("tickets: rename failed in {}: {}", p.channel.id, e.message));
                return Ok(Err(if e.is_perms() { perms_fail("ManageChannels", &p.channel) } else { Fail::new("UNKNOWN") }));
            }
        }
        let notice = Notice::Rename { actor_id: ctx.actor.id.clone(), channel_id: cid };
        Ok(finish(&app, &p.channel, build_notice(&notice, ctx.settings.as_deref()), notes).await)
    })
    .await
}

/// Called once the transcript is safe, before the pause (`onSaved({ text })`).
pub type OnSaved = Box<dyn FnOnce(String) -> BoxFuture<()> + Send>;

/// `deleteTicket(ctx)`: transcript first (channel and/or DM), notice, a 5 s pause, then the channel. The status becomes
/// DELETED only after the channel is gone. The caller must not use its interaction after `on_saved` (the channel
/// holding it is about to disappear).
pub async fn delete_ticket(app: &App, ctx: ActionCtx, on_saved: Option<OnSaved>) -> Outcome<()> {
    // Phase 1 inside the mutex: checks, then mark the ticket as being deleted and release the mutex.
    let (a, c) = (app.clone(), ctx.clone());
    let prep = state(app)
        .ticket_mutex
        .run(&ctx.ticket_id, move || async move {
            match prepare_action(&a, Action::Delete, &c).await {
                Ok(Ok(p)) => {
                    set_deleting(&a, &p.t.ticket.id, true);
                    Ok(p)
                }
                Ok(Err(f)) => Err(f),
                Err(e) => {
                    bot_util::log_error!("tickets: delete failed", bot_core::error::log_arg(&e));
                    Err(Fail::new("UNKNOWN"))
                }
            }
        })
        .await?;
    let id = prep.t.ticket.id.clone();
    let r = delete_phase_two(app, &ctx, prep, on_saved).await;
    set_deleting(app, &id, false);
    match r {
        Ok(v) => v,
        Err(e) => {
            bot_util::log_error!("tickets: deleting a ticket failed", bot_core::error::log_arg(&e));
            Err(Fail::new("UNKNOWN"))
        }
    }
}

async fn delete_phase_two(app: &App, ctx: &ActionCtx, p: Prepared, on_saved: Option<OnSaved>) -> anyhow::Result<Outcome<()>> {
    let Prepared { t, channel, .. } = p;
    let settings = ctx.settings.as_deref();
    let want_channel = t.panel.as_ref().and_then(|x| x.transcript_channel_id.clone());
    let want_dm = t.panel.as_ref().is_some_and(|x| x.dm_transcript);

    let mut tc: Option<GuildChannel> = None;
    if let Some(tid) = &want_channel {
        let (r, st) = resolve_channel(app, ctx.guild_id, tid).await;
        match r {
            Some(ch) => {
                let u = usable_channel(app, ctx.guild_id, &ch, &TRANSCRIPT_POST_PERMS);
                if !u.ok {
                    let reason = if u.missing.is_empty() {
                        u.reason.unwrap_or_else(|| "unusable".into()).to_lowercase()
                    } else {
                        format!("missing {}", permission_names(&u.missing).join(", "))
                    };
                    return Ok(Err(Fail::with("TRANSCRIPT_CHANNEL", json!({ "channelId": ch.id.to_string(), "reason": reason }))));
                }
                tc = Some(ch);
            }
            None if st != ChannelState::Gone => {
                return Ok(Err(Fail::with("TRANSCRIPT_CHANNEL", json!({ "channelId": tid, "reason": "I couldn't reach it right now" }))));
            }
            // Gone: the channelDelete hook clears the setting; the delete continues without a channel copy.
            None => {}
        }
    }

    let mut dm_note: Option<String> = None;
    if tc.is_some() || want_dm {
        let person = ctx.actor.person();
        let tr = match build_transcript(app, ctx.guild_id, &t, &channel, settings, Some(&person), Some(&person)).await {
            Ok(tr) => tr,
            Err(reason) => return Ok(Err(Fail::with("TRANSCRIPT_ABORT", json!({ "reason": reason })))),
        };
        if let Some(tch) = &tc {
            let payload = Payload::embed(tr.embed.clone()).file(tr.file.clone()).no_mentions();
            if let Err(e) = io(app).send_message(tch.id, payload).await {
                return Ok(Err(Fail::with("TRANSCRIPT_ABORT", json!({ "reason": describe_discord_error(&e) }))));
            }
        }
        if want_dm {
            let guild_name = bot_core::interaction_helpers::cached_guild(app, ctx.guild_id).map(|g| g.name).unwrap_or_default();
            let embed = tr.embed.clone().footer(CreateEmbedFooter::new(clamp_text(&format!("Sent from {guild_name}"), 2048)));
            let sent = match sf(&t.ticket.owner_id) {
                Some(owner) => io(app).send_dm(UserId::new(owner), Payload::embed(embed).file(tr.dm_file.clone())).await,
                None => Err(super::rules::IoError::code(10013, "Unknown User")),
            };
            if let Err(e) = sent {
                bot_util::log_debug!(format!("tickets: transcript DM failed: {}", e.message));
                dm_note = Some("The opener has DMs closed, so they did not get a copy.".into());
            }
        }
    }

    let tc_id = tc.as_ref().map(|c| c.id.to_string());
    let dm = want_dm && dm_note.is_none();
    let notice = Notice::Delete { actor_id: ctx.actor.id.clone(), transcript_channel_id: tc_id.clone(), dm };
    post_notice(app, &channel, build_notice(&notice, settings)).await;
    if let Some(cb) = on_saved {
        let text = format!("{}{}", delete_notice_text(&ctx.actor.id, tc_id.as_deref(), dm), dm_note.as_ref().map(|n| format!("\n⚠️ {n}")).unwrap_or_default());
        cb(text).await;
    }

    let delay = state(app).delete_delay_ms.load(Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    if let Err(e) = io(app).delete_channel(channel.id, reason_for("delete", &ctx.actor, t.ticket.number)).await
        && !e.is(10003)
    {
        bot_util::log_warn!(format!("tickets: deleting channel {} failed: {}", channel.id, e.message));
        return Ok(Err(Fail::with("DELETE_FAILED", json!({ "reason": describe_discord_error(&e), "transcriptSaved": tc.is_some() || want_dm }))));
    }
    let closed_by = t.ticket.closed_by_id.clone().unwrap_or_else(|| ctx.actor.id.clone());
    let closed_at = t.ticket.closed_at.unwrap_or_else(|| app.clock().now()).naive_utc();
    sqlx::query(q(format!(
        r#"UPDATE "Ticket" SET "status" = 'DELETED', "closedById" = $1, "closedAt" = $2, "updatedAt" = {SQL_NOW} WHERE "id" = $3 AND "status" <> 'DELETED'"#
    )))
    .bind(closed_by)
    .bind(closed_at)
    .bind(&t.ticket.id)
    .execute(app.db())
    .await
    .map_err(DbError::from)?;
    Ok(Ok(()))
}

/// The payload of an on-demand transcript (`{ embeds, files }`).
pub struct TranscriptReply {
    pub payload: Payload,
}

/// `transcriptForActor({ guild, ticket, channel, settings, actor })`: the staff check, the per-ticket throttle and the
/// transcript itself.
pub async fn transcript_for_actor(app: &App, guild_id: GuildId, t: &TicketFull, channel: &GuildChannel, settings: Option<&GuildSettings>, actor: &Actor) -> Outcome<TranscriptReply> {
    if t.status() == "DELETED" {
        return Err(Fail::new("TICKET_DELETED"));
    }
    can_perform(Action::Transcript, actor.id == t.ticket.owner_id, is_ticket_staff(app, Some(actor), t.panel.as_ref(), settings))?;
    if !allow_transcript(app, &t.ticket.id) {
        return Err(Fail::new("TRANSCRIPT_THROTTLED"));
    }
    match build_transcript(app, guild_id, t, channel, settings, Some(&actor.person()), None).await {
        Ok(tr) => Ok(TranscriptReply { payload: Payload::embed(tr.embed).file(tr.file) }),
        Err(reason) => Err(Fail::with("TRANSCRIPT_FAILED", json!({ "reason": reason }))),
    }
}

/// Shortens how long a rename is waited for before the rate-limit note (tests; the JS 15 s `withTimeout`).
pub fn set_rename_timeout_ms(app: &App, ms: u64) {
    state(app).rename_timeout_ms.store(ms, Ordering::SeqCst);
}

/// Shortens the delete pause (tests; the JS `sleep(DELETE_DELAY_MS)`).
pub fn set_delete_delay_ms(app: &App, ms: u64) {
    state(app).delete_delay_ms.store(ms, Ordering::SeqCst);
}

/// The settings as the action helpers take them.
pub fn settings_ref(s: &Option<Arc<GuildSettings>>) -> Option<&GuildSettings> {
    s.as_deref()
}
