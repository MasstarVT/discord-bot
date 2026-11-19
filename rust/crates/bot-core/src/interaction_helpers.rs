//! Port of `src/utils/interactionHelpers.js`: defer and reply without failing, permission and role checks,
//! autocomplete replies and the confirm dialog. Nothing here fails; problems are logged and reported through the
//! return value.
//!
//! The pure parts (permission names, `missing_bot_permissions`, the channel and role verdicts, the autocomplete
//! choice list, the confirm payload) take plain values and are tested offline; the rest reads the serenity cache
//! through [`App::discord`] and answers through the [`Responder`].

use std::time::Duration;

use serde_json::Value;
use serenity::all::{
    ButtonStyle, ChannelId, Guild, GuildChannel, GuildId, InteractionId, Member, Message, Permissions, Role, RoleId, User, UserId,
};
use serenity::builder::{
    AutocompleteChoice, CreateActionRow, CreateAllowedMentions, CreateAttachment, CreateButton, CreateEmbed, CreateEmbedFooter,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditInteractionResponse, EditMessage,
};

use crate::collectors::CollectorFilter;
use crate::context::App;
use crate::embed_builder::{error_embed, info_embed, success_embed, warn_embed};
use crate::interaction::{AutocompleteCtx, CommandCtx, ComponentCtx, InteractionError, ModalCtx, Responder};
use crate::permissions::{self, js_has, permission_names};

// ── The interaction a helper answers ────────────────────────────────────────────

/// What the helpers need from any repliable interaction (command, component, modal): the app, the responder and
/// who, where and when.
pub trait Interactive: std::ops::Deref<Target = Responder> + Send + Sync {
    fn app(&self) -> &App;
    fn responder(&self) -> &Responder;
    fn interaction_id(&self) -> InteractionId;
    fn user(&self) -> &User;
    fn guild_id(&self) -> Option<GuildId>;
    fn channel_id(&self) -> ChannelId;
    /// The invoking member (in a guild).
    fn member(&self) -> Option<&Member>;
    /// `interaction.memberPermissions`: the member's permissions in this channel, as Discord sent them.
    fn member_permissions(&self) -> Option<Permissions> {
        self.member().and_then(|m| m.permissions)
    }
}

macro_rules! interactive {
    ($ty:ty, $member:expr) => {
        impl Interactive for $ty {
            fn app(&self) -> &App {
                &self.app
            }
            fn responder(&self) -> &Responder {
                &self.responder
            }
            fn interaction_id(&self) -> InteractionId {
                self.interaction.id
            }
            fn user(&self) -> &User {
                &self.interaction.user
            }
            fn guild_id(&self) -> Option<GuildId> {
                self.interaction.guild_id
            }
            fn channel_id(&self) -> ChannelId {
                self.interaction.channel_id
            }
            fn member(&self) -> Option<&Member> {
                let f: fn(&Self) -> Option<&Member> = $member;
                f(self)
            }
        }
    };
}

interactive!(CommandCtx, |c| c.interaction.member.as_deref());
interactive!(AutocompleteCtx, |c| c.interaction.member.as_deref());
interactive!(ComponentCtx, |c| c.interaction.member.as_ref());
interactive!(ModalCtx, |c| c.interaction.member.as_ref());

// ── Reply helpers ──────────────────────────────────────────────────────────────

/// Discord codes that mean the interaction can no longer be answered (`GONE_CODES`): unknown interaction (10062),
/// already acknowledged (40060), unknown webhook (10015), invalid webhook token (50027).
pub const INTERACTION_GONE_CODES: [u64; 4] = [10062, 40060, 10015, 50027];

/// `isInteractionGone(err)`: expired or already acknowledged, which is normal and not worth a warning.
pub fn is_interaction_gone(err: &InteractionError) -> bool {
    matches!(err, InteractionError::AlreadyReplied) || err.code().is_some_and(|c| INTERACTION_GONE_CODES.contains(&c))
}

/// `err?.code` as the JS logs it.
fn code_text(err: &InteractionError) -> String {
    match err {
        InteractionError::AlreadyReplied => "InteractionAlreadyReplied".into(),
        InteractionError::NotReplied => "InteractionNotReplied".into(),
        e => e.code().map(|c| c.to_string()).unwrap_or_else(|| "undefined".into()),
    }
}

fn err_arg(err: &InteractionError) -> bot_util::logger::LogArg {
    bot_util::logger::LogArg::Error { message: err.to_string(), stack: None }
}

/// `safeDefer(interaction, { ephemeral })`: defers unless already acknowledged. `true` when the interaction is
/// acknowledged.
pub async fn safe_defer(r: &Responder, ephemeral: bool) -> bool {
    if r.is_acknowledged() {
        return true;
    }
    match r.defer_reply(ephemeral).await {
        Ok(()) => true,
        Err(e) => {
            if is_interaction_gone(&e) {
                bot_util::log_debug!(format!("safeDefer: interaction gone ({})", code_text(&e)));
            } else {
                bot_util::log_warn!("safeDefer failed", err_arg(&e));
            }
            false
        }
    }
}

/// A message payload (`{ content, embeds, components, files, ephemeral, allowedMentions }`) that can be sent as a
/// reply, an edit of the reply or a follow-up. Unset fields are left out, so an edit keeps them.
#[derive(Clone, Debug, Default)]
pub struct Payload {
    pub content: Option<String>,
    pub embeds: Option<Vec<CreateEmbed>>,
    pub components: Option<Vec<CreateActionRow>>,
    pub files: Vec<CreateAttachment>,
    pub ephemeral: bool,
    pub allowed_mentions: Option<CreateAllowedMentions>,
}

impl Payload {
    pub fn new() -> Self {
        Self::default()
    }

    /// `{ content }` (a string payload).
    pub fn text(content: impl Into<String>) -> Self {
        Self::new().content(content)
    }

    /// `{ embeds: [embed] }`.
    pub fn embed(embed: CreateEmbed) -> Self {
        Self::new().embeds(vec![embed])
    }

    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    pub fn embeds(mut self, embeds: Vec<CreateEmbed>) -> Self {
        self.embeds = Some(embeds);
        self
    }

    pub fn components(mut self, rows: Vec<CreateActionRow>) -> Self {
        self.components = Some(rows);
        self
    }

    pub fn file(mut self, file: CreateAttachment) -> Self {
        self.files.push(file);
        self
    }

    pub fn ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    pub fn allowed_mentions(mut self, mentions: CreateAllowedMentions) -> Self {
        self.allowed_mentions = Some(mentions);
        self
    }

    /// `allowedMentions: { parse: [] }`: pings nobody.
    pub fn no_mentions(self) -> Self {
        self.allowed_mentions(CreateAllowedMentions::new())
    }

    /// As `interaction.reply(payload)`.
    pub fn to_reply(&self) -> CreateInteractionResponseMessage {
        let mut m = CreateInteractionResponseMessage::new().ephemeral(self.ephemeral);
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(e) = &self.embeds {
            m = m.embeds(e.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        if !self.files.is_empty() {
            m = m.add_files(self.files.clone());
        }
        m
    }

    /// As `interaction.update(payload)` (the clicked message).
    pub fn to_update(&self) -> CreateInteractionResponseMessage {
        let mut m = CreateInteractionResponseMessage::new();
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(e) = &self.embeds {
            m = m.embeds(e.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        if !self.files.is_empty() {
            m = m.add_files(self.files.clone());
        }
        m
    }

    /// As `interaction.editReply(withoutEphemeral(payload))`.
    pub fn to_edit(&self) -> EditInteractionResponse {
        let mut m = EditInteractionResponse::new();
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(e) = &self.embeds {
            m = m.embeds(e.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        for f in &self.files {
            m = m.new_attachment(f.clone());
        }
        m
    }

    /// As `interaction.editReply({ ...payload, attachments: [] })`: like [`Payload::to_edit`], and when the payload
    /// brings no files of its own, the message's existing attachments are removed (e.g. a stale captcha image).
    pub fn to_edit_clearing_attachments(&self) -> EditInteractionResponse {
        let m = self.to_edit();
        if self.files.is_empty() { m.clear_attachments() } else { m }
    }

    /// As `interaction.followUp(payload)`.
    pub fn to_follow_up(&self) -> CreateInteractionResponseFollowup {
        let mut m = CreateInteractionResponseFollowup::new().ephemeral(self.ephemeral);
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(e) = &self.embeds {
            m = m.embeds(e.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        if !self.files.is_empty() {
            m = m.add_files(self.files.clone());
        }
        m
    }

    /// As `message.edit(payload)` / `channel.send(payload)` through the channel route.
    pub fn to_message_edit(&self) -> EditMessage {
        let mut m = EditMessage::new();
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(e) = &self.embeds {
            m = m.embeds(e.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        m
    }

    /// As `channel.send(payload)`.
    pub fn to_message(&self) -> serenity::builder::CreateMessage {
        let mut m = serenity::builder::CreateMessage::new();
        if let Some(c) = &self.content {
            m = m.content(c.clone());
        }
        if let Some(e) = &self.embeds {
            m = m.embeds(e.clone());
        }
        if let Some(c) = &self.components {
            m = m.components(c.clone());
        }
        if let Some(a) = &self.allowed_mentions {
            m = m.allowed_mentions(a.clone());
        }
        if !self.files.is_empty() {
            m = m.add_files(self.files.clone());
        }
        m
    }
}

impl From<&str> for Payload {
    fn from(s: &str) -> Self {
        Payload::text(s)
    }
}

impl From<String> for Payload {
    fn from(s: String) -> Self {
        Payload::text(s)
    }
}

impl From<CreateEmbed> for Payload {
    fn from(e: CreateEmbed) -> Self {
        Payload::embed(e)
    }
}

/// How [`safe_reply`] answers in the interaction's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyRoute {
    /// Not acknowledged: `reply`.
    Reply,
    /// Deferred with `deferReply` (ephemeral is a boolean): `editReply` (falls back to `followUp` on 10008).
    EditReply,
    /// Already replied, or acknowledged by `deferUpdate` / `update`: `followUp` (never overwrite the clicked panel).
    FollowUp,
}

/// The route `safeReply` takes for a state.
pub fn reply_route(state: crate::interaction::AckState) -> ReplyRoute {
    if !state.deferred && !state.replied {
        ReplyRoute::Reply
    } else if state.deferred && !state.replied && state.ephemeral.is_some() {
        ReplyRoute::EditReply
    } else {
        ReplyRoute::FollowUp
    }
}

/// What a sent answer produced.
#[derive(Debug, Clone)]
pub enum Sent {
    /// The answer was an interaction reply (its message is `@original`; `fetch_reply` reads it).
    Original,
    /// The answer is this message (an edited reply or a follow-up).
    Message(Box<Message>),
}

/// `safeReply(interaction, payload)`: replies in the way that fits the interaction's state (see [`ReplyRoute`]).
/// Errors that mean the interaction is gone are logged at debug level, the rest as a warning. `None` when nothing
/// was sent.
pub async fn safe_reply(r: &Responder, payload: impl Into<Payload>) -> Option<Sent> {
    let p = payload.into();
    let result: Result<Sent, InteractionError> = match reply_route(r.state()) {
        ReplyRoute::Reply => r.reply(p.to_reply()).await.map(|_| Sent::Original),
        ReplyRoute::EditReply => match r.edit_reply(p.to_edit()).await {
            Ok(m) => Ok(Sent::Message(Box::new(m))),
            // 10008: the original was deleted (deleteReply).
            Err(e) if e.code() == Some(10008) => r.follow_up(p.to_follow_up()).await.map(|m| Sent::Message(Box::new(m))),
            Err(e) => Err(e),
        },
        ReplyRoute::FollowUp => r.follow_up(p.to_follow_up()).await.map(|m| Sent::Message(Box::new(m))),
    };
    match result {
        Ok(s) => Some(s),
        Err(e) => {
            if is_interaction_gone(&e) {
                bot_util::log_debug!(format!("safeReply: interaction gone ({})", code_text(&e)));
            } else {
                bot_util::log_warn!("safeReply failed", err_arg(&e));
            }
            None
        }
    }
}

/// [`safe_reply`] that returns the sent message (`withResponse: true`): on the reply route it reads `@original`.
pub async fn safe_reply_message(r: &Responder, payload: impl Into<Payload>) -> Option<Message> {
    match safe_reply(r, payload).await? {
        Sent::Message(m) => Some(*m),
        Sent::Original => r.fetch_reply().await.ok(),
    }
}

/// `replyError(interaction, title, description, { ephemeral = true })`.
pub async fn reply_error(r: &Responder, title: &str, description: Option<&str>, ephemeral: bool) -> Option<Sent> {
    safe_reply(r, Payload::embed(error_embed(title, description, None)).ephemeral(ephemeral)).await
}

/// `replySuccess(interaction, title, description, { ephemeral = false })`.
pub async fn reply_success(r: &Responder, title: &str, description: Option<&str>, ephemeral: bool) -> Option<Sent> {
    safe_reply(r, Payload::embed(success_embed(title, description, None)).ephemeral(ephemeral)).await
}

// ── Permissions ────────────────────────────────────────────────────────────────

/// The keys of `perms` that are real `PermissionFlagsBits` keys (an unknown key is a programming error: logged and
/// ignored, as the JS does).
fn known_keys<'a>(perms: &[&'a str]) -> Vec<&'a str> {
    let mut keys = Vec::new();
    for k in perms {
        if permissions::is_flag(k) {
            keys.push(*k);
        } else {
            bot_util::log_warn!(format!("missingBotPermissions: unknown permission \"{k}\" ignored"));
        }
    }
    keys
}

/// `missingBotPermissions`'s decision: the keys of `perms` (in order) that `bits` lacks; every key when the bits are
/// unknown (`null`). Administrator satisfies everything.
pub fn missing_from(bits: Option<Permissions>, perms: &[&str]) -> Vec<String> {
    let keys = known_keys(perms);
    match bits {
        None => keys.into_iter().map(str::to_string).collect(),
        Some(b) => keys.into_iter().filter(|k| !permissions::flag(k).is_some_and(|f| js_has(b, f))).map(str::to_string).collect(),
    }
}

/// Where the bot's permissions are read (`channelOrGuild`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermTarget {
    /// `channel.permissionsFor(me)`.
    Channel(GuildId, ChannelId),
    /// `guild.members.me.permissions`.
    Guild(GuildId),
}

/// The bot's permissions there, from the serenity cache; `None` when they cannot be computed (no connection, guild,
/// channel or bot member cached).
pub fn bot_permissions(app: &App, target: PermTarget) -> Option<Permissions> {
    let d = app.discord()?;
    let bot = d.cache.current_user().id;
    match target {
        PermTarget::Guild(g) => {
            let guild = d.cache.guild(g)?;
            let me = permissions::me(&guild, bot)?;
            Some(permissions::guild_permissions(&guild, bot, &me.roles))
        }
        PermTarget::Channel(g, c) => {
            let guild = d.cache.guild(g)?;
            let me = permissions::me(&guild, bot)?;
            let channel = permissions::find_channel(&guild, c)?;
            permissions::member_permissions_in(&guild, channel, bot, &me.roles, true)
        }
    }
}

/// `missingBotPermissions(channelOrGuild, perms)`: the keys the bot lacks there (every key when that cannot be told).
pub fn missing_bot_permissions(app: &App, target: PermTarget, perms: &[&str]) -> Vec<String> {
    missing_from(bot_permissions(app, target), perms)
}

/// `requireBotPermissions(interaction, perms, { channel, ephemeral })`: answers 'Missing Permissions' and returns
/// false when the bot lacks a permission. `channel` `Some` checks there (`in <#id>`), `None` server-wide. True
/// outside a guild.
pub async fn require_bot_permissions(ctx: &impl Interactive, perms: &[&str], channel: Option<ChannelId>, ephemeral: bool) -> bool {
    let Some(g) = ctx.guild_id() else { return true };
    let target = match channel {
        Some(c) => PermTarget::Channel(g, c),
        None => PermTarget::Guild(g),
    };
    let missing = missing_bot_permissions(ctx.app(), target, perms);
    if missing.is_empty() {
        return true;
    }
    let names = permission_names(&missing).join(", ");
    let place = match channel {
        Some(c) => format!("in <#{c}>"),
        None => "in this server".to_string(),
    };
    reply_error(ctx.responder(), "Missing Permissions", Some(&format!("I need **{names}** {place} to do that.")), ephemeral).await;
    false
}

/// The result of [`check_channel_usable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelUsable {
    pub ok: bool,
    pub missing: Vec<String>,
    pub reason: Option<String>,
}

/// The default `needs` of `checkChannelUsable`.
pub const CHANNEL_POST_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "EmbedLinks"];

/// `checkChannelUsable`'s decision for a channel (`None`: not found) with the bot's permissions there.
pub fn channel_usable_verdict(channel: Option<&GuildChannel>, bot_perms: Option<Permissions>, needs: &[&str]) -> ChannelUsable {
    let Some(ch) = channel else {
        return ChannelUsable { ok: false, missing: Vec::new(), reason: Some("Channel not found".into()) };
    };
    if !permissions::is_text_based(ch) {
        return ChannelUsable { ok: false, missing: Vec::new(), reason: Some("That is not a text channel".into()) };
    }
    let need: Vec<&str> =
        if permissions::is_thread(ch) { needs.iter().map(|k| if *k == "SendMessages" { "SendMessagesInThreads" } else { k }).collect() } else { needs.to_vec() };
    let missing = missing_from(bot_perms, &need);
    if missing.is_empty() {
        return ChannelUsable { ok: true, missing, reason: None };
    }
    let reason = format!("I need **{}** in <#{}>.", permission_names(&missing).join(", "), ch.id);
    ChannelUsable { ok: false, missing, reason: Some(reason) }
}

/// `checkChannelUsable(channel, { needs })` for a channel of a cached guild.
pub fn check_channel_usable(app: &App, guild_id: GuildId, channel_id: Option<ChannelId>, needs: &[&str]) -> ChannelUsable {
    let channel = channel_id.and_then(|c| cached_channel(app, guild_id, c));
    let perms = channel.as_ref().and_then(|c| bot_permissions(app, PermTarget::Channel(guild_id, c.id)));
    channel_usable_verdict(channel.as_ref(), perms, needs)
}

/// A cached guild channel or thread (`guild.channels.cache.get(id)`).
pub fn cached_channel(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Option<GuildChannel> {
    let d = app.discord()?;
    let guild = d.cache.guild(guild_id)?;
    permissions::find_channel(&guild, channel_id).cloned()
}

/// A clone of a cached guild (`client.guilds.cache.get(id)`).
pub fn cached_guild(app: &App, guild_id: GuildId) -> Option<Guild> {
    app.discord()?.cache.guild(guild_id).map(|g| g.clone())
}

/// Runs `f` on the cached guild without cloning it (`client.guilds.cache.get(id)` on a hot path). `None` without a
/// Discord connection or when the guild is not cached. The closure runs under the cache's shard lock: it must not
/// touch the serenity cache itself (no `cache.update`, no other `cache.guild` lookups), or it deadlocks.
pub fn with_cached_guild<R>(app: &App, guild_id: GuildId, f: impl FnOnce(&Guild) -> R) -> Option<R> {
    let d = app.discord()?;
    let guild = d.cache.guild(guild_id)?;
    Some(f(&guild))
}

/// `member.roles.cache.map(r => r.id)`: `@everyone` (the guild id) first, then the member's roles in the member's
/// order, dropping ids the guild no longer has.
pub fn member_role_ids(guild: &Guild, member: &Member) -> Vec<RoleId> {
    let mut ids = vec![permissions::everyone_role_id(guild)];
    for r in &member.roles {
        if guild.roles.contains_key(r) && !ids.contains(r) {
            ids.push(*r);
        }
    }
    ids
}

/// Why [`fetch_channel`] found no channel.
#[derive(Debug)]
pub enum FetchChannelError {
    /// No Discord connection.
    Offline,
    /// Discord answered with an error (unknown channel, missing access, ...).
    Rest(serenity::Error),
    /// The channel belongs to another guild, or is not a guild channel (discord.js `GuildChannelUnowned`).
    Unowned,
}

impl std::fmt::Display for FetchChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => f.write_str("Discord is not connected"),
            Self::Rest(e) => f.write_str(&crate::discord::error_message(e)),
            Self::Unowned => f.write_str("The fetched channel does not belong to this manager's guild."),
        }
    }
}

impl std::error::Error for FetchChannelError {}

/// `guild.channels.fetch(id)`: the cached channel or thread, else a REST `GET /channels/{id}` whose result is put
/// into the cache ([`crate::discord::cache_created_channel`]). A channel of another guild is refused (`Unowned`), as
/// discord.js does, so it never lands in this guild's cache.
#[allow(clippy::result_large_err)] // serenity::Error, as serenity returns it
pub async fn fetch_channel(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Result<GuildChannel, FetchChannelError> {
    if let Some(c) = cached_channel(app, guild_id, channel_id) {
        return Ok(c);
    }
    let http = app.http().ok_or(FetchChannelError::Offline)?;
    let channel = http.get_channel(channel_id).await.map_err(FetchChannelError::Rest)?;
    let serenity::all::Channel::Guild(gc) = channel else { return Err(FetchChannelError::Unowned) };
    if gc.guild_id != guild_id {
        return Err(FetchChannelError::Unowned);
    }
    crate::discord::cache_created_channel(app, &gc);
    Ok(gc)
}

/// [`fetch_channel`] with every failure as `None` (`guild.channels.fetch(id).catch(() => null)`).
pub async fn resolve_channel(app: &App, guild_id: GuildId, channel_id: ChannelId) -> Option<GuildChannel> {
    fetch_channel(app, guild_id, channel_id).await.ok()
}

/// The bot's permissions in `channel` (which need not be cached; a thread's parent must be), from the cached guild.
pub fn bot_permissions_for(app: &App, guild_id: GuildId, channel: &GuildChannel) -> Option<Permissions> {
    let d = app.discord()?;
    let bot = d.cache.current_user().id;
    let guild = d.cache.guild(guild_id)?;
    let me = permissions::me(&guild, bot)?;
    permissions::member_permissions_in(&guild, channel, bot, &me.roles, true)
}

/// `checkChannelUsable(channel, { needs })` for a channel object in hand (fetched over REST, for example).
pub fn check_channel_usable_for(app: &App, guild_id: GuildId, channel: &GuildChannel, needs: &[&str]) -> ChannelUsable {
    channel_usable_verdict(Some(channel), bot_permissions_for(app, guild_id, channel), needs)
}

/// [`check_channel_usable`] that fetches an uncached channel first ([`resolve_channel`]).
pub async fn check_channel_usable_fetch(app: &App, guild_id: GuildId, channel_id: Option<ChannelId>, needs: &[&str]) -> ChannelUsable {
    let channel = match channel_id {
        Some(c) => resolve_channel(app, guild_id, c).await,
        None => None,
    };
    match channel {
        Some(ch) => check_channel_usable_for(app, guild_id, &ch, needs),
        None => channel_usable_verdict(None, None, needs),
    }
}

/// A message's components as builders (`ActionRowBuilder.from(row)` / `components: message.components` in
/// discord.js), e.g. to re-send them with buttons disabled. Select menus keep their kind, options, channel types,
/// placeholder, bounds and disabled flag (serenity's model drops `default_values`, so those are not carried over).
pub fn components_to_builders(rows: &[serenity::all::ActionRow]) -> Vec<CreateActionRow> {
    use serenity::all::{ActionRowComponent, ButtonKind, ComponentType};
    use serenity::builder::{CreateInputText, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption};
    let mut out = Vec::new();
    for row in rows {
        let mut buttons = Vec::new();
        for c in &row.components {
            match c {
                ActionRowComponent::Button(b) => {
                    let mut nb = match &b.data {
                        ButtonKind::Link { url } => CreateButton::new_link(url.clone()),
                        ButtonKind::Premium { sku_id } => CreateButton::new_premium(*sku_id),
                        ButtonKind::NonLink { custom_id, style } => CreateButton::new(custom_id.clone()).style(*style),
                    };
                    if let Some(l) = &b.label {
                        nb = nb.label(l.clone());
                    }
                    if let Some(e) = &b.emoji {
                        nb = nb.emoji(e.clone());
                    }
                    buttons.push(nb.disabled(b.disabled));
                }
                ActionRowComponent::SelectMenu(m) => {
                    let kind = match m.kind {
                        ComponentType::UserSelect => CreateSelectMenuKind::User { default_users: None },
                        ComponentType::RoleSelect => CreateSelectMenuKind::Role { default_roles: None },
                        ComponentType::MentionableSelect => CreateSelectMenuKind::Mentionable { default_users: None, default_roles: None },
                        ComponentType::ChannelSelect => CreateSelectMenuKind::Channel {
                            channel_types: if m.channel_types.is_empty() { None } else { Some(m.channel_types.clone()) },
                            default_channels: None,
                        },
                        _ => CreateSelectMenuKind::String {
                            options: m
                                .options
                                .iter()
                                .map(|o| {
                                    let mut no = CreateSelectMenuOption::new(o.label.clone(), o.value.clone()).default_selection(o.default);
                                    if let Some(d) = &o.description {
                                        no = no.description(d.clone());
                                    }
                                    if let Some(e) = &o.emoji {
                                        no = no.emoji(e.clone());
                                    }
                                    no
                                })
                                .collect(),
                        },
                    };
                    let mut menu = CreateSelectMenu::new(m.custom_id.clone().unwrap_or_default(), kind).disabled(m.disabled);
                    if let Some(p) = &m.placeholder {
                        menu = menu.placeholder(p.clone());
                    }
                    if let Some(n) = m.min_values {
                        menu = menu.min_values(n);
                    }
                    if let Some(n) = m.max_values {
                        menu = menu.max_values(n);
                    }
                    out.push(CreateActionRow::SelectMenu(menu));
                }
                ActionRowComponent::InputText(t) => {
                    let mut it = CreateInputText::new(t.style.unwrap_or(serenity::all::InputTextStyle::Short), t.label.clone().unwrap_or_default(), t.custom_id.clone())
                        .required(t.required);
                    if let Some(v) = &t.value {
                        it = it.value(v.clone());
                    }
                    if let Some(p) = &t.placeholder {
                        it = it.placeholder(p.clone());
                    }
                    if let Some(n) = t.min_length {
                        it = it.min_length(n);
                    }
                    if let Some(n) = t.max_length {
                        it = it.max_length(n);
                    }
                    out.push(CreateActionRow::InputText(it));
                }
                _ => {}
            }
        }
        if !buttons.is_empty() {
            out.push(CreateActionRow::Buttons(buttons));
        }
    }
    out
}

/// The permissions a role must not carry unless deliberately allowed (`DANGEROUS_PERMISSIONS`).
pub const DANGEROUS_PERMISSIONS: [&str; 10] = [
    "Administrator",
    "ManageGuild",
    "ManageRoles",
    "BanMembers",
    "KickMembers",
    "ManageChannels",
    "ManageWebhooks",
    "MentionEveryone",
    "ModerateMembers",
    "ManageMessages",
];

/// The dangerous permissions a role really carries (`has(flag, false)`: no Administrator shortcut).
pub fn dangerous_of(role_perms: Permissions) -> Vec<&'static str> {
    DANGEROUS_PERMISSIONS.iter().copied().filter(|k| permissions::flag(k).is_some_and(|f| permissions::has_exact(role_perms, f))).collect()
}

/// The result of [`check_assignable_role`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCheck {
    pub ok: bool,
    pub reason: Option<String>,
    /// `PermissionFlagsBits` keys.
    pub dangerous: Vec<&'static str>,
}

/// `checkAssignableRole(guild, role, { executor, allowDangerous })`: exists, not @everyone, not managed, the bot has
/// Manage Roles, below the bot's highest role, below the executor's highest role (the guild owner is exempt), then
/// the dangerous permissions. `bot_id` is the bot's user id.
pub fn check_assignable_role(guild: &Guild, role: Option<&Role>, bot_id: UserId, executor: Option<&Member>, allow_dangerous: bool) -> RoleCheck {
    let fail = |reason: &str| RoleCheck { ok: false, reason: Some(reason.to_string()), dangerous: Vec::new() };
    let Some(role) = role else { return fail("Role not found.") };
    if role.id.get() == guild.id.get() {
        return fail("@everyone cannot be assigned.");
    }
    if role.managed {
        return fail("That role is managed by an integration.");
    }
    let Some(me) = permissions::me(guild, bot_id) else { return fail("I need the Manage Roles permission.") };
    if !js_has(permissions::guild_permissions(guild, bot_id, &me.roles), Permissions::MANAGE_ROLES) {
        return fail("I need the Manage Roles permission.");
    }
    let position = permissions::role_position(guild, role.id).unwrap_or(0);
    if position >= permissions::highest_position(guild, &me.roles) {
        return fail("That role is above my highest role.");
    }
    if let Some(ex) = executor
        && ex.user.id != guild.owner_id
        && position >= permissions::highest_position(guild, &ex.roles)
    {
        return fail("That role is above your highest role.");
    }
    let dangerous = dangerous_of(role.permissions);
    if !dangerous.is_empty() && !allow_dangerous {
        let names = permission_names(&dangerous).join(", ");
        return RoleCheck { ok: false, reason: Some(format!("That role has dangerous permissions: {names}.")), dangerous };
    }
    RoleCheck { ok: true, reason: None, dangerous }
}

/// [`check_assignable_role`] on the cached guild (`Role not found.` when the guild or role is not cached).
pub fn check_assignable_role_cached(app: &App, guild_id: GuildId, role_id: RoleId, executor: Option<&Member>, allow_dangerous: bool) -> RoleCheck {
    let Some(d) = app.discord() else {
        return RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() };
    };
    let bot = d.cache.current_user().id;
    let Some(guild) = d.cache.guild(guild_id) else {
        return RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() };
    };
    let role = guild.roles.get(&role_id).cloned();
    check_assignable_role(&guild, role.as_ref(), bot, executor, allow_dangerous)
}

/// The result of [`pingable_role_ids`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pingable {
    pub allowed: Vec<String>,
    pub refused: Vec<String>,
}

/// `pingableRoleIds(channel, executor, roleIds)`: a role may be pinged when it is mentionable, or when `executor`
/// (its id and roles) has Mention Everyone in the channel; @everyone and unknown roles are refused. Unique ids, in
/// order.
pub fn pingable_role_ids<S: AsRef<str>>(guild: &Guild, channel: &GuildChannel, executor: Option<(UserId, &[RoleId])>, role_ids: &[S]) -> Pingable {
    let mut out = Pingable::default();
    let mut can_mention_all: Option<bool> = None;
    let mut seen: Vec<String> = Vec::new();
    for raw in role_ids {
        let raw = raw.as_ref().to_string();
        if seen.contains(&raw) {
            continue;
        }
        seen.push(raw.clone());
        let role = raw.parse::<u64>().ok().filter(|v| *v != 0).and_then(|v| guild.roles.get(&RoleId::new(v)));
        let Some(role) = role.filter(|r| r.id.get() != guild.id.get()) else {
            out.refused.push(raw);
            continue;
        };
        if role.mentionable {
            out.allowed.push(raw);
            continue;
        }
        let can = *can_mention_all.get_or_insert_with(|| {
            executor
                .and_then(|(id, roles)| permissions::member_permissions_in(guild, channel, id, roles, true))
                .is_some_and(|p| js_has(p, Permissions::MENTION_EVERYONE))
        });
        if can { out.allowed.push(raw) } else { out.refused.push(raw) }
    }
    out
}

// ── Autocomplete ───────────────────────────────────────────────────────────────

/// One autocomplete choice: `{ name?, value }` with a string or number value.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub name: Option<String>,
    pub value: Value,
}

impl Choice {
    pub fn new(name: impl Into<String>, value: impl Into<Value>) -> Self {
        Self { name: Some(name.into()), value: value.into() }
    }

    /// A choice whose name is its value.
    pub fn value(value: impl Into<Value>) -> Self {
        Self { name: None, value: value.into() }
    }
}

fn js_string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => bot_util::js::number_to_string(n.as_f64().unwrap_or(0.0)),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `respondAutocomplete`'s list: at most 25, names clamped to 100 (an empty name becomes `—`), string values cut
/// to 100, empty or missing values dropped.
pub fn autocomplete_list(choices: &[Choice]) -> Vec<(String, Value)> {
    choices
        .iter()
        .take(25)
        .map(|c| {
            let raw = c.name.clone().unwrap_or_else(|| js_string_of(&c.value));
            let name = bot_util::text::clamp_text(&raw, 100);
            let name = if name.is_empty() { "—".to_string() } else { name };
            let value = match &c.value {
                Value::String(s) => Value::String(bot_util::js::utf16_prefix(s, 100).to_string()),
                v => v.clone(),
            };
            (name, value)
        })
        .filter(|(_, v)| !matches!(v, Value::Null) && v.as_str() != Some(""))
        .collect()
}

/// `respondAutocomplete(interaction, choices)`. Never fails (a late answer after 3 s is normal).
pub async fn respond_autocomplete(r: &Responder, choices: &[Choice]) {
    if r.state().responded {
        return;
    }
    let list = autocomplete_list(choices)
        .into_iter()
        .map(|(name, value)| match value {
            Value::String(s) => AutocompleteChoice::new(name, s),
            Value::Number(n) if n.is_i64() => AutocompleteChoice::new(name, n.as_i64().unwrap_or(0)),
            Value::Number(n) => AutocompleteChoice::new(name, n.as_f64().unwrap_or(0.0)),
            other => AutocompleteChoice::new(name, other.to_string()),
        })
        .collect();
    if let Err(e) = r.respond(list).await {
        bot_util::log_debug!(format!("respondAutocomplete failed: {e}"));
    }
}

// ── Confirm dialog ─────────────────────────────────────────────────────────────

/// `awaitConfirm` options (JS defaults: title 'Are you sure?', labels Confirm / Cancel, danger, 30 s, ephemeral).
#[derive(Debug, Clone)]
pub struct ConfirmOptions {
    pub title: Option<String>,
    pub description: Option<String>,
    pub confirm_label: String,
    pub cancel_label: String,
    pub danger: bool,
    pub timeout_ms: f64,
    pub ephemeral: bool,
}

impl Default for ConfirmOptions {
    fn default() -> Self {
        Self {
            title: None,
            description: None,
            confirm_label: "Confirm".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            timeout_ms: 30_000.0,
            ephemeral: true,
        }
    }
}

/// The JS clamp of a collector time: `min(840000, max(5000, floor(ms) || fallback))`.
pub fn clamp_collector_ms(ms: f64, fallback: u64) -> u64 {
    let f = ms.floor();
    let v = if f.is_finite() && f != 0.0 { f } else { fallback as f64 };
    v.clamp(5_000.0, 840_000.0) as u64
}

/// The confirm prompt: its embed and its button row, for `nonce`.
pub fn confirm_prompt(opts: &ConfirmOptions, nonce: &str) -> (CreateEmbed, CreateActionRow) {
    let title = bot_util::text::clamp_text(opts.title.as_deref().filter(|t| !t.is_empty()).unwrap_or("Are you sure?"), 200);
    let description = opts.description.as_deref().filter(|d| !d.is_empty()).map(|d| bot_util::text::clamp_text(d, 4000));
    let embed = if opts.danger { warn_embed(&title, description.as_deref(), None) } else { info_embed(&title, description.as_deref(), None) };
    let row = CreateActionRow::Buttons(vec![
        CreateButton::new(format!("confirm:{nonce}:yes"))
            .label(bot_util::text::clamp_text(&opts.confirm_label, 80))
            .style(if opts.danger { ButtonStyle::Danger } else { ButtonStyle::Success }),
        CreateButton::new(format!("confirm:{nonce}:no")).label(bot_util::text::clamp_text(&opts.cancel_label, 80)).style(ButtonStyle::Secondary),
    ]);
    (embed, row)
}

/// A random 12-hex-digit nonce (`randomBytes(6).toString('hex')`).
pub fn nonce12() -> String {
    let bytes: [u8; 6] = rand::random();
    hex::encode(bytes)
}

/// Where a prompt message was sent, for the later edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAt {
    /// The interaction's own reply (`@original`).
    Original,
    /// A follow-up message.
    FollowUp,
}

/// Sends a panel the way `awaitConfirm` and the paginator do: `reply` when not acknowledged, `editReply` after a
/// `deferReply`, else `followUp`. Returns the message and where it is.
pub async fn send_panel(r: &Responder, payload: &Payload) -> Result<(Message, PromptAt), InteractionError> {
    match reply_route(r.state()) {
        ReplyRoute::Reply => {
            r.reply(payload.to_reply()).await?;
            Ok((r.fetch_reply().await?, PromptAt::Original))
        }
        ReplyRoute::EditReply => Ok((r.edit_reply(payload.to_edit()).await?, PromptAt::Original)),
        ReplyRoute::FollowUp => Ok((r.follow_up(payload.to_follow_up()).await?, PromptAt::FollowUp)),
    }
}

/// `interaction.editReply({ message: id, ...edit })`, falling back to `message.edit(edit)` through the channel route.
pub async fn edit_panel(app: &App, r: &Responder, at: PromptAt, message: &Message, edit: &Payload) -> bool {
    let first = match at {
        PromptAt::Original => r.edit_reply(edit.to_edit()).await.map(|_| ()),
        PromptAt::FollowUp => r.edit_follow_up(message.id, edit.to_follow_up()).await.map(|_| ()),
    };
    if first.is_ok() {
        return true;
    }
    match app.http() {
        Some(http) => message.channel_id.edit_message(http.as_ref(), message.id, edit.to_message_edit()).await.is_ok(),
        None => false,
    }
}

/// "These buttons aren't for you." to a stranger's click (`collector.on('ignore')`).
pub async fn answer_stranger(click: &ComponentCtx) {
    let _ = click.reply(CreateInteractionResponseMessage::new().content("These buttons aren't for you.").ephemeral(true)).await;
}

/// `awaitConfirm(interaction, opts)`: shows the prompt with Confirm and Cancel and waits for the invoker. Other users
/// are told the buttons are not for them. The prompt loses its buttons when it ends (footer Confirmed / Cancelled /
/// timed out). `true` only when the invoker confirmed; never fails.
pub async fn await_confirm(ctx: &impl Interactive, opts: ConfirmOptions) -> bool {
    let nonce = nonce12();
    let (base, row) = confirm_prompt(&opts, &nonce);
    let payload = Payload::embed(base.clone()).components(vec![row]).ephemeral(opts.ephemeral);
    let (message, at) = match send_panel(ctx.responder(), &payload).await {
        Ok(v) => v,
        Err(e) => {
            if is_interaction_gone(&e) {
                bot_util::log_debug!(format!("awaitConfirm: interaction gone ({})", code_text(&e)));
            } else {
                bot_util::log_warn!("awaitConfirm failed", err_arg(&e));
            }
            return false;
        }
    };
    let time = Duration::from_millis(clamp_collector_ms(opts.timeout_ms, 30_000));
    let deadline = tokio::time::Instant::now() + time;
    let invoker = ctx.user().id;
    let yes = format!("confirm:{nonce}:yes");
    let mine = format!("confirm:{nonce}:");
    let mut collector = ctx.app().collectors().collect(CollectorFilter::message(message.id).prefixes(["confirm"]));
    let picked = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break None;
        }
        let Some(click) = collector.next(left).await else { break None };
        if click.user().id == invoker && click.custom_id().starts_with(&mine) {
            break Some(click);
        }
        answer_stranger(&click).await;
    };
    collector.stop();
    match picked {
        Some(click) => {
            let result = click.custom_id() == yes;
            let footer = if result { "Confirmed" } else { "Cancelled" };
            let update = Payload::embed(base.footer(CreateEmbedFooter::new(footer))).components(Vec::new());
            if let Err(e) = click.update(update.to_update()).await {
                bot_util::log_debug!(format!("awaitConfirm end failed: {e}"));
            }
            result
        }
        None => {
            let edit = Payload::embed(base.footer(CreateEmbedFooter::new("Timed out — nothing was changed."))).components(Vec::new());
            edit_panel(ctx.app(), ctx.responder(), at, &message, &edit).await;
            false
        }
    }
}
