//! Interaction responses with the discord.js state machine, and the contexts commands, autocompletes and components
//! receive.
//!
//! Serenity does not track whether an interaction was acknowledged; discord.js does (`replied`, `deferred`,
//! `ephemeral`), and the router's error reply and the collector grace period depend on it. Every answer to an
//! interaction therefore goes through a [`Responder`]:
//!
//! | discord.js | Responder | state after success |
//! |---|---|---|
//! | `reply(opts)` | [`Responder::reply`] | replied, ephemeral from the flags |
//! | `deferReply({ ephemeral })` | [`Responder::defer_reply`] | deferred, ephemeral `Some(bool)` |
//! | `deferUpdate()` | [`Responder::defer_update`] | deferred (ephemeral unchanged) |
//! | `update(opts)` | [`Responder::update`] | replied |
//! | `showModal(modal)` | [`Responder::show_modal`] | replied |
//! | `editReply(opts)` | [`Responder::edit_reply`] | replied |
//! | `followUp(opts)` | [`Responder::follow_up`] | replied |
//! | `deleteReply()` | [`Responder::delete_reply`] | unchanged |
//! | `respond(choices)` | [`Responder::respond`] | responded |
//!
//! `reply`, `defer_*`, `update` and `show_modal` fail with [`InteractionError::AlreadyReplied`] after an
//! acknowledgement; `edit_reply`, `follow_up` and `delete_reply` fail with [`InteractionError::NotReplied`] before one,
//! exactly like discord.js. Never call serenity's `create_response` on the raw interaction.
//!
//! The responder sends through an [`InteractionBackend`]: [`SerenityBackend`] in production, [`RecordingBackend`] in
//! tests (it records every call as JSON and fabricates the returned messages).

mod recording;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serenity::all::{
    ChannelId, CommandDataOption, CommandDataOptionValue, CommandInteraction, ComponentInteraction, GuildId, Http, InteractionId, Member,
    Message, MessageId, ModalInteraction, User,
};
use serenity::builder::{
    AutocompleteChoice, Builder, CreateAutocompleteResponse, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, CreateModal, EditInteractionResponse,
};
use serenity::model::application::ActionRowComponent;

use crate::context::App;
use crate::permission_checker::MemberAccess;
use crate::settings::Settings;

pub use recording::{RecordedCall, RecordingBackend};

/// The ephemeral message flag.
pub const EPHEMERAL_FLAG: u64 = 1 << 6;

/// Why a response could not be sent.
#[derive(Debug, Clone, thiserror::Error)]
pub enum InteractionError {
    /// discord.js `InteractionAlreadyReplied`.
    #[error("The reply to this interaction has already been sent or deferred.")]
    AlreadyReplied,
    /// discord.js `InteractionNotReplied`.
    #[error("The reply to this interaction has not been sent or deferred.")]
    NotReplied,
    /// Discord refused the request (expired token, unknown interaction, ...) or the network failed.
    #[error("{0}")]
    Http(String),
    /// Discord refused the request with a JSON error code (`err.code` in discord.js: 10062 unknown interaction,
    /// 10008 unknown message, 40060 already acknowledged, ...).
    #[error("{message}")]
    Api { code: u64, message: String },
}

impl InteractionError {
    /// discord.js `err.code`: the Discord error code, or `InteractionAlreadyReplied` / `InteractionNotReplied` as
    /// [`InteractionError::AlreadyReplied`] / [`InteractionError::NotReplied`] (no number).
    pub fn code(&self) -> Option<u64> {
        match self {
            Self::Api { code, .. } => Some(*code),
            _ => None,
        }
    }
}

/// Where responses go.
#[async_trait]
pub trait InteractionBackend: Send + Sync + 'static {
    /// `POST /interactions/{id}/{token}/callback`.
    async fn create_response(&self, id: InteractionId, token: &str, response: CreateInteractionResponse) -> Result<(), InteractionError>;
    /// `GET /webhooks/{app}/{token}/messages/@original`.
    async fn get_response(&self, token: &str) -> Result<Message, InteractionError>;
    /// `PATCH .../messages/@original`.
    async fn edit_response(&self, token: &str, edit: EditInteractionResponse) -> Result<Message, InteractionError>;
    /// `DELETE .../messages/@original`.
    async fn delete_response(&self, token: &str) -> Result<(), InteractionError>;
    /// `POST /webhooks/{app}/{token}`.
    async fn create_followup(&self, token: &str, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError>;
    /// `PATCH .../messages/{id}`.
    async fn edit_followup(&self, token: &str, id: MessageId, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError>;
    /// `DELETE .../messages/{id}`.
    async fn delete_followup(&self, token: &str, id: MessageId) -> Result<(), InteractionError>;
}

/// The production backend: serenity's HTTP client (its default allowed mentions apply to every builder).
#[derive(Clone)]
pub struct SerenityBackend {
    pub http: Arc<Http>,
}

fn http_err(e: serenity::Error) -> InteractionError {
    match crate::discord::error_code(&e) {
        Some(code) => InteractionError::Api { code, message: crate::discord::error_message(&e) },
        None => InteractionError::Http(e.to_string()),
    }
}

#[async_trait]
impl InteractionBackend for SerenityBackend {
    async fn create_response(&self, id: InteractionId, token: &str, response: CreateInteractionResponse) -> Result<(), InteractionError> {
        response.execute(self.http.as_ref(), (id, token)).await.map_err(http_err)
    }

    async fn get_response(&self, token: &str) -> Result<Message, InteractionError> {
        self.http.get_original_interaction_response(token).await.map_err(http_err)
    }

    async fn edit_response(&self, token: &str, edit: EditInteractionResponse) -> Result<Message, InteractionError> {
        edit.execute(self.http.as_ref(), token).await.map_err(http_err)
    }

    async fn delete_response(&self, token: &str) -> Result<(), InteractionError> {
        self.http.delete_original_interaction_response(token).await.map_err(http_err)
    }

    async fn create_followup(&self, token: &str, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        followup.execute(self.http.as_ref(), (None, token)).await.map_err(http_err)
    }

    async fn edit_followup(&self, token: &str, id: MessageId, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        followup.execute(self.http.as_ref(), (Some(id), token)).await.map_err(http_err)
    }

    async fn delete_followup(&self, token: &str, id: MessageId) -> Result<(), InteractionError> {
        self.http.delete_followup_message(token, id).await.map_err(http_err)
    }
}

/// A backend for an app without a Discord connection: every call fails.
pub struct DisconnectedBackend;

#[async_trait]
impl InteractionBackend for DisconnectedBackend {
    async fn create_response(&self, _: InteractionId, _: &str, _: CreateInteractionResponse) -> Result<(), InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
    async fn get_response(&self, _: &str) -> Result<Message, InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
    async fn edit_response(&self, _: &str, _: EditInteractionResponse) -> Result<Message, InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
    async fn delete_response(&self, _: &str) -> Result<(), InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
    async fn create_followup(&self, _: &str, _: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
    async fn edit_followup(&self, _: &str, _: MessageId, _: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
    async fn delete_followup(&self, _: &str, _: MessageId) -> Result<(), InteractionError> {
        Err(InteractionError::Http("no Discord connection".into()))
    }
}

/// discord.js's acknowledgement state of an interaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AckState {
    pub replied: bool,
    pub deferred: bool,
    /// `null` until `reply` or `deferReply` sets it.
    pub ephemeral: Option<bool>,
    /// An autocomplete answered (`responded`).
    pub responded: bool,
}

struct ResponderInner {
    backend: Arc<dyn InteractionBackend>,
    id: InteractionId,
    token: String,
    state: Mutex<AckState>,
}

/// Answers one interaction and tracks its state. Clones share the state (like the one discord.js object the router,
/// the handler and a collector all hold).
#[derive(Clone)]
pub struct Responder {
    inner: Arc<ResponderInner>,
}

impl std::fmt::Debug for Responder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Responder").field("id", &self.inner.id).field("state", &self.state()).finish()
    }
}

fn flags_of<T: serde::Serialize>(builder: &T) -> u64 {
    serde_json::to_value(builder).ok().and_then(|v| v.get("flags").and_then(serde_json::Value::as_u64)).unwrap_or(0)
}

impl Responder {
    pub fn new(backend: Arc<dyn InteractionBackend>, id: InteractionId, token: impl Into<String>) -> Self {
        Self { inner: Arc::new(ResponderInner { backend, id, token: token.into(), state: Mutex::new(AckState::default()) }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AckState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The current state.
    pub fn state(&self) -> AckState {
        *self.lock()
    }

    pub fn replied(&self) -> bool {
        self.lock().replied
    }

    pub fn deferred(&self) -> bool {
        self.lock().deferred
    }

    pub fn ephemeral(&self) -> Option<bool> {
        self.lock().ephemeral
    }

    /// `replied || deferred`.
    pub fn is_acknowledged(&self) -> bool {
        let s = self.lock();
        s.replied || s.deferred
    }

    /// Overrides the state (tests that simulate a handler, and collector hand-offs).
    pub fn set_state(&self, state: AckState) {
        *self.lock() = state;
    }

    fn require_unacknowledged(&self) -> Result<(), InteractionError> {
        if self.is_acknowledged() { Err(InteractionError::AlreadyReplied) } else { Ok(()) }
    }

    fn require_acknowledged(&self) -> Result<(), InteractionError> {
        if self.is_acknowledged() { Ok(()) } else { Err(InteractionError::NotReplied) }
    }

    /// `reply(options)`.
    pub async fn reply(&self, message: CreateInteractionResponseMessage) -> Result<(), InteractionError> {
        self.require_unacknowledged()?;
        let ephemeral = flags_of(&message) & EPHEMERAL_FLAG != 0;
        self.inner.backend.create_response(self.inner.id, &self.inner.token, CreateInteractionResponse::Message(message)).await?;
        let mut s = self.lock();
        s.ephemeral = Some(ephemeral);
        s.replied = true;
        Ok(())
    }

    /// `reply({ content, ephemeral })`.
    pub async fn reply_text(&self, content: impl Into<String>, ephemeral: bool) -> Result<(), InteractionError> {
        self.reply(CreateInteractionResponseMessage::new().content(content).ephemeral(ephemeral)).await
    }

    /// `reply({ embeds: [embed], ephemeral })`.
    pub async fn reply_embed(&self, embed: CreateEmbed, ephemeral: bool) -> Result<(), InteractionError> {
        self.reply(CreateInteractionResponseMessage::new().embed(embed).ephemeral(ephemeral)).await
    }

    /// `deferReply({ ephemeral })`.
    pub async fn defer_reply(&self, ephemeral: bool) -> Result<(), InteractionError> {
        self.require_unacknowledged()?;
        let msg = CreateInteractionResponseMessage::new().ephemeral(ephemeral);
        self.inner.backend.create_response(self.inner.id, &self.inner.token, CreateInteractionResponse::Defer(msg)).await?;
        let mut s = self.lock();
        s.deferred = true;
        s.ephemeral = Some(ephemeral);
        Ok(())
    }

    /// `deferUpdate()` (components and modals from a message).
    pub async fn defer_update(&self) -> Result<(), InteractionError> {
        self.require_unacknowledged()?;
        self.inner.backend.create_response(self.inner.id, &self.inner.token, CreateInteractionResponse::Acknowledge).await?;
        self.lock().deferred = true;
        Ok(())
    }

    /// `update(options)`: edits the message the component is on.
    pub async fn update(&self, message: CreateInteractionResponseMessage) -> Result<(), InteractionError> {
        self.require_unacknowledged()?;
        self.inner.backend.create_response(self.inner.id, &self.inner.token, CreateInteractionResponse::UpdateMessage(message)).await?;
        self.lock().replied = true;
        Ok(())
    }

    /// `showModal(modal)`.
    pub async fn show_modal(&self, modal: CreateModal) -> Result<(), InteractionError> {
        self.require_unacknowledged()?;
        self.inner.backend.create_response(self.inner.id, &self.inner.token, CreateInteractionResponse::Modal(modal)).await?;
        self.lock().replied = true;
        Ok(())
    }

    /// `respond(choices)` (autocomplete; at most 25 choices, as Discord requires).
    pub async fn respond(&self, choices: Vec<AutocompleteChoice>) -> Result<(), InteractionError> {
        if self.lock().responded {
            return Err(InteractionError::AlreadyReplied);
        }
        let resp = CreateAutocompleteResponse::new().set_choices(choices);
        self.inner.backend.create_response(self.inner.id, &self.inner.token, CreateInteractionResponse::Autocomplete(resp)).await?;
        self.lock().responded = true;
        Ok(())
    }

    /// `editReply(options)`.
    pub async fn edit_reply(&self, edit: EditInteractionResponse) -> Result<Message, InteractionError> {
        self.require_acknowledged()?;
        let msg = self.inner.backend.edit_response(&self.inner.token, edit).await?;
        self.lock().replied = true;
        Ok(msg)
    }

    /// `deleteReply()`.
    pub async fn delete_reply(&self) -> Result<(), InteractionError> {
        self.require_acknowledged()?;
        self.inner.backend.delete_response(&self.inner.token).await
    }

    /// `fetchReply()`.
    pub async fn fetch_reply(&self) -> Result<Message, InteractionError> {
        self.inner.backend.get_response(&self.inner.token).await
    }

    /// `followUp(options)`.
    pub async fn follow_up(&self, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        self.require_acknowledged()?;
        let msg = self.inner.backend.create_followup(&self.inner.token, followup).await?;
        self.lock().replied = true;
        Ok(msg)
    }

    /// `webhook.editMessage(id, options)` for a follow-up.
    pub async fn edit_follow_up(&self, id: MessageId, followup: CreateInteractionResponseFollowup) -> Result<Message, InteractionError> {
        self.require_acknowledged()?;
        self.inner.backend.edit_followup(&self.inner.token, id, followup).await
    }

    /// `deleteReply(messageId)` for a follow-up.
    pub async fn delete_follow_up(&self, id: MessageId) -> Result<(), InteractionError> {
        self.require_acknowledged()?;
        self.inner.backend.delete_followup(&self.inner.token, id).await
    }
}

/// The time Discord created an interaction (its snowflake), in epoch milliseconds.
pub fn created_ms(id: InteractionId) -> f64 {
    id.created_at().timestamp_millis() as f64
}

/// `ackBudgetMs(interaction)`: what is left of the 3 s acknowledgement window (minus the default reserve).
pub fn ack_budget_ms(id: InteractionId) -> u64 {
    bot_util::async_util::ack_budget_ms(created_ms(id), bot_util::async_util::ACK_RESERVE_MS)
}

/// `(group, subcommand)` of a command's options (`getSubcommandGroup(false)`, `getSubcommand(false)`).
pub fn subcommand_path(options: &[CommandDataOption]) -> (Option<String>, Option<String>) {
    let Some(first) = options.first() else { return (None, None) };
    match &first.value {
        CommandDataOptionValue::SubCommandGroup(inner) => {
            let sub = inner.iter().find(|o| matches!(o.value, CommandDataOptionValue::SubCommand(_))).map(|o| o.name.clone());
            (Some(first.name.clone()), sub)
        }
        CommandDataOptionValue::SubCommand(_) => (None, Some(first.name.clone())),
        _ => (None, None),
    }
}

/// The leaf options under the subcommand (or the top-level options when there is none).
pub fn leaf_options(options: &[CommandDataOption]) -> &[CommandDataOption] {
    match options.first().map(|o| &o.value) {
        Some(CommandDataOptionValue::SubCommandGroup(inner)) => match inner.first().map(|o| &o.value) {
            Some(CommandDataOptionValue::SubCommand(leaf)) => leaf,
            _ => &[],
        },
        Some(CommandDataOptionValue::SubCommand(leaf)) => leaf,
        _ => options,
    }
}

/// A full [`Member`] from the partial member Discord sends with a message or a resolved option, plus its user
/// (discord.js builds a `GuildMember` from the same data).
pub fn member_from_partial(partial: serenity::all::PartialMember, user: User, guild_id: Option<GuildId>) -> Member {
    let guild = partial.guild_id.or(guild_id).map(|g| g.to_string()).unwrap_or_else(|| "1".into());
    let json = serde_json::json!({
        "user": user,
        "nick": partial.nick,
        "avatar": partial.avatar,
        "roles": partial.roles,
        "joined_at": partial.joined_at,
        "premium_since": partial.premium_since,
        "deaf": partial.deaf,
        "mute": partial.mute,
        "flags": 0,
        "pending": partial.pending,
        "permissions": partial.permissions,
        "communication_disabled_until": null,
        "guild_id": guild,
        "unusual_dm_activity_until": partial.unusual_dm_activity_until,
    });
    serde_json::from_value(json).unwrap_or_else(|_| {
        // Every field above round-trips through serenity's own serde; this branch is unreachable in practice.
        serde_json::from_value(serde_json::json!({ "user": user, "roles": [], "deaf": false, "mute": false, "flags": 0, "guild_id": guild }))
            .unwrap_or_else(|e| panic!("a minimal member deserializes: {e}"))
    })
}

/// The member's permission view (guild-level permissions from the cache when it has the guild).
pub fn member_access(app: &App, guild_id: Option<GuildId>, member: Option<&Member>) -> Option<MemberAccess> {
    let (g, m) = (guild_id?, member?);
    let cache = app.discord().map(|d| d.cache.clone());
    Some(MemberAccess::from_cache(cache.as_deref(), g, m))
}

/// The member's permission level (`resolvePermissionLevel(interaction.member, settings)`). Settings come from the memo,
/// else a load bounded by `budget_ms` (at most 1000 ms); without settings a moderator role cannot count, Discord
/// permissions still do. `Member` outside a guild. Persistent components re-check levels with this themselves.
pub async fn permission_level_of(app: &App, guild_id: Option<GuildId>, member: Option<&Member>, budget_ms: u64) -> bot_config::PermissionLevel {
    let Some(access) = member_access(app, guild_id, member) else { return bot_config::PermissionLevel::Member };
    let mut settings = None;
    if let Some(g) = guild_id {
        let gid = g.to_string();
        settings = app.settings().peek_settings(&gid);
        if settings.is_none() {
            let a = app.clone();
            let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&gid).await) };
            settings = bot_util::async_util::with_timeout(load, budget_ms.min(1000), "permission settings").await.ok().flatten();
        }
    }
    crate::permission_checker::resolve_permission_level(Some(&access), settings.as_deref(), app.config().bot_owner_id.as_deref())
}

/// What a slash command's `execute` receives.
#[derive(Clone)]
pub struct CommandCtx {
    pub app: App,
    pub interaction: CommandInteraction,
    pub responder: Responder,
    /// The settings the router loaded for its gate (only when it needed them: a gated command or a level above
    /// MEMBER). Commands that need settings call `app.settings().get_settings(..)` (memoised).
    pub settings: Option<Settings>,
    /// Existing private channels `resolve_new_options` used instead of creating one (`reuseNote`).
    pub reused_private: Vec<ChannelId>,
}

impl std::ops::Deref for CommandCtx {
    type Target = Responder;
    fn deref(&self) -> &Responder {
        &self.responder
    }
}

impl CommandCtx {
    pub fn new(app: App, interaction: CommandInteraction, responder: Responder, settings: Option<Settings>) -> Self {
        Self { app, interaction, responder, settings, reused_private: Vec::new() }
    }

    pub fn guild_id(&self) -> Option<GuildId> {
        self.interaction.guild_id
    }

    /// `interaction.commandName`.
    pub fn command_name(&self) -> &str {
        &self.interaction.data.name
    }

    /// `options.getUser(name)`'s id (users, and members, of a user option).
    pub fn get_user_id(&self, name: &str) -> Option<serenity::all::UserId> {
        match self.option(name)? {
            CommandDataOptionValue::User(id) => Some(*id),
            _ => None,
        }
    }

    /// `options.getUser(name)`: the resolved user.
    pub fn get_user(&self, name: &str) -> Option<User> {
        let id = self.get_user_id(name)?;
        self.interaction.data.resolved.users.get(&id).cloned()
    }

    /// `options.getMember(name)`: the resolved (partial) member of a user option, completed with the user.
    pub fn get_member(&self, name: &str) -> Option<Member> {
        let id = self.get_user_id(name)?;
        let partial = self.interaction.data.resolved.members.get(&id)?.clone();
        let user = self.interaction.data.resolved.users.get(&id)?.clone();
        Some(member_from_partial(partial, user, self.interaction.guild_id))
    }

    /// `options.getChannel(name)`'s id.
    pub fn get_channel_id(&self, name: &str) -> Option<ChannelId> {
        match self.option(name)? {
            CommandDataOptionValue::Channel(id) => Some(*id),
            _ => None,
        }
    }

    /// `options.getChannel(name)`: the resolved (partial) channel. Read the full channel from the cache with its id.
    pub fn get_channel(&self, name: &str) -> Option<serenity::all::PartialChannel> {
        let id = self.get_channel_id(name)?;
        self.interaction.data.resolved.channels.get(&id).cloned()
    }

    /// `options.getRole(name)`'s id.
    pub fn get_role_id(&self, name: &str) -> Option<serenity::all::RoleId> {
        match self.option(name)? {
            CommandDataOptionValue::Role(id) => Some(*id),
            _ => None,
        }
    }

    /// `options.getRole(name)`: the resolved role.
    pub fn get_role(&self, name: &str) -> Option<serenity::all::Role> {
        let id = self.get_role_id(name)?;
        self.interaction.data.resolved.roles.get(&id).cloned()
    }

    /// `options.getMentionable(name)`'s id: a user or a role.
    pub fn get_mentionable_id(&self, name: &str) -> Option<serenity::all::GenericId> {
        match self.option(name)? {
            CommandDataOptionValue::Mentionable(id) => Some(*id),
            CommandDataOptionValue::User(id) => Some(serenity::all::GenericId::new(id.get())),
            CommandDataOptionValue::Role(id) => Some(serenity::all::GenericId::new(id.get())),
            _ => None,
        }
    }

    /// `options.getAttachment(name)`: the resolved attachment.
    pub fn get_attachment(&self, name: &str) -> Option<serenity::all::Attachment> {
        match self.option(name)? {
            CommandDataOptionValue::Attachment(id) => self.interaction.data.resolved.attachments.get(id).cloned(),
            _ => None,
        }
    }

    /// `options.get(name) !== null`: whether the leaf option was given.
    pub fn has_option(&self, name: &str) -> bool {
        self.option(name).is_some()
    }

    /// The leaf options (under the subcommand) as Discord sent them, mutable: `newTargets` substitutes a created
    /// role or channel here so every getter sees it as if it had been picked.
    pub fn leaf_options_mut(&mut self) -> &mut Vec<CommandDataOption> {
        let opts = &mut self.interaction.data.options;
        let depth = match opts.first().map(|o| &o.value) {
            Some(CommandDataOptionValue::SubCommandGroup(_)) => 2,
            Some(CommandDataOptionValue::SubCommand(_)) => 1,
            _ => 0,
        };
        if depth == 0 {
            return opts;
        }
        let nested = depth == 2
            && matches!(&opts[0].value, CommandDataOptionValue::SubCommandGroup(v) if matches!(v.first().map(|o| &o.value), Some(CommandDataOptionValue::SubCommand(_))));
        let inner = match &mut opts[0].value {
            CommandDataOptionValue::SubCommandGroup(v) | CommandDataOptionValue::SubCommand(v) => v,
            _ => unreachable!("depth checked above"),
        };
        if !nested {
            return inner;
        }
        match &mut inner[0].value {
            CommandDataOptionValue::SubCommand(leaf) => leaf,
            _ => unreachable!("nested checked above"),
        }
    }

    pub fn channel_id(&self) -> ChannelId {
        self.interaction.channel_id
    }

    pub fn user(&self) -> &User {
        &self.interaction.user
    }

    pub fn member(&self) -> Option<&Member> {
        self.interaction.member.as_deref()
    }

    /// `options.getSubcommandGroup(false)`.
    pub fn subcommand_group(&self) -> Option<String> {
        subcommand_path(&self.interaction.data.options).0
    }

    /// `options.getSubcommand(false)`.
    pub fn subcommand(&self) -> Option<String> {
        subcommand_path(&self.interaction.data.options).1
    }

    /// The options under the subcommand, resolved (users, members, roles, channels, attachments).
    pub fn options(&self) -> Vec<serenity::all::ResolvedOption<'_>> {
        let mut resolved = self.interaction.data.options();
        loop {
            match resolved.first().map(|o| &o.value) {
                Some(serenity::all::ResolvedValue::SubCommandGroup(inner)) | Some(serenity::all::ResolvedValue::SubCommand(inner)) => {
                    resolved = inner.clone();
                }
                _ => return resolved,
            }
        }
    }

    /// A leaf option's raw value by name.
    pub fn option(&self, name: &str) -> Option<&CommandDataOptionValue> {
        leaf_options(&self.interaction.data.options).iter().find(|o| o.name == name).map(|o| &o.value)
    }

    /// `options.getString(name)`.
    pub fn get_string(&self, name: &str) -> Option<String> {
        self.option(name).and_then(|v| v.as_str()).map(str::to_string)
    }

    /// `options.getInteger(name)`.
    pub fn get_integer(&self, name: &str) -> Option<i64> {
        self.option(name).and_then(CommandDataOptionValue::as_i64)
    }

    /// `options.getNumber(name)`.
    pub fn get_number(&self, name: &str) -> Option<f64> {
        self.option(name).and_then(CommandDataOptionValue::as_f64)
    }

    /// `options.getBoolean(name)`.
    pub fn get_boolean(&self, name: &str) -> Option<bool> {
        self.option(name).and_then(CommandDataOptionValue::as_bool)
    }

    /// The invoking member's permission view.
    pub fn member_access(&self) -> Option<MemberAccess> {
        member_access(&self.app, self.interaction.guild_id, self.member())
    }

    /// The invoking member's permission level (uses the router's settings when it loaded them).
    pub async fn permission_level(&self) -> bot_config::PermissionLevel {
        if let (Some(access), Some(s)) = (self.member_access(), &self.settings) {
            return crate::permission_checker::resolve_permission_level(Some(&access), Some(s), self.app.config().bot_owner_id.as_deref());
        }
        permission_level_of(&self.app, self.interaction.guild_id, self.member(), self.ack_budget_ms()).await
    }

    /// `ackBudgetMs(interaction)`.
    pub fn ack_budget_ms(&self) -> u64 {
        ack_budget_ms(self.interaction.id)
    }

    /// Whether the interaction's channel is age-restricted (`interaction.channel?.nsfw`), from the serenity cache: the
    /// channel's flag, or for a thread its parent's. `None` when the channel is not cached (serenity's
    /// `PartialChannel` in the interaction payload drops Discord's `nsfw` field, so it cannot be read from there).
    pub fn channel_nsfw(&self) -> Option<bool> {
        let g = self.interaction.guild_id?;
        let ch = crate::interaction_helpers::cached_channel(&self.app, g, self.interaction.channel_id)?;
        if crate::permissions::is_thread(&ch) {
            let parent = ch.parent_id?;
            return crate::interaction_helpers::cached_channel(&self.app, g, parent).map(|p| p.nsfw);
        }
        Some(ch.nsfw)
    }
}

/// What a command's `autocomplete` receives.
#[derive(Clone)]
pub struct AutocompleteCtx {
    pub app: App,
    pub interaction: CommandInteraction,
    pub responder: Responder,
}

impl std::ops::Deref for AutocompleteCtx {
    type Target = Responder;
    fn deref(&self) -> &Responder {
        &self.responder
    }
}

impl AutocompleteCtx {
    pub fn guild_id(&self) -> Option<GuildId> {
        self.interaction.guild_id
    }

    pub fn subcommand_group(&self) -> Option<String> {
        subcommand_path(&self.interaction.data.options).0
    }

    pub fn subcommand(&self) -> Option<String> {
        subcommand_path(&self.interaction.data.options).1
    }

    /// `options.getFocused(true)`: the focused option's name and typed text.
    pub fn focused(&self) -> Option<(String, String)> {
        self.interaction.data.autocomplete().map(|a| (a.name.to_string(), a.value.to_string()))
    }

    /// `options.getFocused()`: the typed text ('' when none).
    pub fn focused_value(&self) -> String {
        self.focused().map(|f| f.1).unwrap_or_default()
    }

    /// A leaf option's raw value by name (the focused one is serenity's `Autocomplete` variant).
    pub fn option(&self, name: &str) -> Option<&CommandDataOptionValue> {
        leaf_options(&self.interaction.data.options).iter().find(|o| o.name == name).map(|o| &o.value)
    }

    /// `options.getString(name)` inside an autocomplete: a non-focused option's value, or the focused option's text.
    pub fn get_string(&self, name: &str) -> Option<String> {
        match self.option(name)? {
            CommandDataOptionValue::Autocomplete { value, .. } => Some(value.clone()),
            v => v.as_str().map(str::to_string),
        }
    }

    /// `options.getInteger(name)` (the focused option's text parsed as an integer).
    pub fn get_integer(&self, name: &str) -> Option<i64> {
        match self.option(name)? {
            CommandDataOptionValue::Autocomplete { value, .. } => value.trim().parse().ok(),
            v => v.as_i64(),
        }
    }

    /// `options.getNumber(name)` (the focused option's text parsed as a number).
    pub fn get_number(&self, name: &str) -> Option<f64> {
        match self.option(name)? {
            CommandDataOptionValue::Autocomplete { value, .. } => value.trim().parse().ok(),
            v => v.as_f64(),
        }
    }

    /// `options.getBoolean(name)`.
    pub fn get_boolean(&self, name: &str) -> Option<bool> {
        self.option(name).and_then(CommandDataOptionValue::as_bool)
    }

    /// The id of a user option (autocomplete payloads carry no resolved data).
    pub fn get_user_id(&self, name: &str) -> Option<serenity::all::UserId> {
        match self.option(name)? {
            CommandDataOptionValue::User(id) => Some(*id),
            _ => None,
        }
    }

    /// The id of a channel option.
    pub fn get_channel_id(&self, name: &str) -> Option<ChannelId> {
        match self.option(name)? {
            CommandDataOptionValue::Channel(id) => Some(*id),
            _ => None,
        }
    }

    /// The id of a role option.
    pub fn get_role_id(&self, name: &str) -> Option<serenity::all::RoleId> {
        match self.option(name)? {
            CommandDataOptionValue::Role(id) => Some(*id),
            _ => None,
        }
    }

    pub fn user(&self) -> &User {
        &self.interaction.user
    }

    pub fn member(&self) -> Option<&Member> {
        self.interaction.member.as_deref()
    }

    pub fn channel_id(&self) -> ChannelId {
        self.interaction.channel_id
    }

    /// The invoking member's permission view.
    pub fn member_access(&self) -> Option<MemberAccess> {
        member_access(&self.app, self.interaction.guild_id, self.member())
    }

    /// The invoking member's permission level (settings from the memo, else a load bounded by the ack budget).
    pub async fn permission_level(&self) -> bot_config::PermissionLevel {
        permission_level_of(&self.app, self.interaction.guild_id, self.member(), self.ack_budget_ms()).await
    }

    /// `ackBudgetMs(interaction)`.
    pub fn ack_budget_ms(&self) -> u64 {
        ack_budget_ms(self.interaction.id)
    }
}

/// Which registry a component interaction is routed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentKind {
    Button,
    /// Every select menu kind (string, user, role, mentionable, channel).
    Select,
    Modal,
}

impl ComponentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Button => "button",
            Self::Select => "select",
            Self::Modal => "modal",
        }
    }

    /// The registry's component type.
    pub fn component_type(self) -> bot_config::ComponentType {
        match self {
            Self::Button => bot_config::ComponentType::Button,
            Self::Select => bot_config::ComponentType::Select,
            Self::Modal => bot_config::ComponentType::Modal,
        }
    }
}

/// What a button or select handler (and a collector) receives.
#[derive(Clone)]
pub struct ComponentCtx {
    pub app: App,
    pub interaction: ComponentInteraction,
    pub responder: Responder,
    pub kind: ComponentKind,
}

impl std::ops::Deref for ComponentCtx {
    type Target = Responder;
    fn deref(&self) -> &Responder {
        &self.responder
    }
}

impl ComponentCtx {
    pub fn custom_id(&self) -> &str {
        &self.interaction.data.custom_id
    }

    /// `parseCustomId(customId)`: `(prefix, params)`.
    pub fn parsed(&self) -> (String, Vec<String>) {
        let p = bot_config::parse_custom_id(&self.interaction.data.custom_id);
        (p.prefix.to_string(), p.params.iter().map(|s| s.to_string()).collect())
    }

    pub fn guild_id(&self) -> Option<GuildId> {
        self.interaction.guild_id
    }

    pub fn user(&self) -> &User {
        &self.interaction.user
    }

    pub fn member(&self) -> Option<&Member> {
        self.interaction.member.as_ref()
    }

    /// The selected values of a string select (empty for buttons and other selects).
    pub fn values(&self) -> Vec<String> {
        match &self.interaction.data.kind {
            serenity::all::ComponentInteractionDataKind::StringSelect { values } => values.clone(),
            serenity::all::ComponentInteractionDataKind::UserSelect { values } => values.iter().map(|v| v.to_string()).collect(),
            serenity::all::ComponentInteractionDataKind::RoleSelect { values } => values.iter().map(|v| v.to_string()).collect(),
            serenity::all::ComponentInteractionDataKind::ChannelSelect { values } => values.iter().map(|v| v.to_string()).collect(),
            serenity::all::ComponentInteractionDataKind::MentionableSelect { values } => values.iter().map(|v| v.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    /// The invoking member's permission view (persistent components re-check levels themselves).
    pub fn member_access(&self) -> Option<MemberAccess> {
        member_access(&self.app, self.interaction.guild_id, self.member())
    }

    /// The invoking member's permission level (persistent components re-check levels with it).
    pub async fn permission_level(&self) -> bot_config::PermissionLevel {
        permission_level_of(&self.app, self.interaction.guild_id, self.member(), self.ack_budget_ms()).await
    }

    pub fn ack_budget_ms(&self) -> u64 {
        ack_budget_ms(self.interaction.id)
    }
}

/// What a modal submit handler receives.
#[derive(Clone)]
pub struct ModalCtx {
    pub app: App,
    pub interaction: ModalInteraction,
    pub responder: Responder,
}

impl std::ops::Deref for ModalCtx {
    type Target = Responder;
    fn deref(&self) -> &Responder {
        &self.responder
    }
}

impl ModalCtx {
    pub fn custom_id(&self) -> &str {
        &self.interaction.data.custom_id
    }

    /// `parseCustomId(customId)`: `(prefix, params)`.
    pub fn parsed(&self) -> (String, Vec<String>) {
        let p = bot_config::parse_custom_id(&self.interaction.data.custom_id);
        (p.prefix.to_string(), p.params.iter().map(|s| s.to_string()).collect())
    }

    pub fn guild_id(&self) -> Option<GuildId> {
        self.interaction.guild_id
    }

    pub fn user(&self) -> &User {
        &self.interaction.user
    }

    pub fn member(&self) -> Option<&Member> {
        self.interaction.member.as_ref()
    }

    /// `fields.getTextInputValue(id)`: the submitted text of an input (`None` when the modal has no such input).
    pub fn value(&self, field_id: &str) -> Option<String> {
        self.interaction.data.components.iter().flat_map(|row| row.components.iter()).find_map(|c| match c {
            ActionRowComponent::InputText(t) if t.custom_id == field_id => Some(t.value.clone().unwrap_or_default()),
            _ => None,
        })
    }

    pub fn member_access(&self) -> Option<MemberAccess> {
        member_access(&self.app, self.interaction.guild_id, self.member())
    }

    /// The invoking member's permission level.
    pub async fn permission_level(&self) -> bot_config::PermissionLevel {
        permission_level_of(&self.app, self.interaction.guild_id, self.member(), self.ack_budget_ms()).await
    }

    pub fn ack_budget_ms(&self) -> u64 {
        ack_budget_ms(self.interaction.id)
    }
}
