//! Port of `test/logging.test.js` (and the declaration checks of `test/invites.test.js`), plus the golden checks.
//!
//! Offline: every embed builder and diff against JS-produced output (`fixtures/golden/feat-logging/cases.json`), the
//! registry and predicates, `send_log` (dead-channel memo, batching, the queue cap, permissions, files) through a
//! recording [`LogApi`], and the hooks through the hook runner. With `TEST_DATABASE_URL` (and `TEST_REDIS_URL`):
//! `/logging` end to end, the channelDelete cleanup, the bot-managed channel lookup and the timeout key shared with
//! the JS bot.

use std::sync::{Arc, Mutex};

use bot_core::command::option_type;
use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, offline_discord_as, redis_url, run_node};
use feat_logging::log_events::{self as ev, LogFile};
use feat_logging::logging_service::{
    self as svc, ApiError, AuditEntry, ChannelChain, LOG_EVENTS, LogApi, LogType, QUEUE_MAX_ITEMS, SendLogOpts, channel_cleanup_data, fit_embed_data,
    flush_log_queue, forget_dead_channels, get_log_channel_health, is_ignored_channel, is_log_channel_dead, is_log_event_enabled, is_log_event_key,
    log_event_keys, mark_log_channel_deleted, send_log, set_log_api, toggle_log_events,
};
use serde_json::{Value, json};
use serenity::all::{ChannelId, GuildChannel, GuildId, Permissions as P};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_logging::register(&mut reg);
    reg
}

fn golden() -> Value {
    let path = bot_core::testing::golden_dir().join("feat-logging").join("cases.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("run node rust/fixtures/gen/feat-logging-golden.mjs")).expect("json")
}

/// Removes every `timestamp` key (the JS builders stamp the real time).
fn strip(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().filter(|(k, _)| k.as_str() != "timestamp").map(|(k, v)| (k.clone(), strip(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(strip).collect()),
        other => other.clone(),
    }
}

fn de<T: serde::de::DeserializeOwned>(v: &Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}: {v}"))
}

fn known(args: &Value) -> Option<std::collections::HashSet<String>> {
    args.get("known").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}

fn names(args: &Value) -> Option<std::collections::HashMap<String, String>> {
    args.get("names").and_then(Value::as_object).map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|n| (k.clone(), n.to_string()))).collect())
}

fn opt_str(v: &Value) -> Option<&str> {
    v.as_str()
}

fn lines_json(lines: &[ev::Line]) -> Value {
    Value::Array(lines.iter().map(|l| json!({ "name": l.name, "value": l.value })).collect())
}


fn run_embed_case(fun: &str, a: &Value, now: i64) -> Value {
    let k = known(a);
    let opt = |v: &Value| v.as_str().map(String::from);
    match fun {
        "messageDelete" => {
            ev::build_message_delete_embed(&de(&a["message"]), opt_str(&a["botReason"]), opt_str(&a["botId"]), now, k.as_ref())
        }
        "messageEdit" => {
            let old: ev::LogMessage = de(&a["oldMessage"]);
            ev::build_message_edit_embed(Some(&old), &de(&a["newMessage"]), a["oldKnown"].as_bool().unwrap_or(false), now, k.as_ref())
        }
        "bulkDelete" => {
            let list: Vec<ev::LogMessage> = de(&a["messages"]);
            let (embed, file) = ev::build_bulk_delete_embed(
                a["channel"]["id"].as_str(),
                a["channel"]["name"].as_str(),
                &list,
                opt_str(&a["botReason"]),
                opt_str(&a["botId"]),
                now,
                k.as_ref(),
            );
            let file = file.map(|f: LogFile| {
                let name = f.name.replace(&format!("-{now}.txt"), "-N.txt");
                json!({ "name": name, "text": String::from_utf8(f.bytes).unwrap_or_default() })
            });
            json!({ "embed": embed, "file": file })
        }
        "invitePosted" => {
            let codes: Vec<String> = de(&a["codes"]);
            ev::build_invite_posted_embed(&de(&a["message"]), &codes, opt_str(&a["vanityCode"]), now, None)
        }
        "findInviteCodes" => json!(ev::find_invite_codes(a["content"].as_str())),
        "memberJoin" => {
            let member: ev::LogMember = de(&a["member"]);
            let invite: Option<Option<ev::InviteInfo>> = if a["inviteState"] == "unset" { None } else { Some(de(&a["invite"])) };
            ev::build_member_join_embed(&member, invite.as_ref().map(Option::as_ref), a["memberCount"].as_u64(), now)
        }
        "memberLeave" => ev::build_member_leave_embed(&de(&a["member"]), now),
        "timeInServer" => json!(ev::format_time_in_server(a["ms"].as_i64().unwrap_or(0))),
        "diffMember" => {
            let old: Option<ev::LogMember> = de(&a["oldMember"]);
            let d = ev::diff_member_update(old.as_ref(), &de(&a["newMember"]), a["oldKnown"].as_bool().unwrap_or(false), now);
            json!({
                "nickname": d.nickname.map(|n| json!({ "before": n.before, "after": n.after })),
                "roles": d.roles.map(|r| json!({ "added": r.added, "removed": r.removed })),
                "avatar": d.avatar.map(|x| json!({ "before": x.before, "after": x.after })),
                "timeout": d.timeout.map(|t| {
                    let from = match &t { ev::TimeoutChange::Changed { from, .. } => *from, _ => None };
                    json!({ "kind": t.kind(), "until": t.until(), "from": from })
                }),
            })
        }
        "memberRoles" => {
            let r = &a["roles"];
            ev::build_member_roles_embed(&de(&a["member"]), &ev::RolesChange { added: de(&r["added"]), removed: de(&r["removed"]) }, now)
        }
        "nickname" => ev::build_nickname_embed(&de(&a["member"]), &ev::NicknameChange { before: opt(&a["nickname"]["before"]), after: opt(&a["nickname"]["after"]) }, now),
        "timeout" => {
            let t = &a["timeout"];
            let change = match t["kind"].as_str().unwrap_or("") {
                "removed" => ev::TimeoutChange::Removed,
                "changed" => ev::TimeoutChange::Changed { from: t["from"].as_i64(), until: t["until"].as_i64() },
                "active-unknown" => ev::TimeoutChange::ActiveUnknown { until: t["until"].as_i64() },
                _ => ev::TimeoutChange::Added { until: t["until"].as_i64() },
            };
            ev::build_timeout_embed(&de(&a["member"]), &change, now)
        }
        "avatar" => ev::build_avatar_embed(&de(&a["member"]), &ev::AvatarChange { before: opt(&a["avatar"]["before"]), after: opt(&a["avatar"]["after"]) }, now),
        "userUpdate" => {
            let (o, n): (ev::LogUser, ev::LogUser) = (de(&a["oldUser"]), de(&a["user"]));
            ev::build_user_update_embed(&n, &ev::diff_user(Some(&o), Some(&n)), now).unwrap_or(Value::Null)
        }
        "ban" => ev::build_ban_embed(&de(&a["user"]), opt_str(&a["executorId"]), opt_str(&a["reason"]), a["unban"].as_bool().unwrap_or(false), opt_str(&a["botId"]), now),
        "voice" => {
            let old: Option<ev::LogVoiceState> = de(&a["oldState"]);
            let new: Option<ev::LogVoiceState> = de(&a["newState"]);
            let c = ev::classify_voice(old.as_ref(), new.as_ref());
            let kind = c.kind.map(|k| match k {
                ev::VoiceKind::Join => "join",
                ev::VoiceKind::Leave => "leave",
                ev::VoiceKind::Move => "move",
                ev::VoiceKind::State => "state",
            });
            let embed = ev::build_voice_embed(&de(&a["member"]), k.as_ref(), names(a).as_ref(), &c, now);
            json!({ "c": { "kind": kind, "oldId": c.old_id, "newId": c.new_id, "changes": c.changes }, "embed": embed })
        }
        "channelCreate" => ev::build_channel_create_embed(&de(&a["channel"]), now),
        "channelUpdate" => {
            let (o, n): (ev::LogChannel, ev::LogChannel) = (de(&a["old"]), de(&a["new"]));
            let lines = ev::diff_channel(&o, &n);
            json!({ "lines": lines_json(&lines), "embed": ev::build_channel_update_embed(&n, &lines, now) })
        }
        "channelDelete" => {
            let occ: Vec<String> = de(&a["occupants"]);
            ev::build_channel_delete_embed(&de(&a["channel"]), &occ, opt_str(&a["botReason"]), opt_str(&a["botId"]), now)
        }
        "threadCreate" => ev::build_thread_create_embed(&de(&a["thread"]), now),
        "threadUpdate" => {
            let (o, n): (ev::LogThread, ev::LogThread) = (de(&a["old"]), de(&a["new"]));
            let lines = ev::diff_thread(&o, &n);
            json!({ "lines": lines_json(&lines), "embed": ev::build_thread_update_embed(&n, &lines, now) })
        }
        "threadDelete" => ev::build_thread_delete_embed(&de(&a["thread"]), opt_str(&a["botReason"]), opt_str(&a["botId"]), now, k.as_ref()),
        "roleCreate" => ev::build_role_create_embed(&de(&a["role"]), now),
        "roleUpdate" => {
            let (o, n): (ev::LogRole, ev::LogRole) = (de(&a["old"]), de(&a["new"]));
            let lines = ev::diff_role(&o, &n);
            json!({ "lines": lines_json(&lines), "embed": ev::build_role_update_embed(&n, &lines, now) })
        }
        "roleDelete" => ev::build_role_delete_embed(&de(&a["role"]), now),
        "guildUpdate" => {
            let (o, n): (ev::LogGuild, ev::LogGuild) = (de(&a["old"]), de(&a["new"]));
            let lines = ev::diff_guild(&o, &n);
            json!({ "lines": lines_json(&lines), "embed": ev::build_guild_update_embed(&o, &n, &lines, now) })
        }
        "emojiCreate" => ev::build_emoji_create_embed(&de(&a["emoji"]), now),
        "emojiUpdate" => {
            let (o, n): (ev::LogEmoji, ev::LogEmoji) = (de(&a["old"]), de(&a["new"]));
            let lines = ev::diff_emoji(&o, &n);
            json!({ "lines": lines_json(&lines), "embed": ev::build_emoji_update_embed(&n, &lines, now) })
        }
        "emojiDelete" => ev::build_emoji_delete_embed(&de(&a["emoji"]), now),
        "inviteCreate" => ev::build_invite_create_embed(&de(&a["invite"]), now),
        "inviteDelete" => ev::build_invite_delete_embed(&de(&a["invite"]), now),
        "fitEmbedData" => fit_embed_data(&a["data"]),
        "extractMentions" => {
            let m: ev::LogMessage = de(&a["message"]);
            json!(svc::extract_mentions(Some(&m), a["exclude"].as_str()))
        }
        "channelTypeLabels" => json!([0u8, 1, 2, 4, 5, 10, 11, 12, 13, 14, 15, 16, 99].map(svc::channel_type_label)),
        "toggle" => json!([
            toggle_log_events(&["junk", "roleDelete", "roleDelete", "messageEdit"], "memberBan", false),
            toggle_log_events(&["messageEdit"], "all", true),
            toggle_log_events::<&str>(&[], "all", false),
            toggle_log_events(&["memberBan"], "memberBan", true),
        ]),
        "registry" => Value::Array(LOG_EVENTS.iter().map(|e| json!({ "key": e.key, "type": e.log_type.key(), "emoji": e.emoji, "label": e.label })).collect()),
        other => panic!("unknown case fn {other}"),
    }
}

// ── Golden: commands, plugins, every builder ──────────────────────────────────

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["logging", "invites"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "logging");
    bot_core::testing::assert_plugin_matches_golden(&reg, "invites");
}

#[test]
fn every_embed_builder_matches_the_js_output() {
    let g = golden();
    let now = g["now"].as_i64().unwrap_or(0);
    let cases = g["embeds"].as_array().cloned().unwrap_or_default();
    assert!(cases.len() > 80, "{} cases", cases.len());
    let mut failures = Vec::new();
    for c in cases.iter().filter(|c| c["fn"] != "messageSnapshot") {
        let got = strip(&run_embed_case(c["fn"].as_str().unwrap_or(""), &c["args"], now));
        let want = strip(&c["out"]);
        if got != want {
            failures.push(format!("{}:\n  rust {got}\n  js   {want}", c["name"]));
        }
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

#[test]
fn message_snapshots_are_written_and_read_exactly_as_the_js_does() {
    use feat_logging::message_snapshot::{Snapshot, build_snapshot, snapshot_to_message};
    let g = golden();
    let now = g["now"].as_i64().unwrap_or(0);
    let cases: Vec<&Value> = g["embeds"].as_array().expect("embeds").iter().filter(|c| c["fn"] == "messageSnapshot").collect();
    assert!(cases.len() >= 5, "{} cases", cases.len());
    for c in cases {
        let a = &c["args"];
        let written = serde_json::to_value(build_snapshot(&de::<ev::LogMessage>(&a["message"])).expect("author")).expect("json");
        assert_eq!(written, c["out"]["snapshot"], "{}: the JSON Rust stores differs from the JS", c["name"]);
        // The snapshot JS wrote deserializes in Rust and renders the same delete embed.
        let from_js: Snapshot = de(&c["out"]["snapshot"]);
        let embed = ev::build_message_delete_embed(&snapshot_to_message(&from_js), None, None, now, known(a).as_ref());
        assert_eq!(strip(&embed), strip(&c["out"]["embed"]), "{}: the embed rendered from the snapshot differs from the JS", c["name"]);
    }
}

#[test]
fn cache_key_builders_match_the_js() {
    let g = golden();
    let k = &g["keys"];
    let until = k["timeoutUntilSec"].as_i64().unwrap_or(0);
    assert_eq!(feat_logging::plugins::logging::timeout_key("955007000000000001", "222222222222222222", until), k["timeout"].as_str().unwrap_or(""));
    assert_eq!(feat_logging::invite_tracker::snapshot_key("955007000000000001"), k["snapshot"].as_str().unwrap_or(""));
    assert_eq!(feat_logging::commands::invites::link_cooldown_key("955007000000000001", "222222222222222222"), k["linkCooldown"].as_str().unwrap_or(""));
}

/// The adapters build discord.js's CDN URLs (checked against `@discordjs/rest`'s own `CDN`).
#[test]
fn adapter_urls_match_discord_js() {
    use feat_logging::adapters;
    let g = golden();
    let cdn = &g["cdn"];
    let hash = cdn["hash"].as_str().unwrap_or("");
    let user = |avatar: Option<String>| -> serenity::all::User {
        serde_json::from_value(json!({ "id": "111111111111111111", "username": "u", "discriminator": "0", "avatar": avatar })).expect("user")
    };
    let plain = user(Some(hash.into()));
    let animated = user(Some(format!("a_{hash}")));
    assert_eq!(adapters::display_avatar_url(&plain, None), cdn["avatar"].as_str().unwrap_or(""));
    assert_eq!(adapters::display_avatar_url(&plain, Some(256)), cdn["avatar256"].as_str().unwrap_or(""));
    assert_eq!(adapters::display_avatar_url(&animated, None), cdn["animatedAvatar"].as_str().unwrap_or(""));
    assert_eq!(adapters::display_avatar_url(&animated, Some(256)), cdn["animatedAvatar256"].as_str().unwrap_or(""));
    assert_eq!(adapters::display_avatar_url(&user(None), None), cdn["defaultAvatar"].as_str().unwrap_or(""));
    let member = |avatar: String| -> serenity::all::Member {
        serde_json::from_value(json!({
            "guild_id": G, "user": { "id": "111111111111111111", "username": "u", "discriminator": "0", "avatar": null }, "avatar": avatar,
            "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
        }))
        .expect("member")
    };
    assert_eq!(adapters::member_avatar_url(gid(), &member(hash.into())).as_deref(), cdn["memberAvatar256"].as_str());
    assert_eq!(adapters::member_avatar_url(gid(), &member(format!("a_{hash}"))).as_deref(), cdn["animatedMemberAvatar256"].as_str());
    let h: serenity::all::ImageHash = hash.parse().expect("hash");
    let ah: serenity::all::ImageHash = format!("a_{hash}").parse().expect("hash");
    assert_eq!(adapters::guild_icon_url(gid(), Some(&h)).as_deref(), cdn["icon"].as_str());
    assert_eq!(adapters::guild_icon_url(gid(), Some(&ah)).as_deref(), cdn["animatedIcon"].as_str());
    let emoji: serenity::all::Emoji = serde_json::from_value(json!({ "id": "1234", "name": "party", "animated": false, "available": true, "managed": false, "require_colons": true, "roles": [] })).expect("emoji");
    let e = adapters::log_emoji(&emoji);
    assert_eq!(e.image_url.as_deref(), cdn["emoji"].as_str());
    assert_eq!(e.mention, "<:party:1234>");
    assert_eq!(adapters::log_user(&plain).tag.as_deref(), Some("u"));
}

#[test]
fn log_embed_and_fit_embed_are_public() {
    let e = ev::log_embed(0, ev::log_colors::ALERT, "t");
    assert_eq!((e["color"].clone(), e["title"].clone(), e["timestamp"].clone()), (json!(0xff9800), json!("t"), json!("1970-01-01T00:00:00.000Z")));
    assert_eq!(ev::fit_embed(&json!({ "title": "x".repeat(300) }))["title"].as_str().map(bot_util::js::utf16_len), Some(256));
}

// ── 1. Registry and predicates ────────────────────────────────────────────────

#[test]
fn log_events_registry() {
    let keys = log_event_keys();
    assert_eq!(keys.len(), 32);
    for e in LOG_EVENTS {
        assert!(e.log_type != LogType::Mod, "{}", e.key);
        assert!(bot_util::js::utf16_len(e.label) <= 40);
        assert!(!e.emoji.is_empty());
    }
    assert_eq!(svc::MESSAGE_LOG_EVENTS, ["messageDelete", "messageEdit", "messageBulkDelete", "invitePosted"]);
    for k in svc::MESSAGE_LOG_EVENTS {
        assert_eq!(svc::log_event(k).map(|e| e.log_type), Some(LogType::Message));
    }
    for k in ["threadCreate", "threadUpdate", "threadDelete"] {
        assert_eq!(svc::log_event(k).map(|e| e.log_type), Some(LogType::Server));
    }
    assert_eq!(svc::log_event("userUpdate").map(|e| e.log_type), Some(LogType::Member));
    assert_eq!(LogType::Mod.label(), "Moderation");
    assert_eq!(LogType::ALL.map(LogType::key), ["MESSAGE", "MEMBER", "SERVER", "VOICE", "JOIN_LEAVE", "MOD"]);
}

fn settings_with(f: impl FnOnce(&mut bot_db::GuildSettings)) -> bot_db::GuildSettings {
    let mut s = bot_core::settings::MemorySettingsStore::new_row("1", serde_json::Map::new()).expect("a default row");
    f(&mut s);
    s
}

#[test]
fn is_log_event_enabled_and_key() {
    assert!(is_log_event_enabled(None, "messageEdit"));
    assert!(!is_log_event_enabled(None, "nope"));
    let s = settings_with(|s| s.disabled_log_events = vec!["messageEdit".into()]);
    assert!(!is_log_event_enabled(Some(&s), "messageEdit"));
    assert!(is_log_event_enabled(Some(&s), "messageDelete"));
    assert!(!is_log_event_key("toString"));
}

#[test]
fn toggle_log_events_strips_junk_and_follows_the_registry_order() {
    assert_eq!(toggle_log_events::<&str>(&[], "messageEdit", false), vec!["messageEdit"]);
    assert!(toggle_log_events(&["messageEdit"], "messageEdit", true).is_empty());
    assert_eq!(toggle_log_events::<&str>(&[], "all", false).len(), 32);
    assert!(toggle_log_events(&["messageEdit", "junk"], "all", true).is_empty());
    assert_eq!(toggle_log_events(&["junk", "roleDelete", "roleDelete", "messageEdit"], "memberBan", false), vec!["messageEdit", "memberBan", "roleDelete"]);
    assert!(toggle_log_events(&["junk"], "", true).is_empty());
    assert_eq!(toggle_log_events(&["memberBan"], "memberBan", false), vec!["memberBan"]);
}

#[test]
fn is_ignored_channel_checks_the_channel_its_parent_and_the_category() {
    let s = settings_with(|s| s.log_ignored_channel_ids = vec!["C1".into(), "CAT".into()]);
    let chain = |id: &str, p: Option<&str>, gp: Option<&str>| ChannelChain { id: Some(id.into()), parent_id: p.map(String::from), grandparent_id: gp.map(String::from) };
    assert!(is_ignored_channel(Some(&s), Some(&chain("C1", None, None))));
    assert!(is_ignored_channel(Some(&s), Some(&chain("T1", Some("C1"), None))));
    assert!(is_ignored_channel(Some(&s), Some(&chain("T1", Some("C9"), Some("CAT")))));
    assert!(is_ignored_channel(Some(&s), Some(&chain("C2", Some("CAT"), None))));
    assert!(!is_ignored_channel(Some(&s), Some(&chain("C2", Some("C3"), None))));
    assert!(!is_ignored_channel(Some(&s), None));
    assert!(!is_ignored_channel(None, Some(&chain("C1", None, None))));
    assert!(!is_ignored_channel(Some(&settings_with(|_| {})), Some(&chain("C1", None, None))));
}

#[test]
fn channel_cleanup_data_nulls_every_log_field_and_the_ignore_entry() {
    let s = settings_with(|s| {
        s.server_log_channel_id = Some("X".into());
        s.mod_log_channel_id = Some("X".into());
        s.message_log_channel_id = Some("Y".into());
    });
    assert_eq!(Value::Object(channel_cleanup_data(Some(&s), "X").unwrap_or_default()), json!({ "serverLogChannelId": null, "modLogChannelId": null }));
    let s = settings_with(|s| s.log_ignored_channel_ids = vec!["A".into(), "X".into(), "B".into()]);
    assert_eq!(Value::Object(channel_cleanup_data(Some(&s), "X").unwrap_or_default()), json!({ "logIgnoredChannelIds": ["A", "B"] }));
    let s = settings_with(|s| s.server_log_channel_id = Some("Y".into()));
    assert!(channel_cleanup_data(Some(&s), "X").is_none());
    assert!(channel_cleanup_data(None, "X").is_none());
}

#[test]
fn invite_codes_and_the_hook_filter() {
    assert_eq!(ev::find_invite_codes(Some("join discord.gg/abc and https://discord.com/invite/Xy-9 and discordapp.com/invite/abc")), vec!["abc", "Xy-9"]);
    assert!(ev::find_invite_codes(Some("discord.gg/")).is_empty());
    assert!(ev::find_invite_codes(Some("discord.gg/a")).is_empty());
    assert!(ev::find_invite_codes(None).is_empty());
    use feat_logging::plugins::logging::has_invite_hint;
    assert!(!has_invite_hint("hello"));
    assert!(has_invite_hint("see DISCORD.GG/abc"));
    assert!(has_invite_hint("https://discord.com/invite/abc"));
}

#[test]
fn ghost_pings() {
    let now = 10_000_000_000;
    let author = ev::LogUser { id: "111".into(), username: Some("a".into()), ..Default::default() };
    let role = ev::LogRoleRef { id: "444".into(), name: "Mods".into() };
    let base = ev::LogMessage { id: "1".into(), author: Some(author.clone()), mention_roles: vec![role], created_timestamp: Some(now - 5 * 60_000), ..Default::default() };
    assert!(ev::is_ghost_ping(Some(&base), None, now).0);
    assert_eq!(ev::is_ghost_ping(Some(&base), None, now).1.len(), 1);
    assert!(!ev::is_ghost_ping(Some(&base), Some("automod"), now).0);
    let old = ev::LogMessage { created_timestamp: Some(now - 2 * 3_600_000), ..base.clone() };
    assert!(!ev::is_ghost_ping(Some(&old), None, now).0);
    let selfie = ev::LogMessage { mention_roles: vec![], mention_users: vec![author], ..base.clone() };
    assert!(!ev::is_ghost_ping(Some(&selfie), None, now).0);
    let robot = ev::LogUser { id: "333".into(), bot: true, ..Default::default() };
    let bots = ev::LogMessage { mention_roles: vec![], mention_users: vec![robot], ..base.clone() };
    assert!(!ev::is_ghost_ping(Some(&bots), None, now).0);
    assert!(!ev::is_ghost_ping(Some(&ev::LogMessage { partial: true, ..base }), None, now).0);
}

#[test]
fn bulk_transcript_is_capped() {
    let t = 1_767_268_800_000;
    let user = ev::LogUser { id: "1".into(), username: Some("u".into()), ..Default::default() };
    let list: Vec<ev::LogMessage> = (0..20_000)
        .map(|i| ev::LogMessage { id: i.to_string(), content: Some("y".repeat(100)), author: Some(user.clone()), created_timestamp: Some(t + i), ..Default::default() })
        .collect();
    let text = ev::build_bulk_delete_transcript(&list, Some("x"));
    assert!(bot_util::js::utf16_len(&text) <= 1_000_100);
    assert!(text.ends_with("more messages)") && text.contains("… transcript truncated ("), "{}", &text[text.len() - 80..]);
}

#[test]
fn fit_embed_keeps_25_fields_and_6000_characters() {
    let fields: Vec<Value> = (0..30).map(|i| json!({ "name": format!("n{i}"), "value": format!("v{i}") })).collect();
    let f = fit_embed_data(&json!({ "title": "t", "fields": fields }));
    assert_eq!(f["fields"].as_array().map(Vec::len), Some(25));
    assert_eq!(f["fields"][24], json!({ "name": "…", "value": "and 6 more changes" }));
    let big: Vec<Value> = (0..10).map(|i| json!({ "name": format!("n{i}"), "value": "v".repeat(1000) })).collect();
    let f = fit_embed_data(&json!({ "title": "title", "description": "d".repeat(4000), "fields": big }));
    assert!(svc::embed_length(&f) <= 6000);
}

// ── 2. send_log ───────────────────────────────────────────────────────────────

/// One recorded post: channel id, embeds, attached file name.
type Sent = (String, Vec<Value>, Option<String>);

/// A recording Discord: every send, plus scripted failures.
#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<Sent>>,
    send_error: Mutex<Option<u64>>,
    fetches: Mutex<u32>,
    fetch_result: Mutex<Option<Result<Option<GuildChannel>, ApiError>>>,
    audit: Mutex<Vec<AuditEntry>>,
}

impl Recorder {
    fn titles(&self) -> Vec<Vec<String>> {
        self.sent.lock().unwrap().iter().map(|(_, e, _)| e.iter().map(|x| x["title"].as_str().unwrap_or("").to_string()).collect()).collect()
    }
    fn count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
    fn embed(&self, n: usize) -> Value {
        self.sent.lock().unwrap().get(n).map(|s| s.1[0].clone()).unwrap_or(Value::Null)
    }
    fn field(&self, n: usize, name: &str) -> Option<String> {
        self.embed(n)["fields"].as_array()?.iter().find(|f| f["name"] == name).and_then(|f| f["value"].as_str().map(String::from))
    }
}

#[async_trait::async_trait]
impl LogApi for Recorder {
    async fn fetch_channel(&self, _g: GuildId, _c: ChannelId) -> Result<Option<GuildChannel>, ApiError> {
        *self.fetches.lock().unwrap() += 1;
        self.fetch_result.lock().unwrap().clone().unwrap_or(Ok(None))
    }
    async fn send(&self, channel_id: ChannelId, embeds: Vec<Value>, file: Option<LogFile>) -> Result<(), ApiError> {
        if let Some(code) = *self.send_error.lock().unwrap() {
            self.sent.lock().unwrap().push(("FAILED".into(), Vec::new(), None));
            return Err(ApiError::new(Some(code), "refused"));
        }
        self.sent.lock().unwrap().push((channel_id.to_string(), embeds, file.map(|f| f.name)));
        Ok(())
    }
    async fn ban_audit_entries(&self, _g: GuildId, _unban: bool) -> Result<Vec<AuditEntry>, ApiError> {
        Ok(self.audit.lock().unwrap().clone())
    }
}

const G: &str = "955007000000000001";
const LOG: &str = "700000000000000010";
const OTHER: &str = "700000000000000001";
const BOT_ROLE: &str = "820000000000000001";

fn gid() -> GuildId {
    GuildId::new(G.parse().unwrap())
}

fn guild_with(bot_perms: P, bot_member: bool) -> FakeGuild {
    let mut g = FakeGuild::new(G, "600000000000000099").role(BOT_ROLE, 1, bot_perms).role_with("444444444444444444", 2, P::empty(), |r| r["name"] = json!("Mods"));
    if bot_member {
        g = g.member(TEST_BOT_ID, &[BOT_ROLE]);
    }
    g.member("222222222222222222", &[]).channel(LOG, 0, "logs", &[]).channel(OTHER, 0, "general", &[]).channel("700000000000000003", 2, "Lounge", &[])
}

const ALL: P = P::VIEW_CHANNEL.union(P::SEND_MESSAGES).union(P::EMBED_LINKS).union(P::ATTACH_FILES);

struct Setup {
    t: TestApp,
    rec: Arc<Recorder>,
}

async fn setup(perms: P, bot_member: bool, field: &str) -> Setup {
    setup_json(guild_with(perms, bot_member).json(), field).await
}

async fn setup_json(guild: Value, field: &str) -> Setup {
    let t = TestApp::new().with_guilds([G]);
    t.set_discord(offline_discord_as(&[guild]));
    let rec = Arc::new(Recorder::default());
    set_log_api(&t, rec.clone());
    t.settings().update_settings(G, SettingsPatch::new().set("loggingEnabled", true).set(field, LOG)).await.expect("memory store");
    Setup { t, rec }
}

impl Setup {
    async fn settings(&self) -> Option<bot_core::Settings> {
        self.t.settings().get_settings_safe(G).await
    }
    async fn log(&self, t: LogType, embed: Value, opts: SendLogOpts) {
        send_log(&self.t, gid(), t, embed, opts).await;
    }
    async fn flush(&self) {
        flush_log_queue(&self.t, None).await;
    }
}

fn plain(title: &str, description: &str) -> Value {
    json!({ "title": title, "description": description, "color": 1 })
}

#[tokio::test]
async fn send_log_honours_the_toggle_the_mod_exemption_and_disabled_events() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("loggingEnabled", false).set("modLogChannelId", OTHER)).await.expect("store");
    let opts = || SendLogOpts::default().settings(None);
    s.log(LogType::Message, plain("t", "d"), opts()).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 0, "logging off");
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(log_event_keys()))).await.expect("store");
    s.log(LogType::Mod, plain("mod", "d"), opts().event("messageDelete")).await;
    s.flush().await;
    assert_eq!(s.rec.titles(), vec![vec!["mod"]], "the moderation log ignores the toggles");

    s.t.settings().update_settings(G, SettingsPatch::new().set("loggingEnabled", true).set("disabledLogEvents", json!(["messageEdit"]))).await.expect("store");
    s.log(LogType::Message, plain("t", "d"), opts().event("messageEdit")).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 1);
    s.log(LogType::Message, plain("t", "d"), opts().event("messageDelete")).await;
    s.log(LogType::Voice, plain("v", "d"), opts()).await; // no voice channel configured
    s.flush().await;
    let sent = s.rec.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].0, LOG);
    assert_eq!(sent[1].1.len(), 1);
}

#[tokio::test]
async fn a_missing_channel_is_fetched_once_then_remembered_and_transient_failures_briefly() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("messageLogChannelId", "700000000000000099")).await.expect("store");
    let st = s.settings().await;
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default().settings(st.clone())).await;
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default().settings(st.clone())).await;
    assert_eq!(*s.rec.fetches.lock().unwrap(), 1);
    assert!(is_log_channel_dead(&s.t, "700000000000000099"));
    forget_dead_channels(&s.t, G, None);
    assert!(!is_log_channel_dead(&s.t, "700000000000000099"));

    *s.rec.fetch_result.lock().unwrap() = Some(Err(ApiError::new(Some(500), "boom")));
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default().settings(st.clone())).await;
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default().settings(st.clone())).await;
    assert_eq!(*s.rec.fetches.lock().unwrap(), 2, "skipped for a moment");
    assert!(is_log_channel_dead(&s.t, "700000000000000099"));
    s.t.clock.advance(31_000);
    assert!(!is_log_channel_dead(&s.t, "700000000000000099"), "not the 10-minute TTL");

    *s.rec.fetch_result.lock().unwrap() = Some(Err(ApiError::new(Some(10003), "Unknown Channel")));
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default().settings(st)).await;
    s.t.clock.advance(31_000);
    assert!(is_log_channel_dead(&s.t, "700000000000000099"));
    s.t.clock.advance(10 * 60_000);
    assert!(!is_log_channel_dead(&s.t, "700000000000000099"));
}

/// loggingService.js `sendLog` fetches an uncached log channel with `guild.channels.fetch(id)`: discord.js refuses a
/// channel of another guild (`GuildChannelUnowned`, a transient 30 s skip) and caches what it fetched, so the next
/// post makes no REST call.
#[tokio::test]
async fn an_uncached_log_channel_is_fetched_for_this_guild_only_and_then_cached() {
    const FOREIGN: &str = "700000000000000071";
    const FETCHED: &str = "700000000000000072";
    let rest = bot_core::testing::FakeRest::new();
    rest.on("GET", &format!("/channels/{FOREIGN}"), 200, json!({ "id": FOREIGN, "type": 0, "guild_id": "955007000000000999", "name": "elsewhere", "position": 0, "permission_overwrites": [] }));
    rest.on("GET", &format!("/channels/{FETCHED}"), 200, json!({ "id": FETCHED, "type": 0, "guild_id": G, "name": "fresh-logs", "position": 0, "permission_overwrites": [] }));
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[guild_with(ALL, true)], &rest);
    let post = |ch: &'static str| {
        let t = &t;
        async move {
            t.settings().update_settings(G, SettingsPatch::new().set("loggingEnabled", true).set("messageLogChannelId", ch)).await.expect("store");
            send_log(t, gid(), LogType::Message, plain("a", "b"), SendLogOpts::default()).await;
            flush_log_queue(t, None).await;
        }
    };

    post(FOREIGN).await;
    assert!(rest.calls_to("POST", &format!("/channels/{FOREIGN}/messages")).is_empty(), "never posts into another guild's channel");
    assert!(is_log_channel_dead(&t, FOREIGN));
    t.clock.advance(31_000);
    assert!(!is_log_channel_dead(&t, FOREIGN), "GuildChannelUnowned is not a 'gone' code: a 30 s skip");

    post(FETCHED).await;
    post(FETCHED).await;
    assert_eq!(rest.calls_to("POST", &format!("/channels/{FETCHED}/messages")).len(), 2);
    assert_eq!(rest.calls_to("GET", &format!("/channels/{FETCHED}")).len(), 1, "the fetched channel was cached");
}

#[tokio::test]
async fn a_missing_permission_marks_the_channel_dead_until_forgotten() {
    let s = setup(P::VIEW_CHANNEL, true, "messageLogChannelId").await;
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 0);
    assert!(is_log_channel_dead(&s.t, LOG));
    let h = get_log_channel_health(&s.t, gid(), Some(LOG));
    assert_eq!((h.state.as_str(), h.text.as_str()), ("no-permission", "missing Send Messages, Embed Links"));
    assert_eq!(get_log_channel_health(&s.t, gid(), None).state.as_str(), "unset");
    let missing = get_log_channel_health(&s.t, gid(), Some("700000000000000555"));
    assert_eq!((missing.state.as_str(), missing.text.as_str()), ("missing", "channel not found (deleted?)"));
    assert_eq!(get_log_channel_health(&s.t, gid(), Some("700000000000000003")).state.as_str(), "no-permission");
}

#[tokio::test]
async fn an_unknown_bot_member_never_marks_a_channel_dead() {
    let s = setup(P::VIEW_CHANNEL, false, "messageLogChannelId").await;
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default()).await;
    s.flush().await;
    assert!(!is_log_channel_dead(&s.t, LOG));
    assert_eq!(s.rec.count(), 1, "the send is attempted");
}

#[tokio::test]
async fn bursts_are_batched_by_ten_and_by_6000_characters() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    for i in 0..12 {
        s.log(LogType::Message, plain(&format!("e{i}"), "d"), SendLogOpts::default()).await;
    }
    assert_eq!(s.rec.count(), 0, "nothing before the flush");
    s.flush().await;
    let t = s.rec.titles();
    assert_eq!(t.iter().map(Vec::len).collect::<Vec<_>>(), vec![10, 2]);
    assert_eq!((t[0][0].as_str(), t[1][1].as_str()), ("e0", "e11"));

    s.rec.sent.lock().unwrap().clear();
    for i in 0..4 {
        s.log(LogType::Message, plain(&format!("e{i}"), &"x".repeat(2500)), SendLogOpts::default()).await;
    }
    s.flush().await;
    assert_eq!(s.rec.titles().iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2]);
}

#[tokio::test]
async fn the_queue_is_capped_and_says_how_many_entries_were_dropped() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    for i in 0..QUEUE_MAX_ITEMS + 5 {
        s.log(LogType::Message, plain(&format!("e{i}"), "d"), SendLogOpts::default()).await;
    }
    s.flush().await;
    let first = s.rec.embed(0);
    assert_eq!(first["title"], "⚠️  Log entries dropped");
    assert!(first["description"].as_str().unwrap_or("").starts_with("5 log entries were dropped"));
    assert_eq!(s.rec.titles()[1][0], "e5");
    assert_eq!(s.rec.count(), 1 + QUEUE_MAX_ITEMS / 10);
}

#[tokio::test]
async fn a_50013_marks_the_channel_dead_other_errors_drop_the_batch_only() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    *s.rec.send_error.lock().unwrap() = Some(50013);
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 1);
    assert!(is_log_channel_dead(&s.t, LOG));
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 1, "a no-op now");
    forget_dead_channels(&s.t, G, None);
    *s.rec.send_error.lock().unwrap() = Some(500);
    s.log(LogType::Message, plain("a", "b"), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 2);
    assert!(!is_log_channel_dead(&s.t, LOG));
}

#[tokio::test]
async fn an_attachment_goes_alone_and_is_omitted_without_attach_files() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    let file = || LogFile { name: "a.txt".into(), bytes: b"hi".to_vec() };
    s.log(LogType::Message, plain("a", "x"), SendLogOpts::default()).await;
    s.log(LogType::Message, plain("b", "x"), SendLogOpts::default().file(file())).await;
    s.log(LogType::Message, plain("c", "x"), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.titles(), vec![vec!["a"], vec!["b"], vec!["c"]]);
    assert_eq!(s.rec.sent.lock().unwrap()[1].2.as_deref(), Some("a.txt"));

    let n = setup(P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS, true, "messageLogChannelId").await;
    n.log(LogType::Message, plain("b", "x"), SendLogOpts::default().file(file())).await;
    n.flush().await;
    assert_eq!(n.rec.sent.lock().unwrap()[0].2, None);
    let fields = n.rec.embed(0)["fields"].as_array().cloned().unwrap_or_default();
    assert_eq!(fields.last().map(|f| f["name"].clone()), Some(json!("Attachment omitted")));
}

#[tokio::test]
async fn bad_input_never_posts_and_a_raw_embed_is_clamped() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    svc::send_log_by_key(&s.t, gid(), "NOPE", plain("t", "d"), SendLogOpts::default()).await;
    s.log(LogType::Message, Value::Null, SendLogOpts::default()).await;
    s.log(LogType::Message, json!({}), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 0);
    s.log(LogType::Message, json!({ "title": "raw", "description": "x".repeat(5000) }), SendLogOpts::default()).await;
    s.flush().await;
    assert_eq!(s.rec.embed(0)["description"].as_str().map(bot_util::js::utf16_len), Some(4096));
}

#[tokio::test]
async fn mark_deleted_and_forget() {
    let t = TestApp::new();
    assert!(!is_log_channel_dead(&t, "c1"));
    mark_log_channel_deleted(&t, G, "c1");
    assert!(is_log_channel_dead(&t, "c1"));
    forget_dead_channels(&t, G, Some("c1"));
    assert!(!is_log_channel_dead(&t, "c1"));
    mark_log_channel_deleted(&t, G, "c2");
    mark_log_channel_deleted(&t, "other-guild", "c3");
    forget_dead_channels(&t, G, None);
    assert!(!is_log_channel_dead(&t, "c2"));
    assert!(is_log_channel_dead(&t, "c3"));
}

#[tokio::test]
async fn the_shutdown_flush_is_registered_on_the_first_post() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    s.log(LogType::Message, plain("late", "d"), SendLogOpts::default()).await;
    bot_core::client::run_shutdown_flushes(&s.t).await;
    assert_eq!(s.rec.titles(), vec![vec!["late"]], "the queue was flushed at shutdown");
}

#[tokio::test]
async fn logging_reports_could_not_save_when_the_write_fails() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    s.t.settings_store.set_available(false);
    let i = FakeInteraction::slash("logging").guild(G).user("600000000000000099").permissions(P::MANAGE_GUILD | P::ADMINISTRATOR).subcommand("disable");
    bot_core::router::route_interaction(&s.t, i.build()).await;
    let calls = s.t.responses.calls();
    let last = calls.last().expect("an answer");
    assert_eq!(last.embed_title(), Some("❌  Could Not Save"));
    assert_eq!(last.embed_description(), Some("Something went wrong while saving. Please try again in a moment."));
}

// ── 3. Autocomplete and the payload ───────────────────────────────────────────

#[test]
fn logging_autocomplete_filters_and_shows_every_event_on_without_settings() {
    use feat_logging::commands::logging::autocomplete_choices;
    let all = autocomplete_choices("", None);
    assert_eq!(all.len(), 25);
    assert_eq!(all[0], ("All events".to_string(), "all".to_string()));
    assert_eq!(all[1].1, "messageDelete");
    assert!(all[1..].iter().all(|(n, _)| n.starts_with("✅ ")));
    let values = |q: &str| autocomplete_choices(q, None).into_iter().map(|c| c.1).collect::<Vec<_>>();
    assert_eq!(values(" BAN "), vec!["memberBan", "memberUnban"]);
    assert_eq!(values("voice"), vec!["voiceJoin", "voiceLeave", "voiceMove", "voiceState"]);
    assert!(values("user").contains(&"userUpdate".to_string()));
    let s = settings_with(|s| s.disabled_log_events = vec!["memberBan".into()]);
    assert_eq!(autocomplete_choices("member banned", Some(&s))[0].0, "❌ Member banned — Members");
}

#[test]
fn logging_payload() {
    let json = feat_logging::commands::logging::command().payload();
    let names: Vec<&str> = json["options"].as_array().unwrap().iter().filter_map(|o| o["name"].as_str()).collect();
    assert_eq!(names, vec!["enable", "disable", "status", "channel", "event", "ignore"]);
    assert_eq!(json["default_member_permissions"], "32");
    let meta = feat_logging::commands::logging::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("logging"), json!(false), json!(2)));
    assert_eq!(bot_config::limits::LOG_IGNORED_CHANNELS, 100);
}

// ── 4. Hooks through the hook runner ──────────────────────────────────────────

fn user_json(id: &str, bot: bool) -> Value {
    json!({ "id": id, "username": format!("user{}", &id[id.len() - 3..]), "discriminator": "0", "avatar": null, "bot": bot })
}

fn message(id: &str, author: &str, content: &str, extra: impl FnOnce(&mut Value)) -> serenity::all::Message {
    let mut v = json!({
        "id": id, "channel_id": OTHER, "guild_id": G, "author": user_json(author, false), "content": content,
        "timestamp": bot_util::js::to_iso_string(bot_util::js::now_ms() - 60_000.0), "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0, "components": []
    });
    extra(&mut v);
    serde_json::from_value(v).expect("message")
}

fn member(id: &str, extra: impl FnOnce(&mut Value)) -> serenity::all::Member {
    let mut v = json!({ "guild_id": G, "user": user_json(id, false), "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "nick": null });
    extra(&mut v);
    serde_json::from_value(v).expect("member")
}

fn installed(s: &Setup) {
    s.t.install(registry());
}

#[tokio::test]
async fn message_delete_hook_ghost_ping_bot_deletion_bots_ignored_and_partials() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    let del = |m: Option<serenity::all::Message>, id: &str| MessageDelete {
        guild_id: gid(),
        message_id: serenity::all::MessageId::new(id.parse().unwrap()),
        channel_id: ChannelId::new(OTHER.parse().unwrap()),
        partial: m.is_none(),
        message: m,
        channel: None,
    };
    let victim = user_json("222222222222222222", false);
    let ghost = message("801", "111111111111111111", "hi", |v| v["mentions"] = json!([victim]));
    s.t.run_hooks(del(Some(ghost), "801")).await;
    s.flush().await;
    assert_eq!(s.rec.embed(0)["title"], "👻 Ghost Ping Detected — Message Deleted");
    assert!(s.rec.field(0, "⚠️ Ghost Pinged").unwrap_or_default().contains("<@222222222222222222>"));

    bot_core::deletion_tracker::mark_bot_deletion(&s.t, &["802"], Some("automod"), None);
    let m = message("802", "111111111111111111", "hi", |v| v["mentions"] = json!([victim]));
    s.t.run_hooks(del(Some(m), "802")).await;
    s.flush().await;
    assert_eq!(s.rec.embed(1)["title"], "🗑️ Message Deleted");
    assert_eq!(s.rec.field(1, "Deleted By").as_deref(), Some(format!("<@{TEST_BOT_ID}> (automod)").as_str()));

    let before = s.rec.count();
    let bot_author = message("803", "333333333333333333", "hi", |v| v["author"]["bot"] = json!(true));
    s.t.run_hooks(del(Some(bot_author), "803")).await;
    bot_core::deletion_tracker::mark_bot_deletion(&s.t, &["806"], Some("purge"), None);
    s.t.run_hooks(del(None, "806")).await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("logIgnoredChannelIds", json!([OTHER]))).await.expect("store");
    s.t.run_hooks(del(Some(message("804", "111111111111111111", "x", |_| {})), "804")).await;
    s.flush().await;
    assert_eq!(s.rec.count(), before, "bot authors, purged partials and ignored channels are not logged");

    s.t.settings().update_settings(G, SettingsPatch::new().set("logIgnoredChannelIds", json!([]))).await.expect("store");
    s.t.run_hooks(del(None, "807")).await;
    s.flush().await;
    assert!(s.rec.embed(before)["description"].as_str().unwrap_or("").contains("not cached"));

    // discord.js reads createdTimestamp from the snowflake: an uncached delete still says when the message was sent.
    s.t.run_hooks(del(None, "1573137071177211955")).await;
    s.flush().await;
    assert!(s.rec.embed(before + 1)["description"].as_str().unwrap_or("").contains("not cached"));
    assert_eq!(s.rec.field(before + 1, "Sent").as_deref(), Some("<t:1795135505:R>"));
    assert_eq!(s.rec.embed(before + 1)["footer"]["text"], "Msg ID: 1573137071177211955");
}

// ── Redis message snapshots ───────────────────────────────────────────────────

const AUTHOR: &str = "111111111111111111";

fn skey(id: &str) -> String {
    feat_logging::message_snapshot::snapshot_key(OTHER, id)
}

fn raw(s: &Setup, key: &str) -> Option<Value> {
    s.t.cache().memory().and_then(|m| m.raw(key)).and_then(|r| serde_json::from_str(&r).ok())
}

/// The snapshot calls run detached from the hook: wait until `cond` holds (or give up after ~1 s).
async fn settle(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn create_of(m: serenity::all::Message) -> MessageCreate {
    MessageCreate { guild_id: gid(), message: m, member: None, channel_id: ChannelId::new(OTHER.parse().unwrap()), channel: None }
}

fn delete_of(m: Option<serenity::all::Message>, id: &str) -> MessageDelete {
    MessageDelete {
        guild_id: gid(),
        message_id: serenity::all::MessageId::new(id.parse().unwrap()),
        channel_id: ChannelId::new(OTHER.parse().unwrap()),
        partial: m.is_none(),
        message: m,
        channel: None,
    }
}

fn edit_of(m: serenity::all::Message) -> MessageUpdate {
    MessageUpdate { guild_id: gid(), old_message: None, message: m, old_known: false, member: None, channel_id: ChannelId::new(OTHER.parse().unwrap()), channel: None }
}

fn edited(id: &str, content: &str) -> serenity::all::Message {
    message(id, AUTHOR, content, |v| v["edited_timestamp"] = json!(bot_util::js::to_iso_string(bot_util::js::now_ms()).unwrap_or_default()))
}

#[tokio::test]
async fn message_create_stores_a_snapshot_the_js_bot_can_read() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    let m = message("1001", AUTHOR, "hello `there`", |v| {
        v["attachments"] = json!([{ "id": "5", "filename": "cat.png", "size": 2048, "url": "https://cdn.example/cat.png", "proxy_url": "https://cdn.example/cat.png" }])
    });
    s.t.run_hooks(create_of(m)).await;
    settle(|| raw(&s, &skey("1001")).is_some()).await;
    let snap = raw(&s, &skey("1001")).expect("snapshot stored");
    assert_eq!(snap["id"], "1001");
    assert_eq!(snap["channelId"], OTHER);
    assert_eq!(snap["content"], "hello `there`");
    assert_eq!(snap["author"]["id"], AUTHOR);
    assert_eq!(snap["author"]["bot"], false);
    assert_eq!(snap["attachments"], json!([{ "name": "cat.png", "size": 2048, "url": "https://cdn.example/cat.png" }]));
    assert_eq!(snap["embeds"], 0);
    let ttl_ms = s.t.cache().memory().expect("memory").pttl(&skey("1001"));
    assert!((6 * 86_400_000..=7 * 86_400_000).contains(&ttl_ms), "ttl {ttl_ms}");
}

#[tokio::test]
async fn message_create_skips_bots_webhooks_empty_ignored_and_disabled() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    s.t.run_hooks(create_of(message("1101", "333333333333333333", "bot", |v| v["author"]["bot"] = json!(true)))).await;
    s.t.run_hooks(create_of(message("1102", AUTHOR, "webhook", |v| v["webhook_id"] = json!("123456789012345678")))).await;
    s.t.run_hooks(create_of(message("1103", AUTHOR, "", |_| {}))).await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["messageDelete", "messageEdit"]))).await.expect("store");
    s.t.run_hooks(create_of(message("1104", AUTHOR, "both events off", |_| {}))).await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["messageDelete"])).set("logIgnoredChannelIds", json!([OTHER]))).await.expect("store");
    s.t.run_hooks(create_of(message("1105", AUTHOR, "ignored channel", |_| {}))).await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("logIgnoredChannelIds", json!([])).set("loggingEnabled", false)).await.expect("store");
    s.t.run_hooks(create_of(message("1106", AUTHOR, "module off", |_| {}))).await;
    // One that is stored proves the hook ran at all (only messageEdit is still logged).
    s.t.settings().update_settings(G, SettingsPatch::new().set("loggingEnabled", true)).await.expect("store");
    s.t.run_hooks(create_of(message("1107", AUTHOR, "edit only", |_| {}))).await;
    settle(|| raw(&s, &skey("1107")).is_some()).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    for id in ["1101", "1102", "1103", "1104", "1105", "1106"] {
        assert!(raw(&s, &skey(id)).is_none(), "{id} must not be stored");
    }
    assert!(raw(&s, &skey("1107")).is_some());
}

#[tokio::test]
async fn a_partial_delete_is_rendered_from_the_snapshot_and_the_snapshot_is_forgotten() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    s.t.run_hooks(create_of(message("1201", AUTHOR, "secret words", |_| {}))).await;
    settle(|| raw(&s, &skey("1201")).is_some()).await;
    s.t.run_hooks(delete_of(None, "1201")).await;
    s.flush().await;
    let e = s.rec.embed(0);
    assert_eq!(e["title"], "🗑️ Message Deleted");
    let text = e.to_string();
    assert!(text.contains("secret words") && text.contains(AUTHOR) && !text.contains("not cached"), "{text}");
    settle(|| raw(&s, &skey("1201")).is_none()).await;
    assert!(raw(&s, &skey("1201")).is_none(), "forgotten after the delete");
    // The key is gone: a second delete is the plain "not cached" embed.
    s.t.run_hooks(delete_of(None, "1201")).await;
    s.flush().await;
    assert!(s.rec.embed(1)["description"].as_str().unwrap_or("").contains("not cached"));
}

#[tokio::test]
async fn a_js_written_snapshot_is_read_and_bot_or_purged_ones_are_skipped() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    let mem = s.t.cache().memory().expect("memory");
    let js = |id: &str, bot: bool| {
        json!({ "id": id, "channelId": OTHER, "content": "from the js bot", "author": { "id": AUTHOR, "username": "alice", "tag": "alice", "globalName": null, "avatar": null, "bot": bot },
                "createdTimestamp": null, "attachments": [], "embeds": 0 })
        .to_string()
    };
    mem.put_raw(&skey("1301"), &js("1301", false), Some(60_000));
    mem.put_raw(&skey("1302"), &js("1302", true), Some(60_000));
    mem.put_raw(&skey("1303"), &js("1303", false), Some(60_000));
    mem.put_raw(&skey("1304"), "not json{", Some(60_000));
    s.t.run_hooks(delete_of(None, "1301")).await;
    s.t.run_hooks(delete_of(None, "1302")).await; // a bot author's snapshot: nothing logged
    bot_core::deletion_tracker::mark_bot_deletion(&s.t, &["1303"], Some("purge"), None);
    s.t.run_hooks(delete_of(None, "1303")).await; // a purged partial: nothing logged
    s.t.run_hooks(delete_of(None, "1304")).await; // unreadable data is a miss
    s.flush().await;
    assert_eq!(s.rec.titles().concat().len(), 2, "{:?}", s.rec.titles());
    assert!(s.rec.embed(0).to_string().contains("from the js bot"));
    assert!(s.rec.sent.lock().unwrap()[0].1[1]["description"].as_str().unwrap_or("").contains("not cached"));
    settle(|| raw(&s, &skey("1302")).is_none()).await;
    assert!(raw(&s, &skey("1302")).is_none(), "even a skipped delete forgets the key");
}

#[tokio::test]
async fn an_edit_of_an_uncached_message_uses_the_snapshot_as_the_before_text_and_rewrites_it() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    s.t.run_hooks(create_of(message("1401", AUTHOR, "old words", |_| {}))).await;
    settle(|| raw(&s, &skey("1401")).is_some()).await;
    s.t.run_hooks(edit_of(edited("1401", "new words"))).await;
    s.flush().await;
    let text = s.rec.embed(0).to_string();
    assert!(text.contains("old words") && text.contains("new words") && !text.contains("not cached"), "{text}");
    settle(|| raw(&s, &skey("1401")).is_some_and(|v| v["content"] == "new words")).await;
    assert_eq!(raw(&s, &skey("1401")).expect("rewritten")["content"], "new words");
    // Same text as the snapshot: not logged.
    let before = s.rec.count();
    s.t.run_hooks(edit_of(edited("1401", "new words"))).await;
    s.flush().await;
    assert_eq!(s.rec.count(), before);
    // No snapshot: logged without a before text, and the new text is remembered.
    s.t.run_hooks(edit_of(edited("1402", "fresh"))).await;
    s.flush().await;
    assert_eq!(s.rec.count(), before + 1);
    settle(|| raw(&s, &skey("1402")).is_some()).await;
    assert_eq!(raw(&s, &skey("1402")).expect("stored")["content"], "fresh");
}

#[tokio::test]
async fn an_edit_with_the_edit_event_off_still_refreshes_the_snapshot_for_a_later_delete() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["messageEdit"]))).await.expect("store");
    s.t.run_hooks(create_of(message("1501", AUTHOR, "v1", |_| {}))).await;
    settle(|| raw(&s, &skey("1501")).is_some()).await;
    s.t.run_hooks(edit_of(edited("1501", "v2"))).await;
    settle(|| raw(&s, &skey("1501")).is_some_and(|v| v["content"] == "v2")).await;
    assert_eq!(s.rec.count(), 0, "the edit itself is not logged");
    s.t.run_hooks(delete_of(None, "1501")).await;
    s.flush().await;
    assert!(s.rec.embed(0).to_string().contains("v2"));
}

#[tokio::test]
async fn a_bulk_delete_fills_partial_messages_from_one_batched_read() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    for (id, text) in [("1601", "first words"), ("1602", "second words")] {
        s.t.run_hooks(create_of(message(id, AUTHOR, text, |_| {}))).await;
    }
    settle(|| raw(&s, &skey("1601")).is_some() && raw(&s, &skey("1602")).is_some()).await;
    s.t.cache().memory().expect("memory").clear_calls();
    let ids: Vec<serenity::all::MessageId> = ["1601", "1602", "1603"].iter().map(|i| serenity::all::MessageId::new(i.parse().unwrap())).collect();
    s.t.run_hooks(MessageDeleteBulk { guild_id: gid(), channel_id: ChannelId::new(OTHER.parse().unwrap()), message_ids: ids, messages: Vec::new(), channel: None }).await;
    s.flush().await;
    let (embed, file) = {
        let sent = s.rec.sent.lock().unwrap();
        (sent[0].1[0].clone(), sent[0].2.clone())
    };
    assert_eq!(embed["title"], "🧹 Messages Bulk-Deleted");
    assert!(file.is_some(), "the transcript is attached");
    let calls = s.t.cache().memory().expect("memory").calls();
    assert_eq!(calls.iter().filter(|c| c.op == "mget").count(), 1, "{calls:?}");
    settle(|| raw(&s, &skey("1601")).is_none() && raw(&s, &skey("1602")).is_none()).await;
    assert!(raw(&s, &skey("1601")).is_none() && raw(&s, &skey("1602")).is_none());
}

#[tokio::test]
async fn redis_failures_never_stop_the_logs() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    let mem = s.t.cache().memory().expect("memory");
    mem.fail_ops(&["get", "set", "mget", "del"]);
    s.t.run_hooks(create_of(message("1701", AUTHOR, "x", |_| {}))).await;
    s.t.run_hooks(delete_of(None, "1701")).await;
    s.t.run_hooks(edit_of(edited("1702", "y"))).await;
    s.t.run_hooks(MessageDeleteBulk {
        guild_id: gid(),
        channel_id: ChannelId::new(OTHER.parse().unwrap()),
        message_ids: vec![serenity::all::MessageId::new(1703)],
        messages: Vec::new(),
        channel: None,
    })
    .await;
    mem.set_available(false);
    s.t.run_hooks(delete_of(None, "1704")).await;
    s.flush().await;
    assert_eq!(s.rec.titles().concat().len(), 4, "{:?}", s.rec.titles());
    assert!(s.rec.embed(0)["description"].as_str().unwrap_or("").contains("not cached"));
}

#[tokio::test]
async fn nothing_is_stored_without_a_message_log_channel() {
    // Only the member log channel is set: the message events have nowhere to go.
    let s = setup(ALL, true, "memberLogChannelId").await;
    installed(&s);
    s.t.run_hooks(create_of(message("1801", AUTHOR, "no destination", |_| {}))).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(raw(&s, &skey("1801")).is_none());
    let settings = s.settings().await;
    assert!(!feat_logging::message_snapshot::should_snapshot(&s.t, settings.as_ref(), gid(), &message("1802", AUTHOR, "x", |_| {})));
    s.t.settings().update_settings(G, SettingsPatch::new().set("messageLogChannelId", LOG)).await.expect("store");
    let settings = s.settings().await;
    assert!(feat_logging::message_snapshot::should_snapshot(&s.t, settings.as_ref(), gid(), &message("1802", AUTHOR, "x", |_| {})));
}

#[tokio::test]
async fn a_delete_or_bulk_delete_removes_the_snapshot_even_when_it_is_not_logged() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    for id in ["1901", "1902", "1903", "1904"] {
        s.t.run_hooks(create_of(message(id, AUTHOR, "kept words", |_| {}))).await;
    }
    settle(|| ["1901", "1902", "1903", "1904"].iter().all(|i| raw(&s, &skey(i)).is_some())).await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["messageDelete", "messageBulkDelete"]))).await.expect("store");
    s.t.run_hooks(delete_of(None, "1901")).await;
    settle(|| raw(&s, &skey("1901")).is_none()).await;
    assert!(raw(&s, &skey("1901")).is_none(), "messageDelete is off: the key still goes");
    let ids: Vec<serenity::all::MessageId> = ["1902", "1903"].iter().map(|i| serenity::all::MessageId::new(i.parse().unwrap())).collect();
    s.t.run_hooks(MessageDeleteBulk { guild_id: gid(), channel_id: ChannelId::new(OTHER.parse().unwrap()), message_ids: ids, messages: Vec::new(), channel: None }).await;
    settle(|| raw(&s, &skey("1902")).is_none() && raw(&s, &skey("1903")).is_none()).await;
    assert!(raw(&s, &skey("1902")).is_none() && raw(&s, &skey("1903")).is_none(), "messageBulkDelete is off: the keys still go");
    // The channel was ignored after the message was stored.
    s.t.settings().update_settings(G, SettingsPatch::new().set("logIgnoredChannelIds", json!([OTHER]))).await.expect("store");
    s.t.run_hooks(delete_of(None, "1904")).await;
    settle(|| raw(&s, &skey("1904")).is_none()).await;
    assert!(raw(&s, &skey("1904")).is_none());
    s.flush().await;
    assert_eq!(s.rec.count(), 0, "none of these were logged");
}

#[tokio::test]
async fn an_edit_of_a_rest_fetched_message_without_a_guild_id_still_refreshes_the_snapshot() {
    let s = setup(ALL, true, "messageLogChannelId").await;
    installed(&s);
    s.t.run_hooks(create_of(message("2001", AUTHOR, "A", |_| {}))).await;
    settle(|| raw(&s, &skey("2001")).is_some()).await;
    let no_guild = |content: &str| {
        let mut m = edited("2001", content);
        m.guild_id = None;
        m
    };
    s.t.run_hooks(edit_of(no_guild("B"))).await;
    settle(|| raw(&s, &skey("2001")).is_some_and(|v| v["content"] == "B")).await;
    assert_eq!(raw(&s, &skey("2001")).expect("rewritten")["content"], "B");
    s.t.run_hooks(edit_of(no_guild("C"))).await;
    s.flush().await;
    let befores: Vec<String> = s.rec.sent.lock().unwrap().iter().flat_map(|x| x.1.iter()).filter_map(|e| e["fields"].as_array()?.iter().find(|f| f["name"] == "Before")?["value"].as_str().map(String::from)).collect();
    assert_eq!(befores, vec!["A", "B"], "A -> B -> C shows each previous text");
}

#[tokio::test]
async fn invite_posted_hook_marks_the_vanity_url_and_honours_the_toggle() {
    let mut g = guild_with(ALL, true).json();
    g["vanity_url_code"] = json!("myserver");
    let s = setup_json(g, "messageLogChannelId").await;
    installed(&s);
    let create = |m: serenity::all::Message| MessageCreate { guild_id: gid(), message: m, member: None, channel_id: ChannelId::new(OTHER.parse().unwrap()), channel: None };
    s.t.run_hooks(create(message("901", "111111111111111111", "join discord.gg/myserver now", |_| {}))).await;
    s.t.run_hooks(create(message("902", "111111111111111111", "no link", |_| {}))).await;
    s.flush().await;
    assert_eq!(s.rec.embed(0)["title"], "📨 Invite Posted");
    assert!(s.rec.field(0, "Invites").unwrap_or_default().contains("vanity URL"));
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["invitePosted"]))).await.expect("store");
    s.t.run_hooks(create(message("903", "111111111111111111", "discord.gg/abc", |_| {}))).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 1);
}

#[tokio::test]
async fn member_update_hook_logs_parts_separately_and_dedupes_unknown_timeouts() {
    let s = setup(ALL, true, "memberLogChannelId").await;
    installed(&s);
    let upd = |old: Option<serenity::all::Member>, new: serenity::all::Member, known: bool| GuildMemberUpdate { guild_id: gid(), old_member: old, member: new, old_known: known };
    s.t.run_hooks(upd(None, member("222222222222222222", |v| v["nick"] = json!("Ally")), false)).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 0, "a partial old member logs nothing fake");

    let until = bot_util::js::to_iso_string(bot_util::js::now_ms() + 3_600_000.0).unwrap_or_default();
    let timed = |u: &str| member("222222222222222222", |v| v["communication_disabled_until"] = json!(u));
    s.t.run_hooks(upd(None, timed(&until), false)).await;
    s.t.run_hooks(upd(None, timed(&until), false)).await;
    s.flush().await;
    assert_eq!(s.rec.count(), 1, "the same unknown timeout is logged once");
    assert_eq!(s.rec.embed(0)["title"], "⏱️ Member Timed Out");
    assert!(s.rec.embed(0)["footer"]["text"].as_str().unwrap_or("").ends_with("previous state not cached"));
    let keys = s.t.cache_backend.keys();
    assert!(keys.iter().any(|k| k.starts_with(&format!("log:timeout:{G}:222222222222222222:"))), "{keys:?}");

    let old = member("222222222222222222", |v| {
        v["roles"] = json!(["444444444444444444"]);
        v["nick"] = json!("Old");
    });
    let later = bot_util::js::to_iso_string(bot_util::js::now_ms() + 7_200_000.0).unwrap_or_default();
    let new = member("222222222222222222", |v| {
        v["roles"] = json!(["444444444444444444", BOT_ROLE]);
        v["nick"] = json!("New");
        v["communication_disabled_until"] = json!(later);
    });
    s.t.run_hooks(upd(Some(old.clone()), new.clone(), true)).await;
    s.flush().await;
    let titles: Vec<String> = s.rec.titles().into_iter().flatten().collect();
    assert_eq!(titles[1..], ["🎭 Member Roles Changed", "📝 Nickname Changed", "⏱️ Member Timed Out"]);

    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["memberRoles", "memberTimeout"]))).await.expect("store");
    s.rec.sent.lock().unwrap().clear();
    s.t.run_hooks(upd(Some(old), new, true)).await;
    s.flush().await;
    assert_eq!(s.rec.titles(), vec![vec!["📝 Nickname Changed"]]);
}

#[tokio::test]
async fn the_bot_member_or_its_role_changing_clears_the_dead_channel_memo() {
    let s = setup(P::VIEW_CHANNEL, true, "memberLogChannelId").await;
    installed(&s);
    s.log(LogType::Member, plain("x", "y"), SendLogOpts::default()).await;
    assert!(is_log_channel_dead(&s.t, LOG));
    let bot = member(TEST_BOT_ID, |v| v["roles"] = json!([BOT_ROLE]));
    s.t.run_hooks(GuildMemberUpdate { guild_id: gid(), old_member: Some(bot.clone()), member: bot, old_known: true }).await;
    assert!(!is_log_channel_dead(&s.t, LOG));

    s.log(LogType::Member, plain("x", "y"), SendLogOpts::default()).await;
    assert!(is_log_channel_dead(&s.t, LOG));
    let role: serenity::all::Role = serde_json::from_value(json!({
        "id": BOT_ROLE, "name": "bot", "color": 0, "hoist": false, "position": 1, "permissions": "0", "managed": false, "mentionable": false, "flags": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .expect("role");
    s.t.settings().update_settings(G, SettingsPatch::new().set("disabledLogEvents", json!(["roleUpdate"]))).await.expect("store");
    s.t.run_hooks(RoleUpdate { guild_id: gid(), old_role: Some(role.clone()), role }).await;
    assert!(!is_log_channel_dead(&s.t, LOG));
}

#[tokio::test]
async fn voice_leave_of_a_deleted_channel_and_ban_without_audit_permission() {
    let s = setup(ALL, true, "voiceLogChannelId").await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("memberLogChannelId", OTHER)).await.expect("store");
    installed(&s);
    let vs = |ch: Option<&str>| -> serenity::all::VoiceState {
        serde_json::from_value(json!({
            "guild_id": G, "channel_id": ch, "user_id": "222222222222222222", "session_id": "s", "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null
        }))
        .expect("voice state")
    };
    let m = member("222222222222222222", |_| {});
    s.t.run_hooks(VoiceStateUpdate { guild_id: gid(), old_state: Some(vs(Some("700000000000000777"))), new_state: vs(None), member: m.clone(), change: VoiceChange::Leave }).await;
    s.t.run_hooks(VoiceStateUpdate { guild_id: gid(), old_state: Some(vs(Some(OTHER))), new_state: vs(Some(OTHER)), member: m, change: VoiceChange::State }).await;
    let user: serenity::all::User = serde_json::from_value(user_json("222222222222222222", false)).expect("user");
    s.t.run_hooks(GuildBanAdd { guild_id: gid(), user: user.clone(), reason: Some("raid".into()) }).await;
    s.t.run_hooks(GuildBanRemove { guild_id: gid(), user, reason: None }).await;
    s.flush().await;
    let sent = s.rec.sent.lock().unwrap().clone();
    let voice: Vec<&Value> = sent.iter().filter(|x| x.0 == LOG).flat_map(|x| x.1.iter()).collect();
    assert_eq!(voice.len(), 1);
    assert_eq!(voice[0]["title"], "🔇 Left Voice");
    assert!(voice[0]["description"].as_str().unwrap_or("").ends_with("(deleted channel)"));
    let bans: Vec<&Value> = sent.iter().filter(|x| x.0 == OTHER).flat_map(|x| x.1.iter()).collect();
    assert_eq!(bans.iter().map(|e| e["title"].as_str().unwrap_or("")).collect::<Vec<_>>(), vec!["🔨 Member Banned", "✅ Member Unbanned"]);
    assert_eq!(bans[0]["fields"][2]["value"], "raid");
    assert_eq!(bans[0]["fields"][1]["value"], "Unknown");
}

#[tokio::test(start_paused = true)]
async fn the_ban_hook_reads_the_executor_from_the_audit_log_when_it_may() {
    let s = setup(ALL | P::VIEW_AUDIT_LOG, true, "memberLogChannelId").await;
    installed(&s);
    let t0 = s.t.app.clock().now_ms();
    *s.rec.audit.lock().unwrap() = vec![
        AuditEntry { target_id: Some("someone".into()), executor_id: Some("x".into()), reason: None, created_ms: t0 },
        AuditEntry { target_id: Some("222222222222222222".into()), executor_id: Some(TEST_BOT_ID.into()), reason: Some("via audit".into()), created_ms: t0 },
    ];
    let user: serenity::all::User = serde_json::from_value(user_json("222222222222222222", false)).expect("user");
    s.t.run_hooks(GuildBanAdd { guild_id: gid(), user, reason: Some("raid".into()) }).await;
    s.flush().await;
    assert_eq!(s.rec.field(0, "By").as_deref(), Some(format!("<@{TEST_BOT_ID}> (via bot command)").as_str()));
    assert_eq!(s.rec.field(0, "Reason").as_deref(), Some("via audit"));
}

#[tokio::test]
async fn channel_delete_hook_marks_a_deleted_log_channel_and_lists_voice_occupants() {
    let mut g = guild_with(ALL, true).json();
    g["voice_states"] = json!([
        { "channel_id": "700000000000000003", "user_id": "222222222222222222", "session_id": "s", "deaf": false, "mute": false, "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false, "request_to_speak_timestamp": null },
    ]);
    let s = setup_json(g, "serverLogChannelId").await;
    installed(&s);
    let lounge = bot_core::interaction_helpers::cached_channel(&s.t, gid(), ChannelId::new(700000000000000003)).expect("cached");
    bot_core::deletion_tracker::mark_bot_deletion(&s.t, &["700000000000000003"], Some("tempvoice"), None);
    s.t.run_hooks(ChannelDelete { guild_id: gid(), channel: lounge }).await;
    s.flush().await;
    assert_eq!(s.rec.embed(0)["title"], "🗑️ Channel Deleted");
    assert_eq!(s.rec.field(0, "Connected Members (1)").as_deref(), Some("<@222222222222222222>"));
    assert_eq!(s.rec.field(0, "Deleted By").as_deref(), Some(format!("<@{TEST_BOT_ID}> (tempvoice)").as_str()));

    let log = bot_core::interaction_helpers::cached_channel(&s.t, gid(), ChannelId::new(LOG.parse().unwrap())).expect("cached");
    s.t.run_hooks(ChannelDelete { guild_id: gid(), channel: log }).await;
    assert!(is_log_channel_dead(&s.t, LOG));
    let after = s.t.settings().get_settings(G).await.expect("settings");
    assert_eq!(after.server_log_channel_id, None, "the deleted log channel was cleared");
}

/// logEvents.js `buildMemberLeaveEmbed` sorts `member.roles.cache` by `role.position`, discord.js's rank (raw position,
/// ties broken by id, the older role higher), and the member's uncached role ids are not in that collection.
#[tokio::test]
async fn member_leave_lists_roles_by_discord_js_rank() {
    const OLDER: &str = "830000000000000001";
    const NEWER: &str = "830000000000000002";
    let g = guild_with(ALL, true).role(NEWER, 5, P::empty()).role(OLDER, 5, P::empty());
    let s = setup_json(g.json(), "joinLeaveChannelId").await;
    installed(&s);
    let m = member("222222222222222222", |v| v["roles"] = json!([NEWER, OLDER, "830000000000000999"]));
    let user = m.user.clone();
    s.t.run_hooks(GuildMemberRemove { guild_id: gid(), member: Some(m), user }).await;
    s.flush().await;
    assert_eq!(s.rec.field(0, "Roles").as_deref(), Some(format!("<@&{OLDER}>, <@&{NEWER}>").as_str()));
}

/// discord.js `InviteDeleteAction` / `InviteCreateAction` emit nothing while the invite's channel is not cached (the
/// invites of a deleted channel): no log entry, and the invite snapshot is left alone.
#[tokio::test]
async fn invite_events_of_an_uncached_channel_are_ignored() {
    use feat_logging::invite_tracker::{InviteApi, InviteState, Vanity, current_snapshot, refresh_snapshot, set_invite_api, set_trusted_since};
    struct Two;
    #[async_trait::async_trait]
    impl InviteApi for Two {
        async fn fetch_invites(&self, _g: GuildId) -> Result<Vec<InviteState>, ApiError> {
            Ok(vec![InviteState { code: "abc".into(), ..Default::default() }, InviteState { code: "xyz".into(), ..Default::default() }])
        }
        async fn fetch_vanity(&self, _g: GuildId) -> Result<Option<Vanity>, ApiError> {
            Ok(None)
        }
        async fn fetch_invite_guild(&self, _c: &str) -> Result<Option<String>, ApiError> {
            Ok(None)
        }
        async fn delete_invite(&self, _g: GuildId, _c: &str, _r: &str) -> Result<(), ApiError> {
            Ok(())
        }
        async fn create_invite(&self, _c: ChannelId, _r: &str) -> Result<String, ApiError> {
            Ok("new".into())
        }
    }
    let s = setup(ALL | P::MANAGE_GUILD, true, "serverLogChannelId").await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", true)).await.expect("store");
    installed(&s);
    set_invite_api(&s.t, Arc::new(Two));
    set_trusted_since(&s.t, 0);
    assert!(refresh_snapshot(&s.t, gid()).await.ok);
    for (ch, code) in [(OTHER, "abc"), ("700000000000000444", "xyz")] {
        let invite: serenity::all::InviteDeleteEvent = serde_json::from_value(json!({ "channel_id": ch, "guild_id": G, "code": code })).expect("event");
        s.t.run_hooks(InviteDelete { guild_id: gid(), invite }).await;
    }
    s.flush().await;
    let embeds: Vec<Value> = s.rec.sent.lock().unwrap().iter().flat_map(|x| x.1.clone()).collect();
    assert_eq!(embeds.len(), 1, "only the cached channel's invite is logged");
    assert_eq!(embeds[0]["footer"]["text"], "Code: abc");
    let snap = current_snapshot(&s.t, G).await.expect("snapshot");
    assert!(snap.tombstones.contains_key("abc") && !snap.codes.contains_key("abc"));
    assert!(snap.codes.contains_key("xyz"), "the uncached channel's invite was not recorded as deleted");
}

#[tokio::test]
async fn member_join_logs_the_invite_once_attribution_settles() {
    use feat_logging::invite_tracker::{InviteApi, InviteState, Vanity, set_invite_api, set_trusted_since};
    struct Invites(Mutex<i64>);
    #[async_trait::async_trait]
    impl InviteApi for Invites {
        async fn fetch_invites(&self, _g: GuildId) -> Result<Vec<InviteState>, ApiError> {
            Ok(vec![InviteState { code: "abc".into(), uses: *self.0.lock().unwrap(), inviter_id: Some("999999999999999999".into()), ..Default::default() }])
        }
        async fn fetch_vanity(&self, _g: GuildId) -> Result<Option<Vanity>, ApiError> {
            Ok(None)
        }
        async fn fetch_invite_guild(&self, _c: &str) -> Result<Option<String>, ApiError> {
            Err(ApiError::new(Some(10006), "Unknown Invite"))
        }
        async fn delete_invite(&self, _g: GuildId, _c: &str, _r: &str) -> Result<(), ApiError> {
            Ok(())
        }
        async fn create_invite(&self, _c: ChannelId, _r: &str) -> Result<String, ApiError> {
            Ok("NEW".into())
        }
    }
    let s = setup(ALL | P::MANAGE_GUILD, true, "joinLeaveChannelId").await;
    s.t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", true)).await.expect("store");
    installed(&s);
    let api = Arc::new(Invites(Mutex::new(0)));
    set_invite_api(&s.t, api.clone());
    set_trusted_since(&s.t, 0);
    assert!(feat_logging::invite_tracker::refresh_snapshot(&s.t, gid()).await.ok);
    *api.0.lock().unwrap() = 1;
    let m = member("222222222222222222", |_| {});
    let user = m.user.clone();
    let ctx = s.t.run_hooks(GuildMemberAdd { guild_id: gid(), member: m, user }).await;
    let pending = ctx.state.get::<feat_logging::InvitePending>("invitePromise").expect("published");
    let got = pending.0.await;
    assert_eq!(got.as_ref().and_then(|i| i.code.as_deref()), Some("abc"));
    // The join log is posted by a task waiting on the attribution: wait (bounded) until it is queued or sent.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while s.rec.count() == 0 && std::time::Instant::now() < deadline {
        if svc::queued_len(&s.t, LOG) > 0 {
            s.flush().await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert_eq!(s.rec.field(0, "Invite").as_deref(), Some("discord.gg/abc by <@999999999999999999>"));
    assert_eq!(ctx.state.get::<Option<feat_logging::InviteInfo>>("invite").flatten().and_then(|i| i.code), Some("abc".into()));

    // With the module off the (always-on) invites hook still records the join, as unknown: the field says Unknown.
    s.t.settings().update_settings(G, SettingsPatch::new().set("invitesEnabled", false)).await.expect("store");
    let m = member("222222222222222224", |_| {});
    let user = m.user.clone();
    s.t.run_hooks(GuildMemberAdd { guild_id: gid(), member: m, user }).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while s.rec.count() < 2 && std::time::Instant::now() < deadline {
        if svc::queued_len(&s.t, LOG) > 0 {
            s.flush().await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert_eq!(s.rec.field(1, "Invite").as_deref(), Some("Unknown"));
}

// ── 5. With PostgreSQL: /logging end to end, cleanup, bot-managed channels ────

const DG: &str = "955007000000000101";
const DCH: &str = "955007000000000111";
const DNOPERM: &str = "955007000000000112";
const DOTHER: &str = "955007000000000113";
const DNOATTACH: &str = "955007000000000114";
const DCAT: &str = "955007000000000115";
const ADMIN: &str = "600000000000000009";

async fn db_app(guild: &'static str) -> Option<(DbTestApp, Arc<Recorder>)> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds([guild]);
    t.reset_guilds(&[guild]).await;
    let fake = FakeGuild::new(guild, "600000000000000099")
        .role(BOT_ROLE, 1, ALL)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .role("820000000000000002", 2, P::MANAGE_GUILD)
        .member(ADMIN, &["820000000000000002"])
        .channel(DCH, 0, "logs", &[])
        .channel(DNOPERM, 0, "locked", &[(BOT_ROLE, false, P::empty(), P::SEND_MESSAGES | P::EMBED_LINKS)])
        .channel(DOTHER, 0, "other", &[])
        .channel(DNOATTACH, 0, "noattach", &[(BOT_ROLE, false, P::empty(), P::ATTACH_FILES)])
        .channel(DCAT, 4, "Staff", &[]);
    t.set_discord(offline_discord_as(&[fake.json()]));
    let rec = Arc::new(Recorder::default());
    set_log_api(&t, rec.clone());
    t.install(registry());
    Some((t, rec))
}

fn slash(guild: &str, sub: &str) -> FakeInteraction {
    FakeInteraction::slash("logging").guild(guild).user(ADMIN).roles(&["820000000000000002"]).permissions(P::MANAGE_GUILD).subcommand(sub)
}

async fn route(t: &DbTestApp, i: FakeInteraction) -> Value {
    t.responses.clear();
    bot_core::router::route_interaction(t, i.build()).await;
    let calls = t.responses.calls();
    calls.last().map(|c| c.body["embeds"][0].clone()).unwrap_or(Value::Null)
}

#[tokio::test]
async fn logging_command_end_to_end() {
    let Some((t, _rec)) = db_app(DG).await else { return };
    let row = || async { t.settings().get_settings_with(DG, true).await.expect("row") };
    let chan = |c: &str| json!(c);

    let e = route(&t, slash(DG, "enable")).await;
    assert_eq!(e["title"], "⚠️  Audit Logs Enabled — No Channel Yet");
    assert!(row().await.logging_enabled);
    assert!(!t.responses.calls()[0].ephemeral(), "admin config commands answer publicly");

    route(&t, slash(DG, "disable")).await;
    assert!(!row().await.logging_enabled);
    let e = route(&t, slash(DG, "enable").option("channel", option_type::CHANNEL, chan(DNOPERM))).await;
    assert_eq!(e["title"], "❌  Missing Permissions");
    assert!(e["description"].as_str().unwrap_or("").contains("Send Messages, Embed Links"));
    assert!(!row().await.logging_enabled);

    let e = route(&t, slash(DG, "enable").option("channel", option_type::CHANNEL, chan(DCH))).await;
    assert_eq!(e["title"], "✅  Audit Logs Enabled");
    assert!(e["description"].as_str().unwrap_or("").starts_with(&format!("Every log type will be posted in <#{DCH}>.")));
    let s = row().await;
    for t in [LogType::Message, LogType::Member, LogType::Server, LogType::Voice, LogType::JoinLeave] {
        assert_eq!(t.channel_of(&s), Some(DCH));
    }
    assert_eq!(s.mod_log_channel_id, None);

    route(&t, slash(DG, "channel").option("type", option_type::STRING, json!("voice")).option("channel", option_type::CHANNEL, chan(DOTHER))).await;
    assert_eq!(row().await.voice_log_channel_id.as_deref(), Some(DOTHER));
    let e = route(&t, slash(DG, "channel").option("type", option_type::STRING, json!("message")).option("channel", option_type::CHANNEL, chan(DNOPERM))).await;
    assert_eq!(e["title"], "❌  Missing Permissions");
    let e = route(&t, slash(DG, "channel").option("type", option_type::STRING, json!("joinleave"))).await;
    assert_eq!(e["title"], "✅  Log Channel Cleared");
    assert_eq!(row().await.join_leave_channel_id, None);
    let e = route(&t, slash(DG, "channel").option("type", option_type::STRING, json!("bogus")).option("channel", option_type::CHANNEL, chan(DCH))).await;
    assert_eq!(e["title"], "❌  Unknown Type");
    let e = route(&t, slash(DG, "channel").option("type", option_type::STRING, json!("all")).option("channel", option_type::CHANNEL, chan(DOTHER))).await;
    assert!(e["description"].as_str().unwrap_or("").starts_with("**All** logs will be posted in"));

    route(&t, slash(DG, "disable")).await;
    let e = route(&t, slash(DG, "channel").option("type", option_type::STRING, json!("message")).option("channel", option_type::CHANNEL, chan(DNOATTACH))).await;
    let d = e["description"].as_str().unwrap_or("").to_string();
    assert!(d.contains("Attach Files") && d.contains("currently **off**"), "{d}");
    route(&t, slash(DG, "enable")).await;

    let ev_ = |name: &str, state: &str| slash(DG, "event").option("name", option_type::STRING, json!(name)).option("state", option_type::STRING, json!(state));
    let e = route(&t, ev_("messageEdit", "off")).await;
    assert_eq!(e["title"], "✅  Log Event Updated");
    assert_eq!(e["description"], "✏️ **Message edited** is now **off**.");
    assert_eq!(row().await.disabled_log_events, vec!["messageEdit"]);
    let e = route(&t, ev_("messageEdit", "off")).await;
    assert!(e["description"].as_str().unwrap_or("").starts_with("It was already off."));
    route(&t, ev_(" roleDelete ", "off")).await;
    assert_eq!(row().await.disabled_log_events, vec!["messageEdit", "roleDelete"]);
    let e = route(&t, ev_("all", "off")).await;
    assert_eq!(e["description"], "Every log event is now **off**.");
    assert_eq!(row().await.disabled_log_events.len(), 32);
    route(&t, ev_("all", "on")).await;
    assert!(row().await.disabled_log_events.is_empty());
    assert_eq!(route(&t, ev_("nope", "on")).await["title"], "❌  Unknown Event");
    t.settings().update_settings(DG, SettingsPatch::new().set("disabledLogEvents", json!(["junk", "memberBan"]))).await.expect("write");
    route(&t, ev_("voiceMove", "off")).await;
    assert_eq!(row().await.disabled_log_events, vec!["memberBan", "voiceMove"]);

    let e = route(&t, slash(DG, "status")).await;
    assert_eq!(e["title"], "📜 Audit Logs");
    let field = |e: &Value, n: &str| e["fields"].as_array().and_then(|f| f.iter().find(|x| x["name"] == n)).map(|f| f["value"].as_str().unwrap_or("").to_string()).unwrap_or_default();
    assert_eq!(field(&e, "Status"), "✅ On");
    assert!(field(&e, "Channels").contains(&format!("**Messages** → <#{DNOATTACH}> ✅")));
    assert!(field(&e, "Channels").contains("**Moderation** → not set"));
    assert!(field(&e, "Disabled events").contains("Member banned"));
    assert_eq!(field(&e, "Ignored channels"), "None.");
    assert_eq!(e["footer"]["text"], "30/32 events on · MEE6: Moderator › Audit logs");
    t.settings().update_settings(DG, SettingsPatch::new().set("messageLogChannelId", "955007000000000199").set("modLogChannelId", "955007000000000198")).await.expect("write");
    let e = route(&t, slash(DG, "status")).await;
    assert!(field(&e, "Channels").contains("**Messages** → <#955007000000000199> ⚠️ channel not found (deleted?)"));
    assert!(field(&e, "Channels").contains("**Moderation** → <#955007000000000198> (set with `/modconfig logchannel`)"));
    t.reset_guilds(&[DG]).await;
}

#[tokio::test]
async fn logging_ignore_add_remove_list_and_the_limit() {
    const IG: &str = "955007000000000102";
    let Some((t, _)) = db_app(IG).await else { return };
    let ig = |sub: &str, ch: Option<&str>| {
        let i = FakeInteraction::slash("logging").guild(IG).user(ADMIN).roles(&["820000000000000002"]).permissions(P::MANAGE_GUILD).group("ignore", sub);
        match ch {
            Some(c) => i.option("channel", option_type::CHANNEL, json!(c)),
            None => i,
        }
    };
    assert_eq!(route(&t, ig("list", None)).await["description"], "No channels are ignored.");
    assert_eq!(route(&t, ig("add", Some(DOTHER))).await["title"], "✅  Channel Ignored");
    assert_eq!(route(&t, ig("add", Some(DOTHER))).await["title"], "⚠️  Already Ignored");
    let added = route(&t, ig("add", Some(DCAT))).await;
    assert!(added["description"].as_str().unwrap_or("").contains("in every channel of **Staff**"), "{added}");
    let row = || async { t.settings().get_settings_with(IG, true).await.expect("row") };
    assert_eq!(row().await.log_ignored_channel_ids, vec![DOTHER, DCAT]);
    let list = route(&t, ig("list", None)).await;
    assert_eq!(list["description"], format!("<#{DOTHER}>\n<#{DCAT}>"));
    assert_eq!(list["footer"]["text"], "2/100");
    assert_eq!(route(&t, ig("remove", Some(DOTHER))).await["title"], "✅  Channel No Longer Ignored");
    assert_eq!(route(&t, ig("remove", Some(DOTHER))).await["title"], "⚠️  Not Ignored");
    assert_eq!(row().await.log_ignored_channel_ids, vec![DCAT]);
    t.settings().update_settings(IG, SettingsPatch::new().set("logIgnoredChannelIds", json!(["955007000000009999"]))).await.expect("write");
    assert!(route(&t, ig("list", None)).await["description"].as_str().unwrap_or("").contains("#deleted-channel (`955007000000009999`)"));
    let ids: Vec<String> = (0..100).map(|n| format!("9550070000{n:08}")).collect();
    t.settings().update_settings(IG, SettingsPatch::new().set("logIgnoredChannelIds", json!(ids))).await.expect("write");
    assert_eq!(route(&t, ig("add", Some(DOTHER))).await["title"], "❌  Limit Reached");
    assert_eq!(row().await.log_ignored_channel_ids.len(), 100);
    assert!(route(&t, ig("list", None)).await["description"].as_str().map(bot_util::js::utf16_len).unwrap_or(0) <= 4000);
    t.reset_guilds(&[IG]).await;
}

#[tokio::test]
async fn channel_delete_cleans_the_settings_even_while_the_module_is_off() {
    const CG: &str = "955007000000000103";
    let Some((t, rec)) = db_app(CG).await else { return };
    t.settings()
        .update_settings(CG, SettingsPatch::new().set("loggingEnabled", false).set("serverLogChannelId", DOTHER).set("modLogChannelId", DOTHER).set("joinLeaveChannelId", DCH).set("logIgnoredChannelIds", json!([DOTHER, DCH])))
        .await
        .expect("write");
    let other = bot_core::interaction_helpers::cached_channel(&t, GuildId::new(CG.parse().unwrap()), ChannelId::new(DOTHER.parse().unwrap())).expect("cached");
    t.run_hooks(ChannelDelete { guild_id: GuildId::new(CG.parse().unwrap()), channel: other }).await;
    flush_log_queue(&t, None).await;
    let after = t.settings().get_settings_with(CG, true).await.expect("row");
    assert_eq!((after.server_log_channel_id.clone(), after.mod_log_channel_id.clone()), (None, None));
    assert_eq!(after.join_leave_channel_id.as_deref(), Some(DCH));
    assert_eq!(after.log_ignored_channel_ids, vec![DCH]);
    assert!(is_log_channel_dead(&t, DOTHER));
    assert_eq!(rec.count(), 0, "nothing is logged while the module is off");
    t.reset_guilds(&[CG]).await;
}

#[tokio::test]
async fn channel_update_skips_name_only_renames_of_bot_managed_channels() {
    const MG: &str = "955007000000000104";
    let Some((t, rec)) = db_app(MG).await else { return };
    t.settings().update_settings(MG, SettingsPatch::new().set("loggingEnabled", true).set("serverLogChannelId", DCH)).await.expect("write");
    let stat = "955007000000000172";
    bot_db::sqlx::query(r#"INSERT INTO "StatChannel" ("id", "guildId", "channelId", "kind", "template", "updatedAt") VALUES ($1, $2, $3, 'MEMBERS', 'Members: {count}', (now() AT TIME ZONE 'UTC'))"#)
        .bind(bot_db::new_cuid())
        .bind(MG)
        .bind(stat)
        .execute(t.db())
        .await
        .expect("stat channel");
    let channel = |id: &str, name: &str, topic: Option<&str>| -> serenity::all::GuildChannel {
        serde_json::from_value(json!({ "id": id, "guild_id": MG, "type": 2, "name": name, "topic": topic, "position": 1, "permission_overwrites": [], "nsfw": false, "bitrate": 64000, "user_limit": 0 })).expect("channel")
    };
    let rename = |id: &str| ChannelUpdate { guild_id: GuildId::new(MG.parse().unwrap()), old_channel: Some(channel(id, "Members: 1", None)), channel: channel(id, "Members: 2", None) };
    t.run_hooks(rename(stat)).await;
    flush_log_queue(&t, None).await;
    assert_eq!(rec.count(), 0, "a stat channel rename is not logged");
    t.run_hooks(rename("955007000000000173")).await;
    flush_log_queue(&t, None).await;
    assert_eq!(rec.count(), 1);
    t.run_hooks(ChannelUpdate { guild_id: GuildId::new(MG.parse().unwrap()), old_channel: Some(channel(stat, "a", Some("x"))), channel: channel(stat, "b", Some("y")) }).await;
    flush_log_queue(&t, None).await;
    assert_eq!(rec.count(), 2, "other changes of a stat channel are logged");
    t.reset_guilds(&[MG]).await;
}

/// Cross-language: the timeout dedupe key the Rust hook writes is seen by the JS bot's `cache.setNX` (and the
/// reverse), so switching bots never logs one timeout twice.
#[tokio::test]
async fn the_timeout_dedupe_key_is_shared_with_the_js_bot() {
    let (Some(db), Some(redis)) = (database_url(), redis_url()) else { return };
    const TG: &str = "955007000000000105";
    let t = DbTestApp::with_redis(&db, &redis).await.with_guilds([TG]);
    t.reset_guilds(&[TG]).await;
    let fake = FakeGuild::new(TG, "600000000000000099").role(BOT_ROLE, 1, ALL).member(TEST_BOT_ID, &[BOT_ROLE]).channel(DCH, 0, "logs", &[]);
    t.set_discord(offline_discord_as(&[fake.json()]));
    let rec = Arc::new(Recorder::default());
    set_log_api(&t, rec.clone());
    t.install(registry());
    t.settings().update_settings(TG, SettingsPatch::new().set("loggingEnabled", true).set("memberLogChannelId", DCH)).await.expect("write");
    let until_ms = (bot_util::js::now_ms() as i64 / 1000 + 7200) * 1000 + 123;
    let until = bot_util::js::to_iso_string(until_ms as f64).unwrap_or_default();
    let m = member("222222222222222222", |v| {
        v["guild_id"] = json!(TG);
        v["communication_disabled_until"] = json!(until);
    });
    t.run_hooks(GuildMemberUpdate { guild_id: GuildId::new(TG.parse().unwrap()), old_member: None, member: m, old_known: false }).await;
    let key = feat_logging::plugins::logging::timeout_key(TG, "222222222222222222", until_ms / 1000);
    let js = run_node("rust/fixtures/gen/feat-logging-js.mjs", &["setnx", &key]);
    assert_eq!(js, json!({ "fresh": false, "raw": "1" }), "the JS bot sees the Rust key");
    let other = feat_logging::plugins::logging::timeout_key(TG, "222222222222222223", until_ms / 1000);
    let js = run_node("rust/fixtures/gen/feat-logging-js.mjs", &["setnx", &other]);
    assert_eq!(js["fresh"], true);
    assert_eq!(t.cache().set_nx(&other, &1, 60).await, Some(false), "Rust sees the JS key");
    t.reset_guilds(&[TG]).await;
}
