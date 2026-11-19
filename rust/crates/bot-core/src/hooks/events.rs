//! The data each hook point carries: one typed payload per entry of `HOOK_ORDER`, with exactly the fields the JS event
//! files (`src/events/*.js`), `runGuildReady`, `xpService` / `/levelconfig` (`levelUp`) and `/shop buy`
//! (`economyPurchase`) pass to `runHooks`.
//!
//! Objects the JS passes as discord.js structures are serenity models here. Where the JS object may be partial (an
//! uncached message, a removed member) the Rust field is an `Option` beside the ids that are always known. The
//! `channel` of the JS is a `channel_id` plus the cached channel when part B had it. `client` and `guild` are not
//! payload fields: every hook gets the [`crate::App`] and the guild id in its [`super::HookCtx`] (read the guild from
//! `app.discord()`'s cache).
//!
//! Part B's serenity event handlers build these and call `app.hooks().run(payload)`; features emit the internal
//! points (`LevelUp`, `EconomyPurchase`) the same way.

use std::sync::Arc;

use bot_config::HookPoint;
use bot_db::ShopItem;
use serenity::all::{
    ChannelId, Emoji, Guild, GuildChannel, GuildId, InviteCreateEvent, InviteDeleteEvent, Member, Message, MessageId, MessageReaction,
    PartialGuild, Presence, Reaction, Role, RoleId, ScheduledEvent, User, UserId, VoiceState,
};
use tokio::sync::OnceCell;

use crate::context::App;

/// A hook point's payload.
pub trait HookEvent: Clone + Send + Sync + 'static {
    /// The hook point this payload is dispatched on.
    const POINT: HookPoint;
    /// The guild of the dispatch (`ctx.guildId`), `None` for `ready` and `directMessage`.
    fn guild_id(&self) -> Option<GuildId>;
}

macro_rules! hook_event {
    ($ty:ident, $point:ident) => {
        impl HookEvent for $ty {
            const POINT: HookPoint = HookPoint::$point;
            fn guild_id(&self) -> Option<GuildId> {
                Some(self.guild_id)
            }
        }
    };
    ($ty:ident, $point:ident, none) => {
        impl HookEvent for $ty {
            const POINT: HookPoint = HookPoint::$point;
            fn guild_id(&self) -> Option<GuildId> {
                None
            }
        }
    };
}

macro_rules! hook_event_inner {
    ($ty:ident, $point:ident) => {
        impl HookEvent for $ty {
            const POINT: HookPoint = HookPoint::$point;
            fn guild_id(&self) -> Option<GuildId> {
                Some(self.0.guild_id)
            }
        }
        impl std::ops::Deref for $ty {
            type Target = ReactionEvent;
            fn deref(&self) -> &ReactionEvent {
                &self.0
            }
        }
    };
}


/// A value fetched at most once per dispatch, shared by every hook (`memo(fn)` in the reaction event files). It can
/// be pre-filled (tests, or when part B already has the object).
#[derive(Clone, Default)]
pub struct Lazy<T: Clone + Send + Sync + 'static>(Arc<OnceCell<Option<T>>>);

impl<T: Clone + Send + Sync + 'static> std::fmt::Debug for Lazy<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.initialized() { "Lazy(resolved)" } else { "Lazy(pending)" })
    }
}

impl<T: Clone + Send + Sync + 'static> Lazy<T> {
    pub fn new() -> Self {
        Self(Arc::new(OnceCell::new()))
    }

    /// Already resolved to `value`.
    pub fn resolved(value: Option<T>) -> Self {
        Self(Arc::new(OnceCell::new_with(Some(value))))
    }

    /// The value, fetching it with `fetch` the first time.
    pub async fn get_or_fetch<F, Fut>(&self, fetch: F) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<T>>,
    {
        self.0.get_or_init(fetch).await.clone()
    }
}

async fn fetch_message(app: &App, channel_id: ChannelId, message_id: MessageId) -> Option<Message> {
    let d = app.discord()?;
    channel_id.message((&d.cache, d.http.as_ref()), message_id).await.ok()
}

async fn fetch_member(app: &App, guild_id: GuildId, user_id: UserId) -> Option<Member> {
    let d = app.discord()?;
    guild_id.member((&d.cache, d.http.as_ref()), user_id).await.ok()
}

/// `ready`: the client is ready (no guild).
#[derive(Debug, Clone, Default)]
pub struct Ready {}
hook_event!(Ready, Ready, none);

/// Why `guildReady` runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuildReadyReason {
    /// `ready.js`, for every available guild.
    Startup,
    /// `guildCreate.js`: the bot joined.
    Join,
    /// `guildAvailable.js`: a guild came back (hooks treat it like `Startup`).
    Available,
}

impl GuildReadyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Join => "join",
            Self::Available => "available",
        }
    }
}

/// `guildReady` (synthetic, from [`crate::hooks::HookRegistry::run_guild_ready`]): `{ guild, reason }`.
#[derive(Debug, Clone)]
pub struct GuildReady {
    pub guild_id: GuildId,
    pub reason: GuildReadyReason,
}
hook_event!(GuildReady, GuildReady);

/// `guildDelete`: `{ guild }` (the guild as it was cached).
#[derive(Debug, Clone)]
pub struct GuildDelete {
    pub guild_id: GuildId,
    pub guild: Option<Guild>,
}
hook_event!(GuildDelete, GuildDelete);

/// `messageCreate`: a real user message in a guild (no bots, webhooks or system messages): `{ message, member,
/// channel }`.
#[derive(Debug, Clone)]
pub struct MessageCreate {
    pub guild_id: GuildId,
    pub message: Message,
    pub member: Option<Member>,
    pub channel_id: ChannelId,
    pub channel: Option<GuildChannel>,
}
hook_event!(MessageCreate, MessageCreate);

/// `directMessage`: a DM to the bot: `{ guild: null, message, channel, user }`.
#[derive(Debug, Clone)]
pub struct DirectMessage {
    pub message: Message,
    pub channel_id: ChannelId,
    pub user: User,
}
hook_event!(DirectMessage, DirectMessage, none);

/// `botMessage`: a guild message by another bot or a webhook (also their edits, with `edited` and `wasLoading`).
#[derive(Debug, Clone)]
pub struct BotMessage {
    pub guild_id: GuildId,
    pub message: Message,
    pub channel_id: ChannelId,
    pub channel: Option<GuildChannel>,
    /// `true` when this is an edit (`messageUpdate`), `false` for a new message.
    pub edited: bool,
    /// For an edit: the old message was a "thinking" placeholder; `None` when it was not cached (or for a new message).
    pub was_loading: Option<bool>,
}
hook_event!(BotMessage, BotMessage);

/// `ownMessage`: the bot's own guild message (never edits): `{ message, channel }`.
#[derive(Debug, Clone)]
pub struct OwnMessage {
    pub guild_id: GuildId,
    pub message: Message,
    pub channel_id: ChannelId,
    pub channel: Option<GuildChannel>,
}
hook_event!(OwnMessage, OwnMessage);

/// `messageUpdate`: a real edit of a user message: `{ oldMessage, newMessage, message, oldKnown, member, channel }`.
#[derive(Debug, Clone)]
pub struct MessageUpdate {
    pub guild_id: GuildId,
    /// The old message when it was cached (`oldKnown`).
    pub old_message: Option<Message>,
    /// The new message (`message` in the JS too).
    pub message: Message,
    pub old_known: bool,
    pub member: Option<Member>,
    pub channel_id: ChannelId,
    pub channel: Option<GuildChannel>,
}
hook_event!(MessageUpdate, MessageUpdate);

/// `messageDelete`: `{ message, channel, partial }` (bot messages included).
#[derive(Debug, Clone)]
pub struct MessageDelete {
    pub guild_id: GuildId,
    pub message_id: MessageId,
    pub channel_id: ChannelId,
    /// The message as it was cached; `None` when it was not (`partial`).
    pub message: Option<Message>,
    pub partial: bool,
    pub channel: Option<GuildChannel>,
}
hook_event!(MessageDelete, MessageDelete);

/// `messageDeleteBulk`: `{ messages, channel }`.
#[derive(Debug, Clone)]
pub struct MessageDeleteBulk {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub message_ids: Vec<MessageId>,
    /// The deleted messages that were cached.
    pub messages: Vec<Message>,
    pub channel: Option<GuildChannel>,
}
hook_event!(MessageDeleteBulk, MessageDeleteBulk);

/// `messageReactionAdd` / `messageReactionRemove`: `{ reaction, user, message, emojiKey, ensureReaction(),
/// ensureMessage(), getMember() }` (bots and the bot itself filtered out; the user is fetched when partial).
#[derive(Debug, Clone)]
pub struct ReactionEvent {
    pub guild_id: GuildId,
    pub reaction: Reaction,
    pub user: User,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    /// `reaction.emoji.id ?? reaction.emoji.name`.
    pub emoji_key: String,
    pub message: Lazy<Message>,
    pub member: Lazy<Member>,
}

impl ReactionEvent {
    pub fn new(guild_id: GuildId, reaction: Reaction, user: User) -> Self {
        Self {
            guild_id,
            emoji_key: emoji_key(&reaction.emoji),
            channel_id: reaction.channel_id,
            message_id: reaction.message_id,
            reaction,
            user,
            message: Lazy::new(),
            member: Lazy::new(),
        }
    }

    /// `ensureReaction()`: serenity always delivers a complete reaction.
    pub fn ensure_reaction(&self) -> Option<Reaction> {
        Some(self.reaction.clone())
    }

    /// `ensureMessage()`: the reacted message (cache, then REST), fetched once per dispatch; `None` when it cannot be
    /// fetched.
    pub async fn ensure_message(&self, app: &App) -> Option<Message> {
        let (c, m) = (self.channel_id, self.message_id);
        self.message.get_or_fetch(|| fetch_message(app, c, m)).await
    }

    /// `getMember()`: the reacting member (cache, then REST), fetched once per dispatch.
    pub async fn get_member(&self, app: &App) -> Option<Member> {
        let (g, u) = (self.guild_id, self.user.id);
        self.member.get_or_fetch(|| fetch_member(app, g, u)).await
    }
}

/// `messageReactionAdd`.
#[derive(Debug, Clone)]
pub struct MessageReactionAdd(pub ReactionEvent);
hook_event_inner!(MessageReactionAdd, MessageReactionAdd);

/// `messageReactionRemove`.
#[derive(Debug, Clone)]
pub struct MessageReactionRemove(pub ReactionEvent);
hook_event_inner!(MessageReactionRemove, MessageReactionRemove);



/// `reaction.emoji.id ?? reaction.emoji.name`.
pub fn emoji_key(emoji: &serenity::all::ReactionType) -> String {
    match emoji {
        serenity::all::ReactionType::Custom { id, .. } => id.to_string(),
        serenity::all::ReactionType::Unicode(name) => name.clone(),
        _ => String::new(),
    }
}

/// `messageReactionRemoveAll`: `{ message, reactions, ensureMessage() }`.
#[derive(Debug, Clone)]
pub struct MessageReactionRemoveAll {
    pub guild_id: GuildId,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    /// The reactions the cached message had.
    pub reactions: Vec<MessageReaction>,
    pub message: Lazy<Message>,
}
hook_event!(MessageReactionRemoveAll, MessageReactionRemoveAll);

impl MessageReactionRemoveAll {
    /// `ensureMessage()`.
    pub async fn ensure_message(&self, app: &App) -> Option<Message> {
        let (c, m) = (self.channel_id, self.message_id);
        self.message.get_or_fetch(|| fetch_message(app, c, m)).await
    }
}

/// `messageReactionRemoveEmoji`: `{ reaction, message, emojiKey, ensureMessage() }`.
#[derive(Debug, Clone)]
pub struct MessageReactionRemoveEmoji {
    pub guild_id: GuildId,
    pub reaction: Reaction,
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub emoji_key: String,
    pub message: Lazy<Message>,
}
hook_event!(MessageReactionRemoveEmoji, MessageReactionRemoveEmoji);

impl MessageReactionRemoveEmoji {
    /// `ensureMessage()`.
    pub async fn ensure_message(&self, app: &App) -> Option<Message> {
        let (c, m) = (self.channel_id, self.message_id);
        self.message.get_or_fetch(|| fetch_message(app, c, m)).await
    }
}

/// How a voice state changed (`change`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VoiceChange {
    Join,
    Leave,
    Move,
    /// Mute, deafen, stream or camera.
    State,
}

impl VoiceChange {
    /// `join` without an old channel, `leave` without a new one, `move` between channels, else `state`.
    pub fn classify(old_channel: Option<ChannelId>, new_channel: Option<ChannelId>) -> Self {
        match (old_channel, new_channel) {
            (None, _) => Self::Join,
            (Some(_), None) => Self::Leave,
            (Some(a), Some(b)) if a != b => Self::Move,
            _ => Self::State,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Join => "join",
            Self::Leave => "leave",
            Self::Move => "move",
            Self::State => "state",
        }
    }
}

/// `voiceStateUpdate`: `{ oldState, newState, member, change }` (bots filtered out).
#[derive(Debug, Clone)]
pub struct VoiceStateUpdate {
    pub guild_id: GuildId,
    pub old_state: Option<VoiceState>,
    pub new_state: VoiceState,
    pub member: Member,
    pub change: VoiceChange,
}
hook_event!(VoiceStateUpdate, VoiceStateUpdate);

/// `guildMemberAdd`: `{ member, user }` (bots included).
#[derive(Debug, Clone)]
pub struct GuildMemberAdd {
    pub guild_id: GuildId,
    pub member: Member,
    pub user: User,
}
hook_event!(GuildMemberAdd, GuildMemberAdd);

/// `guildMemberRemove`: `{ member, user }`; the member is `None` when it was not cached (partial).
#[derive(Debug, Clone)]
pub struct GuildMemberRemove {
    pub guild_id: GuildId,
    pub member: Option<Member>,
    pub user: User,
}
hook_event!(GuildMemberRemove, GuildMemberRemove);

/// `guildMemberUpdate`: `{ oldMember, newMember, member, oldKnown }`. Hooks must not diff roles or nicknames when
/// `old_known` is false.
#[derive(Debug, Clone)]
pub struct GuildMemberUpdate {
    pub guild_id: GuildId,
    pub old_member: Option<Member>,
    /// The new member (`member` in the JS too).
    pub member: Member,
    pub old_known: bool,
}
hook_event!(GuildMemberUpdate, GuildMemberUpdate);

/// `presenceUpdate`: `{ oldPresence, newPresence, member }` (bots filtered out; no I/O before the hooks).
#[derive(Debug, Clone)]
pub struct PresenceUpdate {
    pub guild_id: GuildId,
    pub old_presence: Option<Presence>,
    pub new_presence: Presence,
    pub member: Member,
}
hook_event!(PresenceUpdate, PresenceUpdate);

/// `channelCreate`: `{ channel }`.
#[derive(Debug, Clone)]
pub struct ChannelCreate {
    pub guild_id: GuildId,
    pub channel: GuildChannel,
}
hook_event!(ChannelCreate, ChannelCreate);

/// `channelUpdate`: `{ oldChannel, newChannel, channel }`.
#[derive(Debug, Clone)]
pub struct ChannelUpdate {
    pub guild_id: GuildId,
    pub old_channel: Option<GuildChannel>,
    /// The new channel (`channel` in the JS too).
    pub channel: GuildChannel,
}
hook_event!(ChannelUpdate, ChannelUpdate);

/// `channelDelete`: `{ channel }`.
#[derive(Debug, Clone)]
pub struct ChannelDelete {
    pub guild_id: GuildId,
    pub channel: GuildChannel,
}
hook_event!(ChannelDelete, ChannelDelete);

/// `threadCreate`: `{ thread, channel }` (newly created threads only).
#[derive(Debug, Clone)]
pub struct ThreadCreate {
    pub guild_id: GuildId,
    pub thread: GuildChannel,
}
hook_event!(ThreadCreate, ThreadCreate);

/// `threadUpdate`: `{ oldThread, newThread, thread, channel }`.
#[derive(Debug, Clone)]
pub struct ThreadUpdate {
    pub guild_id: GuildId,
    pub old_thread: Option<GuildChannel>,
    /// The new thread (`thread` and `channel` in the JS too).
    pub thread: GuildChannel,
}
hook_event!(ThreadUpdate, ThreadUpdate);

/// `threadDelete`: `{ thread, channel }`.
#[derive(Debug, Clone)]
pub struct ThreadDelete {
    pub guild_id: GuildId,
    pub thread_id: ChannelId,
    pub parent_id: Option<ChannelId>,
    /// The thread as it was cached.
    pub thread: Option<GuildChannel>,
}
hook_event!(ThreadDelete, ThreadDelete);

/// `roleCreate`: `{ role }`.
#[derive(Debug, Clone)]
pub struct RoleCreate {
    pub guild_id: GuildId,
    pub role: Role,
}
hook_event!(RoleCreate, RoleCreate);

/// `roleUpdate`: `{ oldRole, newRole, role }`.
#[derive(Debug, Clone)]
pub struct RoleUpdate {
    pub guild_id: GuildId,
    pub old_role: Option<Role>,
    /// The new role (`role` in the JS too).
    pub role: Role,
}
hook_event!(RoleUpdate, RoleUpdate);

/// `roleDelete`: `{ role }`.
#[derive(Debug, Clone)]
pub struct RoleDelete {
    pub guild_id: GuildId,
    pub role_id: RoleId,
    /// The role as it was cached.
    pub role: Option<Role>,
}
hook_event!(RoleDelete, RoleDelete);

/// `guildBanAdd`: `{ ban, user }`.
#[derive(Debug, Clone)]
pub struct GuildBanAdd {
    pub guild_id: GuildId,
    pub user: User,
    /// `ban.reason`: the gateway event carries none (`null` in discord.js too); part B fills it only if it fetches the ban.
    pub reason: Option<String>,
}
hook_event!(GuildBanAdd, GuildBanAdd);

/// `guildBanRemove`: `{ ban, user }`.
#[derive(Debug, Clone)]
pub struct GuildBanRemove {
    pub guild_id: GuildId,
    pub user: User,
    /// `ban.reason` (see [`GuildBanAdd::reason`]).
    pub reason: Option<String>,
}
hook_event!(GuildBanRemove, GuildBanRemove);

/// `guildUpdate`: `{ oldGuild, newGuild }`.
#[derive(Debug, Clone)]
pub struct GuildUpdate {
    pub guild_id: GuildId,
    pub old_guild: Option<Guild>,
    pub new_guild: PartialGuild,
}
hook_event!(GuildUpdate, GuildUpdate);

/// `emojiCreate`: `{ emoji }`.
#[derive(Debug, Clone)]
pub struct EmojiCreate {
    pub guild_id: GuildId,
    pub emoji: Emoji,
}
hook_event!(EmojiCreate, EmojiCreate);

/// `emojiUpdate`: `{ oldEmoji, newEmoji, emoji }`.
#[derive(Debug, Clone)]
pub struct EmojiUpdate {
    pub guild_id: GuildId,
    pub old_emoji: Emoji,
    /// The new emoji (`emoji` in the JS too).
    pub emoji: Emoji,
}
hook_event!(EmojiUpdate, EmojiUpdate);

/// `emojiDelete`: `{ emoji }`.
#[derive(Debug, Clone)]
pub struct EmojiDelete {
    pub guild_id: GuildId,
    pub emoji: Emoji,
}
hook_event!(EmojiDelete, EmojiDelete);

/// `inviteCreate`: `{ invite }` (only for a cached guild).
#[derive(Debug, Clone)]
pub struct InviteCreate {
    pub guild_id: GuildId,
    pub invite: InviteCreateEvent,
}
hook_event!(InviteCreate, InviteCreate);

/// `inviteDelete`: `{ invite }` (only for a cached guild).
#[derive(Debug, Clone)]
pub struct InviteDelete {
    pub guild_id: GuildId,
    pub invite: InviteDeleteEvent,
}
hook_event!(InviteDelete, InviteDelete);

/// `guildScheduledEventUpdate`: `{ oldScheduledEvent (always null: not cached), scheduledEvent }`.
#[derive(Debug, Clone)]
pub struct GuildScheduledEventUpdate {
    pub guild_id: GuildId,
    pub scheduled_event: ScheduledEvent,
}
hook_event!(GuildScheduledEventUpdate, GuildScheduledEventUpdate);

/// `guildScheduledEventDelete`: `{ scheduledEvent }`.
#[derive(Debug, Clone)]
pub struct GuildScheduledEventDelete {
    pub guild_id: GuildId,
    pub scheduled_event: ScheduledEvent,
}
hook_event!(GuildScheduledEventDelete, GuildScheduledEventDelete);

/// `userUpdate`: fanned out to every available guild that has the user cached: `{ oldUser, newUser, user, member }`.
#[derive(Debug, Clone)]
pub struct UserUpdate {
    pub guild_id: GuildId,
    pub old_user: User,
    /// The new user (`user` in the JS too).
    pub user: User,
    pub member: Option<Member>,
}
hook_event!(UserUpdate, UserUpdate);

/// `levelUp` (internal; from the levels service and `/levelconfig xp give`): `{ member, userId, oldLevel, newLevel,
/// channel, source }`.
#[derive(Debug, Clone)]
pub struct LevelUp {
    pub guild_id: GuildId,
    pub member: Option<Member>,
    pub user_id: UserId,
    pub old_level: i64,
    pub new_level: i64,
    pub channel_id: Option<ChannelId>,
    /// `'message'`, `'voice'`, `'admin'`, ... as the emitter passes it.
    pub source: String,
}
hook_event!(LevelUp, LevelUp);

/// `economyPurchase` (internal; `/shop buy`): `{ member, userId, item, quantity, totalPrice, channel }`.
#[derive(Debug, Clone)]
pub struct EconomyPurchase {
    pub guild_id: GuildId,
    pub member: Option<Member>,
    pub user_id: UserId,
    pub item: ShopItem,
    pub quantity: i64,
    pub total_price: i64,
    pub channel_id: Option<ChannelId>,
}
hook_event!(EconomyPurchase, EconomyPurchase);

#[cfg(test)]
mod tests {
    use super::*;

    /// One payload type per hook point, no point left out (the HOOK_ORDER keys).
    #[test]
    fn every_hook_point_has_exactly_one_payload_type() {
        let points = [
            Ready::POINT,
            GuildReady::POINT,
            GuildDelete::POINT,
            MessageCreate::POINT,
            DirectMessage::POINT,
            BotMessage::POINT,
            OwnMessage::POINT,
            MessageUpdate::POINT,
            MessageDelete::POINT,
            MessageDeleteBulk::POINT,
            MessageReactionAdd::POINT,
            MessageReactionRemove::POINT,
            MessageReactionRemoveAll::POINT,
            MessageReactionRemoveEmoji::POINT,
            VoiceStateUpdate::POINT,
            GuildMemberAdd::POINT,
            GuildMemberRemove::POINT,
            GuildMemberUpdate::POINT,
            PresenceUpdate::POINT,
            ChannelCreate::POINT,
            ChannelUpdate::POINT,
            ChannelDelete::POINT,
            ThreadCreate::POINT,
            ThreadUpdate::POINT,
            ThreadDelete::POINT,
            RoleCreate::POINT,
            RoleUpdate::POINT,
            RoleDelete::POINT,
            GuildBanAdd::POINT,
            GuildBanRemove::POINT,
            GuildUpdate::POINT,
            EmojiCreate::POINT,
            EmojiUpdate::POINT,
            EmojiDelete::POINT,
            InviteCreate::POINT,
            InviteDelete::POINT,
            GuildScheduledEventUpdate::POINT,
            GuildScheduledEventDelete::POINT,
            UserUpdate::POINT,
            LevelUp::POINT,
            EconomyPurchase::POINT,
        ];
        assert_eq!(points.to_vec(), HookPoint::ALL.to_vec());
    }

    #[test]
    fn voice_change_and_emoji_key() {
        let a = Some(ChannelId::new(1));
        let b = Some(ChannelId::new(2));
        assert_eq!(VoiceChange::classify(None, a), VoiceChange::Join);
        assert_eq!(VoiceChange::classify(a, None), VoiceChange::Leave);
        assert_eq!(VoiceChange::classify(a, b), VoiceChange::Move);
        assert_eq!(VoiceChange::classify(a, a), VoiceChange::State);
        assert_eq!(VoiceChange::classify(None, None), VoiceChange::Join, "the JS checks the old channel first");
        assert_eq!(emoji_key(&serenity::all::ReactionType::Unicode("⭐".into())), "⭐");
        let custom = serenity::all::ReactionType::Custom { animated: false, id: serenity::all::EmojiId::new(42), name: Some("x".into()) };
        assert_eq!(emoji_key(&custom), "42");
    }
}
