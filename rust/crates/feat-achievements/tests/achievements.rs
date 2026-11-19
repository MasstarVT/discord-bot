//! Port of `test/achievements.test.js`, plus the golden, database and cross-language checks.
//!
//! Offline: the pure helpers (goals, tiers, rewards, progress maths, boost detection, announcement channel and text,
//! badge/name/deadline parsing, member entries), a sweep of every pure helper against Node, the command payloads,
//! the plugin declaration and the hook filters. With `TEST_DATABASE_URL` / `TEST_REDIS_URL`: the raw statements, the
//! three recording phases (concurrent first events included), the compare-and-set claim, payout, catch-up seeding,
//! reset, tier changes, the commands through the router, and both directions of the JS round trip.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use bot_core::prelude::*;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, offline_discord_as, redis_url, run_node};
use feat_achievements::achievement_service::{self as svc, EntryView, Lite, MessageFlush, Origin, Reward};
use serde_json::{Value, json};
use serenity::all::Permissions;

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_achievements::register(&mut reg);
    reg
}

const R1: &str = "123456789012345601";
const R2: &str = "123456789012345602";

fn tiers(goals: &[i64]) -> Vec<svc::Tier> {
    svc::normalize_tiers(&json!(goals.iter().map(|g| json!({ "goal": g })).collect::<Vec<_>>()))
}

fn lite(over: Value) -> Lite {
    let mut base = json!({
        "id": "a", "name": "Chatterbox", "description": null, "emoji": null, "trigger": "MESSAGES", "trackPastProgress": true, "deadlineMs": null,
        "tiers": [{ "goal": 100 }, { "goal": 500 }, { "goal": 2500 }, { "goal": 10000 }],
    });
    for (k, v) in over.as_object().cloned().unwrap_or_default() {
        base[k] = v;
    }
    serde_json::from_value(base).expect("a lite achievement")
}

// ── 1. parseGoals ─────────────────────────────────────────────────────────────

#[test]
fn parse_goals_accepts_lists_suffixes_and_separators() {
    assert_eq!(svc::parse_goals("10,50,100", "MESSAGES"), Ok(vec![10, 50, 100]));
    assert_eq!(svc::parse_goals("1k, 5k > 10k", "MESSAGES"), Ok(vec![1000, 5000, 10000]));
    assert_eq!(svc::parse_goals("1.5k", "MESSAGES"), Ok(vec![1500]));
    assert_eq!(svc::parse_goals("1.1k", "MESSAGES"), Ok(vec![1100]));
    assert_eq!(svc::parse_goals("2m", "MESSAGES"), Ok(vec![2_000_000]));
    assert_eq!(svc::parse_goals("5;10|20/30", "PURCHASES"), Ok(vec![5, 10, 20, 30]));
}

#[test]
fn parse_goals_refuses_bad_input_with_the_spec_texts() {
    let err = |i: &str, t: &str| svc::parse_goals(i, t).err().unwrap_or_default();
    assert!(err("1.0005k", "MESSAGES").contains("is not a whole number"));
    assert!(err("100 50", "MESSAGES").contains("Goals must go up"));
    assert!(err("0", "MESSAGES").contains("at least 1"));
    assert!(err("1 2 3 4 5", "MESSAGES").contains("Give 1 to 4 goals"));
    assert!(err("1001", "LEVEL").contains("Level reached goals can be at most 1,000"));
    assert!(err("101", "BOOST").contains("Server boosts goals can be at most 100"));
    assert!(err("", "MESSAGES").contains("Give 1 to 4 goals"));
    assert_eq!(err("abc", "MESSAGES"), "`\"abc\"` is not a whole number.");
    assert!(err("10 10", "MESSAGES").contains("Goals must go up"));
}

// ── 2/3. normalizeTiers, validateTiers ────────────────────────────────────────

#[test]
fn normalize_tiers_drops_bad_shapes_and_relabels() {
    for bad in [json!(null), json!({}), json!("x"), json!(5), json!({ "version": 2, "tiers": [] })] {
        assert!(svc::normalize_tiers(&bad).is_empty());
    }
    let names: Vec<String> = svc::normalize_tiers(&json!([{ "name": "Gold", "goal": 10, "rewards": [] }, { "name": "Bronze", "goal": 20 }])).into_iter().map(|t| t.name).collect();
    assert_eq!(names, vec!["Bronze", "Silver"]);
    let goals = |v: Value| svc::normalize_tiers(&v).into_iter().map(|t| t.goal).collect::<Vec<_>>();
    assert_eq!(goals(json!([{ "goal": 0 }, { "goal": 1.5 }, { "goal": "5" }, { "goal": 7 }])), vec![7]);
    assert_eq!(goals(json!([{ "goal": 10 }, { "goal": 50 }, { "goal": 40 }, { "goal": 100 }])), vec![10, 50]);
    assert_eq!(goals(json!([{ "goal": 1 }, { "goal": 2 }, { "goal": 3 }, { "goal": 4 }, { "goal": 5 }])).len(), 4);
    assert_eq!(goals(json!([null, "x", { "goal": 3 }])), vec![3]);
    assert!(goals(json!([{ "goal": 10_000_001 }])).is_empty());
}

#[test]
fn rewards_are_validated_deduplicated_and_ordered() {
    let t = svc::normalize_tiers(&json!([{ "goal": 5, "rewards": [
        { "type": "COINS", "amount": 5 }, { "type": "XP", "amount": 10 }, { "type": "XP", "amount": 99 }, { "type": "ROLE_REMOVE", "roleId": R1 },
        { "type": "ROLE_ADD", "roleId": R1 }, { "type": "ROLE_ADD", "roleId": R2 }, { "type": "NOPE", "amount": 1 }, { "type": "ROLE_ADD", "roleId": "abc" },
        { "type": "COINS", "amount": 0 }, { "type": "XP", "amount": 1_000_001 }, { "type": "XP", "amount": 1.5, "extra": 1 }
    ] }]));
    assert_eq!(t[0].rewards, vec![Reward::RoleAdd(R1.into()), Reward::Xp(10), Reward::Coins(5)]);
    assert_eq!(
        svc::normalize_rewards(&json!([{ "type": "ROLE_REMOVE", "roleId": R2 }, { "type": "XP", "amount": 5, "junk": true }])),
        vec![Reward::RoleRemove(R2.into()), Reward::Xp(5)]
    );
    assert!(svc::normalize_rewards(&json!("x")).is_empty());
    assert_eq!(svc::normalize_rewards(&json!([{ "type": "XP", "amount": 0 }, { "type": "XP", "amount": 7 }])), vec![Reward::Xp(7)]);
    assert_eq!(serde_json::to_value(Reward::RoleAdd(R1.into())).ok(), Some(json!({ "type": "ROLE_ADD", "roleId": R1 })));
}

#[test]
fn validate_and_serialize_tiers() {
    assert!(svc::validate_tiers(&json!([]), "MESSAGES").is_err());
    assert!(svc::validate_tiers(&json!([{ "goal": 1001 }]), "LEVEL").is_err());
    assert!(svc::validate_tiers(&json!([{ "goal": 1 }, { "goal": 2 }, { "goal": 3 }, { "goal": 4 }, { "goal": 5 }]), "MESSAGES").is_err());
    assert!(svc::validate_tiers(&json!([{ "goal": 5 }, { "goal": 5 }]), "MESSAGES").is_err());
    assert_eq!(svc::validate_tiers(&json!([{ "goal": 1, "rewards": [{ "type": "XP", "amount": 0 }] }]), "MESSAGES"), Err("Invalid rewards.".into()));
    let four = json!([10, 20, 30, 40].iter().map(|g| json!({ "goal": g, "rewards": [{ "type": "XP", "amount": 5 }] })).collect::<Vec<_>>());
    assert!(svc::validate_tiers(&four, "MESSAGES").is_ok());
    assert!(svc::validate_tiers(&json!([{ "goal": 1 }]), "LEVEL").is_ok());
    let out = svc::tiers_json(&svc::serialize_tiers(&json!([{ "goal": 3, "rewards": [{ "type": "XP", "amount": 1 }] }])));
    assert_eq!(out, json!([{ "name": "Bronze", "goal": 3, "rewards": [{ "type": "XP", "amount": 1 }] }]));
}

// ── 4. evaluateProgress ───────────────────────────────────────────────────────

#[test]
fn evaluate_progress() {
    let t = tiers(&[10, 50, 100]);
    let e = |p, tier, n| svc::evaluate_progress(&t, p, tier, n);
    assert_eq!(e(9, 0, false).almost, Some((0, 10)));
    assert_eq!(e(9, 0, true), svc::Evaluation { advance_to: None, almost: None });
    assert_eq!(e(10, 0, false).advance_to, Some(1));
    assert_eq!(e(120, 0, false).advance_to, Some(3));
    assert_eq!(e(45, 1, false).almost, Some((1, 50)));
    assert_eq!(e(44, 1, false).almost, None);
    assert_eq!(e(120, 3, false), svc::Evaluation { advance_to: None, almost: None });
    assert_eq!(e(120, 5, false), svc::Evaluation { advance_to: None, almost: None });
    assert_eq!(svc::evaluate_progress(&tiers(&[9]), 8, 0, false), svc::Evaluation { advance_to: None, almost: None });
    assert_eq!(e(0, 0, false), svc::Evaluation { advance_to: None, almost: None });
    assert_eq!(e(10, 1, false).advance_to, None);
}

// ── 5/6. isBoostStart, pickAnnounceChannel ────────────────────────────────────

#[test]
fn is_boost_start() {
    let now = 1_700_000_000_000;
    assert!(svc::is_boost_start(true, None, Some(now), now));
    assert!(!svc::is_boost_start(true, Some(now - 9000), Some(now), now));
    assert!(svc::is_boost_start(false, None, Some(now - 30_000), now));
    assert!(!svc::is_boost_start(false, None, Some(now - 5 * 60_000), now));
    assert!(!svc::is_boost_start(true, Some(5), None, now));
    assert!(!svc::is_boost_start(false, None, Some(now + 60_000), now));
    assert!(!svc::is_boost_start(false, None, None, now));
}

#[test]
fn pick_announce_channel() {
    let ok = |c: (char, bool)| c.1;
    let (a, b) = (('A', true), ('B', false));
    assert_eq!(svc::pick_announce_channel("CURRENT", Some(a), Some(b), ok), Some(a));
    assert_eq!(svc::pick_announce_channel("CURRENT", None, Some(a), ok), Some(a));
    assert_eq!(svc::pick_announce_channel("CURRENT", Some(b), None, ok), None);
    assert_eq!(svc::pick_announce_channel("CURRENT", Some(b), Some(a), ok), Some(a));
    assert_eq!(svc::pick_announce_channel("CHANNEL", Some(a), Some(b), ok), Some(a));
    assert_eq!(svc::pick_announce_channel("CHANNEL", Some(b), Some(a), ok), Some(a));
    assert_eq!(svc::pick_announce_channel("OFF", Some(a), Some(a), ok), None);
    assert_eq!(svc::pick_announce_channel("DM", Some(a), Some(a), ok), None);
    assert_eq!(svc::pick_announce_channel::<(char, bool)>("CURRENT", None, None, ok), None);
}

// ── 7. Announcement text ──────────────────────────────────────────────────────

fn render(template: &str, vars: Vec<(String, String)>) -> String {
    let ctx = bot_util::placeholder_parser::RenderContext { vars: Some(svc::to_vars(vars)), ..Default::default() };
    bot_util::placeholder_parser::render_placeholders(template, &ctx, None)
}

#[test]
fn announcement_variables_render_through_the_default_message() {
    let a = lite(json!({ "description": "Talk a lot", "tiers": [{ "goal": 10 }, { "goal": 50 }, { "goal": 100 }] }));
    let vars = svc::build_announcement_vars(&a, 2, 1234, "1", "Ann*", "+100 XP");
    assert_eq!(render(svc::DEFAULT_ACHIEVEMENT_MESSAGE, vars.clone()), "🏆 <@1> unlocked **Chatterbox (Gold)**!");
    assert_eq!(
        render("{tier} {tier.emoji} {goal} {progress} {rewards} {achievement.name} {achievement.description}", vars.clone()),
        "Gold 🥇 100 1,234 +100 XP Chatterbox Talk a lot"
    );
    assert_eq!(render("{player.name}", vars), "Ann\\*");
    let single = lite(json!({ "name": "Booster", "tiers": [{ "goal": 1 }] }));
    let sv = svc::build_announcement_vars(&single, 0, 1, "1", "Ann", "");
    assert_eq!(render(svc::DEFAULT_ACHIEVEMENT_MESSAGE, sv.clone()), "🏆 <@1> unlocked **Booster**!");
    assert!(sv.contains(&("tier".into(), "Bronze".into())));
    assert!(sv.contains(&("rewards".into(), String::new())));
    assert_eq!(svc::achievement_label("Chatterbox", 3, 0), "Chatterbox (Bronze)");
    let sample = svc::sample_vars(Some("9"), Some("Bo"));
    assert!(sample.contains(&("player".into(), "<@9>".into())));
    assert!(sample.contains(&("achievement".into(), "Chatterbox (Gold)".into())));
    assert!(sample.contains(&("goal".into(), "2,500".into())));
    assert!(svc::template_mentions_player("Hey {PLAYER}"));
    assert!(svc::template_mentions_player("{user.mention}"));
    assert!(!svc::template_mentions_player("{player.name}"));
}

// ── 8-12. Badge emoji, names, deadlines, rewards merge ────────────────────────

#[test]
fn normalize_badge_emoji() {
    for good in ["🏆", "🇺🇸", "👨‍👩‍👧", "❤️"] {
        assert!(svc::normalize_badge_emoji(good, |_| false, false).is_ok(), "{good}");
    }
    let many = "🏆".repeat(20);
    for bad in ["abc", "12", "🏆 🏆", "", many.as_str()] {
        assert!(svc::normalize_badge_emoji(bad, |_| false, false).is_err(), "{bad}");
    }
    let custom = "<:x:123456789012345678>";
    assert_eq!(svc::normalize_badge_emoji(custom, |_| true, false), Ok(Some(custom.into())));
    assert_eq!(svc::normalize_badge_emoji(custom, |_| false, false), Err("I can only show custom emojis from servers I'm in.".into()));
    assert_eq!(svc::normalize_badge_emoji("none", |_| false, true), Ok(None));
    assert_eq!(svc::normalize_badge_emoji("NONE", |_| false, true), Ok(None));
    assert!(svc::normalize_badge_emoji("none", |_| false, false).is_err());
}

#[test]
fn normalize_achievement_name() {
    assert_eq!(svc::normalize_achievement_name(Some("  Chatter\n box ")), Some("Chatter box".into()));
    assert_eq!(svc::normalize_achievement_name(Some("")), None);
    assert_eq!(svc::normalize_achievement_name(Some("   ")), None);
    assert_eq!(svc::normalize_achievement_name(Some(&"x".repeat(51))), None);
    assert_eq!(svc::normalize_achievement_name(Some(&"x".repeat(50))), Some("x".repeat(50)));
    assert_eq!(svc::normalize_achievement_name(None), None);
}

#[test]
fn parse_deadline() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z").map(|d| d.to_utc()).expect("date");
    let d = svc::parse_deadline("30d", "UTC", &now, false).expect("ok").expect("a date");
    assert_eq!(d.timestamp_millis(), now.timestamp_millis() + 30 * 86_400_000);
    assert_eq!(svc::parse_deadline("none", "UTC", &now, true), Ok(None));
    assert_eq!(svc::parse_deadline("OFF", "UTC", &now, true), Ok(None));
    assert!(svc::parse_deadline("none", "UTC", &now, false).err().unwrap_or_default().contains("Use a date like 2026-12-31 23:59"));
    assert!(svc::parse_deadline("2000-01-01", "UTC", &now, false).err().unwrap_or_default().contains("in the future"));
    assert!(svc::parse_deadline("30s", "UTC", &now, false).err().unwrap_or_default().contains("in the future"));
    assert!(svc::parse_deadline("6y", "UTC", &now, false).err().unwrap_or_default().contains("at most 5 years"));
    assert!(svc::parse_deadline("2040-01-01", "UTC", &now, false).err().unwrap_or_default().contains("at most 5 years"));
    let v = svc::parse_deadline("2026-12-31 23:59", "UTC", &now, false).expect("ok").expect("a date");
    assert_eq!(v.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "2026-12-31T23:59:00.000Z");
    assert!(svc::parse_deadline("garbage", "Europe/Paris", &now, false).err().unwrap_or_default().contains("server time zone: Europe/Paris"));
}

#[test]
fn merge_rewards_and_remap_tier_after_removal() {
    assert_eq!(svc::merge_rewards(&[Reward::Xp(10), Reward::RoleAdd(R1.into())], &[Reward::Xp(20)], &["XP"]), vec![Reward::RoleAdd(R1.into()), Reward::Xp(20)]);
    assert_eq!(svc::merge_rewards(&[], &[Reward::Xp(20)], &["XP"]), vec![Reward::Xp(20)]);
    assert_eq!(svc::merge_rewards(&[Reward::Xp(10)], &[], &[]), vec![Reward::Xp(10)]);
    assert_eq!(svc::merge_rewards(&[Reward::RoleAdd(R1.into())], &[Reward::RoleRemove(R1.into())], &["ROLE_REMOVE"]), vec![Reward::RoleAdd(R1.into())]);
    assert_eq!(svc::remap_tier_after_removal(2, 1), 1);
    assert_eq!(svc::remap_tier_after_removal(3, 1), 2);
    assert_eq!(svc::remap_tier_after_removal(1, 1), 1);
    assert_eq!(svc::remap_tier_after_removal(0, 0), 0);
}

// ── 13/14. displayProgress, formatMemberEntry ─────────────────────────────────

#[test]
fn display_progress() {
    let base = lite(json!({ "tiers": [{ "goal": 100 }] }));
    let dp = |a: &Lite, row, ul, boost| svc::display_progress(a, row, ul, boost);
    assert_eq!(dp(&base, None, Some((120, 3)), false), svc::DisplayProgress { progress: 120, tier: 0, pending: true });
    assert_eq!(dp(&base, None, Some((50, 0)), false), svc::DisplayProgress { progress: 50, tier: 0, pending: false });
    let no_past = lite(json!({ "tiers": [{ "goal": 100 }], "trackPastProgress": false }));
    assert_eq!(dp(&no_past, None, Some((120, 0)), false), svc::DisplayProgress { progress: 0, tier: 0, pending: false });
    assert_eq!(dp(&base, Some((7, 0)), Some((120, 0)), false), svc::DisplayProgress { progress: 7, tier: 0, pending: false });
    assert_eq!(dp(&base, Some((100, 9)), None, false), svc::DisplayProgress { progress: 100, tier: 1, pending: false });
    let level = lite(json!({ "tiers": [{ "goal": 100 }], "trigger": "LEVEL" }));
    assert_eq!(dp(&level, None, None, false).progress, 0);
    assert_eq!(dp(&level, None, Some((0, 12)), false).progress, 12);
    let boost = lite(json!({ "tiers": [{ "goal": 1 }], "trigger": "BOOST" }));
    assert!(dp(&boost, None, None, true).pending);
    let reactions = lite(json!({ "tiers": [{ "goal": 100 }], "trigger": "REACTIONS" }));
    assert_eq!(dp(&reactions, None, Some((500, 0)), false).progress, 0);
}

#[test]
fn format_member_entry() {
    let now = chrono::Utc::now().timestamp_millis();
    let v = |progress, tier| EntryView { progress, tier, pending: false, enabled: None };
    let done = svc::format_member_entry(&lite(json!({})), &v(12345, 4), now);
    assert!(done.contains("✅ Completed · 12,345 messages"), "{done}");
    assert!(done.contains("💬 **Chatterbox** — 💎 Diamond"), "{done}");
    let three = lite(json!({ "tiers": [{ "goal": 100 }, { "goal": 500 }, { "goal": 2500 }] }));
    let mid = svc::format_member_entry(&three, &v(130, 1), now);
    assert!(mid.contains("🥈 Silver") && mid.contains("130/500 → 🥈 Silver"), "{mid}");
    let one = lite(json!({ "tiers": [{ "goal": 100 }] }));
    let single = svc::format_member_entry(&one, &v(100, 1), now);
    assert!(single.contains("— ✅ Unlocked") && single.contains("✅ Completed · 100 messages"), "{single}");
    let level = svc::format_member_entry(&lite(json!({ "trigger": "LEVEL" })), &v(7, 0), now);
    assert!(level.contains("▰") || level.contains("▱"));
    assert!(level.contains("7/100"));
    let level1 = lite(json!({ "trigger": "LEVEL", "tiers": [{ "goal": 100 }] }));
    assert!(svc::format_member_entry(&level1, &v(120, 1), now).contains("Completed · level 120"));
    assert!(svc::format_member_entry(&lite(json!({ "deadlineMs": now - 1000 })), &v(1, 0), now).contains("⌛ ended"));
    let ends = svc::format_member_entry(&lite(json!({ "deadlineMs": now + 86_400_000 })), &v(1, 0), now);
    assert!(ends.contains("⏳ ends <t:") && ends.contains(":R>"), "{ends}");
    assert!(svc::format_member_entry(&lite(json!({})), &EntryView { progress: 1, tier: 0, pending: false, enabled: Some(false) }, now).contains("(retired)"));
    assert!(svc::format_member_entry(&lite(json!({})), &EntryView { progress: 150, tier: 0, pending: true, enabled: None }, now).contains("unlocks on your next activity"));
    let long = svc::format_member_entry(
        &lite(json!({ "name": "n".repeat(50), "description": "d".repeat(200), "emoji": "<:abcdefghij:123456789012345678>" })),
        &v(1, 1),
        now,
    );
    assert_eq!(long.split('\n').count(), 3);
    assert!(long.split('\n').all(|l| bot_util::js::utf16_len(l) <= 300));
}

// ── 15. Reward availability ───────────────────────────────────────────────────

#[derive(Default)]
struct FakeProvider {
    grants: Mutex<Vec<(String, i64, String)>>,
    refuse: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl RewardProvider for FakeProvider {
    async fn grant(&self, _app: &App, args: RewardArgs) -> BotResult<RewardResult> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(RewardResult::fail("module-disabled"));
        }
        self.grants.lock().unwrap_or_else(|e| e.into_inner()).push((args.user_id.clone(), args.amount, args.reason.clone()));
        Ok(RewardResult::ok(Some(args.amount)))
    }
    async fn take(&self, _app: &App, _args: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(None))
    }
}

fn amounts(p: &FakeProvider) -> Vec<i64> {
    p.grants.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|g| g.1).collect()
}

fn providers(app: &App) -> (Arc<FakeProvider>, Arc<FakeProvider>) {
    let (xp, coins) = (Arc::new(FakeProvider::default()), Arc::new(FakeProvider::default()));
    let _ = app.rewards().register_provider(RewardKind::Xp, xp.clone(), Some("test"));
    let _ = app.rewards().register_provider(RewardKind::Coins, coins.clone(), Some("test"));
    (xp, coins)
}

#[tokio::test]
async fn reward_unavailable_message_and_format_rewards() {
    let g = "960000000000000901";
    let t = TestApp::new().with_guilds([g]);
    let on = t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", true).set("economyEnabled", true)).await.expect("memory store");
    assert_eq!(
        svc::reward_unavailable_message(&t, RewardKind::Xp, Some(&on)).as_deref(),
        Some("XP rewards are not available on this bot (the ⭐ Levels module is not installed).")
    );
    assert_eq!(
        svc::reward_unavailable_message(&t, RewardKind::Coins, Some(&on)).as_deref(),
        Some("Coin rewards are not available on this bot (the 🪙 Economy module is not installed).")
    );
    providers(&t);
    let off = t.settings().update_settings(g, SettingsPatch::new().set("xpEnabled", false).set("economyEnabled", false)).await.expect("memory store");
    assert_eq!(
        svc::reward_unavailable_message(&t, RewardKind::Xp, Some(&off)).as_deref(),
        Some("XP rewards need the **⭐ Levels** module. Enable it with `/module enable leveling`.")
    );
    assert!(svc::reward_unavailable_message(&t, RewardKind::Coins, Some(&off)).unwrap_or_default().contains("/module enable economy"));
    assert_eq!(svc::reward_unavailable_message(&t, RewardKind::Xp, Some(&on)), None);
    assert_eq!(svc::reward_unavailable_message(&t, RewardKind::Coins, Some(&on)), None);

    let roles: HashSet<String> = [R1.to_string()].into_iter().collect();
    let text = svc::format_rewards_with(&[Reward::RoleAdd(R1.into()), Reward::RoleRemove(R2.into()), Reward::Xp(1500), Reward::Coins(20)], Some(&roles), "🪙", "gold");
    assert_eq!(text, format!("<@&{R1}>, −@deleted-role ⚠️, +1,500 XP, +20 🪙 gold"));
    assert_eq!(svc::format_rewards(&[Reward::Coins(1)], None, None), "+1 🪙 coins");
    assert_eq!(svc::format_rewards(&[], None, None), "");
    let many: Vec<Reward> = (0..100).map(|_| Reward::Xp(123_456)).collect();
    assert!(bot_util::js::utf16_len(&svc::format_rewards(&many, None, None)) <= 900);
}

// ── 16/17. Command payloads and the plugin ────────────────────────────────────

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
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["achievement-config", "achievements", "birthday"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "achievements");
    bot_core::testing::assert_plugin_matches_golden(&reg, "birthdays");
}

#[test]
fn command_payloads() {
    let a = feat_achievements::commands::achievements::command().payload();
    let c = feat_achievements::commands::achievement_config::command().payload();
    assert!(char_count(&a) <= 260);
    assert!(char_count(&c) <= 2800);
    let names = |v: &Value| v["options"].as_array().into_iter().flatten().map(|o| o["name"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>();
    assert_eq!(names(&c), vec!["create", "edit", "delete", "enable", "disable", "list", "info", "reset", "preset", "tier", "announce"]);
    let group = |n: &str| c["options"].as_array().into_iter().flatten().find(|o| o["name"] == n).cloned().unwrap_or(Value::Null);
    assert_eq!(names(&group("tier")), vec!["add", "edit", "remove"]);
    assert_eq!(names(&group("announce")), vec!["mode", "channel", "message"]);
    assert_eq!(group("create")["options"].as_array().map(Vec::len), Some(12));
    assert_eq!(names(&group("announce")["options"][0]), vec!["mode", "channel", "new_channel", "embed"]);
    fn walk(node: &Value, out: &mut Vec<Value>) {
        if node["name"] == "achievement" && node["type"] == 3 {
            out.push(node.clone());
        }
        for o in node["options"].as_array().into_iter().flatten() {
            walk(o, out);
        }
    }
    let mut ach = Vec::new();
    walk(&c, &mut ach);
    assert_eq!(ach.len(), 9);
    assert!(ach.iter().all(|o| o["autocomplete"] == true));
    let meta = feat_achievements::commands::achievement_config::command().metadata();
    assert_eq!((meta["module"].clone(), meta["gated"].clone(), meta["permissionLevel"].clone()), (json!("achievements"), json!(false), json!(2)));
    let meta = feat_achievements::commands::achievements::command().metadata();
    assert_eq!((meta["gated"].clone(), meta["permissionLevel"].clone(), meta["hasAutocomplete"].clone()), (json!(true), json!(0), json!(false)));
    assert_eq!(c["default_member_permissions"], "32");
    assert!(a.get("default_member_permissions").is_none() || a["default_member_permissions"].is_null());
}

#[test]
fn plugin_declaration() {
    let reg = registry();
    let p = reg.plugin_named("achievements").expect("declared");
    assert_eq!(p.module, Some(ModuleKey::Achievements));
    let hooks: Vec<(&str, &str)> = p.hooks.iter().map(|h| (h.event().as_str(), h.id())).collect();
    assert_eq!(
        hooks,
        vec![
            ("messageCreate", "achievements"),
            ("messageReactionAdd", "achievements"),
            ("guildMemberUpdate", "achievements"),
            ("levelUp", "achievements"),
            ("economyPurchase", "achievements")
        ]
    );
    for (event, id) in hooks {
        assert!(bot_config::hook_index(event, id).is_some(), "{event} {id} in HOOK_ORDER");
    }
}

#[tokio::test]
async fn hook_filters_and_may_have_triggers() {
    let t = TestApp::new();
    svc::reset_achievement_state(&t);
    assert!(!svc::message_filter(&t, "g", true));
    assert!(!svc::message_filter(&t, "", false));
    assert!(svc::message_filter(&t, "g", false), "unknown index: let the hook load it");
    assert!(!svc::reaction_filter(&t, "g", true));
    assert!(svc::reaction_filter(&t, "g", false));
    let now = t.clock().now_ms();
    assert!(!svc::member_update_filter(&t, "g", true, true, None, Some(now)));
    assert!(!svc::member_update_filter(&t, "g", false, true, None, None));
    assert!(!svc::member_update_filter(&t, "g", false, true, Some(now), Some(now)));
    assert!(svc::member_update_filter(&t, "g", false, true, None, Some(now)));
    assert!(!svc::level_up_filter(&t, ""));
    assert!(svc::purchase_filter(&t, "g"));
    assert!(svc::may_have_triggers(&t, "unknown-guild", &["LEVEL"]));
}

#[test]
fn active_achievements_build_index_and_to_lite() {
    let a = |over: Value| lite(json!({ "tiers": [{ "goal": 1 }] })).clone_with(over);
    trait W {
        fn clone_with(self, over: Value) -> Lite;
    }
    impl W for Lite {
        fn clone_with(self, over: Value) -> Lite {
            let mut v = serde_json::to_value(&self).unwrap_or(Value::Null);
            for (k, x) in over.as_object().cloned().unwrap_or_default() {
                v[k] = x;
            }
            serde_json::from_value(v).expect("lite")
        }
    }
    let list = vec![a(json!({})), a(json!({ "tiers": [] })), a(json!({ "deadlineMs": 999 })), a(json!({ "deadlineMs": 1000 }))];
    assert_eq!(svc::active_achievements(&list, 1000).len(), 2);
    let idx = svc::build_index(&[a(json!({ "trigger": "LEVEL" })), a(json!({}))], 0);
    assert_eq!(idx.get("LEVEL").len(), 1);
    assert_eq!(idx.get("MESSAGES").len(), 1);
    assert!(idx.get("BOOST").is_empty());
    let row: bot_db::Achievement = serde_json::from_value(json!({
        "id": "i", "guildId": "g", "name": "N", "description": null, "emoji": null, "trigger": "BOOST",
        "tiers": [{ "name": "X", "goal": 2, "rewards": [] }], "trackPastProgress": false, "deadline": "1970-01-01T00:00:05.000Z",
        "enabled": true, "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z"
    }))
    .expect("an Achievement row");
    assert_eq!(
        serde_json::to_value(svc::to_lite(&row)).ok(),
        Some(json!({ "id": "i", "name": "N", "description": null, "emoji": null, "trigger": "BOOST", "tiers": [{ "name": "Bronze", "goal": 2, "rewards": [] }], "trackPastProgress": false, "deadlineMs": 5000 }))
    );
}

#[test]
fn raw_sql_uses_sql_now_and_explicit_casts() {
    let all = svc::raw_sql();
    assert_eq!(all.iter().map(|r| r.0).collect::<Vec<_>>(), vec!["achievements.progressUpdate", "achievements.progressInsert"]);
    for (name, sql) in &all {
        let bare = sql.replace(bot_db::SQL_NOW, "");
        assert!(!bare.to_lowercase().contains("now()"), "{name} has a bare now()");
        assert!(sql.contains("\"updatedAt\""), "{name}");
    }
    let insert = &all[1].1;
    assert!(insert.contains("ON CONFLICT (\"achievementId\", \"userId\") DO NOTHING"));
    assert!(insert.contains("unnest($4::text[], $5::text[], $6::int[])"));
}

#[test]
fn keys_match_the_js_templates() {
    assert_eq!(svc::achievements_key("1"), "guild:1:achievements");
    assert_eq!(svc::reaction_key("1", "2", "3", "⭐"), "ach:react:1:2:3:⭐");
    assert_eq!(svc::boost_key("1", "2", 1767225600123), "ach:boost:1:2:1767225600123");
}

// ── Sweep: every pure helper against the JS ───────────────────────────────────

fn rust_pure(f: &str, a: &Value) -> Value {
    let s = |i: usize| a[i].as_str().unwrap_or_default().to_string();
    let res = |r: Result<Value, String>| match r {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": e }),
    };
    let lite_of = |v: &Value| serde_json::from_value::<Lite>(v.clone()).expect("lite");
    match f {
        "parseGoals" => res(svc::parse_goals(&s(0), &s(1)).map(|g| json!({ "ok": true, "goals": g }))),
        "normalizeAchievementName" => json!(svc::normalize_achievement_name(a[0].as_str())),
        "normalizeBadgeEmoji" => {
            let custom = a[1].as_bool().unwrap_or(false);
            res(svc::normalize_badge_emoji(&s(0), |_| custom, a[2].as_bool().unwrap_or(false)).map(|v| json!({ "ok": true, "value": v })))
        }
        "parseDeadline" => {
            let now = chrono::DateTime::parse_from_rfc3339(&s(2)).map(|d| d.to_utc()).expect("now");
            res(svc::parse_deadline(&s(0), &s(1), &now, a[3].as_bool().unwrap_or(false))
                .map(|v| json!({ "ok": true, "value": v.map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)) })))
        }
        "normalizeTiers" => svc::tiers_json(&svc::normalize_tiers(&a[0])),
        "validateTiers" => res(svc::validate_tiers(&a[0], &s(1)).map(|()| json!({ "ok": true }))),
        "evaluateProgress" => {
            let t = svc::normalize_tiers(&a[0]);
            let r = &a[1];
            let e = svc::evaluate_progress(&t, r["progress"].as_i64().unwrap_or(0), r["tier"].as_i64().unwrap_or(0), r["almostNotified"].as_bool().unwrap_or(false));
            json!({ "advanceTo": e.advance_to, "almost": e.almost.map(|(i, g)| json!({ "tierIndex": i, "goal": g })) })
        }
        "formatRewards" => {
            let rewards = svc::normalize_rewards(&a[0]);
            let roles: Option<HashSet<String>> = a[1].as_array().map(|r| r.iter().filter_map(|x| x.as_str().map(String::from)).collect());
            let (emoji, name) = if a[2].is_null() { ("🪙".to_string(), "coins".to_string()) } else { (a[2]["currencyEmoji"].as_str().unwrap_or("").into(), a[2]["currencyName"].as_str().unwrap_or("").into()) };
            json!(svc::format_rewards_with(&rewards, roles.as_ref(), &emoji, &name))
        }
        "formatMemberEntry" => {
            let v = &a[1];
            let view = EntryView { progress: v["progress"].as_i64().unwrap_or(0), tier: v["tier"].as_i64().unwrap_or(0), pending: v["pending"].as_bool().unwrap_or(false), enabled: v["enabled"].as_bool() };
            json!(svc::format_member_entry(&lite_of(&a[0]), &view, a[2].as_i64().unwrap_or(0)))
        }
        "displayProgress" => {
            let row = a[1].as_object().map(|r| (r["progress"].as_i64().unwrap_or(0), r["tier"].as_i64().unwrap_or(0)));
            let ul = a[2].as_object().map(|u| (u.get("messageCount").and_then(Value::as_i64).unwrap_or(0), u.get("level").and_then(Value::as_i64).unwrap_or(0)));
            let boost = a[3]["premiumSinceTimestamp"].as_i64().is_some_and(|p| p != 0);
            let d = svc::display_progress(&lite_of(&a[0]), row, ul, boost);
            json!({ "progress": d.progress, "tier": d.tier, "pending": d.pending })
        }
        "goalText" => json!(svc::goal_text(&s(0), a[1].as_i64().unwrap_or(0))),
        "isBoostStart" => {
            let c = &a[0];
            json!(svc::is_boost_start(
                c["oldKnown"].as_bool().unwrap_or(false),
                c["oldMember"]["premiumSinceTimestamp"].as_i64(),
                c["newMember"]["premiumSinceTimestamp"].as_i64(),
                a[1].as_i64().unwrap_or(0)
            ))
        }
        "mergeRewards" => {
            let given: Vec<&str> = a[2].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            svc::rewards_json(&svc::merge_rewards(&svc::normalize_rewards(&a[0]), &svc::normalize_rewards(&a[1]), &given))
        }
        "announcement" => {
            let vars = svc::build_announcement_vars(&lite_of(&a[0]), a[1].as_i64().unwrap_or(0), a[2].as_i64().unwrap_or(0), &s(3), &s(4), &s(5));
            let obj: serde_json::Map<String, Value> = vars.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            json!({ "vars": obj, "text": render(&s(6), vars) })
        }
        other => panic!("no Rust twin for {other}"),
    }
}

/// Runs the JS twins of `cases` (written to a file: a Windows command line is limited to 32 KB).
fn js_pure(cases: &[Value], tag: &str) -> Value {
    let path = std::env::temp_dir().join(format!("feat-achievements-{tag}-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string(cases).unwrap_or_default()).expect("temp file");
    let out = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["pure-file", &path.to_string_lossy()]);
    let _ = std::fs::remove_file(&path);
    out
}

#[test]
fn every_pure_helper_agrees_with_the_js() {
    let mut cases: Vec<Value> = Vec::new();
    let mut add = |f: &str, args: Value| cases.push(json!({ "fn": f, "args": args }));
    for t in ["MESSAGES", "REACTIONS", "LEVEL", "BOOST", "PURCHASES", "NOPE"] {
        for g in ["100", "100,500,1k", "1.1k", "1.0005k", "2M", "0", "1 2 3 4 5", "", "  ", "abc", "1e3", "10 10", "5;10|20/30", "1001", "101", "1000001", "10000001", "007", "1.", ".5", "1,,2", "١٢"] {
            add("parseGoals", json!([g, t]));
        }
    }
    for n in [json!("  Chatter\n box "), json!(""), json!("x\u{0007}y"), json!("a\tb  c"), json!("x".repeat(51)), json!("é".repeat(50)), json!("😀".repeat(26)), json!(null)] {
        add("normalizeAchievementName", json!([n]));
    }
    for e in ["🏆", "🇺🇸", "🇺", "👨‍👩‍👧", "❤️", "❤", "#️⃣", "1️⃣", "abc", "12", "🏆 🏆", "", "none", "NONE", "<:x:123456789012345678>", "<a:x_y:123456789012345678>", "<:x:12>", "©️", "🏳️‍🌈", "👍🏽", "™"] {
        for (custom, clear) in [(false, false), (true, true)] {
            add("normalizeBadgeEmoji", json!([e, custom, clear]));
        }
    }
    for d in ["30d", "none", "off", "clear", "2000-01-01", "30s", "6y", "2040-01-01", "2026-12-31 23:59", "12-25", "14:30", "garbage", "in 2 days", "<t:1790000000:R>", "1h"] {
        for (tz, clear) in [("UTC", false), ("Europe/Paris", true), ("", false), ("Asia/Kolkata", false)] {
            add("parseDeadline", json!([d, tz, "2026-06-01T12:00:00.000Z", clear]));
        }
    }
    let tier_inputs = [
        json!([{ "goal": 10 }, { "goal": 50 }, { "goal": 100 }]),
        json!([{ "goal": 5, "rewards": [{ "type": "XP", "amount": 3 }, { "type": "ROLE_ADD", "roleId": R1 }, { "type": "COINS", "amount": 2.0 }] }]),
        json!([{ "goal": 1 }, { "goal": 2 }, { "goal": 3 }, { "goal": 4 }, { "goal": 5 }]),
        json!([{ "goal": 5 }, { "goal": 5 }]),
        json!([{ "goal": 1, "rewards": [{ "type": "XP", "amount": 0 }] }]),
        json!([{ "goal": 1, "rewards": null }]),
        json!([{ "goal": 1001 }]),
        json!([]),
        json!({}),
        json!([null, { "goal": 3 }]),
    ];
    for (i, ti) in tier_inputs.iter().enumerate() {
        add("normalizeTiers", json!([ti]));
        let trigger = ["MESSAGES", "LEVEL", "BOOST", "X"][i % 4];
        add("validateTiers", json!([ti, trigger]));
    }
    let three = json!([{ "goal": 10 }, { "goal": 50 }, { "goal": 100 }]);
    for (p, t, n) in [(9, 0, false), (9, 0, true), (10, 0, false), (120, 0, false), (45, 1, false), (44, 1, false), (120, 3, false), (120, 5, false), (0, 0, false), (90, 2, false), (99, 2, true)] {
        add("evaluateProgress", json!([three, { "progress": p, "tier": t, "almostNotified": n }]));
    }
    add("evaluateProgress", json!([[{ "goal": 9 }], { "progress": 8, "tier": 0, "almostNotified": false }]));
    add("evaluateProgress", json!([[{ "goal": 11 }], { "progress": 10, "tier": 0, "almostNotified": false }]));
    let rewards = json!([{ "type": "ROLE_ADD", "roleId": R1 }, { "type": "ROLE_REMOVE", "roleId": R2 }, { "type": "XP", "amount": 1500 }, { "type": "COINS", "amount": 20 }]);
    add("formatRewards", json!([rewards, [R1], { "currencyName": "gold", "currencyEmoji": "💰" }]));
    add("formatRewards", json!([rewards, null, null]));
    add("formatRewards", json!([[], null, null]));
    let entry_lites = [
        json!({ "id": "a", "name": "Chatterbox", "description": null, "emoji": null, "trigger": "MESSAGES", "trackPastProgress": true, "deadlineMs": null, "tiers": [{ "goal": 100 }, { "goal": 500 }, { "goal": 2500 }, { "goal": 10000 }] }),
        json!({ "id": "b", "name": "Climber", "description": "Reach levels", "emoji": "⭐", "trigger": "LEVEL", "trackPastProgress": true, "deadlineMs": 1_900_000_000_000_i64, "tiers": [{ "goal": 5 }] }),
        json!({ "id": "c", "name": "Shopper", "description": "d".repeat(150), "emoji": null, "trigger": "PURCHASES", "trackPastProgress": false, "deadlineMs": 1_000, "tiers": [{ "goal": 1 }, { "goal": 3 }] }),
        json!({ "id": "d", "name": "Booster", "description": "", "emoji": null, "trigger": "BOOST", "trackPastProgress": true, "deadlineMs": null, "tiers": [{ "goal": 1 }] }),
    ];
    for l in &entry_lites {
        for (p, t, pending, enabled) in [(0, 0, false, json!(null)), (1, 0, true, json!(true)), (7, 0, false, json!(false)), (130, 1, false, json!(null)), (12345, 4, false, json!(true)), (100, 1, true, json!(null)), (2, 1, false, json!(null))] {
            add("formatMemberEntry", json!([l, { "progress": p, "tier": t, "pending": pending, "enabled": enabled }, 1_800_000_000_000_i64]));
        }
        for (row, ul, member) in [
            (json!(null), json!({ "messageCount": 120, "level": 3 }), json!(null)),
            (json!(null), json!(null), json!({ "premiumSinceTimestamp": 5 })),
            (json!({ "progress": 7, "tier": 9 }), json!(null), json!(null)),
            (json!(null), json!({ "messageCount": 0, "level": 12 }), json!(null)),
        ] {
            add("displayProgress", json!([l, row, ul, member]));
        }
        for (ti, pr) in [(0, 1), (2, 1234), (5, 0), (-1, 3)] {
            add("announcement", json!([l, ti, pr, "70001", "Ann*_~", "+100 XP", "{player} {player.name} {achievement} {tier} {tier.emoji} {goal} {progress} {rewards} {achievement.description}"]));
        }
    }
    for t in ["MESSAGES", "REACTIONS", "LEVEL", "BOOST", "PURCHASES"] {
        for g in [1, 2, 1000, 1_000_000] {
            add("goalText", json!([t, g]));
        }
    }
    let now = 1_700_000_000_000_i64;
    for (known, old, new) in [(true, json!(null), json!(now)), (true, json!(now - 9000), json!(now)), (false, json!(null), json!(now - 30_000)), (false, json!(null), json!(now - 300_000)), (true, json!(5), json!(null)), (false, json!(null), json!(now + 60_000)), (true, json!(0), json!(now))] {
        add("isBoostStart", json!([{ "oldKnown": known, "oldMember": { "premiumSinceTimestamp": old }, "newMember": { "premiumSinceTimestamp": new } }, now]));
    }
    add("mergeRewards", json!([[{ "type": "XP", "amount": 10 }, { "type": "ROLE_ADD", "roleId": R1 }], [{ "type": "XP", "amount": 20 }], ["XP"]]));
    add("mergeRewards", json!([[{ "type": "ROLE_ADD", "roleId": R1 }], [{ "type": "ROLE_REMOVE", "roleId": R1 }], ["ROLE_REMOVE"]]));

    let js = js_pure(&cases, "ach");
    let js = js.as_array().cloned().unwrap_or_else(|| panic!("the JS answered {js}"));
    assert_eq!(js.len(), cases.len());
    for (case, expected) in cases.iter().zip(js) {
        let got = rust_pure(case["fn"].as_str().unwrap_or(""), &case["args"]);
        assert_eq!(got, expected, "{case}");
    }
}

// ── Announcements (offline, with a cached guild) ──────────────────────────────

const AG: &str = "960000000000000950";
const ACH_CH: &str = "810000000000000950";
const ACH_CFG: &str = "810000000000000951";
const ACH_LOCKED: &str = "810000000000000952";

fn announce_guild() -> FakeGuild {
    use serenity::all::Permissions as P;
    FakeGuild::new(AG, "600000000000000099")
        .role("820000000000000951", 1, P::VIEW_CHANNEL | P::SEND_MESSAGES | P::EMBED_LINKS)
        .member(TEST_BOT_ID, &["820000000000000951"])
        .member("600000000000000951", &[])
        .channel(ACH_CH, 0, "general", &[])
        .channel(ACH_CFG, 0, "achievements", &[])
        .channel(ACH_LOCKED, 0, "locked", &[(AG, false, P::empty(), P::SEND_MESSAGES)])
}

fn fake_member(id: &str, guild: &str) -> serenity::all::Member {
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null, "global_name": "Ann*" },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "guild_id": guild
    }))
    .expect("a member")
}

#[tokio::test]
async fn announcements_follow_the_mode_the_embed_switch_and_the_channel() {
    let t = TestApp::new().with_guilds([AG]).with_fake_guilds(&[announce_guild()]);
    let member = fake_member("600000000000000951", AG);
    let a = lite(json!({ "tiers": [{ "goal": 3 }, { "goal": 5 }] }));
    let origin = |s: Settings, ch: &str| Origin { guild_id: AG.into(), settings: Some(s), user_id: member.user.id.to_string(), member: Some(member.clone()), channel: Some(ch.parse::<u64>().map(serenity::all::ChannelId::new).expect("id")) };
    let set = |p: SettingsPatch| async { t.settings().update_settings(AG, p).await.expect("memory store") };

    let s = set(SettingsPatch::new().set("achievementAnnounceMode", "CURRENT")).await;
    let ann = svc::plan_announcement(&t, &origin(s.clone(), ACH_CH), &member, &a, 0, 3, "+50 XP").expect("announced");
    assert_eq!(ann.target, svc::AnnounceTarget::Channel(serenity::all::ChannelId::new(ACH_CH.parse().unwrap_or(1))));
    assert_eq!(ann.content.as_deref(), Some("🏆 <@600000000000000951> unlocked **Chatterbox (Bronze)**!"));
    assert_eq!(ann.ping_users, vec!["600000000000000951".to_string()]);
    // CURRENT falls back to the configured channel when the source is unusable.
    let s = set(SettingsPatch::new().set("achievementChannelId", ACH_CFG)).await;
    let ann = svc::plan_announcement(&t, &origin(s.clone(), ACH_LOCKED), &member, &a, 0, 3, "").expect("announced");
    assert_eq!(ann.target, svc::AnnounceTarget::Channel(serenity::all::ChannelId::new(ACH_CFG.parse().unwrap_or(1))));
    // CHANNEL prefers the configured channel.
    let s = set(SettingsPatch::new().set("achievementAnnounceMode", "CHANNEL")).await;
    let ann = svc::plan_announcement(&t, &origin(s, ACH_CH), &member, &a, 1, 5, "").expect("announced");
    assert_eq!(ann.target, svc::AnnounceTarget::Channel(serenity::all::ChannelId::new(ACH_CFG.parse().unwrap_or(1))));
    assert_eq!(ann.content.as_deref(), Some("🏆 <@600000000000000951> unlocked **Chatterbox (Silver)**!"));
    // Embed mode: the text is the embed description and a template that pings puts the mention in the content.
    let s = set(SettingsPatch::new().set("achievementUseEmbed", true).set("achievementMessage", "GG {player}: {achievement} ({rewards})").set("embedColor", 0x123456)).await;
    let ann = svc::plan_announcement(&t, &origin(s.clone(), ACH_CH), &member, &a, 0, 3, "+50 XP").expect("announced");
    assert_eq!(ann.content.as_deref(), Some("<@600000000000000951>"));
    assert_eq!(ann.embeds[0]["description"], "GG <@600000000000000951>: Chatterbox (Bronze) (+50 XP)");
    assert_eq!(ann.embeds[0]["color"], 0x123456);
    let s = set(SettingsPatch::new().set("achievementMessage", "Well done, {player.name}!")).await;
    let ann = svc::plan_announcement(&t, &origin(s, ACH_CH), &member, &a, 0, 3, "").expect("announced");
    assert_eq!(ann.content, None, "no ping without a mention");
    // DM mode: the footer names the server; nothing pings.
    let s = set(SettingsPatch::new().set("achievementAnnounceMode", "DM").set("achievementUseEmbed", false).set("achievementMessage", serde_json::Value::Null)).await;
    let ann = svc::plan_announcement(&t, &origin(s, ACH_CH), &member, &a, 0, 3, "").expect("announced");
    assert_eq!(ann.target, svc::AnnounceTarget::Dm(member.user.id));
    assert_eq!(ann.content.as_deref(), Some("🏆 <@600000000000000951> unlocked **Chatterbox (Bronze)**!
-# Sent from **Test Guild**"));
    assert!(ann.ping_users.is_empty());
    // OFF, or a text that renders empty: nothing.
    let s = set(SettingsPatch::new().set("achievementAnnounceMode", "OFF")).await;
    assert!(svc::plan_announcement(&t, &origin(s, ACH_CH), &member, &a, 0, 3, "").is_none());
    let s = set(SettingsPatch::new().set("achievementAnnounceMode", "CURRENT").set("achievementMessage", "{rewards}")).await;
    assert!(svc::plan_announcement(&t, &origin(s.clone(), ACH_CH), &member, &a, 0, 3, "").is_none(), "never a bare ping");
    // No usable channel: no guessing at a public channel.
    let s = set(SettingsPatch::new().set("achievementMessage", serde_json::Value::Null).set("achievementChannelId", serde_json::Value::Null)).await;
    assert!(svc::plan_announcement(&t, &origin(s, ACH_LOCKED), &member, &a, 0, 3, "").is_none());

    let ctx = bot_core::adapters::render_context(&t, Some(serenity::all::GuildId::new(AG.parse().unwrap_or(1))), &member.user, Some(&member), None);
    let dm = svc::almost_there_text(ctx, &lite(json!({ "tiers": [{ "goal": 100 }] })), 0, 100, 91, "600000000000000951", "Ann");
    assert!(dm.starts_with("🔥 Almost there! You're at **91/100** for **Chatterbox** in **"), "{dm}");
}

// ── Database ──────────────────────────────────────────────────────────────────

async fn db_app(guilds: &[&'static str]) -> Option<DbTestApp> {
    let url = database_url()?;
    let t = DbTestApp::new(&url).await.with_guilds(guilds.iter().copied());
    t.reset_guilds(guilds).await;
    svc::set_message_flush(&t, MessageFlush { delay_ms: 0, ..svc::MESSAGE_FLUSH });
    Some(t)
}

fn new_ach(name: &str, trigger: &str, tiers: Value, track: bool) -> svc::NewAchievement {
    svc::NewAchievement { name: name.into(), description: None, emoji: None, trigger: trigger.into(), track_past_progress: track, deadline: None, tiers }
}

async fn seed(t: &App, g: &str, a: svc::NewAchievement) -> bot_db::Achievement {
    let row = svc::create_achievement(t, g, &a).await.expect("db").expect("created");
    svc::refresh_achievement_cache(t, g).await;
    row
}

async fn progress_of(t: &App, aid: &str, user: &str) -> Option<bot_db::AchievementProgress> {
    sqlx::query_as(r#"SELECT * FROM "AchievementProgress" WHERE "achievementId" = $1 AND "userId" = $2"#).bind(aid).bind(user).fetch_optional(t.db()).await.expect("db")
}

fn origin(g: &str, user: &str, s: Option<Settings>) -> Origin {
    Origin { guild_id: g.into(), settings: s, user_id: user.into(), member: None, channel: None }
}

#[tokio::test]
async fn db_raw_sql_runs_against_the_schema() {
    let Some(url) = database_url() else { return };
    let pool = bot_core::testing::connect_test_pool(&url).await;
    let mut tx = pool.begin().await.expect("tx");
    for (name, sql) in svc::raw_sql() {
        let q = sqlx::query(sqlx::AssertSqlSafe(sql));
        let q = if name.ends_with("Update") {
            q.bind(1_i64).bind(0_i64).bind(svc::PROGRESS_MAX).bind("0").bind(vec!["sample".to_string()])
        } else {
            q.bind("0").bind("0").bind(svc::PROGRESS_MAX).bind(Vec::<String>::new()).bind(Vec::<String>::new()).bind(Vec::<i32>::new())
        };
        let rows = q.fetch_all(&mut *tx).await.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(rows.is_empty(), "{name}");
    }
    tx.rollback().await.expect("rollback");
}

#[tokio::test]
async fn db_messages_count_atomically_unlock_each_tier_once_and_pay() {
    const G: &str = "960000000000000001";
    let Some(t) = db_app(&[G]).await else { return };
    let (xp, coins) = providers(&t);
    let a = seed(
        &t,
        G,
        new_ach(
            "Chatterbox",
            "MESSAGES",
            json!([{ "goal": 3, "rewards": [{ "type": "ROLE_ADD", "roleId": R1 }, { "type": "XP", "amount": 50 }, { "type": "COINS", "amount": 7 }] }, { "goal": 5, "rewards": [{ "type": "XP", "amount": 25 }] }]),
            true,
        ),
    )
    .await;
    let s = t.settings().get_settings_safe(G).await;
    for _ in 0..2 {
        svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    }
    assert_eq!(progress_of(&t, &a.id, "70001").await.map(|r| r.progress), Some(2));
    svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    svc::settle_background(&t).await;
    let row = progress_of(&t, &a.id, "70001").await.expect("row");
    assert_eq!((row.progress, row.tier, row.completed_at.is_none(), row.almost_notified), (3, 1, true, false));
    assert_eq!(amounts(&xp), vec![50]);
    assert_eq!(amounts(&coins), vec![7]);
    let first = xp.grants.lock().unwrap_or_else(|e| e.into_inner())[0].clone();
    assert_eq!((first.0.as_str(), first.2.as_str()), ("70001", "Achievement: Chatterbox (Bronze)"));
    for _ in 0..2 {
        svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    }
    svc::settle_background(&t).await;
    let row = progress_of(&t, &a.id, "70001").await.expect("row");
    assert_eq!((row.progress, row.tier), (5, 2));
    assert!(row.completed_at.is_some());
    assert_eq!(amounts(&xp), vec![50, 25]);
    // Progress keeps counting after the last tier and nothing is paid again.
    svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    svc::settle_background(&t).await;
    assert_eq!(progress_of(&t, &a.id, "70001").await.map(|r| r.progress), Some(6));
    assert_eq!(amounts(&xp), vec![50, 25]);
    t.reset_guilds(&[G]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_concurrent_first_events_create_one_row_and_claim_once() {
    const G: &str = "960000000000000002";
    let Some(t) = db_app(&[G]).await else { return };
    let (xp, _) = providers(&t);
    let a = seed(&t, G, new_ach("Chatterbox", "MESSAGES", json!([{ "goal": 5, "rewards": [{ "type": "XP", "amount": 10 }] }]), true)).await;
    let s = t.settings().get_settings_safe(G).await;
    let mut handles = Vec::new();
    for _ in 0..12 {
        let (app, s) = (t.app.clone(), s.clone());
        handles.push(tokio::spawn(async move { svc::on_message_create(&app, origin(G, "70002", s)).await }));
    }
    for h in handles {
        let _ = h.await;
    }
    svc::settle_background(&t).await;
    let rows: Vec<bot_db::AchievementProgress> = sqlx::query_as(r#"SELECT * FROM "AchievementProgress" WHERE "achievementId" = $1"#).bind(&a.id).fetch_all(t.db()).await.expect("db");
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].progress, rows[0].tier), (12, 1));
    assert_eq!(amounts(&xp), vec![10]);
    t.reset_guilds(&[G]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn db_the_claim_is_a_compare_and_set() {
    const G: &str = "960000000000000003";
    let Some(t) = db_app(&[G]).await else { return };
    let (xp, _) = providers(&t);
    let a = seed(&t, G, new_ach("Chatterbox", "MESSAGES", json!([{ "goal": 2, "rewards": [{ "type": "XP", "amount": 10 }] }]), true)).await;
    let lite = svc::to_lite(&a);
    let id = bot_db::new_cuid();
    sqlx::query(r#"INSERT INTO "AchievementProgress" ("id", "guildId", "achievementId", "userId", "progress", "updatedAt") VALUES ($1, $2, $3, '70003', 2, now() AT TIME ZONE 'UTC')"#)
        .bind(&id)
        .bind(G)
        .bind(&a.id)
        .execute(t.db())
        .await
        .expect("insert");
    let snapshot = svc::ProgressRow { id, achievement_id: a.id.clone(), progress: 2, tier: 0, almost_notified: false };
    let o = origin(G, "70003", t.settings().get_settings_safe(G).await);
    let (r1, r2, r3) = tokio::join!(svc::handle_row(&t, &o, &lite, &snapshot), svc::handle_row(&t, &o, &lite, &snapshot), svc::handle_row(&t, &o, &lite, &snapshot));
    assert!(r1.is_ok() && r2.is_ok() && r3.is_ok());
    svc::settle_background(&t).await;
    assert_eq!(amounts(&xp), vec![10]);
    assert_eq!(progress_of(&t, &a.id, "70003").await.map(|r| r.tier), Some(1));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_seeding_from_user_level_level_and_boost_catch_up() {
    const G: &str = "960000000000000004";
    let Some(t) = db_app(&[G]).await else { return };
    let past = seed(&t, G, new_ach("Past", "MESSAGES", json!([{ "goal": 100 }]), true)).await;
    let fresh = seed(&t, G, new_ach("Fresh", "MESSAGES", json!([{ "goal": 100 }]), false)).await;
    let climber = seed(&t, G, new_ach("Climber", "LEVEL", json!([{ "goal": 3 }, { "goal": 10 }]), true)).await;
    sqlx::query(r#"INSERT INTO "UserLevel" ("id", "guildId", "userId", "messageCount", "level", "updatedAt") VALUES ($1, $2, '70004', 120, 4, now() AT TIME ZONE 'UTC')"#)
        .bind(bot_db::new_cuid())
        .bind(G)
        .execute(t.db())
        .await
        .expect("insert");
    let s = t.settings().get_settings_safe(G).await;
    svc::on_message_create(&t, origin(G, "70004", s.clone())).await;
    svc::settle_background(&t).await;
    let a = progress_of(&t, &past.id, "70004").await.expect("row");
    let b = progress_of(&t, &fresh.id, "70004").await.expect("row");
    assert_eq!((a.progress, a.tier), (120, 1), "max, not plus");
    assert_eq!((b.progress, b.tier), (1, 0));
    let c = progress_of(&t, &climber.id, "70004").await.expect("caught up");
    assert_eq!((c.progress, c.tier), (4, 1));

    // LEVEL counts the absolute level and never goes down; garbage is ignored.
    svc::on_level_up(&t, G, s.clone(), "70004", None, 12, None).await;
    svc::settle_background(&t).await;
    assert_eq!(progress_of(&t, &climber.id, "70004").await.map(|r| (r.progress, r.tier)), Some((12, 2)));
    svc::on_level_up(&t, G, s.clone(), "70004", None, 3, None).await;
    svc::on_level_up(&t, G, s.clone(), "", None, 30, None).await;
    svc::on_level_up(&t, G, s.clone(), "70004", None, 0, None).await;
    assert_eq!(progress_of(&t, &climber.id, "70004").await.map(|r| r.progress), Some(12));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_reactions_count_once_and_fail_closed_purchases_are_bounded() {
    const G: &str = "960000000000000005";
    let Some(t) = db_app(&[G]).await else { return };
    let react = seed(&t, G, new_ach("Reactor", "REACTIONS", json!([{ "goal": 100 }]), true)).await;
    let shop = seed(&t, G, new_ach("Shopper", "PURCHASES", json!([{ "goal": 5000 }]), true)).await;
    let s = t.settings().get_settings_safe(G).await;
    svc::on_reaction_add(&t, G, s.clone(), "70005", "700000000000000001", "⭐", None).await;
    svc::on_reaction_add(&t, G, s.clone(), "70005", "700000000000000001", "⭐", None).await;
    svc::on_reaction_add(&t, G, s.clone(), "70005", "700000000000000001", "🔥", None).await;
    assert_eq!(progress_of(&t, &react.id, "70005").await.map(|r| r.progress), Some(2));
    assert!(t.cache().get(&svc::reaction_key(G, "70005", "700000000000000001", "⭐")).await.is_some());
    svc::on_purchase(&t, G, s.clone(), "70005", None, 3, None).await;
    svc::on_purchase(&t, G, s.clone(), "70005", None, 0, None).await;
    svc::on_purchase(&t, G, s.clone(), "70005", None, 99_999, None).await;
    assert_eq!(progress_of(&t, &shop.id, "70005").await.map(|r| r.progress), Some(3 + 1 + 1000));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn offline_reactions_fail_closed_when_the_cache_is_down() {
    let Some(url) = database_url() else { return };
    const G: &str = "960000000000000006";
    let t = DbTestApp::new(&url).await.with_guilds([G]);
    t.reset_guilds(&[G]).await;
    let react = seed(&t, G, new_ach("Reactor", "REACTIONS", json!([{ "goal": 100 }]), true)).await;
    if let Some(m) = t.cache().memory() {
        m.set_available(false);
    }
    let s = t.settings().get_settings_safe(G).await;
    svc::on_reaction_add(&t, G, s, "70006", "700000000000000001", "⭐", None).await;
    assert!(progress_of(&t, &react.id, "70006").await.is_none(), "an outage can never be farmed");
    if let Some(m) = t.cache().memory() {
        m.set_available(true);
    }
    t.reset_guilds(&[G]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_the_message_buffer_writes_one_update_per_member_and_flushes() {
    const G: &str = "960000000000000007";
    let Some(t) = db_app(&[G]).await else { return };
    svc::set_message_flush(&t, MessageFlush { delay_ms: 150, at: 5, max_members: 10_000 });
    let a1 = seed(&t, G, new_ach("A1", "MESSAGES", json!([{ "goal": 1000 }]), false)).await;
    let s = t.settings().get_settings_safe(G).await;
    for _ in 0..3 {
        svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    }
    svc::on_message_create(&t, origin(G, "70002", s.clone())).await;
    assert!(progress_of(&t, &a1.id, "70001").await.is_none(), "nothing is written while the messages wait");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    svc::flush_message_counts(&t, false).await;
    assert_eq!(progress_of(&t, &a1.id, "70001").await.map(|r| r.progress), Some(3));
    assert_eq!(progress_of(&t, &a1.id, "70002").await.map(|r| r.progress), Some(1));
    // At `at` waiting messages the member is written at once.
    for _ in 0..5 {
        svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
    assert_eq!(progress_of(&t, &a1.id, "70001").await.map(|r| r.progress), Some(8));
    // A new achievement: the waiting message counts for a1 only, later ones for both.
    svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    let a2 = seed(&t, G, new_ach("A2", "MESSAGES", json!([{ "goal": 1000 }]), false)).await;
    svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    svc::on_message_create(&t, origin(G, "70001", s.clone())).await;
    svc::flush_message_counts(&t, false).await;
    assert_eq!(progress_of(&t, &a1.id, "70001").await.map(|r| r.progress), Some(11));
    assert_eq!(progress_of(&t, &a2.id, "70001").await.map(|r| r.progress), Some(2));
    // A reset writes the member's waiting messages first.
    svc::on_message_create(&t, origin(G, "70004", s.clone())).await;
    svc::on_message_create(&t, origin(G, "70004", s.clone())).await;
    svc::reset_member_progress(&t, G, "70004", std::slice::from_ref(&a1.id)).await.expect("reset");
    assert_eq!(progress_of(&t, &a1.id, "70004").await.map(|r| (r.progress, r.tier)), Some((0, 0)));
    // The final flush: later messages are written at once.
    svc::on_message_create(&t, origin(G, "70005", s.clone())).await;
    svc::flush_message_counts(&t, true).await;
    svc::on_message_create(&t, origin(G, "70005", s.clone())).await;
    assert_eq!(progress_of(&t, &a1.id, "70005").await.map(|r| r.progress), Some(2));
    // The shutdown flush (registered at boot by `register`, order 10) writes what waits.
    t.install(registry());
    svc::reset_achievement_state(&t);
    svc::set_message_flush(&t, MessageFlush { delay_ms: 60_000, at: 50, max_members: 10_000 });
    svc::on_message_create(&t, origin(G, "70006", s.clone())).await;
    assert!(progress_of(&t, &a1.id, "70006").await.is_none());
    bot_core::client::run_shutdown_flushes(&t).await;
    assert_eq!(progress_of(&t, &a1.id, "70006").await.map(|r| r.progress), Some(1));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_reset_zeroes_progress_keeps_rows_and_is_not_reseeded() {
    const G: &str = "960000000000000008";
    let Some(t) = db_app(&[G]).await else { return };
    let a = seed(&t, G, new_ach("Past", "MESSAGES", json!([{ "goal": 100 }, { "goal": 200 }]), true)).await;
    let lvl = seed(&t, G, new_ach("Lvl", "LEVEL", json!([{ "goal": 2 }]), true)).await;
    sqlx::query(r#"INSERT INTO "UserLevel" ("id", "guildId", "userId", "messageCount", "level", "updatedAt") VALUES ($1, $2, '70008', 150, 5, now() AT TIME ZONE 'UTC')"#)
        .bind(bot_db::new_cuid())
        .bind(G)
        .execute(t.db())
        .await
        .expect("insert");
    let s = t.settings().get_settings_safe(G).await;
    svc::on_message_create(&t, origin(G, "70008", s.clone())).await;
    svc::settle_background(&t).await;
    assert_eq!(progress_of(&t, &a.id, "70008").await.map(|r| r.tier), Some(1));
    svc::reset_member_progress(&t, G, "70008", &[a.id.clone(), lvl.id.clone(), a.id.clone()]).await.expect("reset");
    let r = progress_of(&t, &a.id, "70008").await.expect("kept");
    assert_eq!((r.progress, r.tier, r.almost_notified, r.completed_at), (0, 0, false, None));
    svc::on_message_create(&t, origin(G, "70008", s.clone())).await;
    svc::settle_background(&t).await;
    assert_eq!(progress_of(&t, &a.id, "70008").await.map(|r| r.progress), Some(1), "not re-seeded from history");
    assert_eq!(progress_of(&t, &lvl.id, "70008").await.map(|r| r.progress), Some(0), "LEVEL waits for the next level-up");
    // A member with no row gets a zero row.
    svc::reset_member_progress(&t, G, "70009", std::slice::from_ref(&a.id)).await.expect("reset");
    assert_eq!(progress_of(&t, &a.id, "70009").await.map(|r| r.progress), Some(0));
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_index_create_resolve_search_view_stats_and_tier_changes() {
    const G: &str = "960000000000000009";
    let Some(t) = db_app(&[G]).await else { return };
    let a = seed(&t, G, new_ach("Chatterbox", "MESSAGES", json!([{ "goal": 10 }, { "goal": 50 }, { "goal": 100 }]), true)).await;
    let dup = svc::create_achievement(&t, G, &new_ach("CHATTERBOX", "LEVEL", json!([{ "goal": 1 }]), true)).await.expect("db");
    assert_eq!(dup.err(), Some(svc::WriteRefusal::NameTaken));
    assert_eq!(svc::resolve_achievement(t.db(), G, &a.id).await.expect("db").map(|r| r.name), Some("Chatterbox".into()));
    assert_eq!(svc::resolve_achievement(t.db(), G, " chatterBOX ").await.expect("db").map(|r| r.id), Some(a.id.clone()));
    assert!(svc::resolve_achievement(t.db(), G, "").await.expect("db").is_none());
    assert!(svc::name_taken(t.db(), G, "chatterbox", None).await.expect("db"));
    assert!(!svc::name_taken(t.db(), G, "chatterbox", Some(&a.id)).await.expect("db"));
    let found = svc::search_achievements(t.db(), G, "TTER").await;
    assert_eq!(found.iter().map(|r| (r.name.as_str(), r.trigger.as_str(), r.enabled)).collect::<Vec<_>>(), vec![("Chatterbox", "MESSAGES", true)]);

    // The index follows config writes and skips disabled and expired achievements.
    let idx = svc::get_achievement_index(&t, G).await.expect("index");
    assert_eq!(idx.get("MESSAGES").len(), 1);
    let past = chrono::Utc::now() - chrono::Duration::days(1);
    let ended = seed(&t, G, svc::NewAchievement { deadline: Some(past), ..new_ach("Ended", "MESSAGES", json!([{ "goal": 1 }]), true) }).await;
    assert_eq!(svc::active_achievements(svc::get_achievement_index(&t, G).await.expect("index").get("MESSAGES"), t.clock().now_ms()).len(), 1);
    svc::update_achievement(t.db(), G, &ended.id, &svc::AchievementPatch { enabled: Some(false), ..Default::default() }).await.expect("db").expect("updated");
    svc::refresh_achievement_cache(&t, G).await;
    assert_eq!(svc::get_achievement_index(&t, G).await.expect("index").get("MESSAGES").len(), 1);
    assert!(!svc::may_have_triggers(&t, G, &["LEVEL", "BOOST"]));
    assert!(svc::may_have_triggers(&t, G, &["MESSAGES"]));

    // Member view: progress rows and disabled achievements only with an unlock.
    let s = t.settings().get_settings_safe(G).await;
    svc::record_progress(&t, &origin(G, "70010", s.clone()), "MESSAGES", &[svc::to_lite(&a)], 55, 0).await.expect("recorded");
    svc::settle_background(&t).await;
    let view = svc::load_member_view(&t, G, "70010", false).await.expect("view");
    assert_eq!(view.len(), 1);
    assert_eq!((view[0].display.progress, view[0].display.tier), (55, 2));
    let (per_tier, total) = svc::get_tier_stats(t.db(), &a.id, 3).await.expect("stats");
    assert_eq!((per_tier, total), (vec![1, 1, 0], 1));

    // Tier changes: remove shifts tiers down and completes; add re-opens; a goal edit resets almost-there.
    let next = svc::normalize_tiers(&json!([{ "goal": 10 }, { "goal": 100 }]));
    svc::apply_tier_change(t.db(), &a.id, &next, svc::TierChange::Remove { index: 0 }).await.expect("tier change");
    let r = progress_of(&t, &a.id, "70010").await.expect("row");
    assert_eq!(r.tier, 1);
    let next = svc::normalize_tiers(&json!([{ "goal": 10 }]));
    svc::apply_tier_change(t.db(), &a.id, &next, svc::TierChange::Remove { index: 1 }).await.expect("tier change");
    let r = progress_of(&t, &a.id, "70010").await.expect("row");
    assert!(r.completed_at.is_some() && r.tier == 1, "{r:?}");
    let next = svc::normalize_tiers(&json!([{ "goal": 10 }, { "goal": 70 }]));
    svc::apply_tier_change(t.db(), &a.id, &next, svc::TierChange::Add).await.expect("tier change");
    assert!(progress_of(&t, &a.id, "70010").await.expect("row").completed_at.is_none());
    let stored = svc::resolve_achievement(t.db(), G, &a.id).await.expect("db").expect("row");
    assert_eq!(stored.tiers, json!([{ "name": "Bronze", "goal": 10, "rewards": [] }, { "name": "Silver", "goal": 70, "rewards": [] }]));
    assert_eq!(svc::delete_achievement(t.db(), G, &a.id).await.expect("db"), 1);
    assert!(progress_of(&t, &a.id, "70010").await.is_none(), "the cascade removes the progress");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_the_cap_limits_achievements() {
    const G: &str = "960000000000000010";
    let Some(t) = db_app(&[G]).await else { return };
    for i in 0..bot_config::limits::ACHIEVEMENTS {
        svc::create_achievement(&t, G, &new_ach(&format!("A{i}"), "MESSAGES", json!([{ "goal": 1 }]), true)).await.expect("db").expect("created");
    }
    let r = svc::create_achievement(&t, G, &new_ach("One more", "MESSAGES", json!([{ "goal": 1 }]), true)).await.expect("db");
    assert_eq!(r.err(), Some(svc::WriteRefusal::Limit));
    t.reset_guilds(&[G]).await;
}

// ── The commands through the router ───────────────────────────────────────────

fn admin(name: &str, g: &str) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user("600000000000000009").roles(&["820000000000000002"]).permissions(Permissions::MANAGE_GUILD)
}

fn last_desc(t: &DbTestApp) -> String {
    t.responses.calls().last().and_then(|c| c.embed_description().map(String::from)).unwrap_or_default()
}

fn last_title(t: &DbTestApp) -> String {
    t.responses.calls().last().and_then(|c| c.embed_title().map(String::from)).unwrap_or_default()
}

use bot_core::command::option_type as ot;
use bot_core::router::route_interaction;

#[tokio::test]
async fn db_achievement_config_create_preset_list_info_tiers_and_announce() {
    const G: &str = "960000000000000011";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("achievementsEnabled", true)).await.expect("settings");
    let run = |i: FakeInteraction| {
        t.responses.clear();
        route_interaction(&t, i.build())
    };

    run(admin("achievement-config", G).subcommand("preset").option("template", ot::STRING, json!("chatterbox"))).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert!(!t.responses.calls()[0].ephemeral(), "admin config replies are public");
    assert_eq!(last_title(&t), "✅  Achievement Added");
    assert!(last_desc(&t).starts_with("💬 **Chatterbox** · Messages sent · on\n*Send messages in the server*\n🥉 Bronze — 100 messages"), "{}", last_desc(&t));

    run(admin("achievement-config", G).subcommand("preset").option("template", ot::STRING, json!("chatterbox"))).await;
    assert_eq!(last_title(&t), "❌  Name Taken");
    assert_eq!(last_desc(&t), "An achievement named **Chatterbox** already exists. Pass `name:` to add it under another name.");

    run(admin("achievement-config", G).subcommand("create").option("name", ot::STRING, json!("Climber")).option("trigger", ot::STRING, json!("LEVEL")).option("goals", ot::STRING, json!("5,10")).option("xp", ot::INTEGER, json!(50))).await;
    assert_eq!(last_title(&t), "❌  Reward Unavailable");
    run(admin("achievement-config", G).subcommand("create").option("name", ot::STRING, json!("Climber")).option("trigger", ot::STRING, json!("LEVEL")).option("goals", ot::STRING, json!("5,10")).option("deadline", ot::STRING, json!("30d"))).await;
    assert_eq!(last_title(&t), "✅  Achievement Created");
    let d = last_desc(&t);
    assert!(d.contains("🥈 Silver — level 10") && d.contains("Deadline: <t:"), "{d}");
    run(admin("achievement-config", G).subcommand("create").option("name", ot::STRING, json!("Bad")).option("trigger", ot::STRING, json!("BOOST")).option("goals", ot::STRING, json!("500"))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("❌  Invalid Goals".into(), "Server boosts goals can be at most 100.".into()));

    run(admin("achievement-config", G).group("tier", "add").option("achievement", ot::STRING, json!("climber")).option("goal", ot::INTEGER, json!(8))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("❌  Invalid Goal".into(), "The new goal must be higher than the top tier's goal (10).".into()));
    run(admin("achievement-config", G).group("tier", "add").option("achievement", ot::STRING, json!("climber")).option("goal", ot::INTEGER, json!(20))).await;
    assert_eq!(last_title(&t), "✅  Tier Added");
    assert!(last_desc(&t).starts_with("🥇 **Gold** added to **Climber**."));
    run(admin("achievement-config", G).group("tier", "edit").option("achievement", ot::STRING, json!("climber")).option("tier", ot::INTEGER, json!(2)).option("goal", ot::INTEGER, json!(25))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("❌  Invalid Goal".into(), "Tier 2 must be above 5 and below 20.".into()));
    run(admin("achievement-config", G).group("tier", "edit").option("achievement", ot::STRING, json!("climber")).option("tier", ot::INTEGER, json!(2)).option("goal", ot::INTEGER, json!(8))).await;
    assert_eq!(last_title(&t), "✅  Tier Updated");
    assert!(last_desc(&t).contains("Members who already passed the new goal unlock it with their next activity."));
    run(admin("achievement-config", G).group("tier", "remove").option("achievement", ot::STRING, json!("climber")).option("tier", ot::INTEGER, json!(4))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("❌  Unknown Tier".into(), "**Climber** has only 3 tier(s).".into()));
    run(admin("achievement-config", G).group("tier", "remove").option("achievement", ot::STRING, json!("climber")).option("tier", ot::INTEGER, json!(1))).await;
    assert_eq!(last_title(&t), "✅  Tier Removed");

    run(admin("achievement-config", G).subcommand("disable").option("achievement", ot::STRING, json!("Climber"))).await;
    assert_eq!(last_title(&t), "✅  Achievement Disabled");
    assert!(last_desc(&t).contains("· off\n"));
    run(admin("achievement-config", G).subcommand("disable").option("achievement", ot::STRING, json!("Climber"))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("ℹ️  No Change".into(), "**Climber** is already off.".into()));
    run(admin("achievement-config", G).subcommand("edit").option("achievement", ot::STRING, json!("Climber"))).await;
    assert_eq!(last_title(&t), "❌  Nothing To Change");
    run(admin("achievement-config", G).subcommand("edit").option("achievement", ot::STRING, json!("Climber")).option("name", ot::STRING, json!("Ladder")).option("description", ot::STRING, json!("Go up"))).await;
    assert_eq!(last_title(&t), "✅  Achievement Updated");
    assert!(last_desc(&t).starts_with("• **Name:** Climber → Ladder\n• **Description:** none → Go up\n\n⭐ **Ladder**"), "{}", last_desc(&t));
    run(admin("achievement-config", G).subcommand("info").option("achievement", ot::STRING, json!("nope"))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("❌  Achievement Not Found".into(), "I couldn't find that achievement. Pick one from the list.".into()));
    run(admin("achievement-config", G).subcommand("info").option("achievement", ot::STRING, json!("ladder"))).await;
    let call = t.responses.calls().last().cloned().expect("a reply");
    assert_eq!(call.embed_title(), Some("ℹ️  ⭐ Ladder"));
    assert_eq!(call.body["embeds"][0]["fields"][0]["value"], "No rewards\nUnlocked by 0 member(s)");

    run(admin("achievement-config", G).subcommand("list")).await;
    let d = last_desc(&t);
    assert!(d.starts_with("2/50 achievements\n\n💬 **Chatterbox** · Messages sent · 100 → 500 → 2,500 → 10,000 · ✅ on"), "{d}");
    assert!(d.ends_with("Announcements: Current channel · Format: plain text"), "{d}");

    run(admin("achievement-config", G).group("announce", "mode").option("mode", ot::STRING, json!("CHANNEL"))).await;
    assert_eq!((last_title(&t), last_desc(&t)), ("❌  Pick a Channel".into(), "Custom channel mode needs a channel. Run this again with `channel:`.".into()));
    run(admin("achievement-config", G).group("announce", "mode").option("mode", ot::STRING, json!("DM")).option("embed", ot::BOOLEAN, json!(true))).await;
    assert_eq!(last_title(&t), "✅  Announcements Updated");
    assert!(last_desc(&t).contains("Members who block DMs from this server get nothing.\nUnlocks are sent as an embed."));
    let s = t.settings().get_settings(G).await.expect("settings");
    assert_eq!((s.achievement_announce_mode.as_str(), s.achievement_use_embed), ("DM", true));
    run(admin("achievement-config", G).group("announce", "message").option("text", ot::STRING, json!("GG {player} {nope}"))).await;
    let call = t.responses.calls().last().cloned().expect("a reply");
    assert_eq!(call.embed_title(), Some("✅  Announcement Message Updated"));
    assert_eq!(call.body["embeds"][0]["fields"][1]["value"], "`{nope}`");
    run(admin("achievement-config", G).group("announce", "message")).await;
    assert_eq!(last_desc(&t), format!("Using the default: {}", svc::DEFAULT_ACHIEVEMENT_MESSAGE));

    // The module is off: replies say so.
    t.settings().update_settings(G, SettingsPatch::new().set("achievementsEnabled", false)).await.expect("settings");
    run(admin("achievement-config", G).subcommand("enable").option("achievement", ot::STRING, json!("Ladder"))).await;
    assert!(last_desc(&t).ends_with(svc::MODULE_OFF_NOTE));

    // Autocomplete searches in the database.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::autocomplete("achievement-config").guild(G).user("600000000000000009").permissions(Permissions::MANAGE_GUILD).subcommand("info").focused("achievement", "chat").build()).await;
    let choices = t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0]["name"], "💬 Chatterbox · Messages sent");

    // /achievements: the member view.
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("achievements").guild(G).user("600000000000000009").build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].method, "reply", "the router refuses a gated command while the module is off");
    t.settings().update_settings(G, SettingsPatch::new().set("achievementsEnabled", true)).await.expect("settings");
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("achievements").guild(G).user("600000000000000009").build()).await;
    let call = t.responses.calls().into_iter().find(|c| c.embed_title().is_some()).expect("a page");
    assert!(call.embed_title().unwrap_or("").ends_with("'s achievements"));
    assert!(call.embed_description().unwrap_or("").starts_with("**0/6** tiers unlocked · **0/2** completed"), "{:?}", call.embed_description());
    t.reset_guilds(&[G]).await;
}

// ── Cross-language ────────────────────────────────────────────────────────────

#[tokio::test]
async fn js_and_rust_share_achievements_progress_and_the_index_cache() {
    let (Some(url), Some(redis)) = (database_url(), redis_url()) else { return };
    const G: &str = "960000000000000020";
    let t = DbTestApp::with_redis(&url, &redis).await.with_guilds([G]);
    t.reset_guilds(&[G]).await;
    t.cache().del_one(&svc::achievements_key(G)).await;
    svc::set_message_flush(&t, MessageFlush { delay_ms: 0, ..svc::MESSAGE_FLUSH });

    // JS writes: the row and the Redis copy of the index.
    let js = run_node(
        "rust/fixtures/gen/feat-achievements-js.mjs",
        &["ach-create", G, r#"{"name":"Talker","trigger":"MESSAGES","description":"Say things","emoji":"🗣️","tiers":[{"goal":3,"rewards":[{"type":"XP","amount":5}]},{"goal":10}],"deadline":"2030-01-01T00:00:00.000Z"}"#],
    );
    assert_eq!(js["ok"], true, "{js}");
    let cached = js["cached"].clone();
    let items = svc::parse_cached_index(&cached, G).expect("the JS cache value reads in Rust");
    assert_eq!(items.len(), 1);
    let back = serde_json::to_value(svc::CachedIndex { v: 1, guild_id: G.into(), items: items.clone() }).unwrap_or(Value::Null);
    assert_eq!(back, cached, "written back JSON-equal");
    assert!(svc::parse_cached_index(&cached, "960000000000000099").is_none(), "another guild's copy is a miss");
    // Rust reads the index from that Redis copy and records progress on the JS row.
    let idx = svc::get_achievement_index(&t, G).await.expect("index");
    assert_eq!(idx.get("MESSAGES").to_vec(), items);
    let s = t.settings().get_settings_safe(G).await;
    for _ in 0..3 {
        svc::on_message_create(&t, origin(G, "70020", s.clone())).await;
    }
    svc::settle_background(&t).await;
    let rows = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-progress", G, "70020"]);
    assert_eq!(rows[0]["progress"], 3, "{rows}");
    assert_eq!(rows[0]["tier"], 1);
    let ours: bot_db::AchievementProgress = progress_of(&t, &items[0].id, "70020").await.expect("row");
    assert_eq!(serde_json::to_value(&ours).unwrap_or(Value::Null), rows[0], "the row reads the same through Prisma and Rust");
    let view = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-view", G, "70020"]);
    assert_eq!(view["view"]["entries"][0]["display"], json!({ "progress": 3, "tier": 1, "pending": false }));

    // Rust writes: a new achievement and a refreshed Redis copy, read by a fresh JS process.
    let rust_row = seed(&t, G, new_ach("Rustacean", "REACTIONS", json!([{ "goal": 2, "rewards": [{ "type": "ROLE_ADD", "roleId": R1 }] }]), false)).await;
    let js_index = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-index", G]);
    assert_eq!(js_index["REACTIONS"][0], serde_json::to_value(svc::to_lite(&rust_row)).unwrap_or(Value::Null));
    assert_eq!(js_index["MESSAGES"][0]["id"], json!(items[0].id));
    let js_cache = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-cache", G]);
    assert_eq!(js_cache, t.cache().get(&svc::achievements_key(G)).await.unwrap_or(Value::Null));
    // JS records on the Rust row; Rust reads it.
    let recorded = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-record", G, "70021", "REACTIONS", "2", "0"]);
    assert_eq!(recorded[0]["progress"], 2, "{recorded}");
    assert_eq!(progress_of(&t, &rust_row.id, "70021").await.map(|r| (r.progress, r.tier)), Some((2, 1)), "JS claimed the tier");
    // The JS dedupe keys equal the Rust ones.
    seed(&t, G, new_ach("Boosty", "BOOST", json!([{ "goal": 3 }]), true)).await;
    let keys = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-keys", G, "70022"]);
    assert_eq!(keys[2], svc::boost_key(G, "70022", 1767225600123), "{keys}");
    assert_eq!(keys[0], svc::reaction_key(G, "70022", "700000000000000001", "⭐"));
    assert_eq!(keys[1], svc::reaction_key(G, "70022", "700000000000000002", "123456789012345678"));
    // A Rust reset reads in JS.
    svc::reset_member_progress(&t, G, "70020", std::slice::from_ref(&items[0].id)).await.expect("reset");
    let rows = run_node("rust/fixtures/gen/feat-achievements-js.mjs", &["ach-progress", G, "70020"]);
    assert_eq!((rows[0]["progress"].clone(), rows[0]["tier"].clone(), rows[0]["completedAt"].clone()), (json!(0), json!(0), Value::Null));
    t.cache().del_one(&svc::achievements_key(G)).await;
    t.reset_guilds(&[G]).await;
}

#[test]
fn offline_discord_handle_builds() {
    let _ = offline_discord_as(&[announce_guild().json()]);
}

// ── More database cases ───────────────────────────────────────────────────────

fn boosting_member(id: &str, guild: &str, since: &str) -> serenity::all::Member {
    serde_json::from_value(json!({
        "user": { "id": id, "username": format!("user{id}"), "discriminator": "0", "avatar": null },
        "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "guild_id": guild,
        "premium_since": since
    }))
    .expect("a boosting member")
}

#[tokio::test]
async fn db_boosts_count_once_per_boost_start_and_an_uncertain_one_fails_closed() {
    const G: &str = "960000000000000012";
    let Some(t) = db_app(&[G]).await else { return };
    let a = seed(&t, G, new_ach("Booster", "BOOST", json!([{ "goal": 5 }]), true)).await;
    let s = t.settings().get_settings_safe(G).await;
    let m = boosting_member("700000000000000031", G, "2026-01-01T00:00:00.123000+00:00");
    svc::on_member_update(&t, G, s.clone(), m.clone(), true).await;
    svc::on_member_update(&t, G, s.clone(), m.clone(), true).await;
    assert_eq!(progress_of(&t, &a.id, "700000000000000031").await.map(|r| r.progress), Some(1), "the same boost counts once");
    assert!(t.cache().get(&svc::boost_key(G, "700000000000000031", 1_767_225_600_123)).await.is_some());
    if let Some(mem) = t.cache().memory() {
        mem.set_available(false);
    }
    let m2 = boosting_member("700000000000000031", G, "2026-02-01T00:00:00.000000+00:00");
    svc::on_member_update(&t, G, s.clone(), m2.clone(), false).await;
    assert_eq!(progress_of(&t, &a.id, "700000000000000031").await.map(|r| r.progress), Some(1), "uncertain transition, Redis down: not counted");
    svc::on_member_update(&t, G, s.clone(), m2, true).await;
    assert_eq!(progress_of(&t, &a.id, "700000000000000031").await.map(|r| r.progress), Some(2), "a certain transition is counted");
    if let Some(mem) = t.cache().memory() {
        mem.set_available(true);
    }
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_almost_there_flips_once_in_dm_mode_and_a_refused_reward_does_not_stop_the_payout() {
    const G: &str = "960000000000000013";
    let Some(t) = db_app(&[G]).await else { return };
    let (xp, coins) = providers(&t);
    let a = seed(
        &t,
        G,
        new_ach("Chatty", "MESSAGES", json!([{ "goal": 10, "rewards": [{ "type": "XP", "amount": 5 }, { "type": "COINS", "amount": 3 }] }]), false),
    )
    .await;
    let lite = svc::to_lite(&a);
    // CURRENT mode: no almost-there.
    let s = t.settings().get_settings_safe(G).await;
    svc::record_progress(&t, &origin(G, "70030", s), "MESSAGES", std::slice::from_ref(&lite), 9, 0).await.expect("recorded");
    assert_eq!(progress_of(&t, &a.id, "70030").await.map(|r| r.almost_notified), Some(false));
    // DM mode: the flag flips once; a stale second evaluation changes nothing.
    let dm = t.settings().update_settings(G, SettingsPatch::new().set("achievementAnnounceMode", "DM")).await.expect("settings");
    svc::record_progress(&t, &origin(G, "70031", Some(dm.clone())), "MESSAGES", std::slice::from_ref(&lite), 9, 0).await.expect("recorded");
    let row = progress_of(&t, &a.id, "70031").await.expect("row");
    assert!(row.almost_notified);
    let stale = svc::ProgressRow { id: row.id.clone(), achievement_id: a.id.clone(), progress: 9, tier: 0, almost_notified: false };
    svc::handle_row(&t, &origin(G, "70031", Some(dm.clone())), &lite, &stale).await.expect("evaluated");
    assert!(progress_of(&t, &a.id, "70031").await.expect("row").almost_notified);
    svc::settle_background(&t).await;
    // The unlock resets the flag; the XP provider refuses, the coins are still paid.
    xp.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
    svc::record_progress(&t, &origin(G, "70031", Some(dm)), "MESSAGES", std::slice::from_ref(&lite), 1, 0).await.expect("recorded");
    svc::settle_background(&t).await;
    let row = progress_of(&t, &a.id, "70031").await.expect("row");
    assert_eq!((row.tier, row.almost_notified), (1, false));
    assert!(amounts(&xp).is_empty());
    assert_eq!(amounts(&coins), vec![3]);
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn offline_the_index_fails_closed_and_nothing_throws() {
    let t = TestApp::new();
    let g = "960000000000000099";
    let l = lite(json!({ "tiers": [{ "goal": 3 }] }));
    assert!(svc::get_achievement_index(&t, g).await.is_none());
    assert!(svc::record_progress(&t, &origin(g, "1", None), "MESSAGES", &[l], 1, 0).await.is_none());
    svc::on_message_create(&t, origin(g, "1", None)).await;
    assert!(svc::search_achievements(t.db(), g, "x").await.is_empty());
    assert!(svc::load_member_view(&t, g, "1", false).await.is_none());
}

async fn wait_for_collector(app: &App) {
    for _ in 0..400 {
        if !app.collectors().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no collector");
}

fn confirm_id(t: &DbTestApp) -> String {
    let prompt = t.responses.calls().into_iter().find(|c| c.body["components"][0]["components"][0]["custom_id"].is_string()).expect("a prompt");
    prompt.body["components"][0]["components"][0]["custom_id"].as_str().unwrap_or_default().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_delete_and_reset_ask_for_confirmation_and_post_publicly() {
    const G: &str = "960000000000000014";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    let a = seed(&t, G, new_ach("Doomed", "LEVEL", json!([{ "goal": 3 }]), true)).await;
    svc::record_progress(&t, &origin(G, "700000000000000041", None), "LEVEL", &[svc::to_lite(&a)], 0, 2).await.expect("recorded");
    let user = "600000000000000009";

    // reset (the module is off: the reply says so).
    let app = t.app.clone();
    let i = admin("achievement-config", G).subcommand("reset").option("member", ot::USER, json!("700000000000000041")).build();
    let task = tokio::spawn(async move { route_interaction(&app, i).await });
    wait_for_collector(&t).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].method, "deferReply");
    assert!(calls[0].ephemeral(), "the prompt is private");
    assert_eq!(calls[1].embed_title(), Some("⚠️  Reset achievement progress?"));
    assert_eq!(calls[1].embed_description(), Some("Reset <@700000000000000041>'s progress on **Doomed**? Roles, XP and coins already given are **kept**."));
    let yes = confirm_id(&t);
    route_interaction(&t, FakeInteraction::button(&yes).guild(G).user(user).message_id("1000000000000000100").build()).await;
    let _ = task.await;
    let done = t.responses.calls().last().cloned().expect("a follow-up");
    assert_eq!(done.method, "followUp");
    assert!(!done.ephemeral(), "the result is public");
    assert_eq!(done.embed_title(), Some("✅  Progress Reset"));
    let d = done.embed_description().unwrap_or_default().to_string();
    assert!(
        d.starts_with("<@700000000000000041>'s progress on **Doomed** was reset to zero. Rewards already given were kept.\nℹ️ Level achievements unlock again when the member next levels up."),
        "{d}"
    );
    assert!(d.ends_with(svc::MODULE_OFF_NOTE));
    assert_eq!(progress_of(&t, &a.id, "700000000000000041").await.map(|r| r.progress), Some(0));

    // delete.
    t.responses.clear();
    let app = t.app.clone();
    let i = admin("achievement-config", G).subcommand("delete").option("achievement", ot::STRING, json!("doomed")).build();
    let task = tokio::spawn(async move { route_interaction(&app, i).await });
    wait_for_collector(&t).await;
    assert_eq!(t.responses.calls()[1].embed_description(), Some("Delete **Doomed** and every member's progress on it? Roles, XP and coins already given are kept."));
    let yes = confirm_id(&t);
    route_interaction(&t, FakeInteraction::button(&yes).guild(G).user(user).message_id("1000000000000000100").build()).await;
    let _ = task.await;
    let done = t.responses.calls().last().cloned().expect("a follow-up");
    assert_eq!((done.method, done.embed_title()), ("followUp", Some("✅  Achievement Deleted")));
    assert!(done.embed_description().unwrap_or_default().starts_with("**Doomed** was deleted."));
    assert!(svc::resolve_achievement(t.db(), G, &a.id).await.expect("db").is_none());
    t.reset_guilds(&[G]).await;
}

// ── Review fixes ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_the_shutdown_flush_is_registered_at_boot_before_any_message() {
    // bot.js always runs `flushMessageCounts({ final: true })` at shutdown; the Rust flush must exist even when no
    // message arrived since boot. After it ran, later messages are written at once.
    const G: &str = "960000000000000031";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    svc::set_message_flush(&t, MessageFlush { delay_ms: 60_000, at: 50, max_members: 10_000 });
    let a = seed(&t, G, new_ach("Boot", "MESSAGES", json!([{ "goal": 1000 }]), false)).await;
    bot_core::client::run_shutdown_flushes(&t).await;
    let s = t.settings().get_settings_safe(G).await;
    svc::on_message_create(&t, origin(G, "70031", s)).await;
    assert_eq!(progress_of(&t, &a.id, "70031").await.map(|r| r.progress), Some(1), "the final flush stopped the buffer");
    t.reset_guilds(&[G]).await;
}

#[tokio::test]
async fn db_achievements_admin_hint_counts_administrator() {
    // `interaction.memberPermissions.has(ManageGuild)`: discord.js's `has` checks Administrator first.
    const G: &str = "960000000000000032";
    let Some(t) = db_app(&[G]).await else { return };
    t.install(registry());
    t.settings().update_settings(G, SettingsPatch::new().set("achievementsEnabled", true)).await.expect("settings");
    t.responses.clear();
    route_interaction(&t, FakeInteraction::slash("achievements").guild(G).user("600000000000000032").permissions(Permissions::ADMINISTRATOR).build()).await;
    let call = t.responses.calls().into_iter().find(|c| c.embed_title().is_some()).expect("a reply");
    assert_eq!(
        call.embed_description(),
        Some("This server has no achievements yet. Create one with `/achievement-config preset` or `/achievement-config create`.")
    );
    t.reset_guilds(&[G]).await;
}
