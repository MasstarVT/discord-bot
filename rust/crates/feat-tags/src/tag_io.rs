//! The production [`TagIo`]: the custom-command executor's view of Discord (the serenity cache for the checks, REST
//! for the writes, and the interaction's [`Responder`] for `/tag use`). The JS executor works on discord.js objects
//! directly; this file is the bridge that lets [`crate::tag_service::run_tag`] stay testable offline.

use bot_core::App;
use bot_core::Responder;
use bot_core::interaction::InteractionError;
use bot_core::interaction_helpers::{
    ChannelUsable, Payload, PermTarget, RoleCheck, check_assignable_role, check_channel_usable, is_interaction_gone, missing_bot_permissions,
    pingable_role_ids, safe_defer, safe_reply,
};
use serenity::all::{ChannelId, Guild, GuildId, MessageId, RoleId, UserId};
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateMessage};

use crate::tag_service::{IoError, NameLookup, NoNames, TagIo};

fn id_of(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

fn serenity_err(e: &serenity::Error) -> IoError {
    IoError { code: bot_core::discord::error_code(e), interaction_gone: false, message: bot_core::discord::error_message(e) }
}

fn interaction_err(e: &InteractionError) -> IoError {
    IoError { code: e.code(), interaction_gone: is_interaction_gone(e), message: e.to_string() }
}

fn no_http() -> IoError {
    IoError { code: None, interaction_gone: false, message: "not connected to Discord".into() }
}

fn bad_id() -> IoError {
    IoError { code: Some(10003), interaction_gone: false, message: "invalid id".into() }
}

/// The executor's Discord for one invocation in one guild: a snapshot of the cached guild, REST through
/// `app.http()`, and (for `/tag use`) the interaction's responder.
pub struct DiscordIo {
    app: App,
    guild_id: GuildId,
    /// The cached guild, snapshotted on first use (a run that never asks costs no copy).
    snapshot: std::sync::OnceLock<Option<Guild>>,
    bot_id: Option<UserId>,
    responder: Option<Responder>,
}

impl DiscordIo {
    pub fn new(app: &App, guild_id: GuildId, responder: Option<Responder>) -> Self {
        let bot_id = app.discord().map(|d| d.cache.current_user().id);
        Self { app: app.clone(), guild_id, snapshot: std::sync::OnceLock::new(), bot_id, responder }
    }

    fn guild(&self) -> Option<&Guild> {
        self.snapshot.get_or_init(|| bot_core::interaction_helpers::cached_guild(&self.app, self.guild_id)).as_ref()
    }

    fn channel(&self, id: &str) -> Option<&serenity::all::GuildChannel> {
        let g = self.guild()?;
        bot_core::permissions::find_channel(g, ChannelId::new(id_of(id)?))
    }
}

impl TagIo for DiscordIo {
    fn channel_exists(&self, channel_id: &str) -> bool {
        self.channel(channel_id).is_some()
    }

    fn channel_usable(&self, channel_id: &str, needs: &[&str]) -> ChannelUsable {
        check_channel_usable(&self.app, self.guild_id, id_of(channel_id).map(ChannelId::new), needs)
    }

    fn check_assignable_role(&self, role_id: &str) -> RoleCheck {
        let (Some(g), Some(bot)) = (self.guild(), self.bot_id) else {
            return RoleCheck { ok: false, reason: Some("Role not found.".into()), dangerous: Vec::new() };
        };
        let role = id_of(role_id).and_then(|v| g.roles.get(&RoleId::new(v)));
        check_assignable_role(g, role, bot, None, false)
    }

    fn role_exists(&self, role_id: &str) -> bool {
        self.guild().is_some_and(|g| crate::tag_service::guild_has_role(g, role_id))
    }

    fn pingable_by_bot(&self, channel_id: &str, role_ids: &[String]) -> Vec<String> {
        let (Some(g), Some(bot)) = (self.guild(), self.bot_id) else { return Vec::new() };
        let Some(channel) = self.channel(channel_id) else { return Vec::new() };
        let me = bot_core::permissions::me(g, bot);
        let executor = me.map(|m| (bot, m.roles.as_slice()));
        pingable_role_ids(g, channel, executor, role_ids).allowed
    }

    fn bot_can_manage_messages(&self, channel_id: &str) -> bool {
        let Some(c) = id_of(channel_id) else { return false };
        missing_bot_permissions(&self.app, PermTarget::Channel(self.guild_id, ChannelId::new(c)), &["ManageMessages"]).is_empty()
    }

    fn names(&self) -> &dyn NameLookup {
        match self.guild() {
            Some(g) => g,
            None => &NoNames,
        }
    }

    async fn fetch_member_roles(&self, user_id: &str) -> Option<Vec<String>> {
        let uid = UserId::new(id_of(user_id)?);
        let g = self.guild_id.to_string();
        if let Some(m) = self.guild().and_then(|guild| guild.members.get(&uid)) {
            return Some(crate::tag_service::member_role_ids(self.guild(), &g, &m.roles));
        }
        let http = self.app.http()?;
        let m = self.guild_id.member(http.as_ref(), uid).await.ok()?;
        Some(crate::tag_service::member_role_ids(self.guild(), &g, &m.roles))
    }

    async fn add_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), IoError> {
        let http = self.app.http().ok_or_else(no_http)?;
        let (Some(u), Some(r)) = (id_of(user_id), id_of(role_id)) else { return Err(bad_id()) };
        http.add_member_role(self.guild_id, UserId::new(u), RoleId::new(r), Some(reason)).await.map_err(|e| serenity_err(&e))
    }

    async fn remove_role(&self, user_id: &str, role_id: &str, reason: &str) -> Result<(), IoError> {
        let http = self.app.http().ok_or_else(no_http)?;
        let (Some(u), Some(r)) = (id_of(user_id), id_of(role_id)) else { return Err(bad_id()) };
        http.remove_member_role(self.guild_id, UserId::new(u), RoleId::new(r), Some(reason)).await.map_err(|e| serenity_err(&e))
    }

    async fn send_channel(&self, channel_id: &str, payload: Payload) -> Result<(), IoError> {
        let http = self.app.http().ok_or_else(no_http)?;
        let Some(c) = id_of(channel_id) else { return Err(bad_id()) };
        ChannelId::new(c).send_message(http.as_ref(), payload.to_message()).await.map(|_| ()).map_err(|e| serenity_err(&e))
    }

    async fn send_dm(&self, user_id: &str, payload: Payload) -> Result<(), IoError> {
        let http = self.app.http().ok_or_else(no_http)?;
        let Some(u) = id_of(user_id) else { return Err(bad_id()) };
        UserId::new(u).direct_message(http.as_ref(), payload.to_message()).await.map(|_| ()).map_err(|e| serenity_err(&e))
    }

    async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<(), IoError> {
        let http = self.app.http().ok_or_else(no_http)?;
        let (Some(c), Some(m)) = (id_of(channel_id), id_of(message_id)) else { return Err(bad_id()) };
        http.delete_message(ChannelId::new(c), MessageId::new(m), None).await.map_err(|e| serenity_err(&e))
    }

    async fn reply_notice(&self, channel_id: &str, message_id: &str, embed: CreateEmbed) -> Result<String, IoError> {
        let http = self.app.http().ok_or_else(no_http)?;
        let (Some(c), Some(m)) = (id_of(channel_id), id_of(message_id)) else { return Err(bad_id()) };
        let msg = CreateMessage::new()
            .embed(embed)
            .reference_message((ChannelId::new(c), MessageId::new(m)))
            .allowed_mentions(CreateAllowedMentions::new().replied_user(false));
        ChannelId::new(c).send_message(http.as_ref(), msg).await.map(|sent| sent.id.to_string()).map_err(|e| serenity_err(&e))
    }

    fn schedule_delete(&self, channel_id: &str, message_id: &str, after_ms: u64) {
        let (Some(c), Some(m)) = (id_of(channel_id), id_of(message_id)) else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let app = self.app.clone();
        rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(after_ms)).await;
            bot_core::deletion_tracker::mark_bot_deletion(&app, &[m.to_string()], Some("custom-command"), None);
            if let Some(http) = app.http() {
                let _ = http.delete_message(ChannelId::new(c), MessageId::new(m), None).await;
            }
        });
    }

    async fn interaction_reply(&self, payload: Payload) -> Result<(), IoError> {
        let r = self.responder.as_ref().ok_or_else(no_http)?;
        r.reply(payload.to_reply()).await.map_err(|e| interaction_err(&e))
    }

    async fn interaction_follow_up(&self, payload: Payload) -> Result<(), IoError> {
        let r = self.responder.as_ref().ok_or_else(no_http)?;
        r.follow_up(payload.to_follow_up()).await.map(|_| ()).map_err(|e| interaction_err(&e))
    }

    async fn interaction_edit_reply(&self, payload: Payload) -> Result<(), IoError> {
        let r = self.responder.as_ref().ok_or_else(no_http)?;
        r.edit_reply(payload.to_edit()).await.map(|_| ()).map_err(|e| interaction_err(&e))
    }

    async fn interaction_defer_ephemeral(&self) -> bool {
        match &self.responder {
            Some(r) => safe_defer(r, true).await,
            None => false,
        }
    }

    async fn interaction_safe_reply(&self, payload: Payload) {
        if let Some(r) = &self.responder {
            safe_reply(r, payload).await;
        }
    }
}
