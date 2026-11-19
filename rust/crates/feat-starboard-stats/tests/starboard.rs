//! Port of `test/starboard.test.js`, plus the golden, JS-sweep, database and cross-language checks.
//!
//! Offline: names, emoji parsing, filters, counting, the decision table, the repost payload (against the JS), the
//! coalescer, the plugin and command declarations, the reaction hook's early exits. With `TEST_DATABASE_URL`: the sync
//! engine end to end against a fake Discord layer (create / update / remove / repost / blocked / 10008), mirrored edits,
//! deletes, the CRUD, `/starboard` through the router, and both directions of the JS round trip (`TEST_REDIS_URL`
//! for the cached board list).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_core::App;
use bot_core::interaction_helpers::ChannelUsable;
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, database_url, redis_url, run_node};
use feat_starboard_stats::discord_io::DiscordError;
use feat_starboard_stats::starboard_io::{ReactUser, SbChannel, SbEmoji, SbGuild, StarboardIo, set_starboard_io};
use feat_starboard_stats::starboard_service::{self as sb, BoardError, BoardLite, BoardPatch, EntryState, SyncAction, UserList};
use serde_json::{Value, json};
use serenity::all::{Message, MessageReaction, ReactionType};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_starboard_stats::register(&mut reg);
    reg
}

const JS: &str = "rust/fixtures/gen/feat-starboard-stats-js.mjs";
const BOT: &str = "600000000000000001";
const EPOCH: i64 = 1_420_070_400_000;

fn user(n: u64) -> String {
    format!("7{n:017}")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

static SEQ: AtomicU64 = AtomicU64::new(1);

/// A snowflake created at `ts`.
fn snowflake_at(ts: i64) -> String {
    (((ts - EPOCH) as u64) << 22).to_string()
}

/// A message id created now.
fn next_message_id() -> String {
    ((((now_ms() - EPOCH) as u64) << 22) + SEQ.fetch_add(1, Ordering::SeqCst)).to_string()
}

fn base_board() -> BoardLite {
    BoardLite {
        id: "b1".into(),
        name: "main".into(),
        channel_id: "board-ch".into(),
        emojis: vec!["⭐".into()],
        threshold: 3,
        self_star: false,
        ignore_bots: true,
        remove_below_threshold: true,
        auto_react: false,
        max_message_age_days: 0,
        allow_nsfw: false,
        ignored_channel_ids: Vec::new(),
        remove_self_stars: false,
        enabled: true,
    }
}

// ── Messages ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct R {
    emoji: Value,
    users: Vec<String>,
    bots: Vec<String>,
    me: bool,
    count: Option<u64>,
}

fn star() -> Value {
    json!({ "id": null, "name": "⭐" })
}
fn glow() -> Value {
    json!({ "id": null, "name": "🌟" })
}
fn pog() -> Value {
    json!({ "id": "123456789012345678", "name": "pog", "animated": false })
}

fn r(emoji: Value, users: &[String]) -> R {
    R { emoji, users: users.to_vec(), bots: Vec::new(), me: false, count: None }
}

fn raw_message(id: &str, channel: &str, author: &str, bot: bool, content: &str) -> Value {
    json!({
        "id": id, "channel_id": channel,
        "author": { "id": author, "username": "alice", "global_name": "Alice", "discriminator": "0", "avatar": null, "bot": bot },
        "content": content, "timestamp": "2023-11-14T22:13:20.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
        "type": 0, "components": []
    })
}

fn to_message(raw: Value) -> Message {
    serde_json::from_value(raw).unwrap_or_else(|e| panic!("message does not deserialize: {e}"))
}

fn with_reactions(mut raw: Value, reactions: &[R]) -> Value {
    raw["reactions"] = Value::Array(
        reactions
            .iter()
            .map(|x| {
                let n = x.count.unwrap_or(x.users.len() as u64);
                json!({ "count": n, "count_details": { "burst": 0, "normal": n }, "me": x.me, "me_burst": false, "emoji": x.emoji, "burst_colors": [] })
            })
            .collect(),
    );
    raw
}

fn reaction(emoji: Value, count: u64) -> MessageReaction {
    serde_json::from_value(json!({ "count": count, "count_details": { "burst": 0, "normal": count }, "me": false, "me_burst": false, "emoji": emoji, "burst_colors": [] }))
        .unwrap_or_else(|e| panic!("reaction: {e}"))
}

// ── The fake Discord layer ───────────────────────────────────────────────────

#[derive(Default)]
struct World {
    ids: Mutex<(String, String)>,
    fetchable: Mutex<HashMap<String, SbChannel>>,
    guild: Mutex<SbGuild>,
    private: Mutex<HashSet<String>>,
    missing: Mutex<HashMap<String, Vec<String>>>,
    guild_missing: Mutex<Vec<String>>,
    messages: Mutex<HashMap<String, Message>>,
    users: Mutex<HashMap<(String, String), Vec<ReactUser>>>,
    fetches: Mutex<Vec<(String, bool, Option<String>)>>,
    sent: Mutex<Vec<(String, String, Value)>>,
    edits: Mutex<Vec<(String, Value)>>,
    deletes: Mutex<Vec<String>>,
    reacts: Mutex<Vec<String>>,
    removed: Mutex<Vec<String>>,
    send_error: Mutex<Option<DiscordError>>,
    edit_error: Mutex<Option<DiscordError>>,
    delete_error: Mutex<Option<DiscordError>>,
    react_error: Mutex<Option<DiscordError>>,
    custom: Mutex<Vec<SbEmoji>>,
    seq: AtomicU64,
}

fn l<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn key_of(emoji: &Value) -> String {
    emoji["id"].as_str().map(str::to_string).unwrap_or_else(|| emoji["name"].as_str().unwrap_or("").to_string())
}

impl World {
    fn new(guild_id: &str, n: u64) -> Arc<Self> {
        let w = World::default();
        *l(&w.ids) = (format!("4{:017}", n * 10 + 1), format!("4{:017}", n * 10 + 2));
        *l(&w.guild) = SbGuild {
            id: guild_id.into(),
            available: true,
            channels: vec![channel(&format!("4{:017}", n * 10 + 1), guild_id), channel(&format!("4{:017}", n * 10 + 2), guild_id)],
            emojis: Vec::new(),
        };
        Arc::new(w)
    }
    fn board_channel(&self) -> String {
        l(&self.ids).0.clone()
    }
    fn source(&self) -> SbChannel {
        let id = l(&self.ids).1.clone();
        l(&self.guild).channel(&id).cloned().unwrap_or_else(|| channel(&id, &self.guild_id()))
    }
    fn guild_id(&self) -> String {
        l(&self.guild).id.clone()
    }
    /// A message with reactions in `channel` (default: the source channel).
    fn message(&self, id: Option<&str>, author: &str, bot: bool, reactions: &[R], channel: Option<&str>) -> Message {
        let id = id.map(str::to_string).unwrap_or_else(next_message_id);
        let ch = channel.map(str::to_string).unwrap_or_else(|| self.source().id);
        let m = to_message(with_reactions(raw_message(&id, &ch, author, bot, "hello"), reactions));
        for x in reactions {
            let users = x.users.iter().map(|u| ReactUser { id: u.clone(), bot: x.bots.contains(u) }).collect();
            l(&self.users).insert((id.clone(), key_of(&x.emoji)), users);
        }
        l(&self.messages).insert(id, m.clone());
        m
    }
    fn sent_count(&self) -> usize {
        l(&self.sent).len()
    }
}

fn channel(id: &str, guild_id: &str) -> SbChannel {
    SbChannel { id: id.into(), guild_id: Some(guild_id.into()), name: format!("chan-{}", &id[id.len() - 3..]), kind: 0, ..Default::default() }
}

#[async_trait::async_trait]
impl StarboardIo for World {
    fn bot_user_id(&self, _app: &App) -> Option<String> {
        Some(BOT.into())
    }
    fn guild(&self, _app: &App, guild_id: &str) -> Option<SbGuild> {
        let g = l(&self.guild).clone();
        (g.id == guild_id).then_some(g)
    }
    fn leaks_private(&self, _app: &App, _g: &str, source_id: &str, target_id: &str) -> bool {
        let p = l(&self.private);
        p.contains(source_id) && !p.contains(target_id)
    }
    fn channel_usable(&self, _app: &App, _g: &str, channel_id: &str, needs: &[&str]) -> ChannelUsable {
        if l(&self.guild).channel(channel_id).is_none() {
            return ChannelUsable { ok: false, missing: Vec::new(), reason: Some("Channel not found".into()) };
        }
        let missing: Vec<String> = l(&self.missing).get(channel_id).cloned().unwrap_or_default().into_iter().filter(|m| needs.contains(&m.as_str())).collect();
        if missing.is_empty() {
            return ChannelUsable { ok: true, missing, reason: None };
        }
        let reason = format!("I need **{}** in <#{channel_id}>.", bot_core::permissions::permission_names(&missing).join(", "));
        ChannelUsable { ok: false, missing, reason: Some(reason) }
    }
    fn guild_missing(&self, _app: &App, _g: &str, perms: &[&str]) -> Vec<String> {
        l(&self.guild_missing).iter().filter(|m| perms.contains(&m.as_str())).cloned().collect()
    }
    fn custom_emoji(&self, _app: &App, emoji_id: &str) -> Option<SbEmoji> {
        l(&self.custom).iter().find(|e| e.id == emoji_id).cloned()
    }
    async fn fetch_channel(&self, _app: &App, channel_id: &str) -> Result<Option<SbChannel>, DiscordError> {
        if let Some(c) = l(&self.fetchable).get(channel_id).cloned() {
            return Ok(Some(c));
        }
        l(&self.guild).channel(channel_id).cloned().map(Some).ok_or_else(|| DiscordError::code(10003))
    }
    async fn fetch_message(&self, _app: &App, _c: &str, message_id: &str) -> Result<Message, DiscordError> {
        l(&self.messages).get(message_id).cloned().ok_or_else(|| DiscordError::code(10008))
    }
    async fn reaction_users(&self, _app: &App, _c: &str, message_id: &str, emoji: &ReactionType, burst: bool, after: Option<&str>) -> Result<Vec<ReactUser>, DiscordError> {
        let key = bot_core::hooks::events::emoji_key(emoji);
        l(&self.fetches).push((key.clone(), burst, after.map(str::to_string)));
        let mut users = l(&self.users).get(&(message_id.to_string(), key)).cloned().unwrap_or_default();
        users.sort_by_key(|u| u.id.parse::<u64>().unwrap_or(0));
        let after = after.and_then(|a| a.parse::<u64>().ok());
        Ok(users.into_iter().filter(|u| after.is_none_or(|a| u.id.parse::<u64>().unwrap_or(0) > a)).take(100).collect())
    }
    async fn send(&self, _app: &App, channel_id: &str, payload: &Value) -> Result<String, DiscordError> {
        if let Some(e) = l(&self.send_error).clone() {
            return Err(e);
        }
        let id = format!("5{:017}", self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        l(&self.sent).push((channel_id.into(), id.clone(), payload.clone()));
        Ok(id)
    }
    async fn edit(&self, _app: &App, _c: &str, message_id: &str, payload: &Value) -> Result<(), DiscordError> {
        if let Some(e) = l(&self.edit_error).clone() {
            return Err(e);
        }
        l(&self.edits).push((message_id.into(), payload.clone()));
        Ok(())
    }
    async fn delete(&self, _app: &App, _c: &str, message_id: &str) -> Result<(), DiscordError> {
        if let Some(e) = l(&self.delete_error).clone() {
            return Err(e);
        }
        l(&self.deletes).push(message_id.into());
        Ok(())
    }
    async fn react(&self, _app: &App, _c: &str, _m: &str, emoji: &ReactionType) -> Result<(), DiscordError> {
        if let Some(e) = l(&self.react_error).clone() {
            return Err(e);
        }
        l(&self.reacts).push(sb::reaction_text(emoji));
        Ok(())
    }
    async fn remove_reaction(&self, _app: &App, _c: &str, _m: &str, _e: &ReactionType, user_id: &str) -> Result<(), DiscordError> {
        l(&self.removed).push(user_id.into());
        Ok(())
    }
}

async fn until<F, Fut>(mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if f().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    false
}

// ═══════════════════════════ Offline cases ════════════════════════════════════

#[test]
fn t01_normalize_board_name() {
    assert_eq!(sb::normalize_board_name("  Hall of Fame! ").as_deref(), Some("hall-of-fame"));
    assert_eq!(sb::normalize_board_name("***"), None);
    assert_eq!(sb::normalize_board_name(""), None);
    assert_eq!(sb::normalize_board_name(&"x".repeat(40)).map(|s| s.len()), Some(32));
    assert_eq!(sb::normalize_board_name("a  b__c--d").as_deref(), Some("a-b__c-d"));
}

fn ok(s: &str, emojis: &[SbEmoji]) -> Vec<String> {
    sb::parse_emoji_list(s, emojis).unwrap_or_else(|e| panic!("{s}: {e}"))
}

#[test]
fn t02_parse_emoji_list() {
    assert_eq!(ok("⭐", &[]), vec!["⭐"]);
    assert_eq!(ok("⭐ 🌟", &[]).len(), 2);
    assert_eq!(ok("⭐🌟", &[]).len(), 2, "no space needed");
    assert_eq!(ok("<:pog:123456789012345678>", &[]), vec!["123456789012345678"]);
    assert_eq!(ok("<a:x:123456789012345678>", &[]), vec!["123456789012345678"]);
    assert_eq!(ok("123456789012345678", &[]), vec!["123456789012345678"]);
    for any in ["any", "ALL", "*"] {
        assert!(ok(any, &[]).is_empty());
    }
    assert_eq!(ok("⭐ ⭐", &[]), vec!["⭐"]);
    assert_eq!(ok("❤️ ❤", &[]), vec!["❤️"], "FE0F-insensitive, first spelling kept");
    assert_eq!(ok("1️⃣ 🇬🇧 👨‍👩‍👧", &[]).len(), 3);
    assert_eq!(ok("👍🏽 🏳️‍🌈 ❤️‍🔥 🫠 🏴󠁧󠁢󠁥󠁮󠁧󠁿", &[]).len(), 5);
    assert_eq!(ok("⭐, 🌟", &[]), vec!["⭐", "🌟"], "commas separate too");
    let guild = [SbEmoji { id: "100000000000000001".into(), name: "pog".into(), animated: false }];
    assert_eq!(ok(":pog:", &guild), vec!["100000000000000001"]);
    assert_eq!(ok(":POG:", &guild), vec!["100000000000000001"], "case-insensitive fallback");
    let nope = sb::parse_emoji_list(":nope:", &guild).unwrap_err();
    assert!(nope.contains(":nope:"));
    assert!(sb::parse_emoji_list("abc", &[]).unwrap_err().contains("not an emoji"));
    assert!(sb::parse_emoji_list("1", &[]).is_err(), "a bare digit is not an emoji");
    assert!(sb::parse_emoji_list("", &[]).is_err());
    assert!(sb::parse_emoji_list(" , ", &[]).is_err());
    let many = ["⭐", "🌟", "💫", "✨", "🔥", "💎", "🎉", "👍", "👏", "🙌", "🏆"];
    assert!(sb::parse_emoji_list(&many.join(" "), &[]).unwrap_err().contains("at most 10"));
    assert!(sb::parse_emoji_list(&many[..10].join(" "), &[]).is_ok());
}

#[test]
fn t02b_emoji_keys_formatting_and_the_unicode_matcher() {
    assert_eq!(sb::normalize_emoji_key("❤️"), "❤");
    assert_eq!(sb::emoji_key_of(&ReactionType::Unicode("⭐".into())), "⭐");
    for e in ["⭐", "❤️", "❤", "👍🏽", "👨‍👩‍👧", "🇬🇧", "1️⃣", "🏴󠁧󠁢󠁥󠁮󠁧󠁿", "🏳️‍🌈", "❤️‍🔥", "🫠"] {
        assert!(sb::is_unicode_emoji(e), "{e}");
    }
    for e in ["a", "1", "#", "", "ab"] {
        assert!(!sb::is_unicode_emoji(e), "{e:?}");
    }
    let none = |_: &str| None;
    assert_eq!(sb::format_emoji_key("⭐", &none, None), "⭐");
    assert_eq!(sb::format_emoji_key("123456789012345678", &none, None), "⭐", "unresolvable id falls back");
    let reactions = [reaction(pog(), 1)];
    assert_eq!(sb::format_emoji_key("123456789012345678", &none, Some(&reactions)), "<:pog:123456789012345678>");
    let client = |id: &str| (id == "123456789012345678").then(|| SbEmoji { id: id.into(), name: "pog".into(), animated: false });
    assert_eq!(sb::format_emoji_key("123456789012345678", &client, None), "<:pog:123456789012345678>");
    assert_eq!(sb::format_emoji_list(&[], &client), "any emoji");
    assert_eq!(sb::format_emoji_list(&["⭐".into(), "123456789012345678".into()], &client), "⭐ <:pog:123456789012345678>");
}

#[test]
fn t03_board_matches_emoji() {
    let with = |e: &[&str]| BoardLite { emojis: e.iter().map(|s| s.to_string()).collect(), ..base_board() };
    assert!(sb::board_matches_emoji(&with(&[]), Some("x")));
    assert!(sb::board_matches_emoji(&with(&[]), None));
    assert!(sb::board_matches_emoji(&with(&["❤️"]), Some("❤")));
    assert!(!sb::board_matches_emoji(&with(&["⭐"]), Some("🌟")));
    assert!(!sb::board_matches_emoji(&with(&["⭐"]), None));
    assert!(sb::board_matches_emoji(&with(&["123456789012345678"]), Some("123456789012345678")));
}

#[test]
fn t04_source_skip_reason() {
    let ch = |id: &str, parent: Option<&str>, nsfw: bool| SbChannel { id: id.into(), parent_id: parent.map(String::from), nsfw, ..Default::default() };
    let thread = |parent: &SbChannel, nsfw: bool| SbChannel {
        id: "thr".into(),
        parent_id: Some(parent.id.clone()),
        nsfw,
        is_thread: true,
        parent_nsfw: parent.nsfw,
        parent_parent_id: parent.parent_id.clone(),
        ..Default::default()
    };
    let board = BoardLite { ignored_channel_ids: vec!["ign".into(), "cat-ignored".into()], ..base_board() };
    let ids: HashSet<String> = ["board-ch", "other-board"].iter().map(|s| s.to_string()).collect();
    let reason = |c: &SbChannel, b: &BoardLite, bc: Option<&SbChannel>| sb::source_skip_reason(b, c, &ids, bc, || false);
    assert_eq!(reason(&ch("src", None, false), &board, None), None);
    assert_eq!(reason(&ch("board-ch", None, false), &board, None), Some("board-channel"));
    assert_eq!(reason(&ch("other-board", None, false), &board, None), Some("board-channel"));
    assert_eq!(reason(&thread(&ch("board-ch", None, false), false), &board, None), Some("board-channel"));
    assert_eq!(reason(&ch("ign", None, false), &board, None), Some("ignored"));
    assert_eq!(reason(&ch("src", Some("cat-ignored"), false), &board, None), Some("ignored"));
    assert_eq!(reason(&thread(&ch("ign", None, false), false), &board, None), Some("ignored"));
    assert_eq!(reason(&thread(&ch("p", Some("cat-ignored"), false), false), &board, None), Some("ignored"), "a thread's category counts");
    assert_eq!(reason(&ch("src", None, true), &board, None), Some("nsfw"));
    let allow = BoardLite { allow_nsfw: true, ..base_board() };
    assert_eq!(reason(&ch("src", None, true), &allow, Some(&ch("bc", None, false))), Some("nsfw"));
    assert_eq!(reason(&ch("src", None, true), &allow, None), Some("nsfw"));
    assert_eq!(reason(&ch("src", None, true), &allow, Some(&ch("bc", None, true))), None);
    assert_eq!(reason(&thread(&ch("p", None, true), false), &board, None), Some("nsfw"), "an NSFW parent of a thread is respected");
    assert_eq!(reason(&thread(&ch("p", None, false), true), &board, None), None, "the thread's own flag is ignored");
    assert_eq!(sb::source_skip_reason(&board, &ch("src", None, false), &ids, None, || true), Some("private"));
}

#[test]
fn t05_message_skip_reason_and_snowflake_time() {
    let now = 1_800_000_000_000;
    let board = base_board();
    let msg = |id: &str, author: &str, bot: bool, kind: u8, webhook: bool| {
        let mut raw = raw_message(id, "800000000000000001", author, bot, "x");
        raw["type"] = json!(kind);
        if webhook {
            raw["webhook_id"] = json!("100000000000000009");
        }
        to_message(raw)
    };
    let recent = snowflake_at(now - 1000);
    let skip = |m: &Message, b: &BoardLite, has: bool| sb::message_skip_reason(b, m, has, now, BOT);
    assert_eq!(skip(&msg(&recent, &user(1), false, 0, false), &board, false), None);
    assert_eq!(skip(&msg(&recent, &user(1), false, 7, false), &board, false), Some("system"));
    assert_eq!(skip(&msg(&recent, &user(1), false, 19, false), &board, false), None, "a reply is not a system message");
    assert_eq!(skip(&msg(&recent, BOT, true, 0, false), &board, false), Some("own-bot"));
    assert_eq!(skip(&msg(&recent, &user(2), true, 0, false), &board, false), Some("bot-author"));
    assert_eq!(skip(&msg(&recent, &user(2), true, 0, false), &board, true), None, "a tracked message keeps updating");
    assert_eq!(skip(&msg(&recent, &user(2), false, 0, true), &board, false), Some("bot-author"));
    assert_eq!(skip(&msg(&recent, &user(2), true, 0, false), &BoardLite { ignore_bots: false, ..base_board() }, false), None);
    let old = msg(&snowflake_at(now - 2 * 86_400_000), &user(1), false, 0, false);
    let aged = |d| BoardLite { max_message_age_days: d, ..base_board() };
    assert_eq!(skip(&old, &aged(1), false), Some("too-old"));
    assert_eq!(skip(&old, &aged(1), true), None);
    assert_eq!(skip(&old, &aged(3), false), None);
    assert_eq!(skip(&old, &aged(0), false), None);
    assert_eq!(sb::snowflake_time("175928847299117063"), 1462015105796);
}

#[test]
fn t06_compute_count() {
    let u = |id: &str, bot: bool| ReactUser { id: id.into(), bot };
    let list = |users: Vec<ReactUser>, extra: i64| UserList { users, extra };
    let count = |b: BoardLite, lists: Vec<UserList>| sb::compute_count(&b, &lists, Some("a"), Some("bot"));
    assert_eq!(count(base_board(), vec![list(vec![u("1", false), u("2", false), u("a", false)], 0)]), 2, "the author is excluded");
    assert_eq!(count(BoardLite { self_star: true, ..base_board() }, vec![list(vec![u("1", false), u("a", false)], 0)]), 2);
    assert_eq!(count(base_board(), vec![list(vec![u("1", false), u("x", true)], 0)]), 1, "bots are excluded with ignoreBots");
    assert_eq!(count(BoardLite { ignore_bots: false, ..base_board() }, vec![list(vec![u("1", false), u("x", true)], 0)]), 2);
    assert_eq!(count(BoardLite { ignore_bots: false, ..base_board() }, vec![list(vec![u("bot", true), u("1", false)], 0)]), 1, "the bot itself");
    assert_eq!(count(base_board(), vec![list(vec![u("1", false), u("2", false)], 0), list(vec![u("2", false), u("3", false)], 0)]), 3);
    assert_eq!(count(base_board(), vec![list(vec![u("1", false)], 7)]), 8, "extra is added");
    assert_eq!(count(base_board(), vec![list(vec![u("1", false)], 2_000_000_000)]), 1_000_000_000, "clamped");
    assert_eq!(count(base_board(), vec![]), 0);
}

#[test]
fn t07_decide_action_every_row_of_the_table() {
    let board = BoardLite { threshold: 3, ..base_board() };
    let no_below = BoardLite { threshold: 3, remove_below_threshold: false, ..base_board() };
    let live = |c| EntryState { blocked: false, board_message_id: Some("m".into()), count: c };
    let dropped = |c| EntryState { blocked: false, board_message_id: None, count: c };
    let blocked = |c| EntryState { blocked: true, board_message_id: None, count: c };
    let d = |e: Option<EntryState>, c, b: &BoardLite| sb::decide_action(e.as_ref(), c, b);
    use SyncAction as A;
    assert_eq!(d(None, 3, &board), A::Create);
    assert_eq!(d(None, 2, &board), A::None);
    assert_eq!(d(Some(live(5)), 2, &board), A::Remove);
    assert_eq!(d(Some(live(5)), 2, &no_below), A::Update);
    assert_eq!(d(Some(live(2)), 2, &no_below), A::None);
    assert_eq!(d(Some(live(3)), 4, &board), A::Update);
    assert_eq!(d(Some(live(3)), 3, &board), A::None);
    assert_eq!(d(Some(dropped(2)), 3, &board), A::Repost);
    assert_eq!(d(Some(dropped(2)), 1, &board), A::Store);
    assert_eq!(d(Some(dropped(2)), 2, &board), A::None);
    assert_eq!(d(Some(blocked(2)), 9, &board), A::Store);
    assert_eq!(d(Some(blocked(9)), 9, &board), A::None);
    assert_eq!(d(Some(blocked(9)), 1, &board), A::Store);
    assert_eq!(SyncAction::Repost.as_str(), "repost");
}

fn att(id: &str, name: &str, content_type: &str) -> Value {
    json!({ "id": id, "filename": name, "url": format!("https://cdn.example/{name}"), "proxy_url": format!("https://cdn.example/{name}"), "size": 1, "content_type": content_type })
}

#[test]
fn t08_build_repost_payload() {
    let m = to_message(raw_message("300000000000000001", "800000000000000001", "700000000000000001", false, "hello"));
    let p = sb::build_repost_payload("900000000000000001", &m, 5, "⭐");
    assert_eq!(p["content"], "⭐ **5** · <#800000000000000001>");
    assert_eq!(p["allowed_mentions"], json!({ "parse": [] }));
    let button = &p["components"][0]["components"][0];
    assert_eq!(button["url"], "https://discord.com/channels/900000000000000001/800000000000000001/300000000000000001");
    assert_eq!(button["style"], 5, "Link style");
    assert!(button.get("custom_id").is_none());
    let e = &p["embeds"][0];
    assert_eq!(e["author"]["name"], "Alice");
    assert_eq!(e["description"], "hello");
    assert_eq!(e["timestamp"], "2023-11-14T22:13:20.000Z");
    assert!(e.get("title").is_none() && e.get("footer").is_none());

    let mut long = raw_message("300000000000000002", "800000000000000001", "700000000000000001", false, &"x".repeat(5000));
    let lp = sb::build_repost_payload("900000000000000001", &to_message(long.clone()), 1, "⭐");
    assert!(lp["embeds"][0]["description"].as_str().unwrap_or("").chars().count() <= 4096);

    long["content"] = json!("");
    long["attachments"] = json!([att("1", "SPOILER_x.png", "image/png"), att("2", "pic.png", "image/png"), att("3", "doc.pdf", "application/pdf")]);
    let ej = sb::build_repost_payload("900000000000000001", &to_message(long.clone()), 1, "⭐");
    assert_eq!(ej["embeds"][0]["image"]["url"], "https://cdn.example/pic.png", "the first NON-spoiler image");
    let field = ej["embeds"][0]["fields"][0]["value"].as_str().unwrap_or("").to_string();
    assert!(field.contains("||[SPOILER_x.png](https://cdn.example/SPOILER_x.png)||"), "{field}");
    assert!(field.contains("doc.pdf"));
    assert!(!field.contains("pic.png"), "the embedded image is not listed again");

    let mut gifv = raw_message("300000000000000003", "800000000000000001", "700000000000000001", false, "");
    gifv["embeds"] = json!([{ "type": "gifv", "thumbnail": { "url": "https://t.example/g.gif" } }]);
    assert_eq!(sb::select_image(&to_message(gifv)).map(|i| i.url).as_deref(), Some("https://t.example/g.gif"));
    let mut stickers = raw_message("300000000000000004", "800000000000000001", "700000000000000001", false, "hi");
    stickers["sticker_items"] = json!([{ "id": "100000000000000011", "name": "wave", "format_type": 1 }, { "id": "100000000000000012", "name": "lottie", "format_type": 3 }]);
    let sj = sb::build_repost_payload("900000000000000001", &to_message(stickers), 1, "⭐");
    assert!(sj["embeds"][0]["description"].as_str().unwrap_or("").contains("Sticker: **wave**"));
    assert_eq!(sj["embeds"][0]["image"]["url"], "https://cdn.discordapp.com/stickers/100000000000000011.png");

    // The worst case stays under the 6000-character embed total.
    let mut worst = raw_message("300000000000000005", "800000000000000001", "700000000000000001", false, &"y".repeat(6000));
    worst["attachments"] = Value::Array((1..=50).map(|i| att(&i.to_string(), &format!("file-{}-{i}.pdf", "n".repeat(70)), "application/pdf")).collect());
    let wj = sb::build_repost_payload("900000000000000001", &to_message(worst), 1, "⭐");
    let e = &wj["embeds"][0];
    let len = |v: &Value| v.as_str().map(|s| s.encode_utf16().count()).unwrap_or(0);
    let total = len(&e["description"]) + len(&e["author"]["name"]) + len(&e["fields"][0]["name"]) + len(&e["fields"][0]["value"]);
    assert!(total <= 6000, "embed total {total}");
    assert!(len(&e["fields"][0]["value"]) <= 1024);
}

/// The repost payload equals what the JS builds from the same raw messages.
#[test]
fn t08b_repost_payloads_match_the_js() {
    let g = "900000000000000001";
    let mut cases = Vec::new();
    let base = |id: &str, content: &str| raw_message(id, "800000000000000001", "700000000000000001", false, content);
    cases.push(base("300000000000000001", "hello"));
    let mut a = base("300000000000000002", "");
    a["attachments"] = json!([att("1", "SPOILER_x.png", "image/png"), att("2", "pic.png", "image/png"), att("3", "doc[1].pdf", "application/pdf")]);
    cases.push(a);
    let mut e = base("300000000000000003", "");
    e["embeds"] = json!([{ "type": "rich", "title": "t", "description": "from embed", "image": { "url": "https://i.example/x.png" } }]);
    cases.push(e);
    let mut s = base("300000000000000004", "hi");
    s["sticker_items"] = json!([{ "id": "100000000000000011", "name": "wave", "format_type": 4 }]);
    cases.push(s);
    let mut m = base("300000000000000005", &"z".repeat(4200));
    m["author"]["avatar"] = json!("a_0123456789abcdef0123456789abcdef");
    m["member"] = json!({ "nick": "Nick", "avatar": "0123456789abcdef0123456789abcdef", "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0 });
    cases.push(m);
    let mut n = base("300000000000000006", "");
    n["author"]["global_name"] = Value::Null;
    n["author"]["avatar"] = json!("0123456789abcdef0123456789abcdef");
    cases.push(n);
    let input: Vec<Value> = cases.iter().map(|c| json!({ "guildId": g, "message": c, "count": 1234, "emojiText": "<:pog:123456789012345678>" })).collect();
    let js = run_node(JS, &["payload", &serde_json::to_string(&input).unwrap_or_default()]);
    let js = js.as_array().cloned().unwrap_or_default();
    assert_eq!(js.len(), cases.len(), "{js:?}");
    for (raw, expected) in cases.iter().zip(js) {
        let ours = sb::build_repost_payload(g, &to_message(raw.clone()), 1234, "<:pog:123456789012345678>");
        assert_eq!(ours, expected, "message {}", raw["id"]);
    }
}

#[test]
fn t09_pick_display_emoji() {
    let none = |_: &str| None;
    let with = |e: &[&str]| BoardLite { emojis: e.iter().map(|s| s.to_string()).collect(), ..base_board() };
    assert_eq!(sb::pick_display_emoji(&with(&["⭐", "🌟"]), &[], &none, None), "⭐");
    let q = [reaction(star(), 2), reaction(glow(), 5), reaction(pog(), 5)];
    assert_eq!(sb::pick_display_emoji(&with(&[]), &q, &none, None), "🌟", "highest count, ties to the first");
    assert_eq!(sb::pick_display_emoji(&with(&[]), &[], &none, None), "⭐");
    assert_eq!(sb::pick_display_emoji(&with(&[]), &[reaction(pog(), 1)], &none, None), "<:pog:123456789012345678>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t10_the_coalescer_semaphore_and_spacing() {
    let mutex = bot_core::sync::KeyedMutex::new();
    let sem = bot_util::async_util::create_semaphore(4);
    let c = sb::Coalescer::new(mutex.clone(), sem.clone(), Arc::new(|_: &str| 0));
    let runs = Arc::new(AtomicU64::new(0));
    let job = |runs: Arc<AtomicU64>| move || async move {
        runs.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let futs: Vec<_> = (0..10).filter_map(|_| c.request("m", job(runs.clone()))).collect();
    for f in futs {
        f.await;
    }
    let n = runs.load(Ordering::SeqCst);
    assert!((1..=2).contains(&n), "ran {n} times");
    assert_eq!(c.waiting_size(), 0);

    // Running + waiting: a request while one runs is accepted, the next one is coalesced.
    runs.store(0, Ordering::SeqCst);
    let first = c.request("k", job(runs.clone()));
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = c.request("k", job(runs.clone()));
    let third = c.request("k", job(runs.clone()));
    assert!(first.is_some() && second.is_some());
    assert!(third.is_none());
    if let (Some(a), Some(b)) = (first, second) {
        a.await;
        b.await;
    }
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    // Different keys run concurrently, up to the semaphore size.
    let active = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let mut futs = Vec::new();
    for i in 0..6 {
        let (a, p) = (active.clone(), peak.clone());
        futs.extend(c.request(&format!("key{i}"), move || async move {
            let now = a.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await;
            a.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for f in futs {
        f.await;
    }
    assert_eq!(peak.load(Ordering::SeqCst), 4);
    assert_eq!((sem.active(), sem.queued()), (0, 0));

    // A failing run does not wedge the key or the semaphore.
    let bad = c.request("bad", || async { Err::<(), &str>("boom") });
    assert_eq!(bad.expect("accepted").await, Err("boom"));
    assert_eq!(c.request("bad", || async { "ok" }).expect("accepted").await, "ok");

    // Spacing: the next run for a key starts at least spacingMs after the previous one ended.
    let spaced = sb::Coalescer::new(mutex.clone(), sem.clone(), Arc::new(|_: &str| 80));
    spaced.request("s", || async {}).expect("accepted").await;
    let ended = std::time::Instant::now();
    let started = spaced.request("s", || async { std::time::Instant::now() }).expect("accepted").await;
    assert!(started.duration_since(ended) >= Duration::from_millis(70), "waited {:?}", started.duration_since(ended));

    // The spacing sleep does not hold the message's mutex.
    let per_key = sb::Coalescer::new(mutex.clone(), sem.clone(), Arc::new(|k: &str| if k == "hot" { 120 } else { 10 }));
    per_key.request("hot", || async {}).expect("accepted").await;
    let sleeping = per_key.request("hot", || async {}).expect("accepted");
    let h = tokio::spawn(sleeping);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let took = std::time::Instant::now();
    mutex.run("hot", || async {}).await;
    assert!(took.elapsed() < Duration::from_millis(60), "the mutex was free while the next sync waited");
    let _ = h.await;
    const { assert!(sb::REPOST_SYNC_SPACING_MS > sb::SYNC_SPACING_MS) };
}

#[test]
fn t11_t12_plugin_and_command_declarations() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["starboard", "stats"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "starboard");
    bot_core::testing::assert_plugin_matches_golden(&reg, "stat-channels");
    let p = reg.plugin_named("starboard").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Starboard));
    for h in &p.hooks {
        assert_eq!(h.id(), "starboard");
        assert!(bot_config::hook_index(h.event().as_str(), "starboard").is_some(), "{} lists starboard", h.event().as_str());
    }
    let json = feat_starboard_stats::commands::starboard::command().payload();
    let subs: Vec<&str> = json["options"].as_array().into_iter().flatten().filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(subs, vec!["create", "edit", "delete", "list"]);
    assert_eq!(json["options"][0]["options"].as_array().map(Vec::len), Some(12));
    assert_eq!(json["options"][1]["options"].as_array().map(Vec::len), Some(16));
    assert_eq!(json["default_member_permissions"], "32");
    let meta = feat_starboard_stats::commands::starboard::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("starboard"), json!(false), json!(2)));
}

#[tokio::test]
async fn t13_reaction_filter_and_guild_may_have_boards() {
    let t = TestApp::new();
    let g = "955018000000000050";
    assert!(sb::reaction_filter(&t, g, Some("⭐")), "unknown memo");
    assert!(sb::guild_may_have_boards(&t, g));
    assert!(!sb::reaction_filter(&t, "", Some("⭐")));
    assert!(!sb::guild_may_have_boards(&t, ""));
    sb::set_boards_memo(&t, g, Some(Vec::new()));
    assert!(!sb::reaction_filter(&t, g, Some("⭐")));
    assert!(!sb::guild_may_have_boards(&t, g));
    sb::set_boards_memo(&t, g, Some(vec![base_board()]));
    assert!(!sb::reaction_filter(&t, g, Some("🌟")));
    assert!(sb::reaction_filter(&t, g, Some("⭐")));
    assert!(sb::reaction_filter(&t, g, None), "RemoveAll has no emoji");
    assert!(sb::guild_may_have_boards(&t, g));
    sb::set_boards_memo(&t, g, Some(vec![BoardLite { enabled: false, ..base_board() }]));
    assert!(!sb::reaction_filter(&t, g, Some("⭐")), "a disabled board never counts");
    sb::set_boards_memo(&t, g, Some(vec![BoardLite { enabled: false, ..base_board() }, BoardLite { id: "b2".into(), emojis: vec![], ..base_board() }]));
    assert!(sb::reaction_filter(&t, g, Some("🌟")), "an \"any\" board takes every emoji");
    // The memo expires after BOARD_MEMO_TTL_MS.
    t.clock.advance(sb::BOARD_MEMO_TTL_MS);
    assert!(sb::peek_boards(&t, g).is_none());
}

#[tokio::test]
async fn t14_fetch_reaction_users_pages_and_truncates() {
    let t = TestApp::new();
    let w = World::new("955018000000000051", 51);
    set_starboard_io(&t, w.clone());
    let setup = |n: u64, count: u64, burst: u64| {
        let id = next_message_id();
        let users: Vec<ReactUser> = (1..=n).map(|i| ReactUser { id: user(i), bot: false }).collect();
        l(&w.users).insert((id.clone(), "⭐".into()), users);
        l(&w.fetches).clear();
        let r: MessageReaction = serde_json::from_value(json!({ "count": count, "count_details": { "burst": burst, "normal": count }, "me": false, "me_burst": false, "emoji": star(), "burst_colors": [] }))
            .unwrap_or_else(|e| panic!("{e}"));
        (id, r)
    };
    let (id, small) = setup(250, 250, 0);
    let res = sb::fetch_reaction_users(&t, "c", &id, &small).await.expect("fetched");
    assert_eq!((res.users.len(), res.extra), (250, 0));
    let calls = l(&w.fetches).clone();
    assert_eq!(calls.len(), 3);
    assert!(!calls[0].1, "ReactionType.Normal");
    assert_eq!(calls[1].2.as_deref(), Some(user(100).as_str()));

    let (id, exact) = setup(100, 100, 0);
    assert_eq!(sb::fetch_reaction_users(&t, "c", &id, &exact).await.expect("fetched").users.len(), 100);
    assert_eq!(l(&w.fetches).len(), 2, "a full page needs one more call to see the end");

    let (id, huge) = setup(1200, 1500, 0);
    let h = sb::fetch_reaction_users(&t, "c", &id, &huge).await.expect("fetched");
    assert_eq!(h.users.len(), sb::MAX_USER_PAGES * 100);
    assert_eq!(h.extra, 500, "the unverified remainder");
    assert_eq!(l(&w.fetches).len(), sb::MAX_USER_PAGES);

    let (id, burst) = setup(3, 5, 2);
    sb::fetch_reaction_users(&t, "c", &id, &burst).await.expect("fetched");
    assert_eq!(l(&w.fetches).iter().map(|f| f.1).collect::<Vec<_>>(), vec![false, true], "super reactions are fetched too");
}

fn input(w: &World, kind: sb::ReactionEventKind, message_id: &str, channel: &str, key: &str, user_id: &str, author: &str) -> sb::ReactionInput {
    sb::ReactionInput {
        kind,
        guild_id: w.guild_id(),
        channel_id: channel.into(),
        message_id: message_id.into(),
        emoji_key: Some(key.into()),
        emoji: Some(ReactionType::Unicode(key.into())),
        user_id: Some(user_id.into()),
        message_author_id: Some(author.into()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t15_handle_reaction_early_exits_self_shortcut_and_coalescing() {
    let t = TestApp::new();
    let w = World::new("955018000000000052", 52);
    set_starboard_io(&t, w.clone());
    let calls: Arc<Mutex<Vec<sb::SyncRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    sb::set_sync_impl(
        &t,
        Some(Arc::new(move |_app, req| {
            let c = c2.clone();
            Box::pin(async move {
                l(&c).push(req);
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(())
            })
        })),
    );
    let g = w.guild_id();
    let board = BoardLite { channel_id: w.board_channel(), ..base_board() };
    sb::set_boards_memo(&t, &g, Some(vec![board.clone()]));
    let msg = w.message(None, &user(1), false, &[], None);
    let id = msg.id.to_string();
    let src = w.source().id;
    use sb::ReactionEventKind::*;
    assert_eq!(sb::handle_reaction(&t, input(&w, Add, &id, &src, "🌟", &user(2), &user(1))).await, "skipped", "no board takes this emoji");
    assert_eq!(sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(1), &user(1))).await, "self");
    assert!(l(&calls).is_empty());
    assert_eq!(sb::handle_reaction(&t, input(&w, Add, &id, &w.board_channel(), "⭐", &user(2), &user(1))).await, "skipped", "board channel");
    l(&w.private).insert(src.clone());
    assert_eq!(sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(2), &user(1))).await, "skipped", "a private source");
    l(&w.private).clear();
    sb::set_boards_memo(&t, &g, Some(vec![BoardLite { enabled: false, ..board.clone() }]));
    assert_eq!(sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(2), &user(1))).await, "skipped", "a disabled board");
    sb::set_boards_memo(&t, &g, Some(vec![BoardLite { self_star: true, ..board.clone() }]));
    let mut outcomes = Vec::new();
    for _ in 0..5 {
        outcomes.push(sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(1), &user(1))).await);
    }
    assert!(outcomes.contains(&"coalesced"), "{outcomes:?}");
    tokio::time::sleep(Duration::from_millis(80)).await;
    let n = l(&calls).len();
    assert!((1..=2).contains(&n), "{n} syncs for 5 events");
    assert_eq!(l(&calls)[0].message_id, id);
    assert_eq!(l(&calls)[0].channel.id, src);
    sb::set_sync_impl(&t, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t16_remove_self_stars() {
    let t = TestApp::new();
    let w = World::new("955018000000000053", 53);
    set_starboard_io(&t, w.clone());
    sb::set_sync_impl(&t, Some(Arc::new(|_app, _req| Box::pin(async { Ok(()) }))));
    let g = w.guild_id();
    sb::set_boards_memo(&t, &g, Some(vec![BoardLite { channel_id: w.board_channel(), remove_self_stars: true, ..base_board() }]));
    let msg = w.message(None, &user(1), false, &[r(star(), &[user(1)])], None);
    let id = msg.id.to_string();
    let src = w.source().id;
    use sb::ReactionEventKind::*;
    assert_eq!(sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(1), &user(1))).await, "self");
    assert!(until(|| async { l(&w.removed).len() == 1 }).await);
    assert_eq!(*l(&w.removed), vec![user(1)]);
    // Somebody else reacting is untouched; so is another emoji.
    sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(2), &user(1))).await;
    sb::handle_reaction(&t, input(&w, Add, &id, &src, "🌟", &user(1), &user(1))).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(l(&w.removed).len(), 1);
    // An unknown author is found through the message.
    let mut partial = input(&w, Add, &id, &src, "⭐", &user(1), &user(1));
    partial.message_author_id = None;
    sb::handle_reaction(&t, partial).await;
    assert!(until(|| async { l(&w.removed).len() == 2 }).await);
    // Without Manage Messages nothing is removed.
    l(&w.missing).insert(src.clone(), vec!["ManageMessages".into()]);
    sb::handle_reaction(&t, input(&w, Add, &id, &src, "⭐", &user(1), &user(1))).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(l(&w.removed).len(), 2);
    l(&w.missing).clear();
    // A Remove event never removes anything.
    sb::handle_reaction(&t, input(&w, Remove, &id, &src, "⭐", &user(1), &user(1))).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(l(&w.removed).len(), 2);
    sb::set_sync_impl(&t, None);
}

#[test]
fn t17_raw_sql_has_no_bare_now() {
    assert_eq!(&sb::RAW_SQL[..3].iter().map(|(n, _)| *n).collect::<Vec<_>>(), &["starboard.lockGuildSettings", "starboard.lockBoard", "starboard.removeIgnoredChannel"]);
    for (name, sql) in sb::RAW_SQL {
        let lower = sql.to_lowercase();
        for (i, _) in lower.match_indices("now()") {
            assert!(lower[i..].starts_with("now() at time zone 'utc'"), "{name}: {sql}");
        }
    }
    assert!(sb::LOCK_BOARD_SQL.contains("FOR UPDATE"));
    assert!(sb::REMOVE_IGNORED_CHANNEL_SQL.contains("AT TIME ZONE 'UTC'"));
}

#[test]
fn t18_settings_summary_matches_the_js() {
    let none = |_: &str| None;
    let s = sb::settings_summary(&BoardLite { channel_id: "c9".into(), max_message_age_days: 7, remove_self_stars: true, ..base_board() }, &none, None);
    assert_eq!(s.lines().count(), 4);
    assert!(s.contains("Channel: <#c9> · Emojis: ⭐"));
    assert!(s.contains("Self-star: removed"));
    assert!(s.contains("Max age: 7 days"));
    assert!(!s.contains("Reposts"));
    assert!(sb::settings_summary(&base_board(), &none, Some(4)).contains("Reposts: 4"));
    let boards = [
        BoardLite { channel_id: "c9".into(), max_message_age_days: 7, remove_self_stars: true, ..base_board() },
        BoardLite { emojis: vec![], self_star: true, ignore_bots: false, allow_nsfw: true, auto_react: true, remove_below_threshold: false, ..base_board() },
    ];
    let mut input: Vec<Value> = boards.iter().map(|b| serde_json::to_value(b).unwrap_or_default()).collect();
    input[1]["reposts"] = json!(4);
    let js = run_node(JS, &["summary", &serde_json::to_string(&input).unwrap_or_default()]);
    assert_eq!(js, json!([sb::settings_summary(&boards[0], &none, None), sb::settings_summary(&boards[1], &none, Some(4))]));
}

/// Names, emoji lists and the unicode matcher agree with the JS on a sweep.
#[test]
fn sweeps_agree_with_the_js() {
    let names = ["  Hall of Fame! ", "***", "", "a  b__c--d", "Ünïcødé board", "x".repeat(40).as_str(), "--edge--", "tab\tand\nnewline", "UPPER_case-1"]
        .map(String::from);
    let js = run_node(JS, &["names", &serde_json::to_string(&names).unwrap_or_default()]);
    let ours: Vec<Value> = names.iter().map(|n| sb::normalize_board_name(n).map(Value::from).unwrap_or(Value::Null)).collect();
    assert_eq!(js, Value::Array(ours));

    let graphemes = [
        "⭐", "❤️", "❤", "👍🏽", "👨‍👩‍👧", "🇬🇧", "1️⃣", "#⃣", "🏴󠁧󠁢󠁥󠁮󠁧󠁿", "🏳️‍🌈", "❤️‍🔥", "🫠", "a", "1", "#", "", "ab", "©", "©️", "™", "↔️", "🇺", "☺", "☺️", "🧑🏿‍🦰", "😀😀", "✔", "‼️",
    ];
    let js = run_node(JS, &["unicode", &serde_json::to_string(&graphemes).unwrap_or_default()]);
    assert_eq!(js, Value::Array(graphemes.iter().map(|g| Value::Bool(sb::is_unicode_emoji(g))).collect()));

    let inputs = [
        "⭐", "⭐ 🌟", "⭐🌟", "<:pog:123456789012345678>", "<a:x:123456789012345678> ⭐", "any", "ALL", "*", "⭐ ⭐", "❤️ ❤", "1️⃣ 🇬🇧 👨‍👩‍👧",
        "👍🏽 🏳️‍🌈 ❤️‍🔥 🫠 🏴󠁧󠁢󠁥󠁮󠁧󠁿", "⭐, 🌟", ":pog:", ":POG:", ":nope:", "abc", "1", "", " , ", "⭐ 🌟 💫 ✨ 🔥 💎 🎉 👍 👏 🙌 🏆", "⭐abc",
        "12345678901234567", "<:bad name:123456789012345678>", &format!("\"{}\"", "w".repeat(50)),
    ];
    let emojis = [SbEmoji { id: "100000000000000001".into(), name: "pog".into(), animated: false }];
    let arg = json!({ "inputs": inputs, "emojis": [{ "id": "100000000000000001", "name": "pog" }] });
    let js = run_node(JS, &["emoji", &arg.to_string()]);
    let ours: Vec<Value> = inputs
        .iter()
        .map(|s| match sb::parse_emoji_list(s, &emojis) {
            Ok(e) => json!({ "ok": true, "emojis": e }),
            Err(e) => json!({ "ok": false, "error": e }),
        })
        .collect();
    assert_eq!(js, Value::Array(ours));
}

/// A cached board list as the JS wrote it, element by element with the JS defaults; a value that is not a board is
/// skipped, never an error.
#[test]
fn cached_board_lite_shape() {
    let cached = json!([
        { "id": "a", "name": "main", "channelId": "1", "emojis": ["⭐"], "threshold": 3, "selfStar": false, "ignoreBots": true,
          "removeBelowThreshold": true, "autoReact": false, "maxMessageAgeDays": 0, "allowNsfw": false, "ignoredChannelIds": [],
          "removeSelfStars": false, "enabled": true },
        { "id": "b", "channelId": "2", "threshold": 2.5 },
        { "id": 3, "channelId": "x" },
        "junk"
    ]);
    let boards: Vec<BoardLite> = cached.as_array().into_iter().flatten().filter_map(sb::to_board_lite).collect();
    assert_eq!(boards.len(), 2);
    assert_eq!(serde_json::to_value(&boards[0]).unwrap_or_default(), cached[0], "written back JSON-equal, in the JS key order");
    let keys: Vec<String> = serde_json::to_value(&boards[0]).ok().and_then(|v| v.as_object().map(|o| o.keys().cloned().collect())).unwrap_or_default();
    assert_eq!(
        keys,
        vec!["id", "name", "channelId", "emojis", "threshold", "selfStar", "ignoreBots", "removeBelowThreshold", "autoReact", "maxMessageAgeDays", "allowNsfw", "ignoredChannelIds", "removeSelfStars", "enabled"]
    );
    assert_eq!((boards[1].threshold, boards[1].ignore_bots, boards[1].enabled, boards[1].name.as_str()), (3, true, true, ""));
    assert_eq!(sb::boards_key("123"), "guild:123:starboards");
    assert_eq!(sb::contains_pattern("a_b%c\\"), "%a_b%c\\%", "Prisma's contains does not escape LIKE wildcards");
}

// ═══════════════════════════ Database cases ═══════════════════════════════════

fn gid(n: u64) -> String {
    format!("95501800000000{n:04}")
}

/// `TEST_DATABASE_URL`, or `None` after printing that the test is skipped.
fn db_url() -> Option<String> {
    let url = database_url();
    if url.is_none() {
        println!("skipped: TEST_DATABASE_URL not set");
    }
    url
}

async fn db(n: u64) -> Option<(DbTestApp, String, Arc<World>)> {
    let url = db_url()?;
    let g = gid(n);
    let t = DbTestApp::new(&url).await.with_guilds([g.clone()]);
    t.reset_guilds(&[&g]).await;
    t.settings().ensure_guild_settings(&g).await.expect("settings row");
    sb::invalidate_boards(&t, &g).await;
    let w = World::new(&g, n);
    set_starboard_io(&t, w.clone());
    Some((t, g, w))
}

async fn mk_board(t: &App, g: &str, w: &World, patch: BoardPatch) -> bot_db::Starboard {
    let p = BoardPatch { name: Some(patch.name.clone().unwrap_or_else(|| "main".into())), channel_id: Some(patch.channel_id.clone().unwrap_or_else(|| w.board_channel())), ..patch };
    sb::create_board(t, g, p).await.expect("board created")
}

async fn entries_of(t: &App, board_id: &str) -> Vec<bot_db::StarboardEntry> {
    sqlx::query_as::<_, bot_db::StarboardEntry>(r#"SELECT * FROM "StarboardEntry" WHERE "starboardId" = $1 ORDER BY "createdAt""#)
        .bind(board_id)
        .fetch_all(t.db())
        .await
        .expect("entries")
}

async fn sync(t: &App, w: &World, m: &Message) -> anyhow::Result<()> {
    let channel = l(&w.guild).channel(&m.channel_id.to_string()).cloned().unwrap_or_else(|| w.source());
    sb::sync_message(t, sb::SyncRequest { guild_id: w.guild_id(), channel, message_id: m.id.to_string(), message: None }).await
}

#[tokio::test]
async fn db_every_raw_statement_prepares_against_the_schema() {
    let Some(url) = db_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.expect("tx");
    for (name, sql) in sb::RAW_SQL {
        sqlx::Executor::prepare(&mut *tx, sqlx::SqlSafeStr::into_sql_str(*sql)).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let n = sqlx::query(sb::REMOVE_IGNORED_CHANNEL_SQL).bind("0").bind("0").execute(&mut *tx).await.expect("runs");
    assert_eq!(n.rows_affected(), 0);
    tx.rollback().await.expect("rollback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_create_board_enforces_the_cap_unique_names_and_defaults() {
    let Some((t, g, w)) = db(1).await else { return };
    let limit = bot_config::limits::STARBOARDS;
    let mut handles = Vec::new();
    for i in 0..limit + 2 {
        let (a, g2, c) = (t.app.clone(), g.clone(), w.board_channel());
        handles.push(tokio::spawn(async move {
            sb::create_board(&a, &g2, BoardPatch { name: Some(format!("b{i}")), channel_id: Some(c), ..Default::default() }).await
        }));
    }
    let mut ok = 0;
    let mut capped = 0;
    for h in handles {
        match h.await.expect("joined") {
            Ok(_) => ok += 1,
            Err(BoardError::Limit) => capped += 1,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!((ok, capped), (limit, 2));
    let names: Vec<(String,)> = sqlx::query_as(r#"SELECT "name" FROM "Starboard" WHERE "guildId" = $1 ORDER BY "name""#).bind(&g).fetch_all(t.db()).await.expect("names");
    let (kept, victim) = (names[0].0.clone(), names[1].0.clone());
    sqlx::query(r#"DELETE FROM "Starboard" WHERE "guildId" = $1 AND "name" = $2"#).bind(&g).bind(&victim).execute(t.db()).await.expect("delete");
    let dup = sb::create_board(&t, &g, BoardPatch { name: Some(kept.clone()), channel_id: Some(w.board_channel()), ..Default::default() }).await;
    assert!(dup.as_ref().err().is_some_and(BoardError::is_unique_violation), "{dup:?}");
    let row = sb::get_board_by_name(&t, &g, Some(&kept)).await.expect("query").expect("row");
    assert_eq!(row.emojis, vec!["⭐"]);
    assert_eq!((row.threshold, row.ignore_bots, row.remove_below_threshold, row.self_star, row.remove_self_stars, row.enabled), (3, true, true, false, false, true));
    assert!(sb::get_board_by_name(&t, &g, None).await.expect("query").is_none());
    assert!(sb::get_board_by_name(&t, &g, Some("missing")).await.expect("query").is_none());
    let d = sb::create_board(&t, &g, BoardPatch { name: Some("defaults".into()), channel_id: Some(w.board_channel()), ..Default::default() }).await.expect("created");
    assert_eq!((d.threshold, d.emojis.clone()), (3, vec!["⭐".to_string()]));
    let more = sb::create_board(&t, &g, BoardPatch { name: Some("one-too-many".into()), channel_id: Some(w.board_channel()), ..Default::default() }).await;
    assert!(matches!(more, Err(BoardError::Limit)), "the guild is back at the cap");
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_get_boards_caches_degrades_and_invalidates() {
    let Some((t, g, w)) = db(2).await else { return };
    assert!(sb::get_boards(&t, &g, false).await.expect("loaded").is_empty());
    assert_eq!(sb::peek_boards(&t, &g).map(|b| b.len()), Some(0));
    let board = mk_board(&t, &g, &w, BoardPatch { name: Some("one".into()), emojis: Some(vec!["⭐".into(), "🌟".into()]), threshold: Some(2), ..Default::default() }).await;
    assert!(sb::peek_boards(&t, &g).is_none(), "createBoard invalidated the memo");
    let boards = sb::get_boards(&t, &g, false).await.expect("loaded");
    assert_eq!(boards.len(), 1);
    assert_eq!(boards[0].emojis, vec!["⭐", "🌟"]);
    // The Redis list (here the in-memory cache) holds the JS shape.
    let cached = t.cache().get(&sb::boards_key(&g)).await.expect("cached");
    assert_eq!(cached, serde_json::to_value(boards.as_ref()).unwrap_or_default());
    // A direct DB change is not seen until invalidated or fresh.
    sqlx::query(r#"UPDATE "Starboard" SET "threshold" = 9 WHERE "id" = $1"#).bind(&board.id).execute(t.db()).await.expect("update");
    assert_eq!(sb::get_boards(&t, &g, false).await.expect("loaded")[0].threshold, 2);
    assert_eq!(sb::get_boards(&t, &g, true).await.expect("loaded")[0].threshold, 9);
    sb::invalidate_boards(&t, &g).await;
    assert!(sb::peek_boards(&t, &g).is_none());
    assert!(t.cache().get(&sb::boards_key(&g)).await.is_none());
    // Postgres unreachable -> None (fail closed).
    let offline = TestApp::new();
    assert!(sb::get_boards(&offline, &g, true).await.is_none());
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_a_board_is_created_updated_dropped_reposted_and_removed_as_the_count_moves() {
    let Some((t, g, w)) = db(3).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(2), ..Default::default() }).await;
    let fan = |n: u64| user(10 + n);
    let msg = w.message(None, &user(1), false, &[r(star(), &[fan(1)])], None);
    let id = msg.id.to_string();
    sync(&t, &w, &msg).await.expect("synced");
    assert_eq!(w.sent_count(), 0);
    assert!(entries_of(&t, &board.id).await.is_empty());

    let two = w.message(Some(&id), &user(1), false, &[r(star(), &[fan(1), fan(2)])], None);
    sync(&t, &w, &two).await.expect("synced");
    let (to, sent_id, payload) = l(&w.sent)[0].clone();
    assert_eq!(to, w.board_channel());
    assert_eq!(payload["content"], format!("⭐ **2** · <#{}>", w.source().id));
    assert_eq!(payload["allowed_mentions"], json!({ "parse": [] }));
    let e = entries_of(&t, &board.id).await.remove(0);
    assert_eq!((e.board_message_id.as_deref(), e.count, e.author_id.as_str(), e.source_message_id.as_str(), e.blocked), (Some(sent_id.as_str()), 2, user(1).as_str(), id.as_str(), false));

    sync(&t, &w, &two).await.expect("synced");
    assert!(l(&w.edits).is_empty(), "unchanged count: no edit");
    let four = w.message(Some(&id), &user(1), false, &[r(star(), &[fan(1), fan(2), fan(3), fan(4)])], None);
    sync(&t, &w, &four).await.expect("synced");
    assert_eq!(l(&w.edits).len(), 1);
    assert_eq!(l(&w.edits)[0], (sent_id.clone(), json!({ "content": format!("⭐ **4** · <#{}>", w.source().id), "allowed_mentions": { "parse": [] } })));
    assert_eq!(entries_of(&t, &board.id).await[0].count, 4);

    let one = w.message(Some(&id), &user(1), false, &[r(star(), &[fan(1)])], None);
    sync(&t, &w, &one).await.expect("synced");
    assert_eq!(*l(&w.deletes), vec![sent_id.clone()]);
    let e = entries_of(&t, &board.id).await.remove(0);
    assert_eq!((e.board_message_id.clone(), e.count, e.blocked), (None, 1, false));

    let back = w.message(Some(&id), &user(1), false, &[r(star(), &[fan(1), fan(2), fan(3)])], None);
    sync(&t, &w, &back).await.expect("synced");
    assert_eq!(w.sent_count(), 2);
    let es = entries_of(&t, &board.id).await;
    assert_eq!(es.len(), 1);
    assert_eq!((es[0].board_message_id.clone(), es[0].count), (Some(l(&w.sent)[1].1.clone()), 3));
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_exclusions_any_boards_and_independent_boards() {
    let Some((t, g, w)) = db(4).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(2), remove_below_threshold: Some(false), ..Default::default() }).await;
    // The author and a bot do not count: 2 real users out of 4 reactors.
    let mut x = r(star(), &[user(1), user(2), user(3), user(4)]);
    x.bots = vec![user(4)];
    let msg = w.message(None, &user(1), false, &[x], None);
    let id = msg.id.to_string();
    sync(&t, &w, &msg).await.expect("synced");
    assert_eq!(entries_of(&t, &board.id).await[0].count, 2);
    assert_eq!(w.sent_count(), 1);
    let lower = w.message(Some(&id), &user(1), false, &[r(star(), &[user(1), user(2)])], None);
    sync(&t, &w, &lower).await.expect("synced");
    let e = entries_of(&t, &board.id).await.remove(0);
    assert_eq!((e.count, e.board_message_id.is_some()), (1, true));
    assert_eq!((l(&w.edits).len(), l(&w.deletes).len()), (1, 0), "kept (removeBelowThreshold off), edited");
    // selfStar on counts the author; the bot's own reaction never counts (fast path: count - me).
    sb::update_board(&t, &g, &board.id, BoardPatch { self_star: Some(true), ignore_bots: Some(false), ..Default::default() }).await.expect("updated");
    let mut again = r(star(), &[user(1), user(2), BOT.to_string()]);
    again.me = true;
    let m = w.message(Some(&id), &user(1), false, &[again], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(entries_of(&t, &board.id).await[0].count, 2);

    // An "any" board counts distinct users over every emoji; the second board needs more.
    let any = sb::create_board(&t, &g, BoardPatch { name: Some("any".into()), channel_id: Some(w.board_channel()), emojis: Some(vec![]), threshold: Some(3), ..Default::default() })
        .await
        .expect("board");
    let fetches_before = l(&w.fetches).len();
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2), user(3)]), r(glow(), &[user(3), user(4)])], None);
    sync(&t, &w, &m).await.expect("synced");
    let es = entries_of(&t, &any.id).await;
    assert_eq!(es.len(), 1);
    assert_eq!(es[0].count, 3, "3 distinct users");
    let last = l(&w.sent).last().cloned().expect("sent");
    assert!(last.2["content"].as_str().unwrap_or("").starts_with("⭐ **3**"), "the highest-count emoji, ties to the first");
    let star_fetches = l(&w.fetches)[fetches_before..].iter().filter(|f| f.0 == "⭐").count();
    assert_eq!(star_fetches, 1, "each emoji fetched once per run across boards");
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_eligibility_filters() {
    let Some((t, g, w)) = db(7).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(1), max_message_age_days: Some(2), ..Default::default() }).await;
    let users = [user(2), user(3)];
    let m = w.message(None, &user(9), true, &[r(star(), &users)], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0, "bot author");
    let old_id = (((now_ms() - 3 * 86_400_000 - EPOCH) as u64) << 22).to_string();
    let m = w.message(Some(&old_id), &user(1), false, &[r(star(), &users)], None);
    let before = l(&w.fetches).len();
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!((w.sent_count(), l(&w.fetches).len()), (0, before), "too old (no fetch needed)");
    let m = w.message(None, &user(1), false, &[r(star(), &users)], Some(&w.board_channel()));
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0, "a message inside the board channel");
    sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some(w.source().id), ..Default::default() }).await.expect("updated");
    let m = w.message(None, &user(1), false, &[r(star(), &users)], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0, "ignored channel");
    sb::update_board(&t, &g, &board.id, BoardPatch { remove_ignored: Some(w.source().id), ..Default::default() }).await.expect("updated");
    let src_id = w.source().id;
    l(&w.guild).channels.iter_mut().for_each(|c| c.nsfw = c.id == src_id);
    let m = w.message(None, &user(1), false, &[r(star(), &users)], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0, "NSFW source, board not allowed");
    l(&w.guild).channels.iter_mut().for_each(|c| c.nsfw = false);
    l(&w.private).insert(w.source().id);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0, "a private source with a public board");
    l(&w.private).clear();
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 1, "a normal message passes");
    // Disabled boards are skipped.
    sb::update_board(&t, &g, &board.id, BoardPatch { enabled: Some(false), ..Default::default() }).await.expect("updated");
    let m = w.message(None, &user(1), false, &[r(star(), &users)], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 1);
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_permissions_a_missing_channel_and_send_errors() {
    let Some((t, g, w)) = db(8).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(1), ..Default::default() }).await;
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    let id = m.id.to_string();
    l(&w.missing).insert(w.board_channel(), vec!["SendMessages".into()]);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0);
    assert!(entries_of(&t, &board.id).await.is_empty(), "no entry: the next event retries");
    l(&w.missing).clear();
    *l(&w.send_error) = Some(DiscordError::code(50013));
    sync(&t, &w, &m).await.expect("synced");
    assert!(entries_of(&t, &board.id).await.is_empty());
    *l(&w.send_error) = Some(DiscordError::other("boom"));
    assert!(sync(&t, &w, &m).await.is_err_and(|e| e.to_string().contains("boom")));
    *l(&w.send_error) = None;
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 1);
    // The board channel is missing from the cache but Discord returns it: its fetched permissions decide.
    let bc = w.board_channel();
    let fetched = SbChannel { bot_permissions: Some((1 << 10) | (1 << 14)), ..l(&w.guild).channel(&bc).cloned().expect("board channel") };
    l(&w.guild).channels.retain(|c| c.id != bc);
    l(&w.fetchable).insert(bc.clone(), fetched.clone());
    let fresh = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    sync(&t, &w, &fresh).await.expect("synced");
    assert_eq!(w.sent_count(), 1, "View Channel and Embed Links but no Send Messages: no post");
    l(&w.fetchable).insert(bc.clone(), SbChannel { bot_permissions: Some((1 << 10) | (1 << 11) | (1 << 14)), ..fetched });
    sync(&t, &w, &fresh).await.expect("synced");
    assert_eq!(w.sent_count(), 2, "posted into the uncached channel Discord returned");
    l(&w.fetchable).clear();

    // The board channel vanished: REST says 10003 -> the board is disabled and its reposts dropped.
    let more = w.message(Some(&id), &user(1), false, &[r(star(), &[user(2), user(3)])], None);
    sync(&t, &w, &more).await.expect("synced");
    let row = sb::get_board_by_name(&t, &g, Some("main")).await.expect("q").expect("row");
    assert!(!row.enabled);
    assert_eq!(entries_of(&t, &board.id).await[0].board_message_id, None);
    // An unavailable guild never disables anything.
    sb::update_board(&t, &g, &board.id, BoardPatch { enabled: Some(true), ..Default::default() }).await.expect("updated");
    l(&w.guild).available = false;
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    sync(&t, &w, &m).await.expect("synced");
    assert!(sb::get_board_by_name(&t, &g, Some("main")).await.expect("q").expect("row").enabled);
    t.reset_guilds(&[&g]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_staff_deletes_hand_deletes_auto_react_and_edits() {
    let Some((t, g, w)) = db(9).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(1), ..Default::default() }).await;
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    let id = m.id.to_string();
    sync(&t, &w, &m).await.expect("synced");
    let repost = entries_of(&t, &board.id).await[0].board_message_id.clone().expect("repost");
    // The bot's own deletion (marked) changes nothing.
    bot_core::deletion_tracker::mark_bot_deletion(&t, &[repost.as_str()], Some("starboard"), None);
    sb::on_message_delete(&t, &g, &w.board_channel(), &repost);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(entries_of(&t, &board.id).await[0].board_message_id.as_deref(), Some(repost.as_str()));
    // Staff delete it: the entry is blocked.
    bot_core::deletion_tracker::mark_bot_deletion(&t, &[repost.as_str()], Some("other"), None);
    sb::on_message_delete(&t, &g, &w.board_channel(), &repost);
    assert!(until(|| async { entries_of(&t, &board.id).await[0].blocked }).await);
    assert_eq!(entries_of(&t, &board.id).await[0].board_message_id, None);
    let more = w.message(Some(&id), &user(1), false, &[r(star(), &[user(2), user(3), user(4)])], None);
    sync(&t, &w, &more).await.expect("synced");
    assert_eq!((w.sent_count(), l(&w.edits).len()), (1, 0), "never reposted, never edited");
    // Deleting the source still removes the (blocked) entry.
    sb::on_message_delete(&t, &g, &w.source().id, &id);
    assert!(until(|| async { entries_of(&t, &board.id).await.is_empty() }).await);

    // A repost deleted by hand (10008 on edit) is blocked.
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    let id = m.id.to_string();
    sync(&t, &w, &m).await.expect("synced");
    *l(&w.edit_error) = Some(DiscordError::code(10008));
    sync(&t, &w, &w.message(Some(&id), &user(1), false, &[r(star(), &[user(2), user(3)])], None)).await.expect("synced");
    let e = entries_of(&t, &board.id).await.remove(0);
    assert_eq!((e.blocked, e.board_message_id.clone()), (true, None));
    *l(&w.edit_error) = None;
    // 50013 on edit only warns: the count is still stored.
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    let id = m.id.to_string();
    sync(&t, &w, &m).await.expect("synced");
    *l(&w.edit_error) = Some(DiscordError::code(50013));
    sync(&t, &w, &w.message(Some(&id), &user(1), false, &[r(star(), &[user(2), user(3)])], None)).await.expect("synced");
    let e = entries_of(&t, &board.id).await.into_iter().find(|e| e.source_message_id == id).expect("entry");
    assert_eq!((e.count, e.board_message_id.is_some()), (2, true));
    *l(&w.edit_error) = None;

    // Source edits are mirrored onto the live repost.
    let mut raw = with_reactions(raw_message(&id, &w.source().id, &user(1), false, "second"), &[r(star(), &[user(2), user(3)])]);
    raw["guild_id"] = json!(g);
    sb::on_message_update(&t, &g, to_message(raw));
    assert!(until(|| async { !l(&w.edits).is_empty() }).await);
    let (edited, payload) = l(&w.edits).last().cloned().expect("edit");
    assert_eq!(Some(edited), e.board_message_id);
    assert_eq!(payload["embeds"][0]["description"], "second");
    assert_eq!(payload["content"], format!("⭐ **2** · <#{}>", w.source().id));

    // autoReact reacts with the first emoji; a failing react never undoes the repost.
    sb::update_board(&t, &g, &board.id, BoardPatch { auto_react: Some(true), emojis: Some(vec!["🌟".into(), "⭐".into()]), ..Default::default() }).await.expect("updated");
    let m = w.message(None, &user(1), false, &[r(glow(), &[user(2)])], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(*l(&w.reacts), vec!["🌟"]);
    *l(&w.react_error) = Some(DiscordError::code(10014));
    let m = w.message(None, &user(1), false, &[r(glow(), &[user(2)])], None);
    let sent_before = w.sent_count();
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), sent_before + 1);
    assert!(entries_of(&t, &board.id).await.iter().any(|e| e.source_message_id == m.id.to_string() && e.board_message_id.is_some()));
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_removal_errors() {
    let Some((t, g, w)) = db(12).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(2), ..Default::default() }).await;
    // 10008 when removing: the entry is dropped (not blocked).
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2), user(3)])], None);
    let id = m.id.to_string();
    sync(&t, &w, &m).await.expect("synced");
    *l(&w.delete_error) = Some(DiscordError::code(10008));
    sync(&t, &w, &w.message(Some(&id), &user(1), false, &[r(star(), &[user(2)])], None)).await.expect("synced");
    let e = entries_of(&t, &board.id).await.remove(0);
    assert_eq!((e.board_message_id.clone(), e.blocked), (None, false));
    // 50013 when removing: the entry stays live, the count is stored.
    *l(&w.delete_error) = None;
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2), user(3)])], None);
    let id = m.id.to_string();
    sync(&t, &w, &m).await.expect("synced");
    *l(&w.delete_error) = Some(DiscordError::code(50013));
    sync(&t, &w, &w.message(Some(&id), &user(1), false, &[r(star(), &[user(2)])], None)).await.expect("synced");
    let e = entries_of(&t, &board.id).await.into_iter().find(|e| e.source_message_id == id).expect("entry");
    assert_eq!((e.board_message_id.is_some(), e.count), (true, 1));
    t.reset_guilds(&[&g]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_deletes_bulk_deletes_and_channel_deletes() {
    let Some((t, g, w)) = db(17).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(1), ..Default::default() }).await;
    let ms: Vec<Message> = (0..3).map(|_| w.message(None, &user(1), false, &[r(star(), &[user(2)])], None)).collect();
    for m in &ms {
        sync(&t, &w, m).await.expect("synced");
    }
    let entries = entries_of(&t, &board.id).await;
    assert_eq!(entries.len(), 3);
    let repost_of = |m: &Message| entries.iter().find(|e| e.source_message_id == m.id.to_string()).and_then(|e| e.board_message_id.clone()).expect("repost");
    sb::on_message_delete(&t, &g, &w.source().id, &ms[0].id.to_string());
    assert!(until(|| async { entries_of(&t, &board.id).await.len() == 2 }).await);
    assert!(l(&w.deletes).contains(&repost_of(&ms[0])));
    sb::on_message_delete_bulk(&t, &g, &w.source().id, vec![ms[1].id.to_string(), "999".into()]);
    assert!(until(|| async { entries_of(&t, &board.id).await.len() == 1 }).await);
    assert!(l(&w.deletes).contains(&repost_of(&ms[1])));
    sb::on_message_delete_bulk(&t, &g, &w.board_channel(), vec![repost_of(&ms[2])]);
    assert!(until(|| async { entries_of(&t, &board.id).await[0].blocked }).await);

    // Deleting a board channel disables its boards, cleans the ignore lists and drops source entries.
    let doomed = "400000000000000998";
    sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some(doomed.into()), ..Default::default() }).await.expect("updated");
    sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some("other-chan".into()), ..Default::default() }).await.expect("updated");
    let gone = sb::create_board(&t, &g, BoardPatch { name: Some("gone".into()), channel_id: Some(doomed.into()), threshold: Some(1), ..Default::default() }).await.expect("board");
    sqlx::query(r#"INSERT INTO "StarboardEntry" ("id","starboardId","sourceChannelId","sourceMessageId","authorId","boardMessageId","count","updatedAt") VALUES ($1,$2,$3,'777',$4,'live',2,now() AT TIME ZONE 'UTC')"#)
        .bind(bot_db::new_cuid())
        .bind(&gone.id)
        .bind(w.source().id)
        .bind(user(1))
        .execute(t.db())
        .await
        .expect("entry");
    let stamp = sb::get_board_by_name(&t, &g, Some("main")).await.expect("q").expect("row").updated_at;
    tokio::time::sleep(Duration::from_millis(20)).await;
    sb::get_boards(&t, &g, false).await;
    sb::on_channel_delete(&t, &g, doomed).await;
    let gone_row = sb::get_board_by_name(&t, &g, Some("gone")).await.expect("q").expect("row");
    assert!(!gone_row.enabled);
    assert_eq!(entries_of(&t, &gone.id).await[0].board_message_id, None);
    let keep = sb::get_board_by_name(&t, &g, Some("main")).await.expect("q").expect("row");
    assert_eq!(keep.ignored_channel_ids, vec!["other-chan"]);
    assert!(keep.updated_at > stamp, "the raw UPDATE sets updatedAt");
    assert!(sb::peek_boards(&t, &g).is_none(), "the cache was invalidated");
    sb::on_channel_delete(&t, &g, &w.source().id).await;
    assert!(entries_of(&t, &board.id).await.is_empty());
    assert!(entries_of(&t, &gone.id).await.is_empty());
    sb::on_channel_delete(&t, &g, "unrelated").await;
    t.reset_guilds(&[&g]).await;
}

#[tokio::test]
async fn db_update_board_delete_list_and_search() {
    let Some((t, g, w)) = db(19).await else { return };
    let board = mk_board(&t, &g, &w, BoardPatch { name: Some("alpha".into()), threshold: Some(1), ..Default::default() }).await;
    let beta = sb::create_board(&t, &g, BoardPatch { name: Some("beta".into()), channel_id: Some("400000000000000192".into()), ..Default::default() }).await.expect("beta");
    let insert = |board_id: &str, src: &str, repost: Option<&str>| {
        let (db, b, s, r) = (t.db().clone(), board_id.to_string(), src.to_string(), repost.map(String::from));
        async move {
            sqlx::query(r#"INSERT INTO "StarboardEntry" ("id","starboardId","sourceChannelId","sourceMessageId","authorId","boardMessageId","count","updatedAt") VALUES ($1,$2,'c',$3,'a',$4,3,now() AT TIME ZONE 'UTC')"#)
                .bind(bot_db::new_cuid()).bind(b).bind(s).bind(r).execute(&db).await.expect("entry");
        }
    };
    insert(&board.id, "s1", Some("r1")).await;
    insert(&board.id, "s2", None).await;
    insert(&beta.id, "s1", Some("r2")).await;
    let live = |id: &str| {
        let (db, id) = (t.db().clone(), id.to_string());
        async move {
            let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "StarboardEntry" WHERE "starboardId" = $1 AND "boardMessageId" IS NOT NULL"#).bind(id).fetch_one(&db).await.expect("count");
            n
        }
    };
    let (before, after) = sb::update_board(&t, &g, &board.id, BoardPatch { threshold: Some(5), self_star: Some(true), ..Default::default() }).await.expect("updated");
    assert_eq!((before.threshold, after.threshold), (1, 5));
    assert_eq!(live(&board.id).await, 1, "a plain change keeps entries");
    let (_, after) = sb::update_board(&t, &g, &board.id, BoardPatch { channel_id: Some("400000000000000193".into()), ..Default::default() }).await.expect("updated");
    assert_eq!(after.channel_id, "400000000000000193");
    assert_eq!((live(&board.id).await, live(&beta.id).await), (0, 1), "a channel change drops this board's live entries only");
    sqlx::query(r#"UPDATE "StarboardEntry" SET "boardMessageId" = 'r9' WHERE "starboardId" = $1 AND "sourceMessageId" = 's1'"#).bind(&board.id).execute(t.db()).await.expect("u");
    sb::update_board(&t, &g, &board.id, BoardPatch { channel_id: Some("400000000000000193".into()), threshold: Some(6), ..Default::default() }).await.expect("updated");
    assert_eq!(live(&board.id).await, 1, "the same channel again drops nothing");

    sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some("x1".into()), ..Default::default() }).await.expect("u");
    sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some("x1".into()), ..Default::default() }).await.expect("u");
    assert_eq!(sb::get_board_by_name(&t, &g, Some("alpha")).await.expect("q").expect("row").ignored_channel_ids, vec!["x1"]);
    sb::update_board(&t, &g, &board.id, BoardPatch { remove_ignored: Some("x1".into()), ..Default::default() }).await.expect("u");
    assert!(sb::get_board_by_name(&t, &g, Some("alpha")).await.expect("q").expect("row").ignored_channel_ids.is_empty());
    let full: Vec<String> = (0..sb::MAX_IGNORED_CHANNELS).map(|i| format!("i{i}")).collect();
    sqlx::query(r#"UPDATE "Starboard" SET "ignoredChannelIds" = $2 WHERE "id" = $1"#).bind(&board.id).bind(&full).execute(t.db()).await.expect("u");
    assert!(matches!(sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some("one-more".into()), ..Default::default() }).await, Err(BoardError::IgnoreLimit)));
    sb::update_board(&t, &g, &board.id, BoardPatch { add_ignored: Some("i3".into()), ..Default::default() }).await.expect("already there");

    assert!(matches!(sb::update_board(&t, &g, &board.id, BoardPatch { remove_self_stars: Some(true), ..Default::default() }).await, Err(BoardError::SettingsConflict)));
    sb::update_board(&t, &g, &board.id, BoardPatch { self_star: Some(false), remove_self_stars: Some(true), ..Default::default() }).await.expect("u");
    assert!(matches!(sb::update_board(&t, &g, &board.id, BoardPatch { self_star: Some(true), ..Default::default() }).await, Err(BoardError::SettingsConflict)));
    assert!(matches!(sb::update_board(&t, &g, "missing", BoardPatch { threshold: Some(2), ..Default::default() }).await, Err(BoardError::NotFound)));
    assert!(matches!(sb::update_board(&t, "0", &board.id, BoardPatch { threshold: Some(2), ..Default::default() }).await, Err(BoardError::NotFound)));
    assert!(sb::update_board(&t, &g, &board.id, BoardPatch { name: Some("beta".into()), ..Default::default() }).await.err().is_some_and(|e| e.is_unique_violation()));
    let (_, renamed) = sb::update_board(&t, &g, &board.id, BoardPatch { name: Some("gamma".into()), enabled: Some(false), ..Default::default() }).await.expect("u");
    assert_eq!((renamed.name.as_str(), renamed.enabled), ("gamma", false));

    let list = sb::list_boards_with_counts(&t, &g).await.expect("list");
    assert_eq!(list.iter().map(|b| (b.board.name.as_str(), b.reposts)).collect::<Vec<_>>(), vec![("gamma", 1), ("beta", 1)]);
    assert!(sb::list_boards_with_counts(&t, "0").await.expect("list").is_empty());
    let names = |rows: Vec<sb::BoardSearchRow>| rows.into_iter().map(|r| r.name).collect::<Vec<_>>();
    assert_eq!(names(sb::search_boards(&t, &g, "GAM").await), vec!["gamma"]);
    assert_eq!(names(sb::search_boards(&t, &g, "").await), vec!["beta", "gamma"]);
    assert!(sb::search_boards(&t, &g, "zzz").await.is_empty());
    // Prisma's `contains` does not escape LIKE wildcards: `_` matches any one character, as in the JS.
    assert_eq!(names(sb::search_boards(&t, &g, "_").await), vec!["beta", "gamma"]);
    assert_eq!(names(sb::search_boards(&t, &g, "g_m").await), vec!["gamma"]);
    assert_eq!(names(sb::search_boards(&t, &g, "%").await), vec!["beta", "gamma"]);
    assert!(sb::search_boards(&t, &g, "\\").await.is_empty(), "a trailing escape character is a database error: no choices");
    // The same queries through the JS bot's own searchBoards, on the same rows.
    let queries = ["GAM", "", "zzz", "_", "g_m", "%", "\\"];
    let js = run_node(JS, &["sb-search", &g, &serde_json::to_string(&queries).unwrap_or_default()]);
    let mut rust = Vec::new();
    for q in queries {
        rust.push(names(sb::search_boards(&t, &g, q).await));
    }
    assert_eq!(serde_json::to_value(&rust).unwrap_or_default(), js, "searchBoards agrees with the JS");

    assert_eq!(sb::delete_board(&t, "0", &board.id).await.expect("d"), 0);
    assert_eq!(sb::delete_board(&t, &g, &board.id).await.expect("d"), 1);
    assert_eq!(sb::delete_board(&t, &g, &board.id).await.expect("d"), 0);
    assert!(entries_of(&t, &board.id).await.is_empty());
    assert_eq!(entries_of(&t, &beta.id).await.len(), 1);
    t.reset_guilds(&[&g]).await;
}

// ── /starboard through the router ─────────────────────────────────────────────

fn admin(sub: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash("starboard").guild(g).user(&user(1)).permissions(serenity::all::Permissions::MANAGE_GUILD).subcommand(sub)
}

fn last_embed(t: &DbTestApp) -> (String, String) {
    let calls = t.responses.calls();
    let c = calls.last().expect("an answer");
    (c.embed_title().unwrap_or("").to_string(), c.embed_description().unwrap_or("").to_string())
}

#[tokio::test]
async fn db_starboard_create_edit_list_and_autocomplete() {
    use bot_core::command::option_type::{BOOLEAN, CHANNEL, INTEGER, STRING};
    let Some((t, g, w)) = db(30).await else { return };
    t.install(registry());
    let route = |i| bot_core::router::route_interaction(&t, i);

    route(admin("create", &g).option("name", STRING, json!("***")).option("channel", CHANNEL, json!(w.board_channel())).build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!t.responses.calls()[0].ephemeral(), "public reply");
    assert_eq!(last_embed(&t).0, "❌  Invalid Name");

    t.responses.clear();
    route(admin("create", &g).option("name", STRING, json!("Main")).option("channel", CHANNEL, json!(w.board_channel())).option("emoji", STRING, json!("abc")).build()).await;
    assert_eq!(last_embed(&t).0, "❌  Invalid Emoji");

    t.responses.clear();
    route(
        admin("create", &g)
            .option("name", STRING, json!("Main"))
            .option("channel", CHANNEL, json!(w.board_channel()))
            .option("self_star", BOOLEAN, json!(true))
            .option("remove_self_stars", BOOLEAN, json!(true))
            .build(),
    )
    .await;
    assert_eq!(last_embed(&t), ("❌  Invalid Options".into(), "Self-stars are counted on this board, so they cannot also be removed.".into()));

    l(&w.missing).insert(w.board_channel(), vec!["EmbedLinks".into()]);
    t.responses.clear();
    route(admin("create", &g).option("name", STRING, json!("Main")).option("channel", CHANNEL, json!(w.board_channel())).build()).await;
    assert_eq!(last_embed(&t), ("❌  Missing Permissions".into(), format!("I need **Embed Links** in <#{}> to post starboard reposts.", w.board_channel())));
    l(&w.missing).clear();

    t.responses.clear();
    route(admin("create", &g).option("name", STRING, json!("Main")).option("channel", CHANNEL, json!(w.board_channel())).option("allow_nsfw", BOOLEAN, json!(true)).build()).await;
    assert_eq!(last_embed(&t).0, "❌  Channel Not Age-Restricted");

    *l(&w.guild_missing) = vec!["ManageMessages".into()];
    t.responses.clear();
    route(
        admin("create", &g)
            .option("name", STRING, json!("Hall of Fame"))
            .option("channel", CHANNEL, json!(w.board_channel()))
            .option("threshold", INTEGER, json!(5))
            .option("remove_self_stars", BOOLEAN, json!(true))
            .build(),
    )
    .await;
    let (title, desc) = last_embed(&t);
    assert_eq!(title, "✅  Starboard Created");
    assert_eq!(
        desc,
        format!(
            "**hall-of-fame** reposts messages with **5+** ⭐ reactions to <#{bc}>.\nChannel: <#{bc}> · Emojis: ⭐\nThreshold: 5 · Self-star: removed · Bots: ignored\nBelow threshold: remove · Auto-react: off\nMax age: no limit · NSFW: blocked\n\n\u{26a0}\u{fe0f} Removing self-stars needs **Manage Messages** in the watched channels, which I do not have server-wide.",
            bc = w.board_channel()
        )
    );
    *l(&w.guild_missing) = Vec::new();
    t.responses.clear();
    route(admin("create", &g).option("name", STRING, json!("hall of fame")).option("channel", CHANNEL, json!(w.board_channel())).build()).await;
    assert_eq!(last_embed(&t), ("❌  Name Taken".into(), "A starboard named **hall-of-fame** already exists.".into()));

    // The module off note.
    t.settings().update_settings(&g, SettingsPatch::new().set("starboardEnabled", false)).await.expect("settings");
    t.responses.clear();
    route(admin("edit", &g).option("name", STRING, json!("hall-of-fame")).build()).await;
    assert_eq!(last_embed(&t), ("❌  Nothing To Change".into(), "Pass at least one option to change.".into()));
    t.responses.clear();
    route(admin("edit", &g).option("name", STRING, json!("nope")).option("threshold", INTEGER, json!(2)).build()).await;
    assert_eq!(last_embed(&t), ("❌  Not Found".into(), "No starboard named **nope**. See `/starboard list`.".into()));
    t.responses.clear();
    let src = w.source().id;
    route(
        admin("edit", &g)
            .option("name", STRING, json!("hall-of-fame"))
            .option("threshold", INTEGER, json!(2))
            .option("enabled", BOOLEAN, json!(false))
            .option("ignore_channel", CHANNEL, json!(src))
            .option("rename", STRING, json!("Best Of"))
            .build(),
    )
    .await;
    let (title, desc) = last_embed(&t);
    assert_eq!(title, "✅  Starboard Updated");
    assert!(desc.starts_with(&format!("**best-of**\n• **Name:** hall-of-fame → best-of\n• **Threshold:** 5 → 2\n• **Enabled:** on → off\n+ <#{src}> ignored\n\n")), "{desc}");
    assert!(desc.contains("This board is **off**. Turn it back on with `/starboard edit name:best-of enabled:True`."));
    assert!(desc.ends_with(feat_starboard_stats::commands::starboard::MODULE_OFF_NOTE));
    t.responses.clear();
    route(admin("edit", &g).option("name", STRING, json!("best-of")).option("ignore_channel", CHANNEL, json!(src)).option("unignore_channel", CHANNEL, json!(src)).build()).await;
    assert_eq!(last_embed(&t).1, "Choose different channels for ignore_channel and unignore_channel.");

    t.responses.clear();
    route(admin("list", &g).build()).await;
    let calls = t.responses.calls();
    let body = &calls.last().expect("answer").body["embeds"][0];
    assert_eq!(body["title"], "ℹ️  Starboards");
    assert!(body["description"].as_str().unwrap_or("").starts_with("1/10 boards\n\n⚠️ The Starboards module is off"));
    assert_eq!(body["fields"][0]["name"], "🌟 best-of (off)");
    assert!(body["fields"][0]["value"].as_str().unwrap_or("").contains(&format!("Reposts: 0\nIgnored: <#{src}>")));

    // Autocomplete: name → #channel (off).
    t.responses.clear();
    route(FakeInteraction::autocomplete("starboard").guild(&g).user(&user(1)).permissions(serenity::all::Permissions::MANAGE_GUILD).subcommand("edit").focused("name", "BEST").build()).await;
    let calls = t.responses.calls();
    let choices = calls.last().expect("answer").choices();
    assert_eq!(choices, vec![json!({ "name": format!("best-of → #{} (off)", l(&w.guild).channels[0].name), "value": "best-of" })]);
    t.reset_guilds(&[&g]).await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

/// JS createBoard + getBoards fill Redis; Rust reads that list. Rust fills Redis; with the rows gone, JS getBoards
/// still returns the Rust list (it came from Redis).
#[tokio::test]
async fn js_and_rust_read_each_others_cached_board_list() {
    let (Some(url), Some(redis)) = (db_url(), redis_url()) else {
        println!("skipped: TEST_REDIS_URL not set");
        return;
    };
    let g = gid(40);
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([g.clone()]);
    t.reset_guilds(&[&g]).await;
    t.cache().del_one(&sb::boards_key(&g)).await;
    let js = run_node(JS, &["sb-create", &g, r#"{"name":"js-board","channelId":"400000000000000401","emojis":["⭐","🌟"],"threshold":4,"autoReact":true}"#]);
    assert!(js.get("error").is_none(), "{js}");
    // The row is gone: what Rust reads can only come from the JS-written cache entry.
    sqlx::query(r#"DELETE FROM "Starboard" WHERE "guildId" = $1"#).bind(&g).execute(t.db()).await.expect("delete");
    let boards = sb::get_boards(&t, &g, false).await.expect("read");
    assert_eq!(serde_json::to_value(boards.as_ref()).unwrap_or_default(), js["boards"]);
    assert_eq!((boards[0].name.as_str(), boards[0].threshold, boards[0].auto_react), ("js-board", 4, true));

    // Rust writes the list; JS reads it.
    sb::invalidate_boards(&t, &g).await;
    let rb = sb::create_board(&t, &g, BoardPatch { name: Some("rust-board".into()), channel_id: Some("400000000000000402".into()), threshold: Some(7), ..Default::default() })
        .await
        .expect("board");
    let ours = sb::get_boards(&t, &g, false).await.expect("read");
    sqlx::query(r#"DELETE FROM "Starboard" WHERE "guildId" = $1"#).bind(&g).execute(t.db()).await.expect("delete");
    let back = run_node(JS, &["sb-boards", &g]);
    assert_eq!(back, serde_json::to_value(ours.as_ref()).unwrap_or_default());
    assert_eq!(back[0]["id"], rb.id);
    t.cache().del_one(&sb::boards_key(&g)).await;
    t.reset_guilds(&[&g]).await;
}

/// JS syncs a message into a repost and an entry; Rust continues the same entry (an edit of the JS repost). Then
/// Rust creates a board and an entry, JS reads both and updates the board.
#[tokio::test]
async fn js_and_rust_continue_each_others_starboard_entries() {
    let Some((t, g, w)) = db(41).await else { return };
    let js = run_node(JS, &["sb-create", &g, &json!({ "name": "main", "channelId": w.board_channel(), "threshold": 2 }).to_string()]);
    assert!(js.get("error").is_none(), "{js}");
    let board_id = js["board"]["id"].as_str().unwrap_or("").to_string();
    let id = next_message_id();
    let raw = raw_message(&id, &w.source().id, &user(1), false, "from js");
    let world = json!({ "botId": BOT, "boardChannelId": w.board_channel(), "sourceChannelId": w.source().id, "userIds": [user(2), user(3)], "message": raw, "firstSentId": "510000000000000000" });
    let log = run_node(JS, &["sb-sync", &g, &world.to_string()]);
    assert!(log.get("error").is_none(), "{log}");
    let js_repost = log["sent"][0]["id"].as_str().unwrap_or("").to_string();
    assert_eq!(log["sent"][0]["payload"]["content"], format!("⭐ **2** · <#{}>", w.source().id));
    let entry = entries_of(&t, &board_id).await.remove(0);
    assert_eq!((entry.board_message_id.as_deref(), entry.count), (Some(js_repost.as_str()), 2));

    // Rust sees the JS entry and edits the JS repost.
    sb::invalidate_boards(&t, &g).await;
    let m = w.message(Some(&id), &user(1), false, &[r(star(), &[user(2), user(3), user(4)])], None);
    sync(&t, &w, &m).await.expect("synced");
    assert_eq!(w.sent_count(), 0, "no second repost");
    assert_eq!(l(&w.edits)[0].0, js_repost);
    assert_eq!(entries_of(&t, &board_id).await[0].count, 3);

    // JS reads the Rust-updated entry and continues: a fourth user edits the same repost.
    let world = json!({ "botId": BOT, "boardChannelId": w.board_channel(), "sourceChannelId": w.source().id, "userIds": [user(2), user(3), user(4), user(5)], "message": raw });
    let log = run_node(JS, &["sb-sync", &g, &world.to_string()]);
    assert_eq!(log["sent"], json!([]));
    assert_eq!(log["edits"], json!([{ "id": js_repost, "content": format!("⭐ **4** · <#{}>", w.source().id) }]));
    let js_entries = run_node(JS, &["sb-entries", &board_id]);
    let ours = serde_json::to_value(entries_of(&t, &board_id).await).unwrap_or_default();
    assert_eq!(js_entries, ours, "the entry reads the same through Prisma and Rust");

    // Rust writes a board and an entry; the JS reads and updates them.
    let rb = sb::create_board(&t, &g, BoardPatch { name: Some("rust".into()), channel_id: Some(w.board_channel()), threshold: Some(1), emojis: Some(vec![]), ..Default::default() })
        .await
        .expect("board");
    let jb = run_node(JS, &["sb-board", &g, "rust"]);
    assert_eq!(jb, serde_json::to_value(&rb).unwrap_or_default());
    let up = run_node(JS, &["sb-update", &g, &rb.id, r#"{"threshold":9,"addIgnored":"400000000000000777"}"#]);
    assert_eq!(up["after"]["threshold"], 9);
    let row = sb::get_board_by_name(&t, &g, Some("rust")).await.expect("q").expect("row");
    assert_eq!((row.threshold, row.ignored_channel_ids.clone()), (9, vec!["400000000000000777".to_string()]));
    assert_eq!(serde_json::to_value(&row).unwrap_or_default(), up["after"]);
    t.reset_guilds(&[&g]).await;
}

// ── The plugin through the hook runner ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_the_plugin_maps_the_gateway_payloads() {
    let Some((t, g, w)) = db(42).await else { return };
    t.install(registry());
    t.settings().get_settings(&g).await.expect("settings memoised for the module gate");
    let board = mk_board(&t, &g, &w, BoardPatch { threshold: Some(1), ..Default::default() }).await;
    let calls: Arc<Mutex<Vec<sb::SyncRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    sb::set_sync_impl(
        &t,
        Some(Arc::new(move |_app, req| {
            let c = c2.clone();
            Box::pin(async move {
                l(&c).push(req);
                Ok(())
            })
        })),
    );
    let gid = serenity::all::GuildId::new(g.parse().unwrap_or(1));
    let src = w.source().id;
    let msg_id = next_message_id();
    let reaction: serenity::all::Reaction = serde_json::from_value(json!({
        "user_id": user(2), "channel_id": src, "message_id": msg_id, "guild_id": g, "emoji": star(),
        "message_author_id": user(1), "burst": false, "burst_colors": [], "type": 0
    }))
    .unwrap_or_else(|e| panic!("reaction: {e}"));
    let reactor: serenity::all::User =
        serde_json::from_value(json!({ "id": user(2), "username": "bob", "discriminator": "0", "avatar": null })).unwrap_or_else(|e| panic!("user: {e}"));
    t.run_hooks(MessageReactionAdd(ReactionEvent::new(gid, reaction, reactor))).await;
    assert!(until(|| async { !l(&calls).is_empty() }).await, "the reaction reached the sync");
    let req = l(&calls)[0].clone();
    assert_eq!((req.guild_id.as_str(), req.channel.id.as_str(), req.message_id.as_str()), (g.as_str(), src.as_str(), msg_id.as_str()));
    sb::set_sync_impl(&t, None);

    // A staff-deleted repost (messageDelete in the board channel) is blocked.
    let m = w.message(None, &user(1), false, &[r(star(), &[user(2)])], None);
    sync(&t, &w, &m).await.expect("synced");
    let repost = entries_of(&t, &board.id).await[0].board_message_id.clone().expect("repost");
    t.run_hooks(MessageDelete {
        guild_id: gid,
        message_id: serenity::all::MessageId::new(repost.parse().unwrap_or(1)),
        channel_id: serenity::all::ChannelId::new(w.board_channel().parse().unwrap_or(1)),
        message: None,
        partial: true,
        channel: None,
    })
    .await;
    assert!(until(|| async { entries_of(&t, &board.id).await[0].blocked }).await);

    // A deleted board channel disables its board (always, even with the module off).
    t.settings().update_settings(&g, SettingsPatch::new().set("starboardEnabled", false)).await.expect("settings");
    let channel: serenity::all::GuildChannel = serde_json::from_value(json!({
        "id": w.board_channel(), "guild_id": g, "type": 0, "name": "board", "position": 0, "permission_overwrites": [], "nsfw": false
    }))
    .unwrap_or_else(|e| panic!("channel: {e}"));
    t.run_hooks(ChannelDelete { guild_id: gid, channel }).await;
    assert!(!sb::get_board_by_name(&t, &g, Some("main")).await.expect("q").expect("row").enabled);
    t.reset_guilds(&[&g]).await;
}

// ── /starboard delete: the confirm prompt ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_starboard_delete_asks_first() {
    use bot_core::command::option_type::STRING;
    let Some((t, g, w)) = db(43).await else { return };
    t.install(registry());
    mk_board(&t, &g, &w, BoardPatch { name: Some("doomed".into()), ..Default::default() }).await;
    let bc = w.board_channel();

    // Not found: an ephemeral defer, then the error.
    bot_core::router::route_interaction(&t, admin("delete", &g).option("name", STRING, json!("nope")).build()).await;
    let calls = t.responses.calls();
    assert!(calls[0].ephemeral(), "the delete prompt is ephemeral");
    assert_eq!(last_embed(&t), ("❌  Not Found".into(), "No starboard named **nope**. See `/starboard list`.".into()));

    // The prompt, a stranger's click, then the invoker's confirmation.
    t.responses.clear();
    let app = t.app.clone();
    let gc = g.clone();
    let run = tokio::spawn(async move {
        bot_core::router::route_interaction(&app, admin("delete", &gc).option("name", STRING, json!("doomed")).build()).await;
    });
    assert!(until(|| async { !t.collectors().is_empty() }).await, "the prompt waits for a click");
    let prompt = t.responses.calls().last().cloned().expect("prompt");
    assert_eq!(prompt.embed_title(), Some("\u{26a0}\u{fe0f}  Delete starboard?"));
    let expected = format!("Delete **doomed**? Reposts already in <#{bc}> stay, but they stop updating.");
    assert_eq!(prompt.embed_description(), Some(expected.as_str()));
    let buttons = prompt.body["components"][0]["components"].as_array().cloned().unwrap_or_default();
    let yes = buttons.iter().find(|b| b["custom_id"].as_str().is_some_and(|id| id.ends_with(":yes"))).expect("a yes button");
    assert_eq!(yes["label"], "Delete");
    let yes_id = yes["custom_id"].as_str().unwrap_or("").to_string();
    bot_core::router::route_interaction(&t, FakeInteraction::button(&yes_id).guild(&g).user(&user(9)).message_id("1000000000000000100").build()).await;
    assert!(sb::get_board_by_name(&t, &g, Some("doomed")).await.expect("q").is_some(), "a stranger cannot confirm");
    bot_core::router::route_interaction(&t, FakeInteraction::button(&yes_id).guild(&g).user(&user(1)).message_id("1000000000000000100").build()).await;
    let _ = run.await;
    assert!(sb::get_board_by_name(&t, &g, Some("doomed")).await.expect("q").is_none());
    let (title, desc) = last_embed(&t);
    assert_eq!(title, "✅  Starboard Deleted");
    assert_eq!(desc, format!("**doomed** was deleted. Its reposts in <#{bc}> were kept."));

    // Delete autocomplete lists boards by name.
    mk_board(&t, &g, &w, BoardPatch { name: Some("keeper".into()), ..Default::default() }).await;
    t.responses.clear();
    bot_core::router::route_interaction(
        &t,
        FakeInteraction::autocomplete("starboard")
            .guild(&g)
            .user(&user(1))
            .permissions(serenity::all::Permissions::MANAGE_GUILD)
            .subcommand("delete")
            .focused("name", "kee")
            .build(),
    )
    .await;
    let choices = t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(choices, vec![json!({ "name": format!("keeper \u{2192} #{}", l(&w.guild).channels[0].name), "value": "keeper" })]);
    t.reset_guilds(&[&g]).await;
}

// ── The serenity layer ─────────────────────────────────────────────────────────

/// discord.js `client.channels.fetch` caches the channel it fetched. A reaction in a thread missing from the cache is
/// fetched over REST; once cached, the privacy check sees the real thread (before, an uncached source counted as
/// private and every board skipped it).
#[tokio::test]
async fn serenity_io_caches_a_fetched_source_thread() {
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use feat_starboard_stats::starboard_io::SerenityStarboardIo;
    use serenity::all::Permissions;
    const G: &str = "955990000000000001";
    const OWNER: &str = "955990000000000002";
    const SRC: &str = "955990000000000010";
    const BOARD: &str = "955990000000000011";
    const TH: &str = "955990000000000012";
    let guild = FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY)
        .member(TEST_BOT_ID, &[])
        .channel(SRC, 0, "general", &[])
        .channel(BOARD, 0, "stars", &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
    rest.on(
        "GET",
        &format!("/channels/{TH}"),
        200,
        json!({ "id": TH, "type": 11, "name": "late-thread", "parent_id": SRC, "owner_id": OWNER, "guild_id": G,
            "message_count": 0, "member_count": 1, "rate_limit_per_user": 0, "nsfw": false, "permission_overwrites": [],
            "thread_metadata": { "archived": false, "auto_archive_duration": 1440, "archive_timestamp": "2025-01-01T00:00:00.000000+00:00", "locked": false } }),
    );
    let io = SerenityStarboardIo;
    assert!(io.guild(&t, G).expect("cached guild").channel(TH).is_none());
    assert!(io.leaks_private(&t, G, TH, BOARD), "an unknown source counts as private");
    let ch = io.fetch_channel(&t, TH).await.expect("fetched").expect("a guild channel");
    assert!(ch.is_thread);
    assert_eq!(ch.parent_id.as_deref(), Some(SRC));
    assert!(io.guild(&t, G).expect("cached guild").channel(TH).is_some(), "cached after the fetch, as discord.js does");
    assert!(!io.leaks_private(&t, G, TH, BOARD), "a public thread of a public channel may feed a public board");
}

/// serenity's message cache never applies reaction events (discord.js keeps `message.reactions` live), so a message
/// cached when it was posted still has no reactions. The sync must read the message over REST: before, a message in
/// the cache never reached a board.
#[tokio::test]
async fn serenity_io_reads_live_reactions_not_the_stale_cached_message() {
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use feat_starboard_stats::starboard_io::SerenityStarboardIo;
    use serenity::all::Permissions;
    const G: &str = "955990000000000101";
    const OWNER: &str = "955990000000000102";
    const SRC: &str = "955990000000000110";
    const MSG: &str = "955990000000000120";
    let guild = FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY)
        .member(TEST_BOT_ID, &[])
        .channel(SRC, 0, "general", &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
    // The message as the gateway delivered it at create time: no reactions yet.
    let mut posted = raw_message(MSG, SRC, OWNER, false, "hello");
    posted["guild_id"] = json!(G);
    let mut ev: serenity::all::MessageCreateEvent = serde_json::from_value(posted.clone()).expect("event");
    let d = t.discord().expect("discord");
    // The bot keeps 200 messages per channel (bot_core::client::cache_settings).
    d.cache.set_max_messages(bot_core::client::cache_settings().max_messages);
    d.cache.update(&mut ev);
    let ch = serenity::all::ChannelId::new(SRC.parse().unwrap());
    let mid = serenity::all::MessageId::new(MSG.parse().unwrap());
    assert!(d.cache.message(ch, mid).is_some_and(|m| m.reactions.is_empty()), "the stale copy is in the cache");
    // Discord's current view: three stars.
    let live = with_reactions(posted, &[R { emoji: star(), users: vec![user(1), user(2), user(3)], bots: Vec::new(), me: false, count: None }]);
    rest.on("GET", &format!("/channels/{SRC}/messages/{MSG}"), 200, live);
    let m = SerenityStarboardIo.fetch_message(&t, SRC, MSG).await.expect("fetched");
    assert_eq!(m.reactions.len(), 1);
    assert_eq!(m.reactions[0].count, 3);
    assert_eq!(rest.calls_to("GET", &format!("/channels/{SRC}/messages/{MSG}")).len(), 1);
}

/// discord.js `message.member` is `guild.members.resolve(author)`: a repost of a message fetched over REST (no member
/// in the payload) still shows the cached member's nickname and server avatar, as PNG at 128 px.
#[tokio::test]
async fn repost_author_uses_the_cached_member() {
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use feat_starboard_stats::starboard_io::SerenityStarboardIo;
    use serenity::all::Permissions;
    const G: &str = "955990000000000201";
    const OWNER: &str = "955990000000000202";
    const SRC: &str = "955990000000000210";
    const AUTHOR: &str = "955990000000000203";
    let guild = FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL)
        .member(TEST_BOT_ID, &[])
        .member_with(AUTHOR, &[], |m| {
            m["nick"] = json!("Nicky");
            m["avatar"] = json!("a_0123456789abcdef0123456789abcdef");
        })
        .channel(SRC, 0, "general", &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
    let m = to_message(raw_message("955990000000000220", SRC, AUTHOR, false, "hi"));
    let member = SerenityStarboardIo.cached_member(&t, G, AUTHOR).expect("cached member");
    let p = sb::build_repost_payload_with(G, &m, Some(&member), 3, "⭐");
    assert_eq!(p["embeds"][0]["author"]["name"], json!("Nicky"));
    assert_eq!(
        p["embeds"][0]["author"]["icon_url"],
        json!(format!("https://cdn.discordapp.com/guilds/{G}/users/{AUTHOR}/avatars/a_0123456789abcdef0123456789abcdef.gif?size=128"))
    );
    // Without a cached member: the author's display name and default avatar (no size), as before.
    let p = sb::build_repost_payload(G, &m, 3, "⭐");
    assert_eq!(p["embeds"][0]["author"]["name"], json!("Alice"));
    assert!(p["embeds"][0]["author"]["icon_url"].as_str().unwrap().starts_with("https://cdn.discordapp.com/embed/avatars/"));
    // A cached member without a nickname shows the user's display name.
    let plain = SerenityStarboardIo.cached_member(&t, G, TEST_BOT_ID).expect("bot member");
    let p = sb::build_repost_payload_with(G, &m, Some(&plain), 3, "⭐");
    assert_eq!(p["embeds"][0]["author"]["name"], json!(format!("user{TEST_BOT_ID}")));
}

/// serenity sends `message_author_id` on reaction adds only. For a removal the author comes from the cached message,
/// as discord.js reads `ctx.message.author` of a non-partial message, so the self-reaction shortcut works both ways.
#[tokio::test]
async fn reaction_removal_reads_the_author_from_the_cached_message() {
    use bot_core::hooks::events::ReactionEvent;
    use bot_core::testing::{FakeGuild, FakeRest, TEST_BOT_ID};
    use feat_starboard_stats::plugins::starboard::reaction_input;
    use feat_starboard_stats::starboard_service::ReactionEventKind;
    use serenity::all::{GuildId, Permissions, Reaction, User};
    const G: &str = "955990000000000301";
    const OWNER: &str = "955990000000000302";
    const SRC: &str = "955990000000000310";
    const MSG: &str = "955990000000000320";
    let guild = FakeGuild::new(G, OWNER).everyone(Permissions::VIEW_CHANNEL).member(TEST_BOT_ID, &[]).channel(SRC, 0, "general", &[]);
    let rest = FakeRest::new();
    let t = TestApp::new().with_fake_guilds_rest(&[guild], &rest);
    let reaction: Reaction = serde_json::from_value(json!({
        "channel_id": SRC, "message_id": MSG, "guild_id": G, "user_id": OWNER, "emoji": star(), "burst": false, "type": 0
    }))
    .expect("reaction");
    let user: User = serde_json::from_value(json!({ "id": OWNER, "username": "owner", "discriminator": "0", "avatar": null })).expect("user");
    let ev = ReactionEvent::new(GuildId::new(G.parse().unwrap()), reaction, user);
    assert_eq!(reaction_input(&t, ReactionEventKind::Remove, &ev).message_author_id, None, "unknown while the message is not cached");
    let mut posted = raw_message(MSG, SRC, OWNER, false, "hello");
    posted["guild_id"] = json!(G);
    let mut created: serenity::all::MessageCreateEvent = serde_json::from_value(posted).expect("event");
    let d = t.discord().expect("discord");
    d.cache.set_max_messages(bot_core::client::cache_settings().max_messages);
    d.cache.update(&mut created);
    assert_eq!(reaction_input(&t, ReactionEventKind::Remove, &ev).message_author_id.as_deref(), Some(OWNER));
}
