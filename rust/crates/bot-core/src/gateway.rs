//! Port of `src/events/*.js` (all 38 files): the gateway events as hook dispatches, with the JS pre-filters, fields and
//! synthetic points.
//!
//! **Intake.** Serenity spawns one task per event for its `EventHandler`s, so they run concurrently and out of
//! gateway order, and it updates its cache before they run (the old message of a delete, the old presence, the old
//! emoji list are gone by then). The only synchronous tap is the shard runner's collector callback, which sees every
//! event in gateway order *before* the cache update. [`register_intake`] installs one per shard: it snapshots what the
//! JS reads from the cache before the update ([`Before`]) and queues the event. One consumer per shard then
//! processes the events in order, each once its cache update is done (the next event of the shard has arrived, or
//! 2 ms passed), and for each it builds the payload and calls `run_hooks` synchronously (the hook runner starts the
//! first hook during that call), polls the dispatch once, and lets it finish in the background. That gives the JS
//! guarantee: hooks are entered in gateway order (counting, invites).
//!
//! Every handler body runs inside [`crate::event_loader::run_event`]: a failure is logged, never fatal.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use bot_config::RECURRING_JOB_TYPES;
use serenity::all::{
    Cache, ChannelId, Emoji, EmojiId, Event, Guild, GuildChannel, GuildId, Member, Message, MessageFlags, MessageType, Presence, Role, User, UserId,
    VoiceState,
};
use serenity::gateway::{ActivityData, CollectorCallback, ShardMessenger};

use crate::context::App;
use crate::event_loader::run_event;
use crate::hooks::events::*;
use crate::hooks::{HookEvent, run_guild_available, run_guild_ready};
use crate::interaction::member_from_partial;
use crate::scheduler::CancelJobs;

/// discord.js `waitGuildTimeout`: how long a shard waits for its READY guilds before it is ready anyway (reset by
/// each guild that arrives).
pub const WAIT_GUILD_TIMEOUT_MS: u64 = 15_000;
/// `MESSAGE_CACHE_LIFETIME_S` of bot.js: cached messages older than this count as not cached (discord.js sweeps them).
pub const MESSAGE_CACHE_LIFETIME_MS: i64 = 3 * 60 * 60 * 1000;
/// `RECENT_EDIT_MS` of messageUpdate.js.
pub const RECENT_EDIT_MS: i64 = 15_000;
/// How long the consumer waits for the cache update of the newest event before it processes it anyway.
const CACHE_SETTLE_MS: u64 = 2;
/// The startup work runs at most this many guilds at once (`mapLimit(available, 4, ...)`).
pub const GUILD_READY_CONCURRENCY: usize = 4;

// ── The snapshot taken before the cache update ──────────────────────────────────

/// What the JS reads from the discord.js cache before an event changes it.
#[derive(Debug, Clone, Default)]
pub struct Before {
    pub message: Option<Message>,
    pub messages: Vec<Message>,
    pub member: Option<Member>,
    pub user: Option<User>,
    pub presence: Option<Presence>,
    pub channel: Option<GuildChannel>,
    pub role: Option<Role>,
    pub guild: Option<Guild>,
    /// The guild was in the cache as unavailable.
    pub guild_unavailable: bool,
    /// The guild was in the cache (available or not).
    pub guild_known: bool,
    pub emojis: Option<HashMap<EmojiId, Emoji>>,
    pub voice: Option<VoiceState>,
}

/// One gateway event with its pre-update snapshot.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub shard: u32,
    pub event: Event,
    pub before: Before,
}

/// A cached message, unless discord.js would have swept it (older than 3 h by its edit or creation time).
fn live_message(cache: &Cache, channel: ChannelId, id: serenity::all::MessageId, now_ms: i64) -> Option<Message> {
    let m = cache.message(channel, id)?.clone();
    let stamp = m.edited_timestamp.unwrap_or(m.timestamp).timestamp_millis();
    (now_ms - stamp < MESSAGE_CACHE_LIFETIME_MS).then_some(m)
}

/// A cached member (`guild.members.cache.get(id)`).
pub fn cached_member(cache: &Cache, guild: GuildId, user: UserId) -> Option<Member> {
    cache.guild(guild)?.members.get(&user).cloned()
}

fn guild_thread(cache: &Cache, guild: GuildId, id: ChannelId) -> Option<GuildChannel> {
    let g = cache.guild(guild)?;
    g.channels.get(&id).cloned().or_else(|| g.threads.iter().find(|t| t.id == id).cloned())
}

/// Reads what the event will change, before serenity updates its cache. Runs on the shard runner: cheap reads only.
pub fn snapshot(cache: &Cache, event: &Event, now_ms: i64) -> Before {
    let mut b = Before::default();
    match event {
        Event::MessageUpdate(e) => b.message = live_message(cache, e.channel_id, e.id, now_ms),
        Event::MessageDelete(e) => b.message = live_message(cache, e.channel_id, e.message_id, now_ms),
        Event::MessageDeleteBulk(e) => b.messages = e.ids.iter().filter_map(|id| live_message(cache, e.channel_id, *id, now_ms)).collect(),
        Event::ReactionRemoveAll(e) => b.message = live_message(cache, e.channel_id, e.message_id, now_ms),
        Event::GuildMemberUpdate(e) => {
            b.member = cached_member(cache, e.guild_id, e.user.id);
            b.user = cache.user(e.user.id).map(|u| u.clone());
        }
        Event::GuildMemberRemove(e) => b.member = cached_member(cache, e.guild_id, e.user.id),
        Event::PresenceUpdate(e) => {
            if let Some(g) = e.presence.guild_id.and_then(|g| cache.guild(g)) {
                b.presence = g.presences.get(&e.presence.user.id).cloned();
            }
            b.user = cache.user(e.presence.user.id).map(|u| u.clone());
        }
        Event::ChannelUpdate(e) => b.channel = guild_thread(cache, e.channel.guild_id, e.channel.id),
        Event::ThreadUpdate(e) => b.channel = guild_thread(cache, e.thread.guild_id, e.thread.id),
        Event::ThreadDelete(e) => b.channel = guild_thread(cache, e.thread.guild_id, e.thread.id),
        Event::ThreadCreate(e) => b.channel = guild_thread(cache, e.thread.guild_id, e.thread.id),
        Event::GuildRoleUpdate(e) => b.role = cache.guild(e.role.guild_id).and_then(|g| g.roles.get(&e.role.id).cloned()),
        Event::GuildRoleDelete(e) => b.role = cache.guild(e.guild_id).and_then(|g| g.roles.get(&e.role_id).cloned()),
        Event::GuildEmojisUpdate(e) => b.emojis = cache.guild(e.guild_id).map(|g| g.emojis.clone()),
        Event::GuildUpdate(e) => b.guild = cache.guild(e.guild.id).map(|g| g.clone()),
        Event::GuildDelete(e) => {
            b.guild = cache.guild(e.guild.id).map(|g| g.clone());
            b.guild_unavailable = cache.unavailable_guilds().get(&e.guild.id).is_some();
            b.guild_known = b.guild.is_some() || b.guild_unavailable;
        }
        Event::GuildCreate(e) => {
            b.guild_unavailable = cache.unavailable_guilds().get(&e.guild.id).is_some();
            b.guild_known = b.guild_unavailable || cache.guild(e.guild.id).is_some();
        }
        Event::VoiceStateUpdate(e) => {
            if let Some(g) = e.voice_state.guild_id.and_then(|g| cache.guild(g)) {
                b.voice = g.voice_states.get(&e.voice_state.user_id).cloned();
            }
        }
        _ => {}
    }
    b
}

// ── Per-shard startup (`ClientReady`) ───────────────────────────────────────────

#[derive(Debug, Default)]
struct ShardStartup {
    /// The READY guilds still missing.
    pending: HashSet<GuildId>,
    done: bool,
    /// Bumped by each guild that arrives (resets the wait).
    generation: u64,
}

/// The gateway's own state: per-shard startup, the runner map (presence), the shard count.
#[derive(Default)]
pub struct GatewayState {
    shards: Mutex<HashMap<u32, ShardStartup>>,
    /// The shard manager's runners (for the presence); never a clone of a messenger, which would keep a replaced
    /// runner's collector list (and with it the intake registration) alive.
    runners: Mutex<Option<Runners>>,
    total_shards: AtomicU32,
    registered: Mutex<HashMap<u32, u64>>,
    next_registration: AtomicU64,
    stopped: AtomicBool,
}

impl GatewayState {
    pub fn new(total_shards: u32) -> Arc<Self> {
        let s = Self::default();
        s.total_shards.store(total_shards.max(1), Ordering::SeqCst);
        Arc::new(s)
    }

    /// Which shard a guild belongs to (`(guildId >> 22) % shardCount`).
    pub fn shard_of(&self, guild_id: GuildId) -> u32 {
        ((guild_id.get() >> 22) % u64::from(self.total_shards.load(Ordering::SeqCst).max(1))) as u32
    }

    /// Whether the shard finished its startup (`ClientReady` fired).
    pub fn shard_ready(&self, shard: u32) -> bool {
        self.shards.lock().unwrap_or_else(|e| e.into_inner()).get(&shard).is_some_and(|s| s.done)
    }

    /// Stops re-registering intakes (shutdown).
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

/// The shard manager's runner map.
pub type Runners = Arc<tokio::sync::Mutex<HashMap<serenity::all::ShardId, serenity::gateway::ShardRunnerInfo>>>;

/// Clears a shard's registration when serenity drops the runner (and with it our collector callback).
struct Registration {
    state: Arc<GatewayState>,
    shard: u32,
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut r = self.state.registered.lock().unwrap_or_else(|e| e.into_inner());
        if r.get(&self.shard) == Some(&self.id) {
            r.remove(&self.shard);
        }
    }
}

/// Installs the ordered intake on one shard runner: a collector callback that snapshots and queues every event, and
/// the shard's consumer task.
pub fn register_intake(app: &App, gw: &Arc<GatewayState>, shard: u32, messenger: &ShardMessenger) {
    let Some(d) = app.discord() else { return };
    let id = gw.next_registration.fetch_add(1, Ordering::SeqCst);
    gw.registered.lock().unwrap_or_else(|e| e.into_inner()).insert(shard, id);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Envelope>();
    let cache = d.cache.clone();
    let clock = app.clock().clone();
    let guard = Registration { state: gw.clone(), shard, id };
    let chunks = app.ext::<crate::guild_members::MemberChunks>();
    messenger.add_collector(CollectorCallback(Box::new(move |event: &Event| {
        let _keep = &guard;
        if let Event::GuildMembersChunk(chunk) = event {
            crate::guild_members::deliver_members_chunk(&chunks, chunk);
        }
        let before = snapshot(&cache, event, clock.now_ms());
        tx.send(Envelope { shard, event: event.clone(), before }).is_ok()
    })));
    let (a, g) = (app.clone(), gw.clone());
    tokio::spawn(consume(a, g, rx));
}

/// The shard's consumer: processes each event once its cache update is done.
async fn consume(app: App, gw: Arc<GatewayState>, mut rx: tokio::sync::mpsc::UnboundedReceiver<Envelope>) {
    let mut pending: Option<Envelope> = None;
    loop {
        let next = if pending.is_some() {
            tokio::time::timeout(Duration::from_millis(CACHE_SETTLE_MS), rx.recv()).await.ok()
        } else {
            Some(rx.recv().await)
        };
        match next {
            // The next event arrived: the cache update of the previous one is done.
            Some(Some(env)) => {
                if let Some(p) = pending.replace(env) {
                    process(&app, &gw, p).await;
                }
            }
            // The intake was dropped (the runner restarted or the bot stopped).
            Some(None) => {
                if let Some(p) = pending.take() {
                    process(&app, &gw, p).await;
                }
                return;
            }
            // Quiet for 2 ms: the runner finished the newest event's cache update.
            None => {
                if let Some(p) = pending.take() {
                    process(&app, &gw, p).await;
                }
            }
        }
    }
}

/// Watches the shard manager and installs the intake on every new shard runner (and again after a runner restart).
pub fn watch_runners(app: App, gw: Arc<GatewayState>, runners: Runners) {
    *gw.runners.lock().unwrap_or_else(|e| e.into_inner()) = Some(runners.clone());
    install_chunk_sender(&app, &gw, &runners);
    tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        while !gw.stopped.load(Ordering::SeqCst) {
            {
                let map = runners.lock().await;
                for (id, info) in map.iter() {
                    let shard = id.0;
                    let known = gw.registered.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&shard);
                    if !known {
                        register_intake(&app, &gw, shard, &info.runner_tx);
                    }
                }
            }
            // Fast while shards connect (READY follows IDENTIFY within a round trip), then relaxed.
            let wait = if started.elapsed() < Duration::from_secs(60) { 10 } else { 250 };
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    });
}

/// Lets [`crate::guild_members::fetch_members_by_ids`] send `REQUEST_GUILD_MEMBERS` on the guild's shard.
fn install_chunk_sender(app: &App, gw: &Arc<GatewayState>, runners: &Runners) {
    let (gw, runners) = (gw.clone(), runners.clone());
    let sender: crate::guild_members::ChunkSender = Arc::new(move |guild_id, ids, nonce| {
        let (gw, runners) = (gw.clone(), runners.clone());
        Box::pin(async move {
            let shard = gw.shard_of(guild_id);
            let map = runners.lock().await;
            match map.get(&serenity::all::ShardId(shard)) {
                Some(info) => {
                    info.runner_tx.chunk_guild(guild_id, None, false, serenity::gateway::ChunkGuildFilter::UserIds(ids), Some(nonce));
                    true
                }
                None => false,
            }
        })
    });
    crate::guild_members::set_chunk_sender(app, Some(sender));
}

// ── Dispatch helpers ──────────────────────────────────────────────────────────

/// Calls `run_hooks` now (its synchronous prefix starts the first hook), polls the dispatch once here, and lets the
/// rest finish in the background.
pub async fn dispatch<E: HookEvent>(app: &App, payload: E) {
    let mut fut = Box::pin(app.run_hooks(payload));
    let polled = std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx))).await;
    if polled.is_pending() {
        tokio::spawn(fut);
    }
}

fn spawn_event<F>(name: &'static str, body: F)
where
    F: Future<Output = crate::error::BotResult<()>> + Send + 'static,
{
    tokio::spawn(run_event(name, body));
}

fn cache_of(app: &App) -> Option<Arc<Cache>> {
    app.discord().map(|d| d.cache.clone())
}

fn bot_id(app: &App) -> Option<UserId> {
    app.discord().map(|d| d.cache.current_user().id)
}

fn cached_channel(app: &App, guild: GuildId, channel: ChannelId) -> Option<GuildChannel> {
    let cache = cache_of(app)?;
    guild_thread(&cache, guild, channel)
}

fn guild_cached(app: &App, guild: GuildId) -> bool {
    cache_of(app).is_some_and(|c| c.guild(guild).is_some())
}

/// The member of a message: the cached member, else the partial member Discord sent with the message.
fn message_member(app: &App, message: &Message) -> Option<Member> {
    let g = message.guild_id?;
    if let Some(m) = cache_of(app).and_then(|c| cached_member(&c, g, message.author.id)) {
        return Some(m);
    }
    message.member.as_deref().map(|p| member_from_partial(p.clone(), message.author.clone(), Some(g)))
}

/// `recentlyEdited(message)`: a fresh `editedTimestamp`.
pub fn recently_edited(edited_ms: Option<i64>, now_ms: i64) -> bool {
    edited_ms.is_some_and(|t| now_ms - t < RECENT_EDIT_MS)
}

/// `USER_MESSAGE_TYPES`: only real user messages enter the pipeline.
pub fn is_user_message_type(kind: MessageType) -> bool {
    matches!(kind, MessageType::Regular | MessageType::InlineReply)
}

/// Where a gateway message goes (`messageCreate.js`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRoute {
    OwnMessage,
    BotMessage,
    DirectMessage,
    MessageCreate,
    /// Dropped (a system message, or a bot message outside a guild).
    Ignored,
}

/// `messageCreate.js`'s decision: bots and webhooks go to `botMessage` (or `ownMessage` for the bot itself) in a
/// guild, before the type filter; then only default messages and replies; a DM is `directMessage`.
pub fn route_message(author_bot: bool, webhook: bool, author_is_me: bool, in_guild: bool, kind: MessageType) -> MessageRoute {
    if author_bot || webhook {
        if !in_guild {
            return MessageRoute::Ignored;
        }
        return if author_is_me { MessageRoute::OwnMessage } else { MessageRoute::BotMessage };
    }
    if !is_user_message_type(kind) {
        return MessageRoute::Ignored;
    }
    if in_guild { MessageRoute::MessageCreate } else { MessageRoute::DirectMessage }
}

/// discord.js `Presence#equals`: status, the three client statuses, and the activities in order (name, type, url,
/// state, details, emoji id and name).
pub fn presence_equals(old: &Presence, new: &Presence) -> bool {
    let cs = |p: &Presence| {
        let c = p.client_status.as_ref();
        (c.and_then(|c| c.web), c.and_then(|c| c.mobile), c.and_then(|c| c.desktop))
    };
    old.status == new.status
        && cs(old) == cs(new)
        && old.activities.len() == new.activities.len()
        && old.activities.iter().zip(&new.activities).all(|(a, b)| {
            a.name == b.name
                && a.kind == b.kind
                && a.url == b.url
                && a.state == b.state
                && a.details == b.details
                && a.emoji.as_ref().and_then(|e| e.id) == b.emoji.as_ref().and_then(|e| e.id)
                && a.emoji.as_ref().map(|e| &e.name) == b.emoji.as_ref().map(|e| &e.name)
        })
}

/// discord.js `GuildMember#equals(old)` after `_update(data)`: true when a `GUILD_MEMBER_UPDATE` changes none of the
/// fields discord.js compares (joined time, nickname, guild avatar and banner, pending, timeout, flags, the role list
/// in order, the avatar decoration). The premium time and the user are not compared.
pub fn member_update_is_noop(old: &Member, e: &serenity::all::GuildMemberUpdateEvent) -> bool {
    let decoration = |d: Option<&serenity::all::AvatarDecorationData>| d.map(|d| (d.asset, d.sku_id));
    old.joined_at.map(|t| t.timestamp_millis()) == Some(e.joined_at.timestamp_millis())
        && old.nick == e.nick
        && old.avatar == e.avatar
        && old.banner == e.banner
        && old.pending == e.pending
        && old.communication_disabled_until.map(|t| t.timestamp_millis()) == e.communication_disabled_until.map(|t| t.timestamp_millis())
        && e.flags.is_none_or(|f| f == old.flags)
        && old.roles == e.roles
        && decoration(old.avatar_decoration_data.as_ref()) == decoration(e.avatar_decoration_data.as_ref())
}

/// discord.js `User#_equals` on the fields the gateway sends.
pub fn user_changed(old: &User, new: &User) -> bool {
    old.name != new.name || old.discriminator != new.discriminator || old.global_name != new.global_name || old.avatar != new.avatar || old.public_flags != new.public_flags
}

/// The emoji diff of `GUILD_EMOJIS_UPDATE` (discord.js `GuildEmojisUpdate`): updates and creates in the event's
/// order, then deletes.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // short-lived, one per changed emoji
pub enum EmojiChange {
    Create(Emoji),
    Update(Emoji, Emoji),
    Delete(Emoji),
}

/// Diffs the cached emoji list against the new one.
pub fn diff_emojis(old: &HashMap<EmojiId, Emoji>, new: &HashMap<EmojiId, Emoji>) -> Vec<EmojiChange> {
    let mut ids: Vec<&EmojiId> = new.keys().collect();
    ids.sort();
    let mut out = Vec::new();
    for id in ids {
        let e = &new[id];
        match old.get(id) {
            None => out.push(EmojiChange::Create(e.clone())),
            Some(o) if o.name != e.name || o.roles != e.roles || o.available != e.available || o.animated != e.animated || o.managed != e.managed => {
                out.push(EmojiChange::Update(o.clone(), e.clone()))
            }
            Some(_) => {}
        }
    }
    let mut gone: Vec<&Emoji> = old.values().filter(|o| !new.contains_key(&o.id)).collect();
    gone.sort_by_key(|e| e.id);
    out.extend(gone.into_iter().cloned().map(EmojiChange::Delete));
    out
}

// ── The events ────────────────────────────────────────────────────────────────

/// Processes one event (in gateway order for its shard). Never fails.
pub async fn process(app: &App, gw: &Arc<GatewayState>, env: Envelope) {
    let Envelope { shard, event, before } = env;
    let now = app.clock().now_ms();
    match event {
        Event::Ready(e) => on_ready(app, gw, shard, e.ready),
        Event::GuildCreate(e) => on_guild_create(app, gw, shard, e.guild, before),
        Event::GuildDelete(e) => {
            // Unavailable guilds (a Discord outage) are not departures: keep everything. discord.js emits guildDelete
            // only for a guild it had cached (GuildDeleteAction), so a delete for an unknown guild does nothing.
            if e.guild.unavailable || before.guild_unavailable || !before.guild_known {
                return;
            }
            let (a, guild) = (app.clone(), before.guild);
            let id = e.guild.id;
            spawn_event("guildDelete", async move { on_guild_delete(&a, id, guild).await });
        }
        Event::InteractionCreate(e) => {
            let a = app.clone();
            spawn_event("interactionCreate", async move {
                crate::router::route_interaction(&a, e.interaction).await;
                Ok(())
            });
        }
        Event::MessageCreate(e) => on_message_create(app, e.message).await,
        Event::MessageUpdate(e) => {
            let a = app.clone();
            spawn_event("messageUpdate", async move { on_message_update(&a, e, before.message, now).await });
        }
        Event::MessageDelete(e) => {
            let Some(g) = e.guild_id else { return };
            if !guild_cached(app, g) {
                return;
            }
            let partial = before.message.is_none();
            dispatch(app, MessageDelete {
                guild_id: g,
                message_id: e.message_id,
                channel_id: e.channel_id,
                message: before.message,
                partial,
                channel: cached_channel(app, g, e.channel_id),
            })
            .await;
        }
        Event::MessageDeleteBulk(e) => {
            let Some(g) = e.guild_id else { return };
            // MessageDeleteBulkAction emits only for a cached channel (`if (channel)`).
            let Some(channel) = cached_channel(app, g, e.channel_id) else { return };
            dispatch(app, MessageDeleteBulk {
                guild_id: g,
                channel_id: e.channel_id,
                message_ids: e.ids,
                messages: before.messages,
                channel: Some(channel),
            })
            .await;
        }
        Event::ReactionAdd(e) => on_reaction(app, e.reaction, true).await,
        Event::ReactionRemove(e) => on_reaction(app, e.reaction, false).await,
        Event::ReactionRemoveAll(e) => {
            let Some(g) = e.guild_id.or_else(|| before.message.as_ref().and_then(|m| m.guild_id)) else { return };
            if !guild_cached(app, g) {
                return;
            }
            let reactions = before.message.as_ref().map(|m| m.reactions.clone()).unwrap_or_default();
            let message = match before.message {
                Some(m) => Lazy::resolved(Some(m)),
                None => Lazy::new(),
            };
            dispatch(app, MessageReactionRemoveAll { guild_id: g, channel_id: e.channel_id, message_id: e.message_id, reactions, message }).await;
        }
        Event::ReactionRemoveEmoji(e) => {
            let Some(g) = e.reaction.guild_id else { return };
            if !guild_cached(app, g) {
                return;
            }
            let r = e.reaction;
            let message = match cache_of(app).and_then(|c| live_message(&c, r.channel_id, r.message_id, now)) {
                Some(m) => Lazy::resolved(Some(m)),
                None => Lazy::new(),
            };
            dispatch(app, MessageReactionRemoveEmoji {
                guild_id: g,
                channel_id: r.channel_id,
                message_id: r.message_id,
                emoji_key: emoji_key(&r.emoji),
                reaction: r,
                message,
            })
            .await;
        }
        Event::VoiceStateUpdate(e) => {
            let new = e.voice_state;
            let Some(g) = new.guild_id else { return };
            let member = new.member.clone().or_else(|| before.voice.as_ref().and_then(|v| v.member.clone()));
            let Some(member) = member.filter(|m| !m.user.bot) else { return };
            let change = VoiceChange::classify(before.voice.as_ref().and_then(|v| v.channel_id), new.channel_id);
            dispatch(app, VoiceStateUpdate { guild_id: g, old_state: before.voice, new_state: new, member, change }).await;
        }
        Event::GuildMemberAdd(e) => {
            // No pre-filter and nothing awaited before the hooks (gateway order for the invites hook).
            let member = e.member;
            let user = member.user.clone();
            dispatch(app, GuildMemberAdd { guild_id: member.guild_id, member, user }).await;
        }
        Event::GuildMemberRemove(e) => {
            dispatch(app, GuildMemberRemove { guild_id: e.guild_id, member: before.member, user: e.user }).await;
        }
        Event::GuildMemberUpdate(e) => {
            let g = e.guild_id;
            let member = cache_of(app).and_then(|c| cached_member(&c, g, e.user.id));
            let old_known = before.member.is_some();
            // GuildMemberUpdateAction emits only when `!member.equals(old)`: a change of the user alone (name,
            // avatar) is a userUpdate, not a guildMemberUpdate.
            let unchanged = before.member.as_ref().is_some_and(|old| member_update_is_noop(old, &e));
            if let Some(member) = member.filter(|_| !unchanged) {
                dispatch(app, GuildMemberUpdate { guild_id: g, old_member: before.member.clone(), member, old_known }).await;
            }
            if let Some(old) = before.user {
                emit_user_update(app, old, e.user).await;
            }
        }
        Event::PresenceUpdate(e) => {
            let p = e.presence;
            if let (Some(old), Some(name)) = (before.user.clone(), p.user.name.clone()) {
                let mut new = old.clone();
                new.name = name;
                new.discriminator = p.user.discriminator;
                new.global_name = p.user.global_name.clone().or(old.global_name.clone());
                new.avatar = p.user.avatar;
                emit_user_update(app, old, new).await;
            }
            // Presence updates are the busiest event: no settings access and no I/O here.
            let Some(g) = p.guild_id else { return };
            let Some(member) = cache_of(app).and_then(|c| cached_member(&c, g, p.user.id)) else { return };
            if member.user.bot {
                return;
            }
            // PresenceUpdateAction emits only when the presence changed (`!newPresence.equals(oldPresence)`).
            if before.presence.as_ref().is_some_and(|old| presence_equals(old, &p)) {
                return;
            }
            dispatch(app, PresenceUpdate { guild_id: g, old_presence: before.presence, new_presence: p, member }).await;
        }
        Event::ChannelCreate(e) => dispatch(app, ChannelCreate { guild_id: e.channel.guild_id, channel: e.channel }).await,
        Event::ChannelUpdate(e) => dispatch(app, ChannelUpdate { guild_id: e.channel.guild_id, old_channel: before.channel, channel: e.channel }).await,
        Event::ChannelDelete(e) => dispatch(app, ChannelDelete { guild_id: e.channel.guild_id, channel: e.channel }).await,
        Event::ThreadCreate(e) => {
            // `newlyCreated`: serenity drops the flag; a thread not cached before whose creation time is recent.
            let created = e.thread.thread_metadata.as_ref().and_then(|m| m.create_timestamp).map(|t| t.timestamp_millis());
            let newly = before.channel.is_none() && created.is_some_and(|t| now - t < 60_000);
            if newly {
                dispatch(app, ThreadCreate { guild_id: e.thread.guild_id, thread: e.thread }).await;
            }
        }
        Event::ThreadUpdate(e) => dispatch(app, ThreadUpdate { guild_id: e.thread.guild_id, old_thread: before.channel, thread: e.thread }).await,
        Event::ThreadDelete(e) => {
            dispatch(app, ThreadDelete { guild_id: e.thread.guild_id, thread_id: e.thread.id, parent_id: Some(e.thread.parent_id), thread: before.channel })
                .await
        }
        Event::GuildRoleCreate(e) => dispatch(app, RoleCreate { guild_id: e.role.guild_id, role: e.role }).await,
        Event::GuildRoleUpdate(e) => dispatch(app, RoleUpdate { guild_id: e.role.guild_id, old_role: before.role, role: e.role }).await,
        Event::GuildRoleDelete(e) => dispatch(app, RoleDelete { guild_id: e.guild_id, role_id: e.role_id, role: before.role }).await,
        Event::GuildBanAdd(e) => dispatch(app, GuildBanAdd { guild_id: e.guild_id, user: e.user, reason: None }).await,
        Event::GuildBanRemove(e) => dispatch(app, GuildBanRemove { guild_id: e.guild_id, user: e.user, reason: None }).await,
        Event::GuildUpdate(e) => dispatch(app, GuildUpdate { guild_id: e.guild.id, old_guild: before.guild, new_guild: e.guild }).await,
        Event::GuildEmojisUpdate(e) => {
            let old = before.emojis.unwrap_or_default();
            for change in diff_emojis(&old, &e.emojis) {
                match change {
                    EmojiChange::Create(emoji) => dispatch(app, EmojiCreate { guild_id: e.guild_id, emoji }).await,
                    EmojiChange::Update(old_emoji, emoji) => dispatch(app, EmojiUpdate { guild_id: e.guild_id, old_emoji, emoji }).await,
                    EmojiChange::Delete(emoji) => dispatch(app, EmojiDelete { guild_id: e.guild_id, emoji }).await,
                }
            }
        }
        // InviteCreateAction / InviteDeleteAction return without emitting when the invite's channel is not cached.
        Event::InviteCreate(e) => {
            let Some(g) = e.guild_id.filter(|g| guild_cached(app, *g)) else { return };
            if cached_channel(app, g, e.channel_id).is_none() {
                return;
            }
            dispatch(app, InviteCreate { guild_id: g, invite: e }).await;
        }
        Event::InviteDelete(e) => {
            let Some(g) = e.guild_id.filter(|g| guild_cached(app, *g)) else { return };
            if cached_channel(app, g, e.channel_id).is_none() {
                return;
            }
            dispatch(app, InviteDelete { guild_id: g, invite: e }).await;
        }
        Event::GuildScheduledEventUpdate(e) => {
            dispatch(app, GuildScheduledEventUpdate { guild_id: e.event.guild_id, scheduled_event: e.event }).await;
        }
        Event::GuildScheduledEventDelete(e) => {
            dispatch(app, GuildScheduledEventDelete { guild_id: e.event.guild_id, scheduled_event: e.event }).await;
        }
        _ => {}
    }
}

async fn on_message_create(app: &App, message: Message) {
    let me = bot_id(app);
    let route = route_message(
        message.author.bot,
        message.webhook_id.is_some(),
        me.is_some_and(|m| m == message.author.id),
        message.guild_id.is_some(),
        message.kind,
    );
    let channel_id = message.channel_id;
    match route {
        MessageRoute::Ignored => {}
        MessageRoute::OwnMessage | MessageRoute::BotMessage => {
            let Some(g) = message.guild_id else { return };
            let channel = cached_channel(app, g, channel_id);
            if route == MessageRoute::OwnMessage {
                dispatch(app, OwnMessage { guild_id: g, message, channel_id, channel }).await;
            } else {
                dispatch(app, BotMessage { guild_id: g, message, channel_id, channel, edited: false, was_loading: None }).await;
            }
        }
        MessageRoute::DirectMessage => {
            let user = message.author.clone();
            dispatch(app, DirectMessage { message, channel_id, user }).await;
        }
        MessageRoute::MessageCreate => {
            let Some(g) = message.guild_id else { return };
            let member = message_member(app, &message);
            let channel = cached_channel(app, g, channel_id);
            dispatch(app, MessageCreate { guild_id: g, message, member, channel_id, channel }).await;
        }
    }
}

async fn on_message_update(app: &App, e: serenity::all::MessageUpdateEvent, old: Option<Message>, now: i64) -> crate::error::BotResult<()> {
    let Some(g) = e.guild_id else { return Ok(()) };
    let cached = cache_of(app).and_then(|c| c.message(e.channel_id, e.id).map(|m| m.clone()));
    let edited = e.edited_timestamp.map(|t| t.timestamp_millis()).or_else(|| cached.as_ref().and_then(|m| m.edited_timestamp.map(|t| t.timestamp_millis())));
    // An uncached message that was not freshly edited (an embed unfurl, a pin) is no edit: skip the REST fetch.
    if cached.is_none() && old.is_none() && !recently_edited(edited, now) {
        return Ok(());
    }
    let new = match cached {
        Some(m) => m,
        None => {
            let Some(http) = app.http() else { return Ok(()) };
            match e.channel_id.message(http.as_ref(), e.id).await {
                Ok(m) => m,
                Err(_) => return Ok(()),
            }
        }
    };
    if new.author.bot || new.webhook_id.is_some() {
        if Some(new.author.id) != bot_id(app) {
            let was_loading = old.as_ref().map(|o| o.flags.is_some_and(|f| f.contains(MessageFlags::LOADING)));
            let channel = cached_channel(app, g, new.channel_id);
            let channel_id = new.channel_id;
            dispatch(app, BotMessage { guild_id: g, message: new, channel_id, channel, edited: true, was_loading }).await;
        }
        return Ok(());
    }
    let old_known = old.is_some();
    let is_edit = match &old {
        Some(o) => o.content != new.content,
        None => recently_edited(new.edited_timestamp.map(|t| t.timestamp_millis()), now),
    };
    if !is_edit {
        return Ok(());
    }
    let member = message_member(app, &new);
    let channel_id = new.channel_id;
    let channel = cached_channel(app, g, channel_id);
    dispatch(app, MessageUpdate { guild_id: g, old_message: old, message: new, old_known, member, channel_id, channel }).await;
    Ok(())
}

async fn on_reaction(app: &App, reaction: serenity::all::Reaction, add: bool) {
    let Some(user_id) = reaction.user_id else { return };
    if bot_id(app) == Some(user_id) {
        return;
    }
    // Partial reactions and messages still carry the guild id. Never fetch them here: hooks fetch lazily.
    let Some(g) = reaction.guild_id else { return };
    let known = reaction.member.as_ref().map(|m| m.user.clone()).or_else(|| cache_of(app).and_then(|c| c.user(user_id).map(|u| u.clone())));
    let app2 = app.clone();
    let finish = move |user: User, app: App| async move {
        if user.bot || !guild_cached(&app, g) {
            return;
        }
        let mut ev = ReactionEvent::new(g, reaction.clone(), user);
        if let Some(m) = reaction.member.clone() {
            ev.member = Lazy::resolved(Some(m));
        }
        if add {
            dispatch(&app, MessageReactionAdd(ev)).await;
        } else {
            dispatch(&app, MessageReactionRemove(ev)).await;
        }
    };
    match known {
        Some(user) => finish(user, app2).await,
        None => {
            // The user is partial: fetch it (the order of this one event is then not kept, as in the JS).
            spawn_event(if add { "messageReactionAdd" } else { "messageReactionRemove" }, async move {
                let Some(http) = app2.http() else { return Ok(()) };
                let Ok(user) = user_id.to_user(http.as_ref()).await else { return Ok(()) };
                finish(user, app2).await;
                Ok(())
            });
        }
    }
}

/// `userUpdate`: discord.js emits it from member and presence updates when the cached user changed; it is fanned
/// out to every available guild that has the user cached, with that guild's member (at most 4 at once).
async fn emit_user_update(app: &App, old: User, new: User) {
    if new.bot || !user_changed(&old, &new) {
        return;
    }
    let Some(cache) = cache_of(app) else { return };
    let unavailable: HashSet<GuildId> = cache.unavailable_guilds().iter().map(|e| *e.key()).collect();
    let guilds: Vec<(GuildId, Member)> = cache
        .guilds()
        .into_iter()
        .filter(|g| !unavailable.contains(g))
        .filter_map(|g| cached_member(&cache, g, new.id).map(|m| (g, m)))
        .collect();
    for (g, member) in guilds {
        dispatch(app, UserUpdate { guild_id: g, old_user: old.clone(), user: new.clone(), member: Some(member) }).await;
    }
}

fn on_ready(app: &App, gw: &Arc<GatewayState>, shard: u32, ready: serenity::all::Ready) {
    let pending: HashSet<GuildId> = ready.guilds.iter().filter(|g| g.unavailable).map(|g| g.id).collect();
    let generation = {
        let mut shards = gw.shards.lock().unwrap_or_else(|e| e.into_inner());
        let s = shards.entry(shard).or_default();
        if s.done {
            // A re-identify: discord.js does not fire ClientReady again.
            return;
        }
        s.pending = pending;
        s.generation += 1;
        s.generation
    };
    maybe_finish_startup(app, gw, shard, generation);
}

fn maybe_finish_startup(app: &App, gw: &Arc<GatewayState>, shard: u32, generation: u64) {
    let empty = gw.shards.lock().unwrap_or_else(|e| e.into_inner()).get(&shard).is_some_and(|s| s.pending.is_empty() && !s.done);
    if empty {
        finish_startup(app, gw, shard);
        return;
    }
    let (a, g) = (app.clone(), gw.clone());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(WAIT_GUILD_TIMEOUT_MS)).await;
        let current = g.shards.lock().unwrap_or_else(|e| e.into_inner()).get(&shard).map(|s| (s.generation, s.done));
        if current == Some((generation, false)) {
            finish_startup(&a, &g, shard);
        }
    });
}

fn on_guild_create(app: &App, gw: &Arc<GatewayState>, shard: u32, guild: Guild, before: Before) {
    let id = guild.id;
    let in_startup = {
        let mut shards = gw.shards.lock().unwrap_or_else(|e| e.into_inner());
        match shards.get_mut(&shard) {
            Some(s) if !s.done => {
                s.pending.remove(&id);
                s.generation += 1;
                Some(s.generation)
            }
            _ => None,
        }
    };
    if let Some(generation) = in_startup {
        // Part of this shard's startup: ClientReady runs the guildReady hooks for it.
        maybe_finish_startup(app, gw, shard, generation);
        return;
    }
    let a = app.clone();
    if before.guild_unavailable || before.guild_known {
        // guildAvailable.js: a guild that was unavailable at startup, or came back from an outage.
        spawn_event("guildAvailable", async move {
            let ran = run_guild_available(&a, id).await;
            let tail = if ran { "" } else { "; startup work ran recently, skipped" };
            bot_util::log_info!(format!("Guild became available: {} ({id}){tail}", guild.name));
            Ok(())
        });
    } else {
        // guildCreate.js: the bot joined.
        spawn_event("guildCreate", async move {
            a.settings().invalidate_settings(&id.to_string()).await;
            run_guild_ready(&a, id, GuildReadyReason::Join).await;
            bot_util::log_info!(format!("Joined guild \"{}\" ({id}).", guild.name));
            Ok(())
        });
    }
}

/// `ready.js` for one shard: the log line, the presence, the scheduler (once per process), the `ready` hooks, then
/// `runGuildReady` for the shard's available guilds, four at a time.
fn finish_startup(app: &App, gw: &Arc<GatewayState>, shard: u32) {
    {
        let mut shards = gw.shards.lock().unwrap_or_else(|e| e.into_inner());
        let s = shards.entry(shard).or_default();
        if s.done {
            return;
        }
        s.done = true;
    }
    app.set_ready_at_ms(app.clock().now_ms());
    let (a, g) = (app.clone(), gw.clone());
    spawn_event("ready", async move {
        let Some(cache) = cache_of(&a) else { return Ok(()) };
        let tag = crate::adapters::user_tag(&cache.current_user().clone().into());
        let mine: Vec<GuildId> = cache.guilds().into_iter().chain(cache.unavailable_guilds().iter().map(|e| *e.key())).filter(|id| g.shard_of(*id) == shard).collect::<HashSet<_>>().into_iter().collect();
        bot_util::log_success!(format!("Ready as {tag} | {} guild(s) | Shard {shard}", mine.len()));
        let runners = g.runners.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(runners) = runners
            && let Some(info) = runners.lock().await.get(&serenity::all::ShardId(shard))
        {
            info.runner_tx.set_activity(Some(ActivityData::watching("your server")));
        }
        a.scheduler().start(&a); // claims jobs of this process's available guilds only (idempotent)
        a.run_hooks(Ready {}).await;
        let unavailable: HashSet<GuildId> = cache.unavailable_guilds().iter().map(|e| *e.key()).collect();
        let available: Vec<GuildId> = mine.iter().copied().filter(|id| !unavailable.contains(id) && cache.guild(*id).is_some()).collect();
        let total = mine.len();
        let count = available.len();
        let app = a.clone();
        bot_util::async_util::map_limit(available, GUILD_READY_CONCURRENCY, move |id, _| {
            let app = app.clone();
            async move {
                run_guild_ready(&app, id, GuildReadyReason::Startup).await;
                Ok::<(), ()>(())
            }
        })
        .await;
        bot_util::log_info!(format!("guildReady completed for {count}/{total} guild(s)."));
        Ok(())
    });
}

/// `guildDelete.js`: clears the guild's cache keys in one SCAN pass, cancels its recurring jobs (and a running
/// backup restore), runs the `guildDelete` hooks. Database rows are kept, so a re-invite restores everything.
pub async fn on_guild_delete(app: &App, id: GuildId, guild: Option<Guild>) -> crate::error::BotResult<()> {
    let g = id.to_string();
    app.settings().invalidate_settings(&g).await;
    app.cache().invalidate_any(&format!("*:{g}:*"), &[format!("guild:{g}:*"), format!("xp:cooldown:{g}:*"), format!("voice:start:{g}:*")]).await;
    let mut types: Vec<String> = RECURRING_JOB_TYPES.iter().map(|t| t.as_str().to_string()).collect();
    types.push("backup.restore".into());
    app.scheduler().cancel_jobs(CancelJobs { guild_id: Some(g.clone()), types: Some(types), ..Default::default() }).await;
    let name = guild.as_ref().map(|x| x.name.clone()).unwrap_or_default();
    app.run_hooks(GuildDelete { guild_id: id, guild }).await;
    bot_util::log_info!(format!("Left guild \"{name}\" ({g}) — cache cleared, recurring jobs cancelled."));
    Ok(())
}
