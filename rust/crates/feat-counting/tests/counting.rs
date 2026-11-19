//! Port of `test/counting.test.js`, plus the golden, database and cross-language checks every feature crate runs.
//!
//! Offline: the parser and rules, the notes, the hook filters, the plugin declaration and the `/counting` payload.
//! With `TEST_DATABASE_URL`: the message and delete handlers against the real `CountingState` table, gateway-order
//! judging through the hook runner, the command, and both directions of the JS round trip.

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeInteraction, TestApp, database_url, fake_message, run_node};
use feat_counting::counting_service::{self as svc, COUNT_MAX, NewState, current_state, judge_count, parse_count};
use feat_counting::plugin::{
    Action, CountMessage, FAIL_EMOJI, OK_EMOJI, Ruined, count_math, deleted_note, deleted_text, in_counting_channel, judge_message,
    looks_like_count, ruined_text,
};
use serde_json::{Value, json};

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_counting::register(&mut reg);
    reg
}

// ── Pure rules ────────────────────────────────────────────────────────────────

#[test]
fn parse_count_only_a_message_that_is_just_a_number_counts() {
    assert_eq!(parse_count("1"), Some(1));
    assert_eq!(parse_count("  42 \n"), Some(42));
    assert_eq!(parse_count("007"), Some(7));
    assert_eq!(parse_count("0"), Some(0));
    assert_eq!(parse_count(&COUNT_MAX.to_string()), Some(COUNT_MAX));
    for chat in ["", " ", "5 lol", "1.5", "-3", "+3", "**5**", "1,000", "1234567890", "five", "٣"] {
        assert_eq!(parse_count(chat), None, "{chat}");
    }
    const { assert!(COUNT_MAX <= 2_147_483_647, "fits the Int column") };
}

#[test]
fn parse_count_maths_that_comes_to_a_whole_number_counts_as_it() {
    let sums: &[(&str, i64)] = &[
        ("2+2", 4),
        ("(3*4)-1", 11),
        (" 10 / 2 ", 5),
        ("2^3", 8),
        ("2^3^2", 512),
        ("(-2)^2", 4),
        ("10%3", 1),
        ("5-(-1)", 6),
        ("2 x 3", 6),
        ("2×3", 6),
        ("6÷2", 3),
        ("7−2", 5),
        ("2\\*3", 6),
        ("1/49*49", 1),
        ("1-1", 0),
        ("-0", 0),
        ("\n2 +\n2", 4),
        ("999999998+1", COUNT_MAX),
        ("(((((1)))))", 1),
    ];
    for (math, n) in sums {
        assert_eq!(parse_count(math), Some(*n), "{math}");
    }
    let long16 = format!("1+{}", "1".repeat(16));
    let many = format!("{}1", "1+".repeat(60));
    let deep = format!("{}1{}", "(".repeat(60), ")".repeat(60));
    for chat in [
        "10/3", "24/7", "10/27", "1/0", "0/0", "9^9^9", "-2^2", "1-2", "999999999+1", "2**3", "**5**", "2(3)", "()", "(1+1", "1+1)", "^0", "(^0)",
        "1+", "x", "5x", "٣+1", "1.5+1", "2+2 lol", &long16, &many, &deep,
    ] {
        assert_eq!(parse_count(chat), None, "{chat}");
    }
    assert_eq!(svc::evaluate_math("-2^2"), Some(-4.0), "a power binds tighter than the minus");
    let zero = svc::evaluate_math("-0").unwrap_or(1.0);
    assert!(zero == 0.0 && zero.is_sign_positive(), "never -0");
}

#[test]
fn js_pow_matches_math_pow() {
    assert!(svc::js_pow(1.0, f64::INFINITY).is_nan());
    assert!(svc::js_pow(-1.0, f64::NEG_INFINITY).is_nan());
    assert_eq!(svc::js_pow(f64::NAN, 0.0), 1.0);
    assert_eq!(svc::js_pow(2.0, 10.0), 1024.0);
}

/// Every parse the JS makes on a few hundred generated inputs: the Rust parser agrees exactly.
#[test]
fn parse_count_agrees_with_the_js_on_a_sweep() {
    let atoms = ["1", "2", "3", "7", "10", "0", "49", "(1+1)", "(2*3)", "-2", "999999999"];
    let ops = ["+", "-", "*", "/", "%", "^", " x ", "÷", "−", "\\*", "**", ""];
    let mut cases: Vec<String> = Vec::new();
    for (i, a) in atoms.iter().enumerate() {
        for (j, op) in ops.iter().enumerate() {
            let b = atoms[(i * 7 + j * 3) % atoms.len()];
            let c = atoms[(i + j * 5) % atoms.len()];
            cases.push(format!("{a}{op}{b}"));
            cases.push(format!("{a}{op}{b}{op}{c}"));
            cases.push(format!("({a}{op}{b}){op}{c}"));
        }
    }
    cases.extend(["  12  ", "1e3", "0x10", "١٢", "1 2", "(", ")", "-", "2^-1", "4^0.5", "8^(1/3)", "0^0", "2^1023*2"].map(String::from));
    let arg = serde_json::to_string(&cases).unwrap_or_default();
    let js = run_node("rust/fixtures/gen/feat-counting-js.mjs", &["parse", &arg]);
    let js = js.as_array().cloned().unwrap_or_default();
    assert_eq!(js.len(), cases.len());
    for (case, expected) in cases.iter().zip(js) {
        assert_eq!(parse_count(case).map(Value::from).unwrap_or(Value::Null), expected, "{case:?}");
    }
}

#[test]
fn current_state_starts_from_zero_and_keeps_the_best_run() {
    let empty = svc::State { count: 0, last_user_id: None, last_message_id: None, best: 0 };
    assert_eq!(current_state(None, Some("C")), empty);
    let row = bot_db::CountingState {
        guild_id: "G".into(),
        channel_id: "C".into(),
        count: 7,
        last_user_id: Some("U".into()),
        last_message_id: Some("M".into()),
        best: 30,
        updated_at: chrono_now(),
    };
    assert_eq!(current_state(Some(&row), Some("C")), svc::State { count: 7, last_user_id: Some("U".into()), last_message_id: Some("M".into()), best: 30 });
    assert_eq!(current_state(Some(&row), Some("OTHER")), svc::State { count: 0, last_user_id: None, last_message_id: None, best: 30 });
}

fn chrono_now() -> bot_db::sqlx::types::chrono::DateTime<bot_db::sqlx::types::chrono::Utc> {
    bot_db::sqlx::types::chrono::Utc::now()
}

#[test]
fn judge_count_needs_the_next_number_from_someone_else() {
    use svc::{Reason, Verdict};
    assert_eq!(judge_count(0, None, "A", 1), Verdict { ok: true, reason: None, expected: 1 });
    assert_eq!(judge_count(4, Some("A"), "B", 5), Verdict { ok: true, reason: None, expected: 5 });
    assert_eq!(judge_count(4, Some("A"), "B", 6), Verdict { ok: false, reason: Some(Reason::Wrong), expected: 5 });
    assert_eq!(judge_count(4, Some("A"), "B", 4), Verdict { ok: false, reason: Some(Reason::Wrong), expected: 5 });
    assert_eq!(judge_count(4, Some("A"), "A", 5), Verdict { ok: false, reason: Some(Reason::Twice), expected: 5 });
    assert_eq!(judge_count(0, None, "A", 2), Verdict { ok: false, reason: Some(Reason::Wrong), expected: 1 });
}

#[test]
fn looks_like_count_and_the_notes() {
    assert!(looks_like_count(Some("12")));
    assert!(looks_like_count(Some("  3")));
    assert!(!looks_like_count(Some("hi 3")));
    assert!(looks_like_count(Some(" (1+1)")));
    assert!(!looks_like_count(Some("**5**")));
    assert!(!looks_like_count(None));
    let r = |count, reason, expected, best, math: Option<&str>, got| {
        ruined_text(&Ruined { user_id: "U", count, reason, expected, best, math: math.map(String::from), got })
    };
    use svc::Reason::{Twice, Wrong};
    let wrong = r(41, Wrong, 42, 50, None, None);
    assert!(wrong.contains("<@U> ruined it at **41**"), "{wrong}");
    assert!(wrong.contains("next number was **42**"));
    assert!(wrong.contains("from **1**"));
    assert!(wrong.contains("Best run: **50**"));
    assert!(r(3, Twice, 4, 3, None, None).contains("twice in a row"));
    assert!(r(3, Wrong, 4, 3, Some("2+3"), Some(5)).contains("ruined it at **3**! `2+3` is **5**. The next number was **4**"));
    assert_eq!(wrong, "<@U> ruined it at **41**! The next number was **42**.\nStart again from **1**. Best run: **50**.");
    assert_eq!(r(1500, Wrong, 1501, 2000, None, None), "<@U> ruined it at **1,500**! The next number was **1,501**.\nStart again from **1**. Best run: **2,000**.");
    assert_eq!(count_math(" 2 +\n3 "), Some("2 + 3".into()));
    assert_eq!(count_math("007"), None);
    assert_eq!(deleted_text(Some("U"), 9), "<@U> deleted their number **9**. The next number is **10**.");
    assert!(deleted_text(None, 9).starts_with("Someone deleted"));
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["counting"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "counting");
}

#[test]
fn plugin_declaration_and_hook_order() {
    let reg = registry();
    let p = reg.plugin_named("counting").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Counting));
    let hooks: Vec<(&str, &str)> = p.hooks.iter().map(|h| (h.event().as_str(), h.id())).collect();
    assert_eq!(hooks, vec![("messageCreate", "counting"), ("messageDelete", "counting")]);
    let at = |id: &str| bot_config::hook_index("messageCreate", id);
    assert_eq!(at("counting"), at("automod").map(|i| i + 1), "right after automod");
    assert!(bot_config::hook_index("messageDelete", "counting").is_some());
}

fn char_count(node: &Value) -> usize {
    let len = |v: &Value| v.as_str().map(|s| s.encode_utf16().count()).unwrap_or(0);
    let mut n = len(&node["name"]) + len(&node["description"]);
    for c in node["choices"].as_array().into_iter().flatten() {
        n += len(&c["name"]) + c["value"].to_string().trim_matches('"').len();
    }
    for o in node["options"].as_array().into_iter().flatten() {
        n += char_count(o);
    }
    n
}

#[test]
fn counting_payload() {
    let json = feat_counting::commands::counting::command().payload();
    assert_eq!(json["name"], "counting");
    assert_eq!(json["default_member_permissions"], "32");
    assert_eq!(json["contexts"], json!([0]));
    let subs: Vec<(String, u64)> =
        json["options"].as_array().unwrap_or(&Vec::new()).iter().map(|o| (o["name"].as_str().unwrap_or("").into(), o["type"].as_u64().unwrap_or(0))).collect();
    assert_eq!(subs, vec![("set".into(), 1), ("reset".into(), 1), ("status".into(), 1)]);
    let set = &json["options"][0]["options"];
    assert_eq!(set[0]["name"], "channel");
    assert_eq!(set[0]["type"], 7);
    assert_eq!(set[1]["name"], "new_channel");
    assert_eq!(set[1]["type"], 3);
    let reset = &json["options"][1]["options"][0];
    assert_eq!((reset["min_value"].as_i64(), reset["max_value"].as_i64()), (Some(0), Some(COUNT_MAX)));
    assert!(char_count(&json) <= 700, "payload is {} chars", char_count(&json));
    let meta = feat_counting::commands::counting::command().metadata();
    assert_eq!(meta["module"], "counting");
    assert_eq!(meta["gated"], false);
    assert_eq!(meta["permissionLevel"], 2);
}

// ── Hook filters (offline: the memoised settings decide) ──────────────────────

#[tokio::test]
async fn the_hook_filters_skip_other_channels_once_the_settings_are_memoised() {
    let t = TestApp::new().with_guilds(["900000000000000008"]);
    // Without a memo the filter passes and the handler decides after loading the settings.
    assert!(in_counting_channel(&t, "900000000000000008", "ANY"));
    t.settings()
        .update_settings("900000000000000008", SettingsPatch::new().set("countingEnabled", true).set("countingChannelId", "800000000000000008"))
        .await
        .expect("memory store");
    assert!(in_counting_channel(&t, "900000000000000008", "800000000000000008"));
    assert!(!in_counting_channel(&t, "900000000000000008", "800000000000000009"), "a number in another channel never takes the lock");
    assert!(!looks_like_count(Some("hi")), "chat is still skipped");
}

/// The hook runner starts the counting hook synchronously when the settings are memoised, so its place in the
/// guild's lock follows the dispatch order (offline: the database is unreachable, so each judged message fails;
/// what is checked is that the lock was queued during `run_hooks` itself).
#[tokio::test]
async fn the_hook_takes_the_lock_during_dispatch() {
    let g = "900000000000000011";
    let t = TestApp::new().with_guilds([g]);
    t.install(registry());
    t.settings()
        .update_settings(g, SettingsPatch::new().set("countingEnabled", true).set("countingChannelId", "800000000000000011"))
        .await
        .expect("memory store");
    let lock = t.ext::<svc::CountingLock>();
    assert!(lock.0.is_empty());
    let fut = t.run_hooks(MessageCreate {
        guild_id: serenity::all::GuildId::new(g.parse().unwrap_or(1)),
        message: fake_message("700000000000000001", g, "800000000000000011", "600000000000000001", "1"),
        member: None,
        channel_id: serenity::all::ChannelId::new(800000000000000011),
        channel: None,
    });
    assert_eq!(lock.0.len(), 1, "queued before the dispatch future was polled");
    fut.await;
}

// ── Database: the handlers against CountingState ──────────────────────────────

const G1: &str = "910000000000000001";
const G2: &str = "910000000000000002";
const G3: &str = "910000000000000003";
const G5: &str = "910000000000000005";
const G6: &str = "910000000000000006";
const G7: &str = "910000000000000007";
const G9: &str = "910000000000000009";
const GX: &str = "910000000000000010";
const GS: &str = "910000000000000011";

struct Room {
    t: DbTestApp,
    guild: &'static str,
    channel: String,
    next: std::sync::atomic::AtomicU64,
}

impl Room {
    async fn new(guild: &'static str, channel: &str) -> Option<Self> {
        let url = database_url()?;
        let t = DbTestApp::new(&url).await.with_guilds([guild]);
        t.reset_guilds(&[guild]).await;
        t.settings()
            .update_settings(guild, SettingsPatch::new().set("countingEnabled", true).set("countingChannelId", channel))
            .await
            .expect("settings row");
        Some(Self { t, guild, channel: channel.into(), next: std::sync::atomic::AtomicU64::new(1000) })
    }

    fn msg(&self, user: &str, content: &str) -> CountMessage {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        CountMessage { id: id.to_string(), channel_id: self.channel.clone(), author_id: user.into(), content: content.into() }
    }

    async fn send(&self, user: &str, content: &str) -> (CountMessage, Option<Vec<Action>>) {
        let m = self.msg(user, content);
        let settings = self.t.settings().get_settings_safe(self.guild).await;
        let actions = judge_message(&self.t, settings, self.guild, &m).await.expect("judged");
        (m, actions)
    }

    async fn row(&self) -> Option<bot_db::CountingState> {
        svc::load_state(self.t.db(), self.guild).await.expect("load")
    }

    async fn done(self) {
        self.t.reset_guilds(&[self.guild]).await;
    }
}

fn reacts(actions: &Option<Vec<Action>>) -> Vec<&'static str> {
    actions.iter().flatten().filter_map(|a| if let Action::React(e) = a { Some(*e) } else { None }).collect()
}

fn notes(actions: &Option<Vec<Action>>) -> Vec<Value> {
    actions.iter().flatten().filter_map(|a| if let Action::Note(e) = a { serde_json::to_value(e).ok() } else { None }).collect()
}

#[tokio::test]
async fn a_run_right_numbers_get_a_tick_and_counting_twice_resets_with_a_note() {
    let Some(r) = Room::new(G1, "810000000000000001").await else { return };
    assert_eq!(reacts(&r.send("A", "1").await.1), vec![OK_EMOJI]);
    assert_eq!(reacts(&r.send("B", "2").await.1), vec![OK_EMOJI]);
    assert!(r.send("A", "nice").await.1.is_none(), "chat is ignored");
    let (three, a) = r.send("A", "3").await;
    assert_eq!(reacts(&a), vec![OK_EMOJI]);
    let row = r.row().await.expect("row");
    assert_eq!((row.count, row.best), (3, 3));
    assert_eq!(row.last_message_id.as_deref(), Some(three.id.as_str()));
    assert!(notes(&a).is_empty());

    let (_, again) = r.send("A", "4").await;
    assert_eq!(reacts(&again), vec![FAIL_EMOJI]);
    let row = r.row().await.expect("row");
    assert_eq!((row.channel_id.as_str(), row.count, row.last_user_id.clone(), row.last_message_id.clone(), row.best), ("810000000000000001", 0, None, None, 3));
    let n = notes(&again);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["title"], "❌  Count Ruined");
    assert!(n[0]["description"].as_str().unwrap_or("").contains("<@A> ruined it at **3**! You can't count twice in a row"));

    // The one who ruined it may start again.
    assert_eq!(reacts(&r.send("A", "1").await.1), vec![OK_EMOJI]);
    r.done().await;
}

#[tokio::test]
async fn a_wrong_number_resets_and_at_zero_it_only_gets_a_cross_and_a_throttled_hint() {
    let Some(r) = Room::new(G2, "810000000000000002").await else { return };
    let (_, early) = r.send("A", "5").await;
    assert_eq!(reacts(&early), vec![FAIL_EMOJI]);
    assert!(r.row().await.is_none(), "nothing to reset at 0");
    let n = notes(&early);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["description"], "The count starts at **1**.");
    assert!(notes(&r.send("B", "0").await.1).is_empty(), "the hint is throttled");

    r.send("A", "1").await;
    r.send("B", "2").await;
    let (_, wrong) = r.send("A", "4").await;
    assert_eq!(reacts(&wrong), vec![FAIL_EMOJI]);
    let row = r.row().await.expect("row");
    assert_eq!((row.count, row.best), (0, 2));
    assert!(notes(&wrong)[0]["description"].as_str().unwrap_or("").contains("<@A> ruined it at **2**! The next number was **3**"));
    r.done().await;
}

#[tokio::test]
async fn maths_counts_and_a_wrong_sum_says_what_it_came_to() {
    let Some(r) = Room::new(G9, "810000000000000009").await else { return };
    assert_eq!(reacts(&r.send("A", "1").await.1), vec![OK_EMOJI]);
    let (two, a) = r.send("B", "(3*4)/6").await;
    assert_eq!(reacts(&a), vec![OK_EMOJI]);
    let row = r.row().await.expect("row");
    assert_eq!(row.count, 2);
    assert_eq!(row.last_message_id.as_deref(), Some(two.id.as_str()));
    assert!(r.send("A", "10/27").await.1.is_none(), "a fraction is chat");
    assert_eq!(reacts(&r.send("A", "2^2 - 1").await.1), vec![OK_EMOJI]);
    assert_eq!(r.row().await.expect("row").best, 3);
    let (_, wrong) = r.send("B", "2 * 3").await;
    assert_eq!(reacts(&wrong), vec![FAIL_EMOJI]);
    assert_eq!(r.row().await.expect("row").count, 0);
    assert!(notes(&wrong)[0]["description"].as_str().unwrap_or("").contains("<@B> ruined it at **3**! `2 * 3` is **6**. The next number was **4**"));
    r.done().await;
}

#[tokio::test]
async fn other_channels_and_no_channel_are_ignored() {
    let Some(r) = Room::new(G3, "810000000000000003").await else { return };
    let elsewhere = CountMessage { id: "1".into(), channel_id: "810000000000000099".into(), author_id: "A".into(), content: "1".into() };
    let s = r.t.settings().get_settings_safe(G3).await;
    assert!(judge_message(&r.t, s, G3, &elsewhere).await.expect("judged").is_none());
    assert!(judge_message(&r.t, None, G3, &elsewhere).await.expect("judged").is_none());
    assert!(r.row().await.is_none());
    r.done().await;
}

#[tokio::test]
async fn a_new_channel_starts_from_one_and_keeps_the_best_run() {
    let Some(r) = Room::new(G5, "810000000000000005").await else { return };
    svc::save_state(&r.t, G5, &NewState { channel_id: "OLD".into(), count: 12, last_user_id: Some("A".into()), last_message_id: Some("M".into()), best: 20 })
        .await
        .expect("saved");
    assert_eq!(reacts(&r.send("A", "1").await.1), vec![OK_EMOJI]);
    let row = r.row().await.expect("row");
    assert_eq!((row.channel_id.as_str(), row.count, row.best), ("810000000000000005", 1, 20));
    r.done().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hook_judges_counts_in_dispatch_order() {
    let Some(r) = Room::new(G6, "810000000000000006").await else { return };
    r.t.install(registry());
    let gid = serenity::all::GuildId::new(G6.parse().unwrap_or(1));
    let cid = serenity::all::ChannelId::new(810000000000000006);
    let mut futs = Vec::new();
    for (i, (u, c)) in [("600000000000000001", "1"), ("600000000000000002", "2"), ("600000000000000001", "3"), ("600000000000000002", "4")].iter().enumerate() {
        let message = fake_message(&format!("70000000000000000{i}"), G6, "810000000000000006", u, c);
        futs.push(r.t.run_hooks(MessageCreate { guild_id: gid, message, member: None, channel_id: cid, channel: None }));
    }
    futures_join(futs).await;
    let row = r.row().await.expect("row");
    assert_eq!((row.count, row.best), (4, 4), "every count was judged in order");
    r.done().await;
}

async fn futures_join<F: std::future::Future>(futs: Vec<F>) {
    for f in futs {
        f.await;
    }
}

#[tokio::test]
async fn deleting_the_current_count_posts_the_next_number() {
    let Some(r) = Room::new(G7, "810000000000000007").await else { return };
    let (one, _) = r.send("A", "1").await;
    let (two, _) = r.send("B", "2").await;
    let s = || r.t.settings().get_settings_safe(G7);
    assert!(deleted_note(&r.t, s().await, G7, "810000000000000007", &one.id).await.expect("ok").is_none(), "an older count is not the current one");
    assert!(deleted_note(&r.t, s().await, G7, "810000000000000099", &two.id).await.expect("ok").is_none(), "not the counting channel");
    let note = deleted_note(&r.t, s().await, G7, "810000000000000007", &two.id).await.expect("ok").expect("a note");
    let v = serde_json::to_value(&note).unwrap_or(Value::Null);
    assert_eq!(v["title"], "ℹ️  Number Deleted");
    assert_eq!(v["description"], "<@B> deleted their number **2**. The next number is **3**.");
    assert_eq!(r.row().await.expect("row").count, 2, "the count is kept");
    r.done().await;
}

// ── The command ───────────────────────────────────────────────────────────────

fn admin_slash(sub: &str, guild: &str) -> FakeInteraction {
    FakeInteraction::slash("counting")
        .guild(guild)
        .user("600000000000000009")
        .roles(&["820000000000000002"])
        .permissions(serenity::all::Permissions::MANAGE_GUILD)
        .subcommand(sub)
}

#[tokio::test]
async fn counting_reset_and_status_answer_publicly() {
    let Some(r) = Room::new(GX, "810000000000000010").await else { return };
    r.t.install(registry());
    let reset = admin_slash("reset", GX).option("number", bot_core::command::option_type::INTEGER, json!(41)).build();
    bot_core::router::route_interaction(&r.t, reset).await;
    assert_eq!(r.t.responses.names(), vec!["deferReply", "editReply"]);
    let calls = r.t.responses.calls();
    assert!(!calls[0].ephemeral(), "admin config commands answer publicly");
    assert_eq!(calls[1].embed_title(), Some("✅  Count Reset"));
    assert_eq!(calls[1].embed_description(), Some("The next number is **42**. (in <#810000000000000010>)"));
    let row = r.row().await.expect("row");
    assert_eq!((row.count, row.best, row.last_user_id.clone()), (41, 0, None), "an admin never sets the best run");

    r.t.responses.clear();
    bot_core::router::route_interaction(&r.t, admin_slash("status", GX).build()).await;
    let calls = r.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("🔢 Counting"));
    let fields: Vec<(String, String)> = calls[1].body["embeds"][0]["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|f| (f["name"].as_str().unwrap_or("").into(), f["value"].as_str().unwrap_or("").into()))
        .collect();
    assert_eq!(
        fields,
        vec![
            ("Status".into(), "✅ On".into()),
            ("Channel".into(), "⚠️ Deleted (`810000000000000010`)".into()),
            ("Next number".into(), "42".into()),
            ("Last counted by".into(), "—".into()),
            ("Best run".into(), "0".into()),
        ]
    );
    r.done().await;
}

#[tokio::test]
async fn counting_set_checks_the_channel_and_keeps_the_best_run() {
    use bot_core::testing::{FakeGuild, TEST_BOT_ID, offline_discord_as};
    use serenity::all::Permissions as P;
    let Some(r) = Room::new(GS, "810000000000000002").await else { return };
    r.t.install(registry());
    let bot_role = P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS | P::ADD_REACTIONS | P::READ_MESSAGE_HISTORY;
    let guild = FakeGuild::new(GS, "600000000000000099")
        .role("820000000000000001", 1, bot_role)
        .member(TEST_BOT_ID, &["820000000000000001"])
        .role("820000000000000002", 2, P::MANAGE_GUILD)
        .member("600000000000000009", &["820000000000000002"])
        .channel("810000000000000002", 0, "counting", &[])
        .channel("810000000000000003", 0, "new-counting", &[])
        .channel("810000000000000004", 0, "locked", &[(GS, false, P::empty(), P::SEND_MESSAGES)])
        .channel("810000000000000005", 4, "a-category", &[]);
    r.t.set_discord(offline_discord_as(&[guild.json()]));
    svc::save_state(&r.t, GS, &NewState { channel_id: "810000000000000002".into(), count: 9, last_user_id: None, last_message_id: None, best: 30 })
        .await
        .expect("saved");

    let set = |c: &str| admin_slash("set", GS).option("channel", bot_core::command::option_type::CHANNEL, json!(c)).build();
    bot_core::router::route_interaction(&r.t, set("810000000000000004")).await;
    let calls = r.t.responses.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1].embed_title(), Some("❌  Cannot Use That Channel"));
    assert_eq!(calls[1].embed_description(), Some("I need **Send Messages** in <#810000000000000004>. Pick another channel, or fix my permissions there."));

    r.t.responses.clear();
    bot_core::router::route_interaction(&r.t, set("810000000000000003")).await;
    let calls = r.t.responses.calls();
    assert_eq!(calls[1].embed_title(), Some("✅  Counting Channel Set"));
    let desc = calls[1].embed_description().unwrap_or_default().to_string();
    assert!(desc.starts_with("Counting is on in <#810000000000000003>. The first number is **1**.
Count up from **1**"), "{desc}");
    let row = r.row().await.expect("row");
    assert_eq!((row.channel_id.as_str(), row.count, row.best), ("810000000000000003", 0, 30));
    let s = r.t.settings().get_settings(GS).await.expect("settings");
    assert_eq!(s.counting_channel_id.as_deref(), Some("810000000000000003"));
    assert!(s.counting_enabled);

    // The same channel again keeps the count.
    svc::save_state(&r.t, GS, &NewState { channel_id: "810000000000000003".into(), count: 5, last_user_id: None, last_message_id: None, best: 30 })
        .await
        .expect("saved");
    r.t.responses.clear();
    bot_core::router::route_interaction(&r.t, set("810000000000000003")).await;
    assert_eq!(r.t.responses.calls()[1].embed_description().map(|d| d.starts_with("Counting is on in <#810000000000000003>.
")), Some(true));
    assert_eq!(r.row().await.expect("row").count, 5);

    // newTargets: both, or neither.
    r.t.responses.clear();
    let both = admin_slash("set", GS)
        .option("channel", bot_core::command::option_type::CHANNEL, json!("810000000000000003"))
        .option("new_channel", bot_core::command::option_type::STRING, json!("fresh"))
        .build();
    bot_core::router::route_interaction(&r.t, both).await;
    assert_eq!(r.t.responses.calls()[1].embed_title(), Some("❌  Pick One"));
    r.t.responses.clear();
    bot_core::router::route_interaction(&r.t, admin_slash("set", GS).build()).await;
    assert_eq!(r.t.responses.calls()[1].embed_title(), Some("❌  Missing Channel"));
    assert_eq!(
        r.t.responses.calls()[1].embed_description(),
        Some("Pick a channel in `channel`, or type a name in `new_channel` to create one.")
    );
    // An existing channel typed by name is reused (exact match after Discord's slug rule).
    r.t.responses.clear();
    let typed = admin_slash("set", GS).option("new_channel", bot_core::command::option_type::STRING, json!(" New Counting ")).build();
    bot_core::router::route_interaction(&r.t, typed).await;
    assert_eq!(r.t.responses.calls()[1].embed_title(), Some("✅  Counting Channel Set"));
    r.done().await;
}

// ── Cross-language: the JS service writes, Rust reads; Rust writes, the JS reads ──

#[tokio::test]
async fn js_and_rust_read_each_others_counting_state() {
    let Some(url) = database_url() else { return };
    let g = "910000000000000020";
    let t = DbTestApp::new(&url).await.with_guilds([g]);
    t.reset_guilds(&[g]).await;

    let js = run_node(
        "rust/fixtures/gen/feat-counting-js.mjs",
        &["save", g, r#"{"channelId":"810000000000000020","count":41,"lastUserId":"600000000000000001","lastMessageId":"700000000000000001","best":77}"#],
    );
    assert!(js.get("error").is_none(), "{js}");
    let row = svc::load_state(t.db(), g).await.expect("load").expect("row");
    assert_eq!((row.channel_id.as_str(), row.count, row.best), ("810000000000000020", 41, 77));
    assert_eq!(row.last_user_id.as_deref(), Some("600000000000000001"));
    let state = current_state(Some(&row), Some("810000000000000020"));
    assert!(judge_count(state.count, state.last_user_id.as_deref(), "600000000000000002", 42).ok);
    assert_eq!(serde_json::to_value(&row).ok().and_then(|v| v.get("updatedAt").cloned()), js.get("updatedAt").cloned(), "same timestamp text");

    svc::save_state(&t, g, &NewState { channel_id: "810000000000000021".into(), count: 3, last_user_id: None, last_message_id: None, best: 77 })
        .await
        .expect("saved");
    let back = run_node("rust/fixtures/gen/feat-counting-js.mjs", &["load", g]);
    assert!(back.is_object() && back.get("error").is_none(), "JS load answered {back}");
    assert_eq!(back["channelId"], "810000000000000021");
    assert_eq!(back["count"], 3);
    assert_eq!(back["lastUserId"], Value::Null);
    assert_eq!(back["best"], 77);
    assert_eq!(back["guildId"], g);
    let ours = serde_json::to_value(svc::load_state(t.db(), g).await.expect("load").expect("row")).unwrap_or(Value::Null);
    assert_eq!(ours, back, "the row reads the same through Prisma and Rust");
    t.reset_guilds(&[g]).await;
}
