//! Offline tests of feat-tickets: golden checks, the ported pure tests of `test/tickets.test.js` checked against
//! JS-produced fixtures (`rust/fixtures/gen/feat-tickets-golden.mjs`), and router flows that need no database.

mod common;

use bot_core::interaction_helpers::Payload;
use bot_core::testing::{FakeGuild, FakeInteraction, TestApp, load_golden};
use bot_db::{TicketPanelOption, TicketPanelStyle};
use bot_util::placeholder_parser::{PlaceholderMember, PlaceholderUser};
use feat_tickets::ticket_service as svc;
use serde_json::{Value, json};
use serenity::all::Permissions;

fn registry() -> bot_core::Registry {
    let mut reg = bot_core::Registry::new();
    feat_tickets::register(&mut reg);
    reg
}

fn fixture() -> Value {
    load_golden("feat-tickets/pure.json")
}

/// Drops what differs only in form: `timestamp`, `animated: false`, `id: null`, `inline: false`, `disabled: false`.
fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                if k == "timestamp" || k == "type" && x == "rich" {
                    continue;
                }
                if (k == "animated" || k == "inline" || k == "disabled" || k == "required") && x == &json!(false) {
                    continue;
                }
                if x.is_null() && (k == "id" || k == "description" || k == "placeholder") {
                    continue;
                }
                out.insert(k.clone(), norm(x));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        x => x.clone(),
    }
}

fn assert_json(label: &str, got: &Value, want: &Value) {
    let (g, w) = (norm(got), norm(want));
    if let Some(d) = bot_core::testing::json_diff(&g, &w, false) {
        panic!("{label}: {d}\n got: {g}\nwant: {w}");
    }
}

fn payload_json(p: &Payload) -> Value {
    json!({
        "embeds": p.embeds.clone().unwrap_or_default().iter().map(|e| serde_json::to_value(e).unwrap()).collect::<Vec<_>>(),
        "components": p.components.clone().unwrap_or_default().iter().map(|r| serde_json::to_value(r).unwrap()).collect::<Vec<_>>(),
    })
}

// ── Golden ────────────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["ticket-panel", "ticket"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "tickets");
}

#[test]
fn customids_match_the_js() {
    let f = fixture();
    let ids = &f["customIds"];
    let t = "ckticket00000000000000001";
    assert_eq!(svc::panel_components_json("p1", TicketPanelStyle::Buttons, &[opt("o1", 0)], &|_| true)[0]["components"][0]["custom_id"], ids["open"]);
    assert_eq!(svc::intro_row_json(t, false)["components"][0]["custom_id"], ids["close"]);
    assert_eq!(svc::panel_components_json("p1", TicketPanelStyle::Select, &[opt("o1", 0)], &|_| true)[0]["components"][0]["custom_id"], ids["select"]);
    assert_eq!(svc::reason_modal_json("p1", "o1", "x")["custom_id"], ids["reason"]);
    assert_eq!(json!(bot_config::custom_id!("ticket-modal", "tpl", "p1", "panel").unwrap()), ids["tplPanel"]);
    assert_eq!(json!(bot_config::custom_id!("ticket-modal", "tpl", "p1", "intro").unwrap()), ids["tplIntro"]);
    assert_eq!(json!(bot_config::custom_id!("ticket-modal", "tpl", "p1", "intro", "o1").unwrap()), ids["tplIntroOption"]);
    // The longest ids fit Discord's 100 characters and round-trip.
    let long = bot_config::custom_id!("ticket-modal", "tpl", "p".repeat(25), "intro", "o".repeat(25)).unwrap();
    assert!(long.len() <= 100);
    assert_eq!(bot_config::parse_custom_id(&long).params.len(), 4);
}

fn opt(id: &str, position: i32) -> TicketPanelOption {
    TicketPanelOption {
        id: id.into(),
        panel_id: "p1".into(),
        label: format!("Option {id}"),
        emoji: None,
        description: None,
        button_style: 1,
        position,
        intro_template: None,
        category_id: None,
    }
}

fn opts_of(v: &Value) -> Vec<TicketPanelOption> {
    v.as_array().unwrap().iter().map(|o| serde_json::from_value(o.clone()).unwrap()).collect()
}

// ── Messages ──────────────────────────────────────────────────────────────────

#[test]
fn error_payloads_match_the_js() {
    let f = fixture();
    for c in f["errors"].as_array().unwrap() {
        let fail = svc::Fail { code: c["code"].as_str().unwrap().into(), data: c["data"].clone() };
        let p = svc::error_payload(&fail);
        assert_json(&format!("errorPayload {}", fail.code), &payload_json(&p)["embeds"], &c["payload"]["embeds"]);
    }
    for c in f["panelErrors"].as_array().unwrap() {
        let code = c["code"].as_str().unwrap();
        let p = svc::panel_error_payload(code, &c["data"]);
        assert_json(&format!("panelErrorPayload {code}"), &payload_json(&p)["embeds"], &c["payload"]["embeds"]);
    }
    for c in f["refreshNotes"].as_array().unwrap() {
        let r = match c["r"].get("code").and_then(Value::as_str) {
            None if c["r"].is_null() => None,
            None => Some(svc::Refresh::Ok { skipped: false }),
            Some("GONE") => Some(svc::Refresh::Gone),
            Some("NO_ACCESS") => Some(svc::Refresh::NoAccess { channel_id: c["r"]["channelId"].as_str().unwrap().into() }),
            Some(_) => Some(svc::Refresh::Error { reason: c["r"]["reason"].as_str().unwrap_or("unknown error").into() }),
        };
        assert_eq!(svc::refresh_note(r.as_ref()), c["note"].as_str().unwrap());
    }
    assert_eq!(svc::MANAGERS_WARNING, f["managersWarning"].as_str().unwrap());
}

#[test]
fn notices_match_the_js() {
    let f = fixture();
    let mut fields = serde_json::Map::new();
    fields.insert("embedColor".into(), json!(0x123456));
    let settings = bot_core::settings::MemorySettingsStore::new_row("955020000000000001", fields).unwrap();
    for c in f["notices"].as_array().unwrap() {
        let d = &c["d"];
        let s = |k: &str| d[k].as_str().unwrap_or_default().to_string();
        let o = |k: &str| d[k].as_str().map(str::to_string);
        let n = match c["kind"].as_str().unwrap() {
            "claim" => svc::Notice::Claim { actor_id: s("actorId"), previous_id: o("previousId") },
            "close" => svc::Notice::Close { actor_id: s("actorId"), ticket_id: s("ticketId"), reason: o("reason") },
            "reopen" => svc::Notice::Reopen { actor_id: s("actorId"), ticket_id: s("ticketId"), claimed: d["claimed"].as_bool().unwrap() },
            "add" => svc::Notice::Add { actor_id: s("actorId"), user_id: s("userId") },
            "remove" => svc::Notice::Remove { actor_id: s("actorId"), user_id: s("userId") },
            "rename" => svc::Notice::Rename { actor_id: s("actorId"), channel_id: s("channelId") },
            _ => svc::Notice::Delete { actor_id: s("actorId"), transcript_channel_id: o("transcriptChannelId"), dm: d["dm"].as_bool().unwrap() },
        };
        let p = svc::build_notice(&n, Some(&settings));
        let label = format!("notice {}", c["kind"]);
        assert_json(&label, &payload_json(&p), &json!({ "embeds": c["payload"]["embeds"], "components": c["payload"]["components"] }));
        let mentions = serde_json::to_value(p.allowed_mentions.clone().unwrap()).unwrap();
        let want = &c["payload"]["allowedMentions"];
        assert_eq!(mentions.get("users").cloned().unwrap_or(json!([])), want.get("users").cloned().unwrap_or(json!([])), "{label} users");
        assert_eq!(mentions.get("parse").cloned().unwrap_or(json!([])), json!([]), "{label} parse");
    }
}

// ── Components ────────────────────────────────────────────────────────────────

#[test]
fn panel_components_and_rows_match_the_js() {
    let f = fixture();
    let c = &f["components"];
    let options = opts_of(&c["options"]);
    let can = |e: &str| e.contains("111111111111111111");
    assert_json("buttons", &json!(svc::panel_components_json("p1", TicketPanelStyle::Buttons, &options, &can)), &c["buttons"]);
    assert_json("select", &json!(svc::panel_components_json("p1", TicketPanelStyle::Select, &options, &can)), &c["select"]);
    assert_json("empty", &json!(svc::panel_components_json("p1", TicketPanelStyle::Buttons, &[], &can)), &c["empty"]);
    let t = "ckticket00000000000000001";
    assert_json("introOpen", &svc::intro_row_json(t, false), &c["introOpen"]);
    assert_json("introClaimed", &svc::intro_row_json(t, true), &c["introClaimed"]);
    assert_json("closed", &svc::closed_row_json(t), &c["closed"]);
    let rows = vec![svc::intro_row_json(t, false), json!({ "type": 1, "components": [{ "type": 2, "style": 5, "label": "Link", "url": "https://example.com" }] })];
    assert_json("disabledAll", &json!(svc::disable_buttons_json(&rows, &|_| true)), &c["disabledAll"]);
    let claim = |b: &Value| b["custom_id"].as_str().is_some_and(|id| id.starts_with("ticket:claim:"));
    assert_json("disabledClaim", &json!(svc::disable_buttons_json(&rows, &claim)), &c["disabledClaim"]);
    assert_json("reasonModal", &svc::reason_modal_json(&"p".repeat(25), &"o".repeat(25), &"x".repeat(80)), &c["reasonModal"]);
    assert_json("reasonModalShort", &svc::reason_modal_json("p1", "o1", "Billing"), &c["reasonModalShort"]);

    // The serenity builders send the same JSON.
    let built: Vec<Value> = svc::build_panel_components("p1", TicketPanelStyle::Buttons, &options, &can).iter().map(|r| serde_json::to_value(r).unwrap()).collect();
    assert_json("buttons (serenity)", &json!(built), &c["buttons"]);
    let built: Vec<Value> = svc::build_panel_components("p1", TicketPanelStyle::Select, &options, &can).iter().map(|r| serde_json::to_value(r).unwrap()).collect();
    assert_json("select (serenity)", &json!(built), &c["select"]);
    assert_json("intro (serenity)", &serde_json::to_value(svc::build_intro_row(t, true)).unwrap(), &c["introClaimed"]);
    assert_json("closed (serenity)", &serde_json::to_value(svc::build_closed_row(t)).unwrap(), &c["closed"]);
}

#[test]
fn overwrites_match_the_js() {
    let f = fixture();
    for c in f["overwrites"].as_array().unwrap() {
        let i = &c["input"];
        let strs = |k: &str| i[k].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect::<Vec<_>>();
        let (managers, added) = (strs("managerRoleIds"), strs("addedUserIds"));
        let out = svc::build_ticket_overwrites(&svc::OverwriteInput {
            guild_id: i["guildId"].as_str().unwrap(),
            bot_id: i["botId"].as_str().unwrap(),
            owner_id: i["ownerId"].as_str().unwrap(),
            manager_role_ids: &managers,
            added_user_ids: &added,
            closed: i["closed"].as_bool().unwrap(),
        });
        assert_eq!(json!(out.iter().map(svc::OverwriteSpec::to_json).collect::<Vec<_>>()), c["out"], "{i}");
    }
}

#[test]
fn manager_sync_payloads_match_the_js() {
    let f = fixture();
    for c in f["merges"].as_array().unwrap() {
        assert_eq!(svc::merge_manager_sync(&c["existing"], &c["incoming"]), c["out"], "merge {} + {}", c["existing"], c["incoming"]);
    }
    let r = |n: u32| format!("9{n:017}");
    assert_eq!(svc::manager_sync_payload("p1", &[r(1), "x".into(), r(1)]), f["syncPayload"]);
    assert_eq!(svc::manager_sync_dedupe_key("123"), "ticket.managersync:123");
}

#[test]
fn discord_errors_match_the_js() {
    let f = fixture();
    for c in f["discordErrors"].as_array().unwrap() {
        let i = &c["input"];
        let e = svc::IoError {
            code: i["code"].as_u64(),
            status: i["status"].as_u64().map(|s| s as u16),
            timeout: i["timeout"].as_bool().unwrap(),
            error_codes: i["errorCodes"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect(),
            ..Default::default()
        };
        assert_eq!(svc::describe_discord_error(&e), c["text"].as_str().unwrap(), "{i}");
        assert_eq!(svc::is_category_full_error(&e), c["categoryFull"].as_bool().unwrap(), "{i}");
    }
}

// ── Names, numbers, emoji ─────────────────────────────────────────────────────

#[test]
fn names_and_numbers_match_the_js() {
    let f = fixture();
    for c in f["sanitized"].as_array().unwrap() {
        assert_eq!(svc::sanitize_channel_name(c["input"].as_str().unwrap()), c["out"].as_str().unwrap(), "sanitize {}", c["input"]);
    }
    for c in f["formats"].as_array().unwrap() {
        let got = match svc::validate_name_format(c["input"].as_str().unwrap()) {
            Ok(fmt) => json!({ "ok": true, "format": fmt }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        assert_eq!(got, c["out"], "format {}", c["input"]);
    }
    let u = &f["renderUser"]["user"];
    let user = PlaceholderUser {
        id: u["id"].as_str().unwrap().into(),
        username: Some(u["username"].as_str().unwrap().into()),
        tag: Some(u["tag"].as_str().unwrap().into()),
        global_name: Some(u["globalName"].as_str().unwrap().into()),
        ..Default::default()
    };
    let member = PlaceholderMember { display_name: Some(f["renderUser"]["member"]["displayName"].as_str().unwrap().into()), user: Some(user.clone()), ..Default::default() };
    for c in f["rendered"].as_array().unwrap() {
        let got = svc::render_channel_name(c["format"].as_str().unwrap(), c["number"].as_i64().unwrap(), Some(user.clone()), Some(member.clone()), c["topic"].as_str());
        assert_eq!(got, c["out"].as_str().unwrap(), "render {}", c["format"]);
    }
    for c in f["numbers"].as_array().unwrap() {
        assert_eq!(svc::format_ticket_number(c["n"].as_i64().unwrap()), c["out"].as_str().unwrap());
    }
    for c in f["panelNames"].as_array().unwrap() {
        assert_eq!(json!(svc::normalize_panel_name(c["input"].as_str().unwrap())), c["out"], "panel name {}", c["input"]);
    }
}

#[test]
fn option_emoji_matches_the_js() {
    let f = fixture();
    let guild = vec![
        svc::GuildEmoji { id: "333333333333333333".into(), name: "Pog".into(), animated: false },
        svc::GuildEmoji { id: "444444444444444444".into(), name: "dance".into(), animated: true },
    ];
    let bot_has = |id: &str| id == "111111111111111111";
    for c in f["emojis"].as_array().unwrap() {
        let input = c["input"].as_str().unwrap();
        let got = match svc::parse_option_emoji(input, &bot_has, &guild) {
            Ok(e) => json!({ "ok": true, "emoji": e }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        assert_eq!(got, c["out"], "parseOptionEmoji {input:?}");
        assert_eq!(svc::is_unicode_emoji(input), c["unicode"].as_bool().unwrap(), "isUnicodeEmoji {input:?}");
    }
}

// ── Rows, rename window, the state machine ────────────────────────────────────

#[test]
fn rows_rename_window_and_rules_match_the_js() {
    let f = fixture();
    let cl = &f["classify"];
    let rows: Vec<svc::RowRef> = cl["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| svc::RowRef { id: r["id"].as_str().unwrap().into(), channel_id: r["channelId"].as_str().unwrap().into(), created_at_ms: r["createdAt"].as_i64().unwrap() })
        .collect();
    let out = svc::classify_ticket_rows(&rows, &|id| id == "1", cl["now"].as_i64().unwrap());
    let live: Vec<&str> = out.live.iter().map(|r| r.id.as_str()).collect();
    let want_live: Vec<&str> = cl["out"]["live"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(live, want_live);
    assert_eq!(json!(out.stale_pending), cl["out"]["stalePending"]);
    assert_eq!(json!(out.stale_gone), cl["out"]["staleGone"]);

    for c in f["renameCases"].as_array().unwrap() {
        let ts: Vec<f64> = c["timestamps"].as_array().unwrap().iter().map(|t| t.as_f64().unwrap()).collect();
        let (allowed, retry) = svc::rename_window_check(&ts, c["now"].as_f64().unwrap());
        assert_eq!(allowed, c["allowed"].as_bool().unwrap());
        assert_eq!(retry, c["retryAt"].as_f64());
    }

    for c in f["transitions"].as_array().unwrap() {
        let Some(action) = svc::Action::parse(c["action"].as_str().unwrap()) else {
            assert_eq!(c["out"], json!({ "ok": false, "code": "UNKNOWN" }));
            continue;
        };
        let source = if c["source"] == "button" { svc::Source::Button } else { svc::Source::Command };
        let got = match svc::transition_for(action, c["status"].as_str().unwrap(), c["claimedById"].as_str(), "a1", c["isAdmin"].as_bool().unwrap(), source) {
            Ok(t) if t.takeover => json!({ "ok": true, "to": t.to, "takeover": true }),
            Ok(t) => json!({ "ok": true, "to": t.to }),
            Err(fl) => fl.to_json(),
        };
        assert_eq!(got, c["out"], "transition {c}");
    }
    for c in f["perms"].as_array().unwrap() {
        let action = svc::Action::parse(c["action"].as_str().unwrap()).unwrap();
        let got = match svc::can_perform(action, c["isOwner"].as_bool().unwrap(), c["isStaff"].as_bool().unwrap()) {
            Ok(()) => json!({ "ok": true }),
            Err(fl) => fl.to_json(),
        };
        assert_eq!(got, c["out"], "canPerform {c}");
    }
}

// ── Templates ─────────────────────────────────────────────────────────────────

#[test]
fn templates_match_the_js() {
    let f = fixture();
    let t = &f["templates"];
    assert_eq!(svc::default_panel_template(false), t["panelButtons"]);
    assert_eq!(svc::default_panel_template(true), t["panelSelect"]);
    assert_eq!(svc::default_intro_template(), t["intro"]);
    for c in t["uses"].as_array().unwrap() {
        assert_eq!(svc::template_uses_token(&c["tpl"], c["token"].as_str().unwrap()), c["out"].as_bool().unwrap(), "{c}");
    }
    let clamped = svc::clamp_template_for_modal(&json!({ "embed": { "description": "a".repeat(4096), "title": "t".repeat(300), "footer": { "text": "f".repeat(2100) } }, "content": "c".repeat(2500) }));
    assert_eq!(clamped, t["clamped"]);
    for c in t["empty"].as_array().unwrap() {
        assert_eq!(svc::is_empty_template(&c["tpl"]), c["out"].as_bool().unwrap(), "{c}");
    }
    assert!(!svc::is_empty_template(&svc::default_intro_template()));
    assert!(bot_core::custom_embed::validate_template(&svc::default_intro_template(), true).ok);
    assert!(bot_core::custom_embed::validate_template(&svc::default_panel_template(false), true).ok);
}

// ── Transcripts ───────────────────────────────────────────────────────────────

#[test]
fn escape_and_bytes_match_the_js() {
    let f = fixture();
    for c in f["escapes"].as_array().unwrap() {
        assert_eq!(svc::escape_html(c["input"].as_str().unwrap_or("")), c["out"].as_str().unwrap());
    }
    for c in f["bytes"].as_array().unwrap() {
        assert_eq!(svc::format_bytes(c["n"].as_f64().unwrap_or(f64::NAN)), c["out"].as_str().unwrap(), "{c}");
    }
}

#[test]
fn transcripts_match_the_js_byte_for_byte() {
    let f = fixture();
    for c in f["transcripts"].as_array().unwrap() {
        let meta: svc::TranscriptMeta = serde_json::from_value(c["meta"].clone()).unwrap();
        let entries: Vec<svc::Entry> = serde_json::from_value(c["entries"].clone()).unwrap();
        let max = c["maxBytes"].as_u64().map(|m| m as usize).unwrap_or(svc::MAX_TRANSCRIPT_BYTES);
        let r = svc::render_transcript_html(&meta, &entries, c["tz"].as_str().unwrap(), max);
        let name = c["name"].as_str().unwrap();
        assert_eq!(r.included as u64, c["out"]["included"].as_u64().unwrap(), "{name} included");
        assert_eq!(r.omitted as u64, c["out"]["omitted"].as_u64().unwrap(), "{name} omitted");
        assert_eq!(r.html, c["out"]["html"].as_str().unwrap(), "{name} html");
    }
}

#[test]
fn transcript_html_is_safe() {
    let f = fixture();
    let nasty = &f["transcripts"][0];
    let meta: svc::TranscriptMeta = serde_json::from_value(nasty["meta"].clone()).unwrap();
    let entries: Vec<svc::Entry> = serde_json::from_value(nasty["entries"].clone()).unwrap();
    let html = svc::render_transcript_html_default(&meta, &entries).html;
    let lower = html.to_lowercase();
    assert!(html.contains("&lt;img src=x"));
    assert!(!lower.contains("<img") && !lower.contains("<script") && !lower.contains("javascript:") && !lower.contains("<iframe") && !lower.contains("<link"));
    assert!(html.contains("default-src 'none'"));
    assert!(!html.contains("onmouseover=\"x"));
}

#[tokio::test]
async fn normalize_message_matches_the_js() {
    let f = fixture();
    let n = &f["normalize"];
    let gid = "955020000000000001";
    let mut guild = FakeGuild::new(gid, "955020000000000009");
    for (id, name) in n["guild"]["roles"].as_object().unwrap() {
        let name = name.as_str().unwrap().to_string();
        guild = guild.role_with(id, 1, Permissions::empty(), move |v| v["name"] = json!(name));
    }
    for (id, name) in n["guild"]["channels"].as_object().unwrap() {
        guild = guild.channel(id, 0, name.as_str().unwrap(), &[]);
    }
    let t = TestApp::new().with_guilds([gid]).with_fake_guilds(&[guild]);
    let cached = bot_core::interaction_helpers::cached_guild(&t, serenity::all::GuildId::new(gid.parse().unwrap()));
    for c in n["cases"].as_array().unwrap() {
        let message: serenity::all::Message = serde_json::from_value(c["raw"].clone()).unwrap();
        let got = serde_json::to_value(svc::normalize_message(&message, cached.as_ref())).unwrap();
        assert_eq!(got, c["out"], "normalizeMessage {}", c["raw"]["content"]);
    }
}

#[test]
fn en_gb_dates_match_intl() {
    // `new Intl.DateTimeFormat('en-GB', { dateStyle: 'medium', timeStyle: 'short' })`: September is "Sept".
    let ms = 1_788_566_400_000.0; // 2026-09-05T00:00:00Z
    assert_eq!(svc::format_en_gb(ms, "UTC"), "5 Sept 2026, 00:00");
    assert_eq!(svc::format_en_gb(ms, "Asia/Tokyo"), "5 Sept 2026, 09:00");
    assert_eq!(svc::to_fixed_1(1.25), "1.3");
    assert_eq!(svc::to_fixed_1(1.5), "1.5");
}

// ── Router flows without a database (`test/tickets.test.js` handler cases) ─────

const G: &str = "955020000000000101";
const OWNER: &str = "955020000000000900";
const ADMIN_ROLE: &str = "955020000000000201";
const ADMIN: &str = "955020000000000301";
const MEMBER: &str = "955020000000000302";

fn world() -> TestApp {
    let guild = FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL)
        .role(ADMIN_ROLE, 2, Permissions::ADMINISTRATOR)
        .member(bot_core::testing::TEST_BOT_ID, &[ADMIN_ROLE])
        .member(ADMIN, &[ADMIN_ROLE])
        .member(MEMBER, &[])
        .channel("955020000000000401", 0, "general", &[]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild]);
    t.install(registry());
    t
}

#[tokio::test]
async fn buttons_unknown_action_and_dm_click_answer_without_io() {
    let t = world();
    bot_core::router::route_interaction(&t, FakeInteraction::button("ticket:bogus:x").guild(G).user(MEMBER).build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(t.responses.calls()[0].content(), Some("This button is no longer active."));
    assert!(t.responses.calls()[0].ephemeral());
}

#[tokio::test]
async fn open_click_without_budget_or_memo_answers_try_again() {
    let t = world();
    // The module gate reads the memoised settings; the handler itself has no budget for a panel lookup.
    t.settings().get_settings(G).await.unwrap();
    // An interaction created long ago has no acknowledgement budget: no database call is made.
    let mut it = FakeInteraction::button("ticket:open:p1:o1").guild(G).user(MEMBER);
    it.json_mut()["id"] = json!("1000000000000000000");
    bot_core::router::route_interaction(&t, it.build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"], "{calls:?}");
    assert_eq!(calls[0].embed_title(), Some("❌  Try Again"));
    assert!(calls[0].ephemeral());
}

#[tokio::test]
async fn message_subcommand_needs_the_intro_type_and_never_defers() {
    let t = world();
    let it = FakeInteraction::slash("ticket-panel")
        .guild(G)
        .user(ADMIN)
        .roles(&[ADMIN_ROLE])
        .permissions(Permissions::ADMINISTRATOR)
        .subcommand("message")
        .option("panel", bot_core::command::option_type::STRING, json!("p1"))
        .option("type", bot_core::command::option_type::STRING, json!("panel"))
        .option("option", bot_core::command::option_type::STRING, json!("o1"));
    bot_core::router::route_interaction(&t, it.build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(calls[0].embed_title(), Some("❌  Invalid Options"));
    assert!(calls[0].ephemeral());

    // A late interaction gets "Try Again" (no lookup, no deferral).
    t.responses.clear();
    let mut it = FakeInteraction::slash("ticket-panel")
        .guild(G)
        .user(ADMIN)
        .roles(&[ADMIN_ROLE])
        .permissions(Permissions::ADMINISTRATOR)
        .subcommand("message")
        .option("panel", bot_core::command::option_type::STRING, json!("p1"));
    it.json_mut()["id"] = json!("1000000000000000000");
    bot_core::router::route_interaction(&t, it.build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Try Again"));
}

#[tokio::test]
async fn template_modal_refuses_a_non_admin_first() {
    let t = world();
    let it = FakeInteraction::modal("ticket-modal:tpl:p1:panel", &[("tpl-content", "hi")]).guild(G).user(MEMBER);
    bot_core::router::route_interaction(&t, it.build()).await;
    let calls = t.responses.calls();
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert_eq!(calls[0].embed_title(), Some("❌  Insufficient Permissions"));
    assert!(calls[0].ephemeral());
}

#[tokio::test]
async fn reason_form_outside_a_cached_guild_answers_politely() {
    let t = world();
    let it = FakeInteraction::modal("ticket-modal:p1:o1", &[("ticket-reason", "help")]).guild("955020000000000999").user(MEMBER);
    bot_core::router::route_interaction(&t, it.build()).await;
    assert_eq!(t.responses.calls()[0].content(), Some("This only works in a server."));
}

#[test]
fn constants_agree_with_the_foundation() {
    assert_eq!(svc::MAX_ADDED_USERS, 20);
    assert!(svc::OPEN_REQUIRED_PERMS.contains(&"ManageRoles"), "the bot needs ManageRoles itself");
    assert!(!svc::MEMBER_ALLOW.contains(&"ManageRoles") && !svc::BOT_ALLOW.contains(&"ManageRoles"));
    assert_eq!(svc::DEFAULT_OPTION_LABEL, "Open Ticket");
    assert_eq!(svc::DEFAULT_OPTION_EMOJI, "✉️");
    // Every statement of RAW_SQL avoids a bare now().
    for (name, sql, _) in svc::RAW_SQL {
        assert!(!sql.contains("now()"), "{name}");
    }
}

#[test]
fn the_plugin_filter_takes_text_announcement_and_category_channels() {
    use serenity::all::ChannelType;
    assert!(feat_tickets::plugin::relevant(ChannelType::Text));
    assert!(feat_tickets::plugin::relevant(ChannelType::News));
    assert!(feat_tickets::plugin::relevant(ChannelType::Category));
    assert!(!feat_tickets::plugin::relevant(ChannelType::Voice));
}

/// `guild.channels.fetch(id)` (the production seam): a fetched channel of this guild is cached, one of another guild is
/// GuildChannelUnowned (no numeric code, so `resolveChannel` reports 'unknown', never 'gone').
#[tokio::test]
async fn serenity_fetch_channel_caches_and_refuses_foreign_channels() {
    use bot_core::testing::{FakeRest, TEST_BOT_ID};
    use svc::TicketIo;
    let g = "955020000000002041";
    let own = "955020000000002042";
    let foreign = "955020000000002043";
    let rest = FakeRest::new();
    let guild = FakeGuild::new(g, "955020000000000900").role("955020000000002044", 3, Permissions::ADMINISTRATOR).member(TEST_BOT_ID, &["955020000000002044"]);
    let t = TestApp::new().with_guilds([g]).with_fake_guilds_rest(&[guild], &rest);
    let channel = |id: &str, guild: &str| json!({ "id": id, "guild_id": guild, "type": 0, "name": format!("c{id}"), "position": 0, "permission_overwrites": [], "nsfw": false });
    rest.on("GET", &format!("/channels/{own}"), 200, channel(own, g));
    rest.on("GET", &format!("/channels/{foreign}"), 200, channel(foreign, "955020000000002045"));
    let io = svc::SerenityTicketIo(t.app.clone());
    let gid = serenity::all::GuildId::new(g.parse().unwrap());
    let got = io.fetch_channel(gid, serenity::all::ChannelId::new(own.parse().unwrap())).await.unwrap().unwrap();
    assert_eq!(got.name, format!("c{own}"));
    assert!(svc::channel_cached(&t, gid, own).is_some(), "a fetched channel is cached");
    let e = io.fetch_channel(gid, serenity::all::ChannelId::new(foreign.parse().unwrap())).await.unwrap_err();
    assert_eq!(e.code, None, "GuildChannelUnowned has no code");
    let (found, state) = svc::resolve_channel(&t, gid, foreign).await;
    assert!(found.is_none());
    assert_eq!(state, svc::ChannelState::Unknown);
}
