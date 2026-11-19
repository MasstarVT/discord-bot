//! Drift tests of collections/automations.rs against feat-automations' automationService tables, the Rust side of
//! test/dashboard-automations.test.js "drift: triggers, condition and action tables match automationService" and
//! "drift: constants, texts, presets and the problem key match automationService".

use dash_collections::collections::automations::{self as a, automation_limits, trigger_sets};
use feat_automations::automation_service as svc;

fn sorted(v: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = v.iter().map(|s| (*s).to_string()).collect();
    out.sort();
    out
}

#[test]
fn trigger_condition_and_action_tables_match_the_service() {
    let keys: Vec<&str> = a::TRIGGER_RULES.iter().map(|(k, _)| *k).collect();
    let svc_keys: Vec<&str> = svc::TRIGGERS.iter().map(|d| d.key).collect();
    assert_eq!(keys, svc_keys);
    for d in &svc::TRIGGERS {
        let mine = a::trigger_rule(d.key).unwrap();
        assert_eq!(mine.label, d.label, "{}", d.key);
        assert_eq!(mine.channel, d.channel, "{}", d.key);
    }
    assert_eq!(sorted(&trigger_sets::ALL), sorted(svc::T_ALL), "T_ALL");
    assert_eq!(sorted(&trigger_sets::CHANNEL), sorted(svc::T_CHANNEL), "T_CHANNEL");
    assert_eq!(sorted(&trigger_sets::CONTENT), sorted(svc::T_CONTENT), "T_CONTENT");
    assert_eq!(sorted(&trigger_sets::LIVE), sorted(svc::T_LIVE), "T_LIVE");
    assert_eq!(sorted(&trigger_sets::COUNTS), sorted(svc::T_COUNTS), "T_COUNTS");
    assert_eq!(sorted(&trigger_sets::THREADY), sorted(svc::T_THREADY), "T_THREADY");

    let keys: Vec<&str> = a::CONDITION_RULES.iter().map(|(k, _)| *k).collect();
    let svc_keys: Vec<&str> = svc::CONDITION_TYPES.iter().map(|d| d.key).collect();
    assert_eq!(keys, svc_keys);
    for d in &svc::CONDITION_TYPES {
        let mine = &a::CONDITION_RULES.iter().find(|(k, _)| *k == d.key).unwrap().1;
        assert_eq!(mine.label, d.label, "{}", d.key);
        assert_eq!(sorted(mine.triggers), sorted(d.triggers), "{}", d.key);
        assert_eq!(mine.no_params, d.no_params, "{}", d.key);
        if let Some(list) = d.list {
            assert_eq!(mine.field, Some(if list == "userIds" { "users" } else { list }), "{}", d.key);
        }
    }

    let keys: Vec<&str> = a::ACTION_RULES.iter().map(|(k, _)| *k).collect();
    let svc_keys: Vec<&str> = svc::ACTION_TYPES.iter().map(|d| d.key).collect();
    assert_eq!(keys, svc_keys);
    for d in &svc::ACTION_TYPES {
        let mine = &a::ACTION_RULES.iter().find(|(k, _)| *k == d.key).unwrap().1;
        assert_eq!(mine.label, d.label, "{}", d.key);
        assert_eq!(sorted(mine.triggers), sorted(d.triggers), "{}", d.key);
        assert_eq!(mine.button, d.button, "{}", d.key);
        assert_eq!(mine.thread, d.thread, "{}", d.key);
        assert_eq!(mine.reward, d.reward, "{}", d.key);
    }
}

#[test]
fn constants_texts_presets_and_the_problem_key_match_the_service() {
    assert_eq!(automation_limits::NAME, svc::AUTOMATION_NAME_MAX);
    assert_eq!(automation_limits::COOLDOWN_SECONDS, svc::MAX_COOLDOWN_SECONDS);
    assert_eq!(automation_limits::REWARD_AMOUNT, svc::MAX_REWARD_AMOUNT);
    assert_eq!(automation_limits::BUTTON_INDEX, svc::MAX_BUTTON_INDEX);
    assert_eq!(a::THREAD_ARCHIVE_MINUTES, svc::THREAD_ARCHIVE_MINUTES);
    assert_eq!(a::DEFAULT_THREAD_NAME, svc::DEFAULT_THREAD_NAME);
    assert_eq!(a::WELCOMER_DEFAULT_TEXT, svc::WELCOMER_DEFAULT_TEXT);
    assert_eq!(a::NEEDS_ACTIONS, svc::error_texts::NEEDS_ACTIONS);
    assert_eq!(a::BUTTONS_USED, svc::error_texts::BUTTONS_USED);
    assert_eq!(a::PRESETS, svc::PRESETS);
    let option = |f: &str| match f {
        "presetChannelId" => "channel",
        "presetMessage" => "message",
        "presetRoleId" | "presetStaffRoleId" => "role",
        "presetEmoji" => "emoji",
        "presetThreshold" => "threshold",
        "presetUserId" => "user",
        other => panic!("unknown preset field {other}"),
    };
    for p in svc::PRESETS {
        let fields = a::PRESET_FIELDS.iter().find(|(k, _)| *k == p).unwrap().1;
        let mine: Vec<&str> = fields.iter().map(|f| option(f)).collect();
        assert_eq!(sorted(&mine), sorted(svc::preset_options(p)), "{p}");
    }
    assert_eq!(a::problem_key("abc"), svc::problem_key("abc"));
    assert_eq!(a::problem_key("abc"), "automation:problem:abc");
}
