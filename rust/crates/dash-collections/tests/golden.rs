//! Compares the crate with what the JS answers (rust/fixtures/gen/dash-collections-golden.mjs):
//! - every collection as the front end sees it (publicCollection) and the whole catalog (buildCatalog), byte for byte;
//! - validateRecordInput over ~8,800 create, update and action inputs;
//! - every exported helper of the collection files over a table of inputs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use dash_collections::collections::{
    automations as auto, cases, embeds, feeds, giveaways_polls as gp, levels, reaction_roles as rr, reminders, small, tags, ticket_panels as tp,
    translate_stats, triggers,
};
use dash_collections::testing::MemoryTasks;
use dash_collections::*;
use dash_schema::RoleRef;
use indexmap::IndexSet;
use serde_json::{Map, Value, json};

fn golden(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden").join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))).expect("valid JSON")
}

fn text(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

#[test]
fn public_collections_match_the_js_byte_for_byte() {
    let want = golden("dash-collections/public.json");
    let got: Vec<Value> = Collections::builtin().iter().map(|c| c.public_json()).collect();
    let want = want.as_array().expect("an array");
    assert_eq!(want.len(), got.len());
    for (w, g) in want.iter().zip(&got) {
        assert_eq!(text(w), text(g), "{}", w["key"]);
    }
}

#[test]
fn the_catalog_matches_dashboard_catalog_json_byte_for_byte() {
    let want = golden("dashboard-catalog.json");
    let got = build_catalog();
    for key in ["categories", "modules", "general", "collections", "placeholders"] {
        assert_eq!(text(&want[key]), text(&got[key]), "catalog.{key}");
    }
    assert_eq!(text(&want), catalog_json());
}

#[test]
fn the_rust_hooks_are_the_js_hooks() {
    for (key, names) in dash_collections::index::js_hook_names() {
        let c = Collections::builtin().get(&key).unwrap_or_else(|| panic!("{key} missing"));
        let mut want = names.clone();
        let mut got = dash_collections::index::rust_hook_names(c);
        want.sort();
        got.sort();
        assert_eq!(want, got, "{key}");
    }
}

#[test]
fn lint_collections_is_clean() {
    assert_eq!(lint_collections(), Vec::<String>::new());
}

fn err_json(e: &StoreError) -> Value {
    match e {
        StoreError::Field(f) => json!({ "name": "FieldError", "code": null, "path": f.path, "message": f.message }),
        StoreError::Record(r) => json!({ "name": "RecordError", "code": r.code.as_str(), "path": r.path, "message": r.message }),
        other => json!({ "name": "Other", "message": other.to_string() }),
    }
}

/// Inputs the JS reads with V8's legacy Date.parse fallback (dash-schema's known datetime gap).
fn legacy_date_gap(case: &Value) -> bool {
    let probe = |v: &Value| match v {
        Value::String(s) => !s.is_empty() && s.chars().any(|c| c.is_ascii_digit()) && !s.starts_with("20"),
        Value::Number(_) | Value::Bool(_) => true,
        _ => false,
    };
    let Some(data) = case["data"].as_object() else { return false };
    let c = Collections::builtin().get(case["key"].as_str().unwrap_or(""));
    data.iter().any(|(k, v)| {
        c.and_then(|c| c.fields.iter().chain(c.actions.iter().flat_map(|a| a.fields.iter())).find(|f| f.field() == k))
            .is_some_and(|f| f.ty() == "datetime" && probe(v))
    })
}

#[test]
fn validate_record_input_matches_the_js() {
    let g = golden("dash-collections/validate.json");
    let now = g["now"].as_f64().expect("now");
    let mut failures = Vec::new();
    let mut checked = 0;
    let mut skipped = 0;
    for case in g["cases"].as_array().expect("cases") {
        let mode = Mode::parse(case["mode"].as_str().expect("mode")).expect("a mode");
        let current = (!case["current"].is_null()).then_some(&case["current"]);
        let got = validate_record_input(case["key"].as_str().unwrap_or(""), &case["data"], mode, case["actionId"].as_str(), current, now);
        let got = match &got {
            Ok(v) => json!({ "ok": { "data": v.data, "refs": v.refs } }),
            Err(e) => json!({ "err": err_json(e) }),
        };
        let want = if case.get("ok").is_some() { json!({ "ok": case["ok"] }) } else { json!({ "err": case["err"] }) };
        if text(&got) != text(&want) {
            if legacy_date_gap(case) {
                eprintln!("known gap: {} {} {}", case["key"], case["mode"], text(&case["data"]).chars().take(120).collect::<String>());
                skipped += 1;
                continue;
            }
            failures.push(format!("{} {} {}:\n  data {}\n  want {}\n  got  {}", case["key"], case["mode"], case["actionId"], text(&case["data"]).chars().take(300).collect::<String>(), text(&want).chars().take(400).collect::<String>(), text(&got).chars().take(400).collect::<String>()));
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "{} of {checked} cases differ ({skipped} known datetime gaps):\n{}", failures.len(), failures.iter().take(25).cloned().collect::<Vec<_>>().join("\n"));
    eprintln!("validate: {checked} cases compared, {skipped} known datetime gaps skipped");
    assert!(checked > 8000, "{checked}");
    assert_eq!(skipped, 0, "the legacy Date.parse forms are ported; no case may be skipped");
}

// ── helpers.json ──────────────────────────────────────────────────────────────────────────────────────────────

fn profile(v: &Value) -> Option<MemberProfile> {
    let o = v.as_object()?;
    Some(MemberProfile {
        id: o.get("id").and_then(Value::as_str).unwrap_or_default().into(),
        username: o.get("username").and_then(Value::as_str).unwrap_or_default().into(),
        global_name: o.get("globalName").and_then(Value::as_str).map(str::to_string),
        nick: o.get("nick").and_then(Value::as_str).map(str::to_string),
        avatar_url: o.get("avatarUrl").and_then(Value::as_str).unwrap_or_default().into(),
        bot: o.get("bot") == Some(&Value::Bool(true)),
    })
}

fn member(v: &Value) -> Option<DashboardMember> {
    if v.is_null() {
        return None;
    }
    Some(DashboardMember {
        owner: v["owner"] == json!(true),
        permissions: v.get("permissions").and_then(Value::as_str).unwrap_or_default().into(),
        highest_role_position: v.get("highestRolePosition").and_then(Value::as_i64),
    })
}

fn resources(v: &Value) -> Option<DiscordResources> {
    (!v.is_null()).then(|| serde_json::from_value(v.clone()).expect("resources"))
}

fn names(v: &Value) -> HashMap<String, String> {
    v.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default()
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().map(|x| dash_schema::js::to_js_string(Some(x))).collect()).unwrap_or_default()
}

fn problem(p: Option<Problem>) -> Value {
    p.map_or(Value::Null, |p| p.to_json())
}

fn rec_err(e: &RecordError) -> Value {
    json!({ "name": "RecordError", "code": e.code.as_str(), "message": e.message, "path": e.path })
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn opt_s(v: &Value) -> Option<&str> {
    v.as_str()
}

fn num(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        v => dash_schema::js::to_number(Some(v)),
    }
}

async fn run_helper(f: &str, a: &[Value], now: f64) -> Result<Value, Value> {
    let a0 = a.first().cloned().unwrap_or(Value::Null);
    let a1 = a.get(1).cloned().unwrap_or(Value::Null);
    let a2 = a.get(2).cloned().unwrap_or(Value::Null);
    let a3 = a.get(3).cloned().unwrap_or(Value::Null);
    let name_of = |id: &Value| (id.as_str() == Some("abcdefgh1")).then(|| "my-auto".to_string());
    Ok(match f {
        "taskOutcome" => {
            let r: Option<TaskResult> = (!a0.is_null()).then(|| serde_json::from_value(a0.clone()).expect("task result"));
            match task_outcome(r.as_ref()) {
                Ok(o) => json!({ "status": o.status, "message": o.message, "data": o.data }),
                Err(e) => return Err(rec_err(&e)),
            }
        }
        "isChannelExempt" => json!(is_channel_exempt(member(&a0).as_ref())),
        "requireChannelAccess" => {
            let checks: Vec<ChannelCheck> = arr(&a0)
                .iter()
                .map(|c| ChannelCheck {
                    channel_id: c["channelId"].as_str().map(str::to_string),
                    needs: c.get("needs").map(str_list),
                    path: c["path"].as_str().map(str::to_string),
                    optional: c["optional"] == json!(true),
                })
                .collect();
            let tasks = Arc::new(MemoryTasks::new());
            if !a1.is_null() {
                tasks.answer("access.channels", serde_json::from_value(a1.clone()).expect("answer"));
            }
            let ctx = StoreCtx {
                guild_id: "1".into(),
                user_id: "2".into(),
                member: member(&a2),
                tasks: (a.get(4) == Some(&json!(true))).then(|| tasks.clone() as Arc<dyn TaskRunner>),
                ..StoreCtx::default()
            };
            match require_channel_access(&ctx, &checks, s(&a3)).await {
                Ok(()) => json!({ "seen": tasks.calls().iter().map(|(_, k, p, _)| json!([k, p])).collect::<Vec<_>>() }),
                Err(StoreError::Record(e)) => return Err(rec_err(&e)),
                Err(e) => return Err(json!({ "name": "Other", "message": e.to_string() })),
            }
        }
        "messageUrl" => json!(embeds::message_url(Some(&a0), Some(&a1), Some(&a2))),
        "embedItem" => embeds::embed_item(&a0),
        "guildIconUrl" => json!(embeds::guild_icon_url(s(&a0["id"]), a0["icon"].as_str())),
        "userAvatarUrl" => json!(embeds::user_avatar_url(s(&a0["id"]), a0["avatar"].as_str())),
        "renderContextFor" => {
            let res = resources(&a0);
            let user: Option<DashboardUser> = a1.get("user").filter(|u| !u.is_null()).map(|u| serde_json::from_value(u.clone()).expect("user"));
            let ctx = embeds::render_context_for(res.as_ref(), a1["channelId"].as_str(), user.as_ref(), a1["userId"].as_str());
            json!(bot_util::placeholder_parser::render_placeholders(s(&a2), &ctx, None))
        }
        "restAllowedMentions" => embeds::rest_allowed_mentions(a0["users"] != json!(false), &str_list(&a0["roles"])),
        "toRestMessage" => embeds::to_rest_message((!a0.is_null()).then_some(&a0), a1 == json!(true), None),
        "memberCanMentionAll" => json!(embeds::member_can_mention_all(member(&a0).as_ref())),
        "memberFor" => {
            let res = DiscordResources { guild: ResGuild { id: "900000000000000002".into(), owner_id: Some("800000000000000002".into()), ..ResGuild::default() }, ..DiscordResources::default() };
            let m = embeds::member_for_parts(member(&a0).as_ref(), a1.as_str().unwrap_or(""), Some(&res));
            match m {
                None => Value::Null,
                Some(m) => {
                    let mut o = Map::new();
                    o.insert("owner".into(), json!(m.owner));
                    if !a0.is_null() {
                        o.insert("permissions".into(), json!(m.permissions));
                    }
                    Value::Object(o)
                }
            }
        }
        "pingRoleProblem" => json!(embeds::ping_role_problem(resources(&a0).as_ref(), s(&a1), member(&a2).as_ref())),
        "assignRoleProblem" => json!(embeds::assign_role_problem(resources(&a0).as_ref(), s(&a1), member(&a2).as_ref())),
        "splitPingableRoles" => {
            let (allowed, refused) = embeds::split_pingable_roles(&str_list(&a0), resources(&a1).as_ref(), member(&a2).as_ref());
            json!({ "allowed": allowed, "refused": refused })
        }
        "discordFailure" => {
            let mut err = DiscordApiError::new("x", a0["status"].as_u64().unwrap_or(0) as u16, s(&a0["code"]));
            err.discord_code = a0["discordCode"].as_i64();
            let op = match s(&a1["op"]) {
                "send" => embeds::DiscordOp::Send,
                "edit" => embeds::DiscordOp::Edit,
                _ => embeds::DiscordOp::Delete,
            };
            let channel_id = a1["channelId"].as_str().or(a1["channel"]["id"].as_str());
            let info = embeds::FailureInfo { op, channel_name: a1["channel"]["name"].as_str(), channel_id, embeds: a1["embeds"] != json!(false) };
            let r = embeds::discord_failure_of(&err, &info);
            json!({ "code": r.code.as_str(), "message": r.message, "path": r.path })
        }
        "tags.actionToForm" => tags::action_to_form(&a0),
        "tags.actionToStored" => tags::action_to_stored(&a0),
        "tags.tagItem" => tags::tag_item(&a0, arr(&a1)),
        "triggers.triggerItem" => triggers::trigger_item(&a0),
        "reminders.scheduleInput" => Value::Object(reminders::schedule_input(a0.as_object().expect("an object"))),
        "reminders.weekdayNames" => json!(reminders::weekday_names(Some(&a0))),
        "reminders.weekdayNumbers" => json!(reminders::weekday_numbers(Some(&a0))),
        "reminders.scheduleText" => json!(reminders::schedule_text(&a0, s(&a1))),
        "reminders.isDone" => json!(reminders::is_done(&a0)),
        "reminders.reminderItem" => reminders::reminder_item(&a0, s(&a1), Some(&a2)),
        "feeds.parseFeedSource" => match feeds::parse_feed_source(s(&a0), s(&a1)) {
            Ok(i) => json!({ "ok": true, "input": i.to_json() }),
            Err(m) => json!({ "ok": false, "message": m }),
        },
        "feeds.feedItem" => feeds::feed_item(&a0, s(&a1), a2.as_str()),
        "levelRoles.totalXpForLevel" => json!(small::total_xp_for_level(a0.as_i64().unwrap_or(0))),
        "levelRoles.highestReachableLevel" => json!(small::highest_reachable_level()),
        "starboards.isEmojiEntry" => json!(small::is_emoji_entry(&a0)),
        "translateChannels.describeLanguages" => json!(small::describe_languages(arr(&a0))),
        "translateStats.buildStatItems" => json!(translate_stats::build_stat_items(arr(&a0), arr(&a1))),
        "statChannels.normalizeTemplate" => json!(small::normalize_stat_template(Some(&a0))),
        "cases.actionPayload" => cases::action_payload(a0.as_object().expect("an object")),
        "cases.caseType" => cases::case_type(&a0),
        "cases.caseStatus" => json!(cases::case_status(&a0, a1.as_f64().unwrap_or(now))),
        "cases.caseItem" => {
            let profiles: HashMap<String, Option<MemberProfile>> =
                a1.as_object().map(|o| o.iter().map(|(k, v)| (k.clone(), profile(v))).collect()).unwrap_or_default();
            cases::case_item(&a0, &profiles, a2.as_f64().unwrap_or(now))
        }
        "cases.reasonText" => json!(cases::reason_text(Some(&a0))),
        "cases.caseNumberQuery" => json!(cases::case_number_query(&if a0.is_null() { String::new() } else { dash_schema::js::to_js_string(Some(&a0)) })),
        "levels.readRankCursor" => levels::read_rank_cursor(s(&a0)).map_or(Value::Null, |c| json!({ "value": c.value, "userId": c.user_id })),
        "levels.afterRankCursor" => levels::after_rank_cursor(s(&a0), s(&a1)),
        "levels.levelProgress" => {
            let p = levels::level_progress(num(&a0));
            json!({ "level": p.level, "into": p.into, "need": p.need })
        }
        "levels.profileName" => json!(levels::profile_name(profile(&a0).as_ref(), a1.as_str())),
        "levels.profileLabel" => json!(levels::profile_label(profile(&a0).as_ref(), s(&a1))),
        "levels.avatarFor" => json!(levels::avatar_for(profile(&a0).as_ref(), s(&a1))),
        "levels.fmt" => json!(levels::fmt(num(&a0))),
        "levels.levelItem" => levels::level_item(&a0, a1.as_i64(), profile(&a2).as_ref()),
        "levels.emptyLevelItem" => levels::empty_level_item(s(&a0), profile(&a1).as_ref()),
        "balances.balanceItem" => {
            let cur = if a3.is_null() { levels::Currency::default() } else { levels::Currency { name: s(&a3["name"]).into(), emoji: s(&a3["emoji"]).into() } };
            levels::balance_item(&a0, a1.as_i64(), profile(&a2).as_ref(), &cur)
        }
        "giveaways.endTimeProblem" => problem(gp::end_time_problem(a0.as_object().expect("an object"), a1 == json!(true), now)),
        "giveaways.giveawaySummary" => json!(gp::giveaway_summary(&a0)),
        "giveaways.giveawayItem" => gp::giveaway_item(&a0, &names(&a1)),
        "polls.durationProblem" => problem(gp::poll_duration_problem(Some(&a0))),
        "polls.pollItem" => {
            let tally = (!a1.is_null()).then(|| gp::PollTally {
                counts: arr(&a1["counts"]).iter().map(|c| c.as_i64().unwrap_or(0)).collect(),
                votes: a1["votes"].as_i64().unwrap_or(0),
                voters: a1["voters"].as_i64().unwrap_or(0),
            });
            gp::poll_item(&a0, tally.as_ref(), &names(&a2))
        }
        "rr.emojiKey" => json!(rr::emoji_key(Some(&a0))),
        "rr.itemsProblem" => problem(rr::items_problem(arr(&a0), opt_s(&a1))),
        "rr.diffPanelItems" => rr::diff_panel_items(arr(&a0), arr(&a1)).to_json(),
        "rr.normalizeItems" => {
            let held: HashSet<String> = str_list(&a3).into_iter().collect();
            match rr::normalize_items(arr(&a0), opt_s(&a1), resources(&a2).as_ref(), &held) {
                Ok(v) => json!(v),
                Err(e) => return Err(rec_err(&e)),
            }
        }
        "rr.panelItem" => rr::panel_item(&a0),
        "tp.defaultPanelTemplate" => tp::default_panel_template(opt_s(&a0)),
        "tp.sanitizeChannelName" => json!(tp::sanitize_channel_name(&if a0.is_null() { String::new() } else { dash_schema::js::to_js_string(Some(&a0)) })),
        "tp.validateNameFormat" => match tp::validate_name_format(Some(&a0)) {
            Ok(f) => json!({ "ok": true, "format": f }),
            Err(e) => json!({ "ok": false, "error": e }),
        },
        "tp.isEmptyTemplate" => json!(tp::is_empty_template(&a0)),
        "tp.canonicalEmoji" => {
            let held: HashSet<String> = str_list(&a2).into_iter().collect();
            match tp::canonical_emoji(Some(&a0), resources(&a1).as_ref(), &held, s(&a3)) {
                Ok(v) => v,
                Err(e) => return Err(rec_err(&e)),
            }
        }
        "tp.normalizeOptions" => {
            let held: HashSet<String> = str_list(&a2).into_iter().collect();
            match tp::normalize_options(arr(&a0), resources(&a1).as_ref(), &held) {
                Ok(v) => json!(v),
                Err(e) => return Err(rec_err(&e)),
            }
        }
        "tp.diffOptions" => tp::diff_options(arr(&a0), arr(&a1)).to_json(),
        "tp.isDefaultTemplate" => json!(tp::is_default_template(&a0, arr(&a1))),
        "tp.panelItem" => tp::panel_item(&a0, a1.as_i64().unwrap_or(0)),
        "auto.conditionToForm" => auto::condition_to_form(&a0, &name_of),
        "auto.conditionToStored" => auto::condition_to_stored(&a0, &|x: &Value| if x.as_str() == Some("my-auto") { json!("abcdefgh1") } else { x.clone() }),
        "auto.actionToForm" => auto::action_to_form(&a0),
        "auto.actionToStored" => auto::action_to_stored(&a0, a1.as_i64()),
        "auto.blankCondition" => Value::Object(auto::blank_condition(a.first())),
        "auto.blankAction" => Value::Object(auto::blank_action(a.first())),
        "auto.flagsToStored" => json!(auto::flags_to_stored(Some(&a0))),
        "auto.planButtons" => {
            let assign = a1["assign"] != json!(false);
            match auto::plan_buttons(arr(&a0), arr(&a1["existing"]), a1.get("buttonSeq"), assign) {
                Ok(p) => p.to_json(),
                Err(p) => json!({ "error": p.to_json() }),
            }
        }
        "auto.shapeProblems" => json!(
            auto::shape_problems(a0.get("trigger"), arr(&a0["conditions"]), arr(&a0["actions"]), a0.get("cooldownSeconds"))
                .iter()
                .map(Problem::to_json)
                .collect::<Vec<_>>()
        ),
        "auto.summaryOf" => json!(auto::summary_of(&a0)),
        "auto.automationItem" => auto::automation_item(&a0, &name_of, &str_list(&a1), a2.as_str()),
        "auto.refKeys" => {
            let keys: IndexSet<String> = str_list(&a1).into_iter().collect();
            json!(auto::action_role_keys((!a0.is_null()).then_some(&a0), keys).into_iter().collect::<Vec<_>>())
        }
        "auto.refKeyOf" => {
            let r = RoleRef { id: s(&a0["id"]).into(), path: s(&a0["path"]).into(), assignable: true, allow_everyone: false, allow_dangerous: false };
            json!(auto::action_role_key_of(&r, &a1.as_object().cloned().unwrap_or_default()))
        }
        "auto.problemKey" => json!(auto::problem_key(s(&a0))),
        other => panic!("no Rust dispatch for {other}"),
    })
}

/// The JS `{ name, code, message, path }` of a throw, reduced to what the Rust error carries.
fn throws_json(t: &Value) -> Value {
    json!({ "name": t["name"], "code": t["code"], "message": t["message"], "path": t["path"] })
}

#[tokio::test]
async fn every_helper_matches_the_js() {
    let g = golden("dash-collections/helpers.json");
    let now = g["now"].as_f64().expect("now");
    let mut failures = Vec::new();
    let mut n = 0;
    for case in g["cases"].as_array().expect("cases") {
        let f = s(&case["fn"]);
        let args = arr(&case["args"]);
        let got = run_helper(f, args, now).await;
        let (got, want) = match (&got, case.get("ok"), case.get("throws")) {
            (Ok(v), Some(w), _) => (text(v), text(w)),
            (Err(e), _, Some(t)) => (text(e), text(&throws_json(t))),
            (Ok(v), None, Some(t)) => (format!("ok {}", text(v)), format!("throws {}", text(t))),
            (Err(e), Some(w), _) => (format!("throws {}", text(e)), format!("ok {}", text(w))),
            _ => panic!("bad case {case}"),
        };
        if got != want {
            failures.push(format!("{f} {}:\n  want {want}\n  got  {got}", text(&case["args"]).chars().take(200).collect::<String>()));
        }
        n += 1;
    }
    assert!(failures.is_empty(), "{} of {n} helper cases differ:\n{}", failures.len(), failures.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
    assert!(n > 800);
}
