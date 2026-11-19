//! Categories (§6.7.3) and panel publishing (§6.4): `ensureCategory`, `resolveOpenParent`, `moveChannel`,
//! `buildPanelPayload`, `publishPanel` and `refreshPanelMessage`. Each runs under the panel's keyed mutex where the JS
//! does, and never fails: problems come back as notes or result codes.

use bot_core::App;
use bot_core::adapters::{placeholder_channel, placeholder_guild};
use bot_core::custom_embed::{RenderOptions, normalize_template, render_template};
use bot_core::interaction_helpers::{Payload, PermTarget, cached_guild, missing_bot_permissions};
use bot_db::sqlx::{self, AssertSqlSafe};
use bot_db::{DbError, GuildSettings, SQL_NOW, TicketPanelStyle};
use bot_util::placeholder_parser::RenderContext;
use bot_util::text::clamp_text;
use serde_json::Value;
use serenity::all::{ChannelId, GuildChannel, GuildId};
use serenity::builder::CreateAllowedMentions;

use super::components::build_panel_components;
use super::db::{PanelWithOptions, clear_panel_message, panel_by_id};
use super::io::{
    ChannelState, NewChannel, SentMessage, bot_id, channel_cached, child_count, guild_channels, invalidate_panel, io, is_category, missing_in_channel, resolve_channel, sf,
    state, usable_channel,
};
use super::messages::{Refresh, live_managers};
use super::rules::{CATEGORY_CHILD_LIMIT, CATEGORY_PERMS, CategoryKind, IoError, OverwriteSpec, default_category_name, default_panel_template, describe_discord_error, is_category_full_error};

/// `FULL_NOTE`.
pub const FULL_NOTE: &str = "The ticket category is full (50 channels), so this ticket was created outside it.";

/// `fullCategoryNote(c)`.
pub fn full_category_note(name: &str) -> String {
    format!("**{name}** is full (50 channels), so I left the ticket where it is.")
}

/// The result of [`ensure_category`].
#[derive(Debug, Clone, Default)]
pub struct EnsuredCategory {
    pub category: Option<GuildChannel>,
    pub created: bool,
    pub reused: bool,
    pub note: Option<String>,
}

fn none(note: Option<String>) -> EnsuredCategory {
    EnsuredCategory { note, ..Default::default() }
}

fn q(s: String) -> AssertSqlSafe<String> {
    AssertSqlSafe(s)
}

/// `ensureCategory(guild, panelId, kind)`: finds, reuses or creates the category for a kind and stores it on the panel.
pub async fn ensure_category(app: &App, guild_id: GuildId, panel_id: &str, kind: CategoryKind) -> EnsuredCategory {
    let (a, p) = (app.clone(), panel_id.to_string());
    state(app)
        .panel_mutex
        .run(&format!("{panel_id}:cat"), move || async move {
            match ensure_category_inner(&a, guild_id, &p, kind).await {
                Ok(v) => v,
                Err(e) => {
                    bot_util::log_warn!("tickets: ensureCategory failed", bot_core::error::log_arg(&e));
                    none(Some("I couldn't set up the ticket category.".into()))
                }
            }
        })
        .await
}

async fn panel_field(app: &App, guild_id: &str, panel_id: &str, field: &str) -> Result<Option<Option<String>>, DbError> {
    let row: Option<(Option<String>,)> = sqlx::query_as(q(format!(r#"SELECT "{field}" FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)))
        .bind(panel_id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await?;
    Ok(row.map(|r| r.0))
}

async fn ensure_category_inner(app: &App, guild_id: GuildId, panel_id: &str, kind: CategoryKind) -> anyhow::Result<EnsuredCategory> {
    let field = kind.field();
    let g = guild_id.to_string();
    let Some(panel) = panel_by_id(app.db(), &g, panel_id).await? else { return Ok(none(None)) };
    let current = match kind {
        CategoryKind::Open => panel.panel.open_category_id.clone(),
        CategoryKind::Claimed => panel.panel.claimed_category_id.clone(),
        CategoryKind::Closed => panel.panel.closed_category_id.clone(),
    };
    if let Some(cur) = &current {
        let (ch, st) = resolve_channel(app, guild_id, cur).await;
        if let Some(c) = ch.filter(is_category) {
            return Ok(EnsuredCategory { category: Some(c), created: false, reused: false, note: None });
        }
        if st == ChannelState::Gone {
            sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "{field}" = NULL, "updatedAt" = {SQL_NOW} WHERE "id" = $1 AND "{field}" = $2"#)))
                .bind(panel_id)
                .bind(cur)
                .execute(app.db())
                .await
                .map_err(DbError::from)?;
            invalidate_panel(app, panel_id);
        } else if st == ChannelState::Unknown {
            return Ok(none(Some("I couldn't check the ticket category right now.".into())));
        }
    }

    let wanted = default_category_name(kind);
    let existing = guild_channels(app, guild_id).into_iter().find(|c| is_category(c) && c.name.to_lowercase() == wanted.to_lowercase());
    let mut created = false;
    let mut category = match existing {
        Some(c) => c,
        None => {
            if !missing_bot_permissions(app, PermTarget::Guild(guild_id), &["ManageChannels", "ManageRoles", "ViewChannel"]).is_empty() {
                return Ok(none(Some(format!("I couldn't create the \"{wanted}\" category (I'm missing permissions)."))));
            }
            let mut overwrites = vec![
                OverwriteSpec { id: g.clone(), kind: 0, allow: None, deny: Some(vec!["ViewChannel"]) },
                OverwriteSpec { id: bot_id(app).map(|b| b.to_string()).unwrap_or_default(), kind: 1, allow: Some(vec!["ViewChannel", "ManageChannels"]), deny: None },
            ];
            overwrites.extend(
                live_managers(app, guild_id, &panel.panel).into_iter().map(|id| OverwriteSpec { id, kind: 0, allow: Some(vec!["ViewChannel"]), deny: None }),
            );
            let spec = NewChannel { name: wanted.to_string(), kind: 4, parent: None, topic: None, overwrites, reason: "Ticket category created on demand".into() };
            match io(app).create_channel(guild_id, spec).await {
                Ok(c) => {
                    created = true;
                    c
                }
                Err(e) => {
                    return Ok(none(Some(if e.is(30013) {
                        format!("This server has 500 channels, so I couldn't create the \"{wanted}\" category.")
                    } else {
                        format!("I couldn't create the \"{wanted}\" category ({}).", describe_discord_error(&e))
                    })));
                }
            }
        }
    };

    let stored = sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "{field}" = $1, "updatedAt" = {SQL_NOW} WHERE "id" = $2 AND "{field}" IS NULL"#)))
        .bind(category.id.to_string())
        .bind(panel_id)
        .execute(app.db())
        .await
        .map_err(DbError::from)?
        .rows_affected();
    if stored == 0
        && let Some(Some(winner_id)) = panel_field(app, &g, panel_id, field).await?
    {
        let (w, _) = resolve_channel(app, guild_id, &winner_id).await;
        if let Some(w) = w.filter(is_category) {
            bot_util::log_debug!(format!("tickets: another writer stored the {} category of panel {panel_id}", kind.as_str()));
            category = w;
            created = false;
        }
    }
    invalidate_panel(app, panel_id);
    Ok(EnsuredCategory { category: Some(category), created, reused: !created, note: None })
}

/// Where a ticket goes: the category (when usable), a note, and the missing permissions when the bot cannot use it.
#[derive(Debug, Clone, Default)]
pub struct Parent {
    pub category: Option<GuildChannel>,
    pub note: Option<String>,
    pub missing: Option<Vec<String>>,
}

fn finish_parent(app: &App, guild_id: GuildId, category: Option<GuildChannel>, note: Option<String>, full_note: &dyn Fn(&str) -> String) -> Parent {
    let Some(c) = category else { return Parent { category: None, note, missing: None } };
    let missing = missing_in_channel(app, guild_id, &c, &CATEGORY_PERMS);
    if !missing.is_empty() {
        return Parent { category: Some(c), note, missing: Some(missing) };
    }
    if child_count(app, guild_id, c.id) >= CATEGORY_CHILD_LIMIT {
        return Parent { category: None, note: Some(full_note(&c.name)), missing: None };
    }
    Parent { category: Some(c), note, missing: None }
}

/// `resolveOpenParent(guild, panel, option, { fullNote })`: the option's category, else the panel's open category
/// (created on demand). `full_note` is the note when the category already holds 50 channels (`FULL_NOTE` by default).
pub async fn resolve_open_parent(
    app: &App,
    guild_id: GuildId,
    panel_id: Option<&str>,
    option: Option<(&str, Option<&str>)>,
    full_note: Option<fn(&str) -> String>,
) -> Parent {
    let mut category = None;
    let mut note = None;
    if let Some((option_id, Some(cat))) = option {
        let (ch, st) = resolve_channel(app, guild_id, cat).await;
        if let Some(c) = ch.filter(is_category) {
            category = Some(c);
        } else if st == ChannelState::Gone {
            if let Err(e) = sqlx::query(r#"UPDATE "TicketPanelOption" SET "categoryId" = NULL WHERE "id" = $1"#).bind(option_id).execute(app.db()).await {
                bot_util::log_warn!("tickets: clearing a deleted option category failed", bot_core::error::log_arg(&anyhow::Error::from(e)));
            }
            if let Some(p) = panel_id {
                invalidate_panel(app, p);
            }
        }
    }
    if category.is_none()
        && let Some(p) = panel_id
    {
        let e = ensure_category(app, guild_id, p, CategoryKind::Open).await;
        category = e.category;
        note = e.note;
    }
    let default_note: fn(&str) -> String = |_| FULL_NOTE.to_string();
    finish_parent(app, guild_id, category, note, &full_note.unwrap_or(default_note))
}

/// `resolveKindParent(guild, panelId, kind)`.
pub async fn resolve_kind_parent(app: &App, guild_id: GuildId, panel_id: Option<&str>, kind: CategoryKind) -> Parent {
    let Some(p) = panel_id else { return Parent::default() };
    let e = ensure_category(app, guild_id, p, kind).await;
    finish_parent(app, guild_id, e.category, e.note, &full_category_note)
}

/// `moveChannel(channel, parent, reason)`: `(moved, note)`. The parent is set alone (`lockPermissions: false`), so the
/// ticket keeps its private overwrites.
pub async fn move_channel(app: &App, channel: &GuildChannel, parent: &Parent, reason: &str) -> (bool, Option<String>) {
    let Some(cat) = &parent.category else { return (false, parent.note.clone()) };
    let name = &cat.name;
    if parent.missing.is_some() {
        return (false, Some(format!("I can't manage channels in the **{name}** category, so I didn't move the ticket.")));
    }
    if channel.parent_id == Some(cat.id) {
        return (false, None);
    }
    match io(app).set_parent(channel.id, cat.id, clamp_text(reason, 512)).await {
        Ok(()) => (true, None),
        Err(e) if is_category_full_error(&e) => (false, Some(format!("**{name}** is full (50 channels), so I didn't move the ticket."))),
        Err(e) => (false, Some(format!("I couldn't move the ticket to **{name}** ({}).", describe_discord_error(&e)))),
    }
}

// ── Panel rendering and publishing (§6.4) ─────────────────────────────────────

/// `client.emojis.cache.has(id)`: an emoji of any server the bot is in.
pub fn bot_has_emoji(app: &App, id: &str) -> bool {
    let Some(e) = sf(id) else { return false };
    let Some(d) = app.discord() else { return false };
    d.cache.guilds().into_iter().any(|g| d.cache.guild(g).is_some_and(|guild| guild.emojis.contains_key(&e.into())))
}

/// `canUseEmoji(str)` of the panel renderer: a custom emoji must be visible to the bot.
pub fn can_use_emoji(app: &App, s: &str) -> bool {
    let re = bot_util::jsre::JsRegex::new(r":(\d{17,20})>$", "");
    match re.exec(s) {
        Some((t, m)) => bot_has_emoji(app, &m.group(&t, 1).unwrap_or_default()),
        None => true,
    }
}

/// The placeholders of a panel message: the guild and the channel.
pub fn panel_render_context(app: &App, guild_id: GuildId, channel: Option<&GuildChannel>) -> RenderContext {
    RenderContext { guild: cached_guild(app, guild_id).as_ref().map(placeholder_guild), channel: channel.map(placeholder_channel), ..Default::default() }
}

/// `settings?.embedColor ?? COLORS.INFO`.
pub fn default_color(settings: Option<&GuildSettings>) -> u32 {
    settings.and_then(|s| s.embed_color).map(|c| c as u32).unwrap_or(bot_config::colors::INFO)
}

/// `buildPanelPayload(panel, { guild, channel, settings, client })`: the panel message (template without link buttons,
/// plus the buttons or dropdown).
pub fn build_panel_payload(app: &App, guild_id: GuildId, panel: &PanelWithOptions, channel: Option<&GuildChannel>, settings: Option<&GuildSettings>) -> Payload {
    let opts = || RenderOptions {
        default_color: Some(default_color(settings)),
        allowed_mentions: Some(CreateAllowedMentions::new()),
        extra_components: build_panel_components(&panel.panel.id, panel.panel.style, &panel.options, &|e| can_use_emoji(app, e)),
    };
    let ctx = panel_render_context(app, guild_id, channel);
    let select = panel.panel.style == TicketPanelStyle::Select;
    let mut base = match &panel.panel.template {
        Some(t) if !t.is_null() => normalize_template(t),
        _ => default_panel_template(select),
    };
    base["buttons"] = Value::Array(Vec::new());
    render_template(&base, &ctx, opts())
        .or_else(|| render_template(&default_panel_template(select), &ctx, opts()))
        .map(|r| r.to_payload())
        .unwrap_or_default()
}

/// A `message.edit({ content: '', ...payload })` payload.
pub fn edit_payload(p: Payload) -> Payload {
    if p.content.is_none() { p.content("") } else { p }
}

/// How `publishPanel` posted the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Published {
    Posted(SentMessage),
    Edited(SentMessage),
}

impl Published {
    pub fn message(&self) -> &SentMessage {
        match self {
            Published::Posted(m) | Published::Edited(m) => m,
        }
    }

    pub fn edited(&self) -> bool {
        matches!(self, Published::Edited(_))
    }
}

/// Why `publishPanel` failed: the panel is gone (`PANEL_GONE`), or the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    PanelGone,
    Failed(IoError),
}

impl PublishError {
    /// The reason the commands show (`'the panel was deleted'` or `describeDiscordError`).
    pub fn reason(&self) -> String {
        match self {
            PublishError::PanelGone => "the panel was deleted".into(),
            PublishError::Failed(e) => describe_discord_error(e),
        }
    }
}

fn db_io(e: impl std::fmt::Display) -> IoError {
    IoError { message: e.to_string(), ..Default::default() }
}

/// `publishPanel({ guild, panelId, channel, settings })`: posts the panel in `channel`, or edits the published one when
/// it lives there. Never fails.
pub async fn publish_panel(app: &App, guild_id: GuildId, panel_id: &str, channel: &GuildChannel, settings: Option<GuildSettings>) -> Result<Published, PublishError> {
    let (a, p, ch) = (app.clone(), panel_id.to_string(), channel.clone());
    state(app).panel_mutex.run(panel_id, move || async move { publish_inner(&a, guild_id, &p, &ch, settings.as_ref()).await }).await
}

async fn publish_inner(app: &App, guild_id: GuildId, panel_id: &str, channel: &GuildChannel, settings: Option<&GuildSettings>) -> Result<Published, PublishError> {
    let g = guild_id.to_string();
    let panel = panel_by_id(app.db(), &g, panel_id).await.map_err(|e| PublishError::Failed(db_io(e)))?.ok_or(PublishError::PanelGone)?;
    let payload = build_panel_payload(app, guild_id, &panel, Some(channel), settings);
    let channel_str = channel.id.to_string();
    let io = io(app);
    match (panel.panel.message_id.as_deref(), panel.panel.channel_id.as_deref()) {
        (Some(m), Some(c)) if c == channel_str => {
            if let Some(mid) = sf(m) {
                match io.edit_message(channel.id, mid.into(), edit_payload(payload.clone())).await {
                    Ok(msg) => return Ok(Published::Edited(msg)),
                    Err(e) if !e.is(10008) => return Err(PublishError::Failed(e)),
                    Err(_) => {}
                }
            }
        }
        (Some(m), Some(c)) => {
            if channel_cached(app, guild_id, c).is_some()
                && let (Some(cid), Some(mid)) = (sf(c), sf(m))
            {
                let _ = io.delete_message(ChannelId::new(cid), mid.into()).await;
            }
        }
        _ => {}
    }
    let msg = io.send_message(channel.id, payload).await.map_err(PublishError::Failed)?;
    let saved = sqlx::query(q(format!(r#"UPDATE "TicketPanel" SET "channelId" = $1, "messageId" = $2, "updatedAt" = {SQL_NOW} WHERE "id" = $3 AND "guildId" = $4"#)))
        .bind(&channel_str)
        .bind(msg.id.to_string())
        .bind(panel_id)
        .bind(&g)
        .execute(app.db())
        .await;
    if let Err(e) = saved {
        let _ = io.delete_message(channel.id, msg.id).await;
        return Err(PublishError::Failed(db_io(e)));
    }
    invalidate_panel(app, panel_id);
    Ok(Published::Posted(msg))
}

/// `refreshPanelMessage(guild, panelId)`: re-renders the published panel message after a change. Never fails; the
/// caller turns a failure into a note (`refresh_note`).
pub async fn refresh_panel_message(app: &App, guild_id: GuildId, panel_id: &str) -> Refresh {
    let (a, p) = (app.clone(), panel_id.to_string());
    state(app)
        .panel_mutex
        .run(panel_id, move || async move {
            match refresh_inner(&a, guild_id, &p).await {
                Ok(r) => r,
                Err(e) => {
                    bot_util::log_warn!("tickets: refreshPanelMessage failed", bot_core::error::log_arg(&e));
                    Refresh::Error { reason: describe_discord_error(&db_io(&e)) }
                }
            }
        })
        .await
}

async fn refresh_inner(app: &App, guild_id: GuildId, panel_id: &str) -> anyhow::Result<Refresh> {
    let g = guild_id.to_string();
    let Some(panel) = panel_by_id(app.db(), &g, panel_id).await? else { return Ok(Refresh::Ok { skipped: true }) };
    let (Some(channel_id), Some(message_id)) = (panel.panel.channel_id.clone(), panel.panel.message_id.clone()) else {
        return Ok(Refresh::Ok { skipped: true });
    };
    let (ch, st) = resolve_channel(app, guild_id, &channel_id).await;
    if st == ChannelState::Gone {
        clear_panel_message(app, panel_id, &g).await?;
        return Ok(Refresh::Gone);
    }
    let Some(ch) = ch else { return Ok(Refresh::Error { reason: "I couldn't reach the channel".into() }) };
    if !usable_channel(app, guild_id, &ch, &["ViewChannel", "SendMessages", "EmbedLinks"]).ok {
        return Ok(Refresh::NoAccess { channel_id });
    }
    let settings = super::lifecycle::load_settings(app, &g).await;
    let payload = edit_payload(build_panel_payload(app, guild_id, &panel, Some(&ch), settings.as_deref()));
    let Some(mid) = sf(&message_id) else { return Ok(Refresh::Gone) };
    match io(app).edit_message(ch.id, mid.into(), payload).await {
        Ok(_) => Ok(Refresh::Ok { skipped: false }),
        Err(e) if e.is(10008) || e.is(10003) => {
            clear_panel_message(app, panel_id, &g).await?;
            Ok(Refresh::Gone)
        }
        Err(e) if e.is_perms() => Ok(Refresh::NoAccess { channel_id }),
        Err(e) => Ok(Refresh::Error { reason: describe_discord_error(&e) }),
    }
}
