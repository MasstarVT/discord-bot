//! A recording fake of the Discord operations (the JS tests' fake guild, channel, message and member objects).
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_core::interaction_helpers::RoleCheck;
use feat_reactionroles::reaction_role_service::{ChannelInfo, DErr, DResult, EmojiInfo, MessageInfo, ReactionInfo, RoleInfo, RrDiscord};
use serde_json::Value;
use serenity::all::Member;

pub const BOT: &str = "600000000000000001";

#[derive(Default)]
pub struct World {
    pub available: bool,
    pub manage_roles: bool,
    pub roles: Vec<RoleInfo>,
    pub dangerous: HashSet<String>,
    pub role_fetch_error: bool,
    /// Roles REST knows but the cache does not.
    pub rest_only_roles: HashSet<String>,
    pub members: HashMap<String, HashSet<String>>,
    pub member_fetch_error: bool,
    pub fail_role: HashMap<String, i64>,
    /// (op, user, role, reason)
    pub calls: Vec<(String, String, String, String)>,
    pub channels: HashMap<String, ChannelInfo>,
    pub channel_fetch_error: Option<Option<i64>>,
    pub no_manage_messages: HashSet<String>,
    pub messages: HashMap<String, MessageInfo>,
    pub edits: Vec<(String, Value)>,
    pub edit_error: Option<i64>,
    pub react_error: Option<i64>,
    pub send_error: Option<i64>,
    pub sent: Vec<(String, Value)>,
    pub deleted: Vec<String>,
    pub reacted: Vec<(String, String)>,
    /// (message, key, user)
    pub removed_user: Vec<(String, String, String)>,
    /// (message, key)
    pub removed_all: Vec<(String, String)>,
    pub created_roles: Vec<Value>,
    pub create_fail: Option<String>,
    pub guild_emojis: Vec<EmojiInfo>,
    pub client_emojis: Vec<EmojiInfo>,
    pub next_id: u64,
}

#[derive(Clone, Default)]
pub struct FakeOps(pub Arc<Mutex<World>>);

impl FakeOps {
    pub fn new() -> Self {
        let w = World { available: true, manage_roles: true, next_id: 1, ..Default::default() };
        Self(Arc::new(Mutex::new(w)))
    }

    pub fn w(&self) -> std::sync::MutexGuard<'_, World> {
        self.0.lock().unwrap()
    }

    pub fn role(&self, id: &str, name: &str, position: i64) -> &Self {
        self.w().roles.push(RoleInfo { id: id.into(), name: name.into(), position, managed: false, editable: position < 50 });
        self
    }

    pub fn channel(&self, id: &str, name: &str) -> &Self {
        self.w().channels.insert(id.into(), ChannelInfo { id: id.into(), name: name.into(), text_based: true });
        self
    }

    pub fn message(&self, guild: &str, channel: &str, id: &str) -> &Self {
        self.w().messages.insert(
            id.into(),
            MessageInfo { id: id.into(), channel_id: channel.into(), guild_id: Some(guild.into()), content: String::new(), first_embed_title: None, reactions: Vec::new() },
        );
        self
    }

    pub fn member(&self, user: &str, roles: &[&str]) -> &Self {
        self.w().members.insert(user.into(), roles.iter().map(|r| r.to_string()).collect());
        self
    }

    pub fn member_roles_now(&self, user: &str) -> HashSet<String> {
        self.w().members.get(user).cloned().unwrap_or_default()
    }

    pub fn add_reaction(&self, message: &str, stored: &str, me: bool) {
        let mut w = self.w();
        if let Some(m) = w.messages.get_mut(message) {
            m.reactions.push(ReactionInfo::of_stored(stored, me));
        }
    }

    pub fn role_calls(&self) -> Vec<(String, String)> {
        self.w().calls.iter().map(|(op, _, r, _)| (op.clone(), r.clone())).collect()
    }
}

#[async_trait]
impl RrDiscord for FakeOps {
    fn bot_id(&self) -> Option<String> {
        Some(BOT.into())
    }
    fn guild_available(&self, _g: &str) -> bool {
        self.w().available
    }
    fn cached_role(&self, _g: &str, role_id: &str) -> Option<RoleInfo> {
        self.w().roles.iter().find(|r| r.id == role_id).cloned()
    }
    fn guild_roles(&self, _g: &str) -> Vec<RoleInfo> {
        self.w().roles.clone()
    }
    fn bot_can_manage_roles(&self, _g: &str) -> bool {
        self.w().manage_roles
    }
    fn check_assignable(&self, guild_id: &str, role_id: &str, _executor: Option<&Member>) -> RoleCheck {
        let w = self.w();
        let fail = |r: &str| RoleCheck { ok: false, reason: Some(r.into()), dangerous: Vec::new() };
        let Some(role) = w.roles.iter().find(|r| r.id == role_id) else { return fail("Role not found.") };
        if role.id == guild_id {
            return fail("@everyone cannot be assigned.");
        }
        if role.managed {
            return fail("That role is managed by an integration.");
        }
        if !w.manage_roles {
            return fail("I need the Manage Roles permission.");
        }
        if role.position >= 50 {
            return fail("That role is above my highest role.");
        }
        if w.dangerous.contains(role_id) {
            return RoleCheck { ok: false, reason: Some("That role has dangerous permissions: Administrator.".into()), dangerous: vec!["Administrator"] };
        }
        RoleCheck { ok: true, reason: None, dangerous: Vec::new() }
    }
    async fn fetch_role_exists(&self, _g: &str, role_id: &str) -> DResult<bool> {
        let w = self.w();
        if w.role_fetch_error {
            return Err(DErr::other("network down"));
        }
        Ok(w.roles.iter().any(|r| r.id == role_id) || w.rest_only_roles.contains(role_id))
    }
    async fn create_role(&self, _g: &str, body: &Value, _reason: &str) -> DResult<RoleInfo> {
        let mut w = self.w();
        let name = body["name"].as_str().unwrap_or_default().to_string();
        if w.create_fail.as_deref() == Some(name.as_str()) {
            return Err(DErr::other("Missing Permissions"));
        }
        w.created_roles.push(body.clone());
        w.next_id += 1;
        let id = format!("9100000000000{:05}", w.next_id);
        let r = RoleInfo { id, name, position: 1, managed: false, editable: true };
        w.roles.push(r.clone());
        Ok(r)
    }
    async fn member_roles(&self, _g: &str, user_id: &str) -> DResult<Vec<String>> {
        let w = self.w();
        if w.member_fetch_error {
            return Err(DErr::other("fetch failed"));
        }
        w.members.get(user_id).map(|s| s.iter().cloned().collect()).ok_or(DErr::code(10007))
    }
    async fn add_role(&self, _g: &str, user_id: &str, role_id: &str, reason: &str) -> DResult<()> {
        let mut w = self.w();
        if let Some(c) = w.fail_role.get(role_id) {
            return Err(DErr::code(*c));
        }
        w.calls.push(("add".into(), user_id.into(), role_id.into(), reason.into()));
        w.members.entry(user_id.into()).or_default().insert(role_id.into());
        Ok(())
    }
    async fn remove_role(&self, _g: &str, user_id: &str, role_id: &str, reason: &str) -> DResult<()> {
        let mut w = self.w();
        if let Some(c) = w.fail_role.get(role_id) {
            return Err(DErr::code(*c));
        }
        w.calls.push(("remove".into(), user_id.into(), role_id.into(), reason.into()));
        w.members.entry(user_id.into()).or_default().remove(role_id);
        Ok(())
    }
    fn cached_channel(&self, _g: &str, channel_id: &str) -> Option<ChannelInfo> {
        self.w().channels.get(channel_id).cloned()
    }
    async fn fetch_channel(&self, _g: &str, channel_id: &str) -> DResult<ChannelInfo> {
        let w = self.w();
        if let Some(e) = w.channel_fetch_error {
            return Err(match e {
                Some(c) => DErr::code(c),
                None => DErr::other("socket hang up"),
            });
        }
        w.channels.get(channel_id).cloned().ok_or(DErr::code(10003))
    }
    fn bot_has_manage_messages(&self, _g: &str, channel_id: &str) -> bool {
        let w = self.w();
        w.channels.contains_key(channel_id) && !w.no_manage_messages.contains(channel_id)
    }
    async fn fetch_message(&self, _g: &str, channel_id: &str, message_id: &str) -> DResult<MessageInfo> {
        self.w().messages.get(message_id).filter(|m| m.channel_id == channel_id).cloned().ok_or(DErr::code(10008))
    }
    async fn send_message(&self, guild_id: &str, channel_id: &str, body: &Value) -> DResult<MessageInfo> {
        let mut w = self.w();
        if let Some(c) = w.send_error {
            return Err(DErr::code(c));
        }
        w.next_id += 1;
        let id = format!("5{:017}", 900_000 + w.next_id);
        let m = MessageInfo {
            id: id.clone(),
            channel_id: channel_id.into(),
            guild_id: Some(guild_id.into()),
            content: body["content"].as_str().unwrap_or_default().into(),
            first_embed_title: None,
            reactions: Vec::new(),
        };
        w.sent.push((channel_id.into(), body.clone()));
        w.messages.insert(id, m.clone());
        Ok(m)
    }
    async fn edit_message(&self, _c: &str, message_id: &str, body: &Value) -> DResult<()> {
        let mut w = self.w();
        if let Some(c) = w.edit_error {
            return Err(DErr::code(c));
        }
        if !w.messages.contains_key(message_id) {
            return Err(DErr::code(10008));
        }
        w.edits.push((message_id.into(), body.clone()));
        Ok(())
    }
    async fn delete_message(&self, _c: &str, message_id: &str) -> DResult<()> {
        let mut w = self.w();
        if w.messages.remove(message_id).is_none() {
            return Err(DErr::code(10008));
        }
        w.deleted.push(message_id.into());
        Ok(())
    }
    async fn react(&self, _c: &str, message_id: &str, stored: &str) -> DResult<()> {
        let mut w = self.w();
        if let Some(c) = w.react_error {
            return Err(DErr::code(c));
        }
        w.reacted.push((message_id.into(), stored.into()));
        if let Some(m) = w.messages.get_mut(message_id) {
            let r = ReactionInfo::of_stored(stored, true);
            match m.reactions.iter_mut().find(|x| x.key() == r.key()) {
                Some(x) => x.me = true,
                None => m.reactions.push(r),
            }
        }
        Ok(())
    }
    async fn remove_user_reaction(&self, _c: &str, message_id: &str, reaction: &ReactionInfo, user_id: &str) -> DResult<()> {
        let mut w = self.w();
        w.removed_user.push((message_id.into(), reaction.key(), user_id.into()));
        if user_id == BOT
            && let Some(m) = w.messages.get_mut(message_id)
            && let Some(x) = m.reactions.iter_mut().find(|x| x.key() == reaction.key())
        {
            x.me = false;
        }
        Ok(())
    }
    async fn remove_emoji_reactions(&self, _c: &str, message_id: &str, reaction: &ReactionInfo) -> DResult<()> {
        let mut w = self.w();
        w.removed_all.push((message_id.into(), reaction.key()));
        if let Some(m) = w.messages.get_mut(message_id) {
            m.reactions.retain(|x| x.key() != reaction.key());
        }
        Ok(())
    }
    fn guild_emojis(&self, _g: &str) -> Vec<EmojiInfo> {
        self.w().guild_emojis.clone()
    }
    fn client_emoji(&self, emoji_id: &str) -> Option<EmojiInfo> {
        self.w().client_emojis.iter().find(|e| e.id == emoji_id).cloned()
    }
}
