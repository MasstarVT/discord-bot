//! Port of `test/automod.test.js` (and the automod parts of `test/sec-forwards.test.js`), plus the golden, sweep,
//! cache-key and cross-language checks.
//!
//! Offline: the config model, matchers, detectors, scope, outcome resolution, the plugin declaration, the `/automod`
//! payload, the action pipeline against a fake guild and a fake REST, the command against the in-memory settings
//! store, and a sweep of every pure function against the JS. With `TEST_DATABASE_URL` / `TEST_REDIS_URL`: the config
//! round trip in both directions and the anti-spam window shared with the JS.

use std::sync::{Arc, Mutex};

use bot_core::hooks::AutomodAction;
use bot_core::prelude::*;
use bot_core::testing::{FakeGuild, FakeInteraction, FakeRest, TEST_BOT_ID, TestApp, fake_message, run_node};
use feat_automod::automod_service::{self as svc, AutomodInput, AutomodMessage, AutomodSink, ChannelScope, GuildInvites, RuleMatch};
use feat_automod::commands::automod as cmd;
use serde_json::{Value, json};
use serenity::all::{GuildId, Permissions};

const G: &str = "955006000000000001";
const UID: &str = "955006000000000002";
const OWNER: &str = "955006000000000003";
const CH: &str = "955006000000000005";
const LOGCH: &str = "955006000000000006";
const BOT_ROLE: &str = "955006000000000020";
const MOD_ROLE: &str = "955006000000000021";

fn registry() -> Registry {
    let mut reg = Registry::new();
    feat_automod::register(&mut reg);
    reg
}

fn cfg(v: Value) -> svc::AutomodConfig {
    svc::normalize_config(Some(&v))
}

fn rm(rule: &str, action: &str) -> RuleMatch {
    RuleMatch::new(rule, action)
}

fn msg(content: &str) -> AutomodMessage {
    AutomodMessage { id: "1".into(), guild_id: G.into(), channel_id: CH.into(), author_id: UID.into(), content: content.into(), ..Default::default() }
}

// ── Config model ──────────────────────────────────────────────────────────────

#[test]
fn normalize_null_is_a_full_all_off_v2_config() {
    let c = svc::normalize_config(None);
    assert_eq!(c.version, 2);
    assert_eq!(c.rules.keys().cloned().collect::<Vec<_>>(), svc::RULE_KEYS.to_vec());
    for k in svc::RULE_KEYS {
        let r = c.rule(k);
        assert_eq!(r.action, "off");
        assert_eq!((r.role_mode.as_str(), r.channel_mode.as_str(), r.timeout_seconds), ("except", "except", 600));
        assert_eq!(r.threshold.is_some(), svc::rule_meta(k).and_then(|m| m.threshold).is_some(), "{k}");
    }
    assert_eq!(c.allowed_domains, svc::DEFAULT_ALLOWED_DOMAINS.to_vec());
    assert_eq!(c.rule("caps").threshold, Some(70));
    assert_eq!(c.rule("zalgo").threshold, Some(4));
}

#[test]
fn normalize_never_fails_is_idempotent_and_cleans() {
    let fixtures = [
        json!("x"),
        json!(42),
        json!([]),
        json!({ "rules": "x" }),
        json!({ "version": 2, "rules": { "caps": { "action": "nuke", "threshold": "x", "roleMode": 1 } }, "badWords": null }),
        json!({ "version": 2, "badWords": [69, "", " Foo ", "foo", null, {}] }),
        json!({ "version": 2, "rules": { "spam": { "action": "ban", "threshold": 999, "timeoutSeconds": 5, "roleIds": ["1", "123456789012345678", 7] } } }),
        json!({ "version": 2, "allowedDomains": [], "blockedDomains": ["Evil.COM", "", "node", 7, "evil.com"], "exemptRoleIds": "x" }),
        json!({ "filterBadWords": true, "badWords": ["a"] }),
    ];
    for f in &fixtures {
        let c = svc::normalize_config(Some(f));
        assert_eq!(c.version, 2);
        assert_eq!(svc::normalize_config(Some(&c.to_json())), c, "idempotent for {f}");
    }
    assert_eq!(cfg(fixtures[5].clone()).bad_words, vec!["69", "foo"]);
    let c = cfg(fixtures[4].clone());
    assert_eq!((c.rule("caps").action.as_str(), c.rule("caps").threshold), ("off", Some(70)));
    let s = cfg(fixtures[6].clone());
    assert_eq!(s.rule("spam").threshold, Some(30), "clamped to the maximum");
    assert_eq!(s.rule("spam").timeout_seconds, 60, "clamped to the minimum");
    assert_eq!(s.rule("spam").role_ids, vec!["123456789012345678"]);
    let d = cfg(fixtures[7].clone());
    assert!(d.allowed_domains.is_empty(), "an explicit [] stays empty");
    assert_eq!(d.blocked_domains, vec!["evil.com"]);
    let hostile = cfg(json!({ "version": 2, "rules": { "caps": { "action": {} } } }));
    assert_eq!(hostile.rule("caps").action, "off");
}

#[test]
fn upgrade_v1_maps_only_the_keys_that_are_present() {
    let u = svc::upgrade_v1(Some(&json!({ "filterInvites": true, "automodAction": "BAN" })));
    assert_eq!(u.rule("invites").action, "ban");
    assert_eq!(u.rule("mentions").action, "off");
    assert_eq!(u.allowed_domains, svc::DEFAULT_ALLOWED_DOMAINS.to_vec());
    let action = |raw: Value| svc::upgrade_v1(Some(&raw)).rule("links").action;
    assert_eq!(action(json!({ "filterLinks": true, "automodAction": "NONE" })), "delete");
    assert_eq!(action(json!({ "filterLinks": true, "automodAction": "MUTE" })), "timeout");
    assert_eq!(action(json!({ "filterLinks": true })), "delete_warn");
    assert_eq!(action(json!({ "filterLinks": true, "automodAction": "KICK" })), "kick");
    assert_eq!(action(json!({ "filterLinks": true, "automodAction": "weird" })), "delete_warn");
    let u = svc::upgrade_v1(Some(&json!({ "massMentionThreshold": 8, "duplicateThreshold": 3, "spamThreshold": 1 })));
    assert_eq!((u.rule("mentions").threshold, u.rule("repeatedText").threshold, u.rule("spam").threshold), (Some(8), Some(3), Some(3)));
    assert_eq!(svc::upgrade_v1(Some(&json!({ "duplicateThreshold": 1 }))).rule("repeatedText").action, "off");
    let l = svc::upgrade_v1(Some(&json!({ "filterBadWords": true, "badWords": ["Foo", null], "blockedDomains": ["evil.com"], "exemptChannelIds": [CH] })));
    assert_eq!((l.bad_words.clone(), l.blocked_domains.clone(), l.exempt_channel_ids.clone()), (vec!["foo".into()], vec!["evil.com".into()], vec![CH.to_string()]));
    assert!(svc::is_legacy_config(Some(&json!({}))));
    assert!(!svc::is_legacy_config(Some(&json!({ "version": 2 }))));
    assert!(!svc::is_legacy_config(Some(&json!([]))));
    assert_eq!(svc::normalize_config(Some(&json!({}))), svc::upgrade_v1(Some(&json!({}))));
}

// ── Detectors ─────────────────────────────────────────────────────────────────

#[test]
fn bad_words_match_whole_words_only() {
    let config = cfg(json!({ "version": 2, "rules": { "badWords": { "action": "delete" } }, "badWords": ["ass", "cunt", "bad phrase"] }));
    let m = svc::build_matchers(&config);
    for ok in ["I passed the class", "my assistant", "Scunthorpe united", "bassist", "cunts_", ""] {
        assert_eq!(svc::check_bad_words(ok, &config, &m), None, "{ok}");
    }
    for hit in ["you ass!", "ASS", "a\u{200B}ss", "||ass||", "\u{1D41A}\u{1D42C}\u{1D42C}", "this is a bad   phrase", "a**s**s", "*ass*", "no, ass."] {
        let r = svc::check_bad_words(hit, &config, &m).unwrap_or_else(|| panic!("should match {hit:?}"));
        assert_eq!((r.rule.as_str(), r.action.as_str()), ("badWords", "delete"));
    }
    assert!(svc::check_bad_words("you ass!", &config, &m).and_then(|r| r.detail).is_some_and(|d| d.starts_with("word \"ass\"")));
}

#[test]
fn patterns_run_safely_and_unsafe_ones_are_dropped() {
    let config = cfg(json!({ "version": 2, "badWordPatterns": ["fr[e3]{2}\\s*n[i1]tro", "(a+)+$", "(oops", "x".repeat(201)] }));
    assert_eq!(config.bad_word_patterns.len(), 3);
    let m = svc::build_matchers(&config);
    assert_eq!(m.patterns.len(), 1);
    assert_eq!(m.pattern_index, vec![0]);
    assert_eq!(svc::check_bad_words("get FREE nitro now", &config, &m).and_then(|r| r.detail), Some("pattern #1".into()));
    assert_eq!(svc::check_bad_words("hello there", &config, &m), None);
    assert!(svc::build_matchers(&svc::normalize_config(None)).bad_words_re.is_none());
}

#[test]
fn a_word_list_stays_linear() {
    let words: Vec<String> = (0..400).map(|i| format!("badword{i} x{i}")).collect();
    let config = cfg(json!({ "version": 2, "badWords": words }));
    let m = svc::build_matchers(&config);
    let t0 = std::time::Instant::now();
    assert_eq!(svc::check_bad_words(&"badword ".repeat(500), &config, &m), None);
    assert!(t0.elapsed().as_millis() < 2000);
}

#[test]
fn invites_links_and_domains() {
    for s in ["discord.gg/abc", "https://discord.com/invite/abc", "discordapp.com/invite/abc", &svc::text_for_links("discord\\.gg/abc"), "x https://www.discord.gg/Abc-12"] {
        assert_eq!(svc::find_invites(s).len(), 1, "{s}");
    }
    assert_eq!(svc::find_invites("https://discord.com/invite/AbC"), vec!["AbC"]);
    assert!(svc::find_invites("discord.game").is_empty());
    assert!(svc::find_invites("mydiscord.gg/abc").is_empty());
    for s in ["I went home.Then slept", "use node.js here", "see readme.md", "Mr.Smith"] {
        assert!(svc::extract_link_hosts(s).is_empty(), "{s}");
    }
    assert_eq!(svc::extract_link_hosts("go https://evil.com/x)."), vec!["evil.com"]);
    assert_eq!(svc::extract_link_hosts("<https://evil.com>"), vec!["evil.com"]);
    assert_eq!(svc::extract_link_hosts("www.evil.com/path"), vec!["www.evil.com"]);
    assert!(svc::extract_link_hosts("https://discord.gg/abc").is_empty());
    assert!(svc::extract_link_hosts("https://discord.com/invite/abc").is_empty());
    assert_eq!(svc::extract_link_hosts("a https://a.com b https://a.com c https://b.org"), vec!["a.com", "b.org"]);
    assert!(!svc::host_matches("netflix.com", "x.com"));
    assert!(svc::host_matches("a.x.com", "x.com"));
    assert_eq!(svc::normalize_domain("https://WWW.Example.com/path").as_deref(), Some("example.com"));
    assert_eq!(svc::normalize_domain("*.example.com").as_deref(), Some("example.com"));
    assert_eq!(svc::normalize_domain("Example.COM:8080/x?y#z").as_deref(), Some("example.com"));
    for bad in ["", "node", "exa mple.com", ".com", "a..b.com"] {
        assert_eq!(svc::normalize_domain(bad), None, "{bad}");
    }
    assert_eq!(svc::normalize_domain("bücher.de").as_deref(), Some("xn--bcher-kva.de"));

    let run = |over: Value, content: &str| {
        let mut raw = json!({ "version": 2, "rules": { "links": { "action": "delete" } } });
        if let (Some(o), Some(r)) = (over.as_object(), raw.as_object_mut()) {
            for (k, v) in o {
                r.insert(k.clone(), v.clone());
            }
        }
        let c = cfg(raw);
        svc::evaluate_content_rules(&msg(content), &["links"], &c, &svc::build_matchers(&c), &GuildInvites::default()).len()
    };
    assert_eq!(run(json!({}), "see https://evil.com/x"), 1);
    assert_eq!(run(json!({}), "gif https://tenor.com/view/abc"), 0);
    assert_eq!(run(json!({}), "cdn https://cdn.discordapp.com/attachments/1/2/x.png"), 0);
    assert_eq!(run(json!({}), "plain text with node.js"), 0);
    assert_eq!(run(json!({}), "join https://discord.gg/abc"), 0);
    let blocked = json!({ "blockedDomains": ["evil.com"] });
    assert_eq!(run(blocked.clone(), "https://sub.evil.com/a"), 1);
    assert_eq!(run(blocked.clone(), "https://good.com/a?u=evil.com"), 0);
    assert_eq!(run(blocked, "https://notevil.com"), 0);
}

#[test]
fn counters() {
    assert_eq!(svc::caps_stats("HELLO WORLD THIS IS LOUD").percent, 100);
    assert_eq!(svc::caps_stats("OK").letters, 2);
    assert_eq!(svc::caps_stats("<:LOUDEMOJI:123456789012345678> hi").letters, 2);
    assert_eq!(svc::caps_stats("<@123456789012345678> https://EXAMPLE.COM/ABC hi").letters, 2);
    assert_eq!(svc::caps_stats("").percent, 0);
    assert_eq!(svc::caps_stats("Hello World").percent, 20);
    assert_eq!(svc::count_emojis(&"\u{1F600}".repeat(3)), 3);
    assert_eq!(svc::count_emojis("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"), 1);
    assert_eq!(svc::count_emojis("\u{1F1EC}\u{1F1E7}"), 1);
    assert_eq!(svc::count_emojis("<:aa:123456789012345678><a:bb:123456789012345678>"), 2);
    assert_eq!(svc::count_emojis("<:a:123456789012345678>"), 0);
    assert_eq!(svc::count_emojis("1 2 #"), 0);
    assert_eq!(svc::count_emojis("1\u{FE0F}\u{20E3}"), 1);
    let sp = svc::AttachmentView { spoiler: true };
    assert_eq!(svc::count_spoilers("||a|| ||b|| ||c||", &[]), 3);
    assert_eq!(svc::count_spoilers("||a|| ||b|| ||c||", std::slice::from_ref(&sp)), 4);
    assert_eq!(svc::count_spoilers("", &[sp, svc::AttachmentView { spoiler: false }]), 1);
    assert_eq!(svc::count_spoilers("||unclosed", &[]), 0);
    assert!(svc::max_combining_run("Z\u{353}\u{354}\u{355}\u{351}\u{352}o") >= 4);
    for ok in ["Tiếng Việt", "Tie\u{302}\u{301}ng Vie\u{323}\u{302}t", "क\u{94D}षत\u{94D}रिय", "สว\u{E31}สด\u{E35}", "❤\u{FE0F}", "1\u{FE0F}\u{20E3}"] {
        assert!(svc::max_combining_run(ok) < 4, "{ok:?}");
    }
    assert_eq!(svc::duplicate_fingerprint(""), None);
    assert_eq!(svc::duplicate_fingerprint("  \u{200B} "), None);
    assert_eq!(svc::duplicate_fingerprint("  Hello   World ").as_deref(), Some("hello world"));
    assert_eq!(svc::text_for_words("b**a**d ||x|| \\_"), "bad x _");
    assert_eq!(svc::normalize_word(" Foo  Bar ").as_deref(), Some("foo bar"));
    assert_eq!(svc::normalize_word("a"), None);
    assert_eq!(svc::normalize_word("!!"), None);
    assert_eq!(svc::normalize_word(&"x".repeat(65)), None);
    assert_eq!(svc::normalize_word("\u{1D41A}\u{1D42C}\u{1D42C}").as_deref(), Some("ass"));
}

#[test]
fn count_mentions_uses_parsed_mentions() {
    let mut m = msg("hi");
    m.author_id = "author".into();
    m.mention_user_ids = vec!["a".into(), "b".into(), "author".into()];
    m.mention_role_count = 1;
    m.mention_everyone = true;
    assert_eq!(svc::count_mentions(&m), 4);
    m.replied_user_id = Some("a".into());
    assert_eq!(svc::count_mentions(&m), 3, "a reply ping without a typed mention is not counted");
    m.content = "hey <@a>".into();
    assert_eq!(svc::count_mentions(&m), 4);
    assert_eq!(svc::count_mentions(&AutomodMessage::default()), 0);
}

// ── Scope, outcome, reasons ───────────────────────────────────────────────────

#[test]
fn scope_and_rule_applies() {
    let scope = |id: &str, parent: Option<&str>, thread: bool, cat: Option<&str>| ChannelScope {
        id: id.into(),
        name: String::new(),
        parent_id: parent.map(str::to_string),
        is_thread: thread,
        thread_category_id: cat.map(str::to_string),
    };
    assert_eq!(svc::scope_channel_ids(Some(&scope("t", Some("p"), true, Some("c")))), vec!["t", "p", "c"]);
    assert_eq!(svc::scope_channel_ids(Some(&scope("x", Some("c"), false, None))), vec!["x", "c"]);
    assert_eq!(svc::scope_channel_ids(Some(&scope("x", None, false, None))), vec!["x"]);
    assert!(svc::scope_channel_ids(None).is_empty());
    let hp = svc::HoneypotConfig { channel_ids: vec![CH.into()], action: "softban".into(), timeout_seconds: 86_400 };
    assert!(svc::is_honeypot_channel(Some(&scope(CH, None, false, None)), &hp));
    assert!(svc::is_honeypot_channel(Some(&scope("60", Some(CH), true, None)), &hp));
    assert!(!svc::is_honeypot_channel(Some(&scope("61", Some(CH), false, None)), &hp), "a channel in a category");
    assert!(!svc::is_honeypot_channel(None, &hp));

    let base = svc::normalize_config(None).rule("caps");
    let with = |f: &dyn Fn(&mut svc::RuleConfig)| {
        let mut r = base.clone();
        f(&mut r);
        r
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(!svc::rule_applies(&with(&|r| r.role_ids = s(&["r"])), &s(&["r"]), &[]));
    assert!(!svc::rule_applies(&with(&|r| { r.role_mode = "only".into(); r.role_ids = s(&["r"]) }), &[], &[]));
    assert!(svc::rule_applies(&with(&|r| { r.role_mode = "only".into(); r.role_ids = s(&["r"]) }), &s(&["r"]), &[]));
    assert!(!svc::rule_applies(&with(&|r| r.role_mode = "only".into()), &s(&["r"]), &[]));
    assert!(svc::rule_applies(&base, &s(&["r"]), &s(&["c"])));
    assert!(svc::rule_applies(&with(&|r| { r.channel_mode = "only".into(); r.channel_ids = s(&["c"]) }), &[], &s(&["x", "c"])));
    assert!(!svc::rule_applies(&with(&|r| r.channel_ids = s(&["c"])), &[], &s(&["x", "c"])));
}

#[test]
fn resolve_outcome_reasons_and_log_fields() {
    let a = svc::resolve_outcome(&[rm("caps", "delete"), rm("invites", "warn")]).expect("outcome");
    assert!(a.deletes);
    assert_eq!((a.punishment.as_deref(), a.primary.rule.as_str(), a.punishment_needs_delete), (Some("WARN"), "invites", false));
    assert_eq!(a.others.iter().map(|m| m.rule.as_str()).collect::<Vec<_>>(), vec!["caps"]);
    let mut l = rm("links", "timeout");
    l.timeout_seconds = Some(60);
    let mut sp = rm("spam", "timeout");
    sp.timeout_seconds = Some(600);
    let b = svc::resolve_outcome(&[l, sp]).expect("outcome");
    assert_eq!((b.punishment.as_deref(), b.timeout_seconds, b.primary.rule.as_str()), (Some("TIMEOUT"), Some(600), "links"));
    let c = svc::resolve_outcome(&[rm("badWords", "delete_warn"), rm("invites", "ban")]).expect("outcome");
    assert_eq!((c.primary.rule.as_str(), c.punishment.as_deref(), c.punishment_needs_delete), ("invites", Some("BAN"), true));
    let d = svc::resolve_outcome(&[rm("caps", "delete")]).expect("outcome");
    assert_eq!((d.punishment, d.deletes), (None, true));
    assert_eq!(svc::resolve_outcome(&[]), None);

    let mut long = rm("badWords", "delete_warn");
    long.detail = Some("x".repeat(5000));
    let reason = svc::build_reason(&svc::resolve_outcome(&[long, rm("caps", "delete")]).expect("outcome"), "general");
    assert!(bot_util::js::utf16_len(&reason) <= 500 && reason.starts_with("[Automod] "));
    let mut caps = rm("caps", "delete");
    caps.detail = Some("90% capitals".into());
    assert_eq!(svc::build_reason(&svc::resolve_outcome(&[caps]).expect("outcome"), "general"), "[Automod] Excessive Caps: 90% capitals in #general");

    let f = svc::build_punishment_log_fields("badWords", Some(&"d".repeat(5000)), &"c".repeat(5000), Some(CH));
    assert_eq!(f.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), vec!["Rule", "Detail", "Content"]);
    assert!(f.iter().all(|x| !x.1.is_empty() && bot_util::js::utf16_len(&x.1) <= 1024));
    assert_eq!(f[0].1, "\u{1F92C} Bad Words");
    assert_eq!(svc::build_punishment_log_fields("x", Some("word \"a\""), "hello", Some(CH))[1].1, format!("word \"a\" in <#{CH}>"));
    assert_eq!(svc::build_punishment_log_fields("x", Some("d"), "", None)[2].1, "*[no text]*");
}

#[test]
fn claim_punishment_allows_one_per_window_unless_more_severe() {
    let slots = svc::PunishmentSlots::default();
    assert!(slots.claim("g", "u", 1, 0));
    assert!(!slots.claim("g", "u", 1, 1000));
    assert!(slots.claim("g", "u", 4, 1000));
    assert!(!slots.claim("g", "u", 2, 2000), "a lesser one after a ban is suppressed");
    assert!(!slots.claim("g", "u", 1, 30_999));
    assert!(slots.claim("g", "u", 1, 31_000));
    assert!(slots.claim("g", "other", 1, 1000));
    assert!(slots.claim("g2", "u", 1, 1000));
}

#[test]
fn rule_keys_labels_and_action_tables() {
    assert_eq!(svc::resolve_rule_key("caps"), Some("caps"));
    assert_eq!(svc::resolve_rule_key("Excessive Caps"), Some("caps"));
    assert_eq!(svc::resolve_rule_key("anti-spam"), Some("spam"));
    assert_eq!(svc::resolve_rule_key("Caps (caps)"), Some("caps"));
    assert_eq!(svc::resolve_rule_key("\u{1F520} Excessive Caps (caps)"), Some("caps"));
    assert_eq!(svc::resolve_rule_key("BADWORDS"), Some("badWords"));
    assert_eq!(svc::resolve_rule_key("duplicates"), Some("repeatedText"));
    for none in ["nope", "", "__proto__"] {
        assert_eq!(svc::resolve_rule_key(none), None);
    }
    for k in svc::RULE_KEYS {
        assert_eq!(svc::resolve_rule_key(k), Some(k));
    }
    for a in svc::ACTIONS {
        assert_eq!(svc::is_deleting_action(a), !["off", "warn"].contains(&a), "{a}");
        if let Some(p) = svc::punishment_of(a) {
            assert!(svc::punishment_rank(p).unwrap_or(0) >= 1);
        }
    }
    let config = cfg(json!({ "version": 2, "badWords": ["aa"], "rules": {
        "caps": { "action": "timeout", "threshold": 80, "timeoutSeconds": 7200, "roleMode": "only", "roleIds": [UID], "channelIds": [CH, LOGCH] },
        "badWords": { "action": "delete" },
    } }));
    assert!(svc::describe_rule_line("caps", &config.rule("caps"), &config)
        .contains("Excessive Caps** — Delete & timeout · ≥ 80% capitals · 2 hours · roles: only 1 · channels: except 2"));
    assert!(svc::describe_rule_line("badWords", &config.rule("badWords"), &config).ends_with("Delete message · 1 words, 0 patterns"));
    assert_eq!(svc::describe_rule_line("emojis", &config.rule("emojis"), &config), "\u{1F600} **Excessive Emojis** — Off");
    assert_eq!(svc::format_threshold("spam", 6), "6 messages in 5 s");
    assert_eq!(svc::format_threshold("repeatedText", 3), "3 identical messages in 30 s");
}

#[test]
fn honeypot_config_and_outcome() {
    let d = svc::default_config();
    assert_eq!((d.honeypot.channel_ids.len(), d.honeypot.action.as_str(), d.honeypot.timeout_seconds), (0, "softban", 86_400));
    assert_eq!(svc::upgrade_v1(Some(&json!({ "filterInvites": true }))).honeypot, d.honeypot);
    let ids = |n: u64| format!("95500600000000{n:04}");
    let c = cfg(json!({ "version": 2, "honeypot": {
        "channelIds": [CH, CH, "nope", ids(7), ids(8), ids(9), ids(10), ids(11)], "action": "explode", "timeoutSeconds": 5,
    } }));
    assert_eq!(c.honeypot.channel_ids, vec![CH.to_string(), ids(7), ids(8), ids(9), ids(10)]);
    assert_eq!((c.honeypot.action.as_str(), c.honeypot.timeout_seconds), ("softban", 60));
    assert_eq!(cfg(json!({ "version": 2, "honeypot": { "action": "kick" } })).honeypot.action, "kick");

    let hp = |action: &str| svc::HoneypotConfig { channel_ids: vec![], action: action.into(), timeout_seconds: 600 };
    let soft = svc::honeypot_outcome(&hp("softban"));
    assert_eq!((soft.punishment.as_deref(), soft.punishment_needs_delete, soft.delete_message_seconds, soft.notice), (Some("SOFTBAN"), false, Some(86_400), false));
    let t = svc::honeypot_outcome(&hp("timeout"));
    assert_eq!((t.punishment.as_deref(), t.timeout_seconds, t.delete_message_seconds), (Some("TIMEOUT"), Some(600), None));
    assert_eq!(svc::build_reason(&soft, "do-not-post"), "[Automod] Honeypot: Posted in a honeypot channel in #do-not-post");
}

#[test]
fn notice_texts() {
    let base = svc::NoticeInput {
        deleted: true,
        status: None,
        punishment: None,
        author_id: UID,
        author_username: Some("someone"),
        label: "Bad Words",
        timeout_seconds: None,
    };
    assert_eq!(svc::build_notice_text(&base), (format!("\u{1F6E1}\u{FE0F} <@{UID}>, your message was removed: **Bad Words**."), true));
    let applied = |p: &'static str, t: Option<i64>| svc::build_notice_text(&svc::NoticeInput { status: Some("applied"), punishment: Some(p), timeout_seconds: t, ..base.clone() });
    assert_eq!(applied("WARN", None).0, format!("\u{1F6E1}\u{FE0F} <@{UID}>, your message was removed and you have been warned: **Bad Words**."));
    assert_eq!(applied("TIMEOUT", Some(7200)).0, format!("\u{1F6E1}\u{FE0F} <@{UID}>, your message was removed and you have been timed out for 2 hours: **Bad Words**."));
    assert_eq!(applied("KICK", None), ("\u{1F6E1}\u{FE0F} **someone** was kicked by automod: **Bad Words**.".to_string(), false));
    assert!(applied("BAN", None).0.contains("was banned by automod"));
    let warned = svc::build_notice_text(&svc::NoticeInput { deleted: false, status: Some("applied"), punishment: Some("WARN"), ..base.clone() });
    assert_eq!(warned, (format!("⚠\u{FE0F} <@{UID}>, you have been warned: **Bad Words**."), true));
}

// ── The sweep against the JS ──────────────────────────────────────────────────

fn js_calls(calls: &[Value]) -> Vec<Value> {
    // Too long for a command line: the calls go through a file.
    let path = std::env::temp_dir().join(format!("feat-automod-calls-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_string(calls).unwrap_or_default()).expect("write the calls");
    let out = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["call", &format!("@{}", path.display())]);
    let _ = std::fs::remove_file(&path);
    out.as_array().cloned().unwrap_or_default().into_iter().map(|r| r.get("ok").cloned().unwrap_or(json!({ "error": r }))).collect()
}

fn opt<T: serde::Serialize>(v: Option<T>) -> Value {
    v.map(|x| json!(x)).unwrap_or(Value::Null)
}

fn match_json(m: &RuleMatch) -> Value {
    let mut v = json!({ "rule": m.rule, "action": m.action });
    if let Some(d) = &m.detail {
        v["detail"] = json!(d);
    }
    if let Some(t) = m.timeout_seconds {
        v["timeoutSeconds"] = json!(t);
    }
    v
}

#[test]
fn every_pure_function_agrees_with_the_js_on_a_sweep() {
    let texts = [
        "", " ", "hello", "HELLO WORLD THIS IS LOUD", "b**a**d ||x|| \\_", "_bad_ __x__ snake_case \\_lit\\_", "a\u{200B}ss \u{AD}x", "ｆｕｌｌｗｉｄｔｈ",
        "discord.gg/abc discord\\.gg/x2 https://discord.com/invite/AbC", "go https://evil.com/x). www.Site.org/p <https://a.b.co>", "**https://x.com** ~~https://y.org~~ _https://z.net_",
        "😀😀😀 👨‍👩‍👧 🇬🇧 1️⃣ <:aa:123456789012345678>", "Z\u{353}\u{354}\u{355}\u{351}\u{352}o", "Tiếng Việt ❤\u{FE0F}", "||a|| ||b||", "İstanbul ΣΑΣ", "日本語のテキスト",
        "  Buy   NOW  ", "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "  Foo  Bar ", "!!", "a", "https://bücher.de/x", "mydiscord.gg/abc", "<@123456789012345678> HI THERE EVERYONE",
    ];
    let configs = [
        json!(null),
        json!({ "version": 2, "rules": { "badWords": { "action": "delete" }, "links": { "action": "warn" }, "invites": { "action": "ban" }, "caps": { "action": "timeout", "threshold": 60 },
            "emojis": { "action": "delete", "threshold": 3 }, "spoilers": { "action": "delete", "threshold": 2 }, "mentions": { "action": "kick" }, "zalgo": { "action": "delete_warn" } },
            "badWords": ["ass", "bad phrase", "日本", "x1"], "badWordPatterns": ["fr[e3]{2}\\s*n[i1]tro", "(a+)+$", "^hel+o$"], "blockedDomains": [], "allowedDomains": ["x.com"] }),
        json!({ "version": 2, "blockedDomains": ["evil.com", "Bücher.DE"], "rules": { "links": { "action": "delete", "roleMode": "only", "roleIds": ["123456789012345678"] } } }),
        json!({ "filterInvites": true, "filterLinks": true, "automodAction": "mute", "massMentionThreshold": 3, "badWords": ["Foo", 7] }),
        json!({ "version": 2, "honeypot": { "channelIds": [CH], "action": "timeout", "timeoutSeconds": 3600 }, "exemptRoleIds": [UID, UID, "x"] }),
        json!({ "version": "2", "rules": [], "badWords": "x", "allowedDomains": "y" }),
    ];
    let mut calls: Vec<Value> = Vec::new();
    let mut expect: Vec<Value> = Vec::new();
    for t in texts {
        calls.push(json!(["textForWords", t]));
        expect.push(json!(svc::text_for_words(t)));
        calls.push(json!(["textForLinks", t]));
        expect.push(json!(svc::text_for_links(t)));
        calls.push(json!(["normalizeWord", t]));
        expect.push(opt(svc::normalize_word(t)));
        calls.push(json!(["normalizeDomain", t]));
        expect.push(opt(svc::normalize_domain(t)));
        calls.push(json!(["findInvites", t]));
        expect.push(json!(svc::find_invites(t)));
        calls.push(json!(["extractLinkHosts", t]));
        expect.push(json!(svc::extract_link_hosts(t)));
        let c = svc::caps_stats(t);
        calls.push(json!(["capsStats", t]));
        expect.push(json!({ "letters": c.letters, "upper": c.upper, "percent": c.percent }));
        calls.push(json!(["countEmojis", t]));
        expect.push(json!(svc::count_emojis(t)));
        calls.push(json!(["countSpoilers", t, 1]));
        expect.push(json!(svc::count_spoilers(t, &[svc::AttachmentView { spoiler: true }])));
        calls.push(json!(["maxCombiningRun", t]));
        expect.push(json!(svc::max_combining_run(t)));
        calls.push(json!(["duplicateFingerprint", t]));
        expect.push(opt(svc::duplicate_fingerprint(t)));
        calls.push(json!(["isSpacelessWord", t]));
        expect.push(json!(svc::is_spaceless_word(t)));
        calls.push(json!(["resolveRuleKey", t]));
        expect.push(opt(svc::resolve_rule_key(t)));
        for raw in &configs {
            let c = svc::normalize_config(Some(raw));
            let m = svc::build_matchers(&c);
            calls.push(json!(["checkBadWords", t, raw]));
            expect.push(svc::check_bad_words(t, &c, &m).map(|x| match_json(&x)).unwrap_or(Value::Null));
            calls.push(json!(["evaluate", t, raw]));
            let active = svc::active_rule_keys(&c);
            expect.push(json!(svc::evaluate_content_rules(&msg(t), &active, &c, &m, &GuildInvites::default()).iter().map(match_json).collect::<Vec<_>>()));
        }
    }
    for raw in &configs {
        let c = svc::normalize_config(Some(raw));
        calls.push(json!(["normalizeConfig", raw]));
        expect.push(c.to_json());
        calls.push(json!(["isLegacyConfig", raw]));
        expect.push(json!(svc::is_legacy_config(Some(raw))));
        calls.push(json!(["describeHoneypot", raw]));
        expect.push(json!(svc::describe_honeypot(&c)));
        calls.push(json!(["describeLinksMode", raw]));
        expect.push(json!(svc::describe_links_mode(&c)));
        for k in svc::RULE_KEYS {
            calls.push(json!(["describeRuleLine", k, raw]));
            expect.push(json!(svc::describe_rule_line(k, &c.rule(k), &c)));
        }
    }
    let match_sets = [
        vec![rm("caps", "delete"), rm("invites", "warn")],
        vec![rm("spam", "timeout"), rm("links", "timeout"), rm("badWords", "delete")],
        vec![rm("zalgo", "warn"), rm("mentions", "kick"), rm("emojis", "ban")],
    ];
    for set in &match_sets {
        calls.push(json!(["buildReason", set.iter().map(match_json).collect::<Vec<_>>(), "général"]));
        expect.push(json!(svc::build_reason(&svc::resolve_outcome(set).expect("outcome"), "général")));
    }
    for action in ["softban", "ban", "kick", "timeout"] {
        let hp = svc::HoneypotConfig { channel_ids: vec![], action: action.into(), timeout_seconds: 3600 };
        calls.push(json!(["honeypotReason", { "action": action, "timeoutSeconds": 3600 }, "do-not-post"]));
        expect.push(json!(svc::build_reason(&svc::honeypot_outcome(&hp), "do-not-post")));
        calls.push(json!(["honeypotWarning", action]));
        expect.push(Value::Null); // compared below on title and description
    }
    for (rule, detail, content, ch) in [("badWords", "word \"x\"", "hello", Some(CH)), ("🍯 Honeypot", "", "", None), ("links", "", "y".repeat(2000).as_str(), Some(CH))] {
        calls.push(json!(["buildPunishmentLogFields", rule, detail, content, ch]));
        expect.push(json!(svc::build_punishment_log_fields(rule, Some(detail), content, ch).iter().map(|(n, v)| json!({ "name": n, "value": v })).collect::<Vec<_>>()));
    }
    for (deleted, status, punishment, t) in [(true, None, None, None), (true, Some("applied"), Some("TIMEOUT"), Some(90_061)), (false, Some("applied"), Some("WARN"), None), (true, Some("applied"), Some("BAN"), None)] {
        let author_name = "a_very_long_username_with_more_than_32_chars";
        calls.push(json!(["buildNoticeText", { "deleted": deleted, "status": status, "punishment": punishment, "author": { "id": UID, "username": author_name }, "label": "Server Invites", "timeoutSeconds": t }]));
        let (text, mention) = svc::build_notice_text(&svc::NoticeInput {
            deleted,
            status,
            punishment,
            author_id: UID,
            author_username: Some(author_name),
            label: "Server Invites",
            timeout_seconds: t,
        });
        expect.push(json!({ "text": text, "mention": mention }));
    }
    let js = js_calls(&calls);
    assert_eq!(js.len(), calls.len());
    for ((call, want), got) in calls.iter().zip(&expect).zip(&js) {
        if call[0] == "honeypotWarning" {
            let e = serde_json::to_value(cmd::honeypot_warning_embed(call[1].as_str().unwrap_or_default())).unwrap_or_default();
            assert_eq!((&e["title"], &e["description"]), (&got["title"], &got["description"]), "{call}");
            continue;
        }
        assert_eq!(want, got, "{call}");
    }
}

// ── Cache keys and declarations ───────────────────────────────────────────────

#[test]
fn cache_keys_match_the_js() {
    let contents = ["Buy   now", "buy NOW", "something else", "ｆｕｌｌ\u{200B} width"];
    let js = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["keys", G, UID, &serde_json::to_string(&contents).unwrap_or_default()]);
    let mut expected = Vec::new();
    for (i, c) in contents.iter().enumerate() {
        let id = format!("70000000000000000{i}");
        expected.push(json!({ "key": svc::dup_key(G, UID, &svc::duplicate_fingerprint(c).unwrap_or_default()), "windowMs": svc::REPEAT_WINDOW_MS, "member": id }));
        expected.push(json!({ "key": svc::rate_key(G, UID), "windowMs": svc::SPAM_WINDOW_MS, "member": id }));
    }
    assert_eq!(js, json!(expected));
}

#[test]
fn commands_and_plugins_match_the_js() {
    let reg = registry();
    bot_core::testing::assert_commands_match_golden(&reg, &["automod", "antiraid"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "automod");
    bot_core::testing::assert_plugin_matches_golden(&reg, "antiraid");
    assert!(bot_core::testing::persistent_handler_problems_for(&reg, &["automod", "antiraid"]).is_empty());
}

#[test]
fn automod_runs_first_on_create_and_after_logging_on_update() {
    let order = |p: bot_config::HookPoint| bot_config::HOOK_ORDER.iter().find(|(h, _)| *h == p).map(|(_, l)| l.to_vec()).unwrap_or_default();
    assert_eq!(order(bot_config::HookPoint::MessageCreate).first(), Some(&"automod"));
    let upd = order(bot_config::HookPoint::MessageUpdate);
    let at = |id: &str| upd.iter().position(|x| *x == id);
    assert!(at("logging") < at("automod"));
}

// ── The pipeline with a fake guild and a fake REST ────────────────────────────

/// What a fake hook context recorded.
#[derive(Default)]
struct Sink {
    stops: Mutex<Vec<String>>,
    action: Mutex<Option<AutomodAction>>,
}

impl AutomodSink for Sink {
    fn stop(&self, reason: &str) {
        self.stops.lock().unwrap_or_else(|e| e.into_inner()).push(reason.into());
    }
    fn set_action(&self, action: &AutomodAction) {
        *self.action.lock().unwrap_or_else(|e| e.into_inner()) = Some(action.clone());
    }
}

impl Sink {
    fn stops(&self) -> Vec<String> {
        self.stops.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn action(&self) -> Option<AutomodAction> {
        self.action.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn bot_perms(manage: bool) -> Permissions {
    let p = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS;
    if manage { p | Permissions::MANAGE_MESSAGES } else { p }
}

fn guild(manage: bool) -> FakeGuild {
    FakeGuild::new(G, OWNER)
        .everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        .role(BOT_ROLE, 5, bot_perms(manage))
        .role(MOD_ROLE, 2, Permissions::KICK_MEMBERS)
        .member(TEST_BOT_ID, &[BOT_ROLE])
        .member(UID, &[])
        .member("955006000000000007", &[MOD_ROLE])
        .member(OWNER, &[])
        .channel(CH, 0, "general", &[])
        .channel(LOGCH, 0, "mod-log", &[])
}

struct Env {
    t: TestApp,
    rest: FakeRest,
}

fn settings_row(raw: Value, mod_log: bool) -> bot_db::GuildSettings {
    bot_db::GuildSettings {
        guild_id: G.into(),
        automod_enabled: true,
        automod_config: Some(raw),
        mod_log_channel_id: mod_log.then(|| LOGCH.to_string()),
        ..Default::default()
    }
}

fn env(manage: bool) -> Env {
    let rest = FakeRest::new();
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[guild(manage)], &rest);
    Env { t, rest }
}

fn member_of(app: &App, id: &str) -> Option<serenity::all::Member> {
    let d = app.discord()?;
    let g = d.cache.guild(GuildId::new(G.parse().ok()?))?;
    g.members.get(&serenity::all::UserId::new(id.parse().ok()?)).cloned()
}

fn input(app: &App, user: &str, id: &str, content: &str) -> AutomodInput {
    let message = fake_message(id, G, CH, user, content);
    let e = MessageCreate { guild_id: GuildId::new(G.parse().unwrap_or(1)), member: member_of(app, user), channel_id: message.channel_id, channel: None, message };
    AutomodInput::from_create(app, &e)
}

async fn run(e: &Env, settings: &bot_db::GuildSettings, user: &str, id: &str, content: &str) -> (Arc<Sink>, Option<svc::Applied>) {
    let sink = Arc::new(Sink::default());
    let inp = input(&e.t, user, id, content);
    let applied = svc::run_automod(&e.t, sink.as_ref(), Some(settings), &inp, false).await;
    (sink, applied)
}

fn mod_log_posts(e: &Env) -> Vec<Value> {
    e.rest.calls_to("POST", &format!("/channels/{LOGCH}/messages")).into_iter().map(|c| c.body).collect()
}

#[tokio::test]
async fn an_invite_is_deleted_the_chain_stops_and_the_state_is_set() {
    let e = env(true);
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), true);
    let (sink, applied) = run(&e, &s, UID, "955006000000000100", "join discord.gg/abc").await;
    assert_eq!(e.rest.calls_to("DELETE", &format!("/channels/{CH}/messages/955006000000000100")).len(), 1);
    assert_eq!(sink.stops(), vec!["automod"]);
    assert_eq!(sink.action(), Some(AutomodAction { rule: "invites".into(), deleted: true }));
    let notices = e.rest.calls_to("POST", &format!("/channels/{CH}/messages"));
    assert_eq!(notices.len(), 1, "one in-channel notice");
    assert!(notices[0].body["content"].as_str().unwrap_or_default().contains("your message was removed: **Server Invites**"));
    assert_eq!(notices[0].body["allowed_mentions"]["users"], json!([UID]));
    let logs = mod_log_posts(&e);
    assert_eq!(logs.len(), 1, "exactly one mod-log embed");
    let embed = &logs[0]["embeds"][0];
    assert!(embed["title"].as_str().unwrap_or_default().contains("Message Deleted"));
    assert_eq!(logs[0]["allowed_mentions"]["parse"], json!([]));
    let names: Vec<&str> = embed["fields"].as_array().map(|f| f.iter().filter_map(|x| x["name"].as_str()).collect()).unwrap_or_default();
    assert_eq!(names, vec!["Member", "Channel", "Rule", "Detail", "Content"]);
    assert!(embed["footer"]["text"].as_str().unwrap_or_default().contains("Further removals for this member and rule are not logged for 10 s"));
    assert!(bot_core::deletion_tracker::was_deleted_by_bot(&e.t, "955006000000000100").is_some(), "marked before the delete");
    assert_eq!(applied.map(|a| a.status), Some(None));

    // A burst: every message is deleted, but one log embed and one notice per member.
    let (sink2, _) = run(&e, &s, UID, "955006000000000101", "join discord.gg/abc").await;
    assert_eq!(sink2.stops(), vec!["automod"]);
    assert_eq!(mod_log_posts(&e).len(), 1, "throttled");
    assert_eq!(e.rest.calls_to("POST", &format!("/channels/{CH}/messages")).len(), 1, "one notice per member per 10 s");
}

#[tokio::test]
async fn no_manage_messages_means_no_delete_no_stop_and_one_not_deleted_embed() {
    let e = env(false);
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete_warn" } } }), true);
    let (sink, _) = run(&e, &s, UID, "955006000000000110", "join discord.gg/abc").await;
    assert!(e.rest.calls_to("DELETE", "/channels/*/messages/*").is_empty());
    assert!(sink.stops().is_empty());
    assert_eq!(sink.action(), Some(AutomodAction { rule: "invites".into(), deleted: false }));
    assert!(e.rest.calls_to("POST", &format!("/channels/{CH}/messages")).is_empty(), "no removed notice");
    let logs = mod_log_posts(&e);
    assert_eq!(logs.len(), 1);
    assert!(logs[0]["embeds"][0]["title"].as_str().unwrap_or_default().contains("Message NOT Deleted"));
    let last = logs[0]["embeds"][0]["fields"].as_array().and_then(|f| f.last().cloned()).unwrap_or_default();
    assert!(last["value"].as_str().unwrap_or_default().contains("Manage Messages"));
    assert!(last["value"].as_str().unwrap_or_default().contains("No punishment was applied"));
    run(&e, &s, UID, "955006000000000111", "join discord.gg/abc").await;
    assert_eq!(mod_log_posts(&e).len(), 1, "one NOT deleted embed per channel per minute");
}

#[tokio::test]
async fn discord_delete_errors() {
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), true);
    let gone = env(true);
    gone.rest.fail("DELETE", "/channels/*/messages/*", 404, 10008, "Unknown Message");
    let (sink, _) = run(&gone, &s, UID, "955006000000000120", "discord.gg/abc").await;
    assert_eq!(sink.stops(), vec!["automod"], "already gone counts as deleted");
    assert!(gone.rest.calls_to("POST", &format!("/channels/{CH}/messages")).is_empty(), "no notice for a message somebody else removed");
    let denied = env(true);
    denied.rest.fail("DELETE", "/channels/*/messages/*", 403, 50013, "Missing Permissions");
    let (sink, _) = run(&denied, &s, UID, "955006000000000121", "discord.gg/abc").await;
    assert!(sink.stops().is_empty());
    assert_eq!(sink.action().map(|a| a.deleted), Some(false));
    assert!(mod_log_posts(&denied)[0]["embeds"][0]["title"].as_str().unwrap_or_default().contains("NOT Deleted"));
    let odd = env(true);
    odd.rest.fail("DELETE", "/channels/*/messages/*", 500, 500, "boom");
    run(&odd, &s, UID, "955006000000000122", "discord.gg/abc").await;
    let last = mod_log_posts(&odd)[0]["embeds"][0]["fields"].as_array().and_then(|f| f.last().cloned()).unwrap_or_default();
    assert!(last["value"].as_str().unwrap_or_default().contains("Deleting failed"));
}

#[tokio::test]
async fn staff_the_owner_and_exemptions_are_skipped() {
    let e = env(true);
    let rules = json!({ "invites": { "action": "delete" } });
    let cases = [
        (UID, json!({ "version": 2, "rules": rules, "exemptRoleIds": [G] })), // @everyone exempt: every member
        ("955006000000000007", json!({ "version": 2, "rules": rules })),      // Kick Members: moderator
        (OWNER, json!({ "version": 2, "rules": rules })),
        (UID, json!({ "version": 2, "rules": rules, "exemptChannelIds": [CH] })),
        (UID, json!({ "version": 2, "rules": { "invites": { "action": "delete", "roleIds": [G] } } })),
        (UID, json!({ "version": 2, "rules": { "invites": { "action": "delete", "channelMode": "only", "channelIds": [LOGCH] } } })),
    ];
    for (i, (user, raw)) in cases.into_iter().enumerate() {
        let (sink, applied) = run(&e, &settings_row(raw, false), user, &format!("95500600000000020{i}"), "discord.gg/abc").await;
        assert!(applied.is_none() && sink.stops().is_empty() && sink.action().is_none(), "case {i}");
    }
    assert!(e.rest.calls_to("DELETE", "/channels/*/messages/*").is_empty());
    // A mod role from the settings counts as staff too.
    let mut s = settings_row(json!({ "version": 2, "rules": rules }), false);
    s.mod_role_ids = vec![MOD_ROLE.into()];
    assert!(run(&e, &s, "955006000000000007", "955006000000000210", "discord.gg/abc").await.1.is_none());
    // Control: an "only" rule for a role the member has is applied.
    let only = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete", "roleMode": "only", "roleIds": [G] } } }), false);
    assert!(run(&e, &only, UID, "955006000000000211", "discord.gg/abc").await.1.is_some());
}

#[tokio::test]
async fn disabled_automod_bots_webhooks_and_malformed_configs_do_nothing() {
    let e = env(true);
    let mut off = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), false);
    off.automod_enabled = false;
    assert!(run(&e, &off, UID, "955006000000000300", "discord.gg/abc").await.1.is_none());
    for raw in [json!("x"), json!(42), json!([]), json!({ "rules": "x" }), json!({ "version": 2, "badWords": [null, "", 7, {}], "blockedDomains": ["", null] })] {
        assert!(run(&e, &settings_row(raw, false), UID, "955006000000000301", "hello world, nothing wrong here").await.1.is_none());
    }
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), false);
    let mut bot = input(&e.t, UID, "955006000000000302", "discord.gg/abc");
    bot.message.author_bot = true;
    assert!(svc::run_automod(&e.t, &Sink::default(), Some(&s), &bot, false).await.is_none());
    let mut hook = input(&e.t, UID, "955006000000000303", "discord.gg/abc");
    hook.message.webhook = true;
    assert!(svc::run_automod(&e.t, &Sink::default(), Some(&s), &hook, false).await.is_none());
    assert!(svc::run_automod(&e.t, &Sink::default(), None, &input(&e.t, UID, "955006000000000304", "discord.gg/abc"), false).await.is_none());
    assert!(e.rest.calls_to("DELETE", "/channels/*/messages/*").is_empty());
}

#[tokio::test]
async fn a_member_who_left_is_not_punished_and_a_failed_lookup_does_nothing() {
    let e = env(true);
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete_warn" } } }), true);
    // Not in the guild cache: the REST lookup says 10007 (the FakeRest default), so the member left.
    let (sink, applied) = run(&e, &s, "955006000000000400", "955006000000000401", "discord.gg/abc").await;
    assert_eq!(sink.stops(), vec!["automod"]);
    assert_eq!(applied.and_then(|a| a.status), Some("no-member"));
    let embed = &mod_log_posts(&e)[0]["embeds"][0];
    assert!(embed["title"].as_str().unwrap_or_default().contains("Warn Not Applied"));
    assert!(embed["fields"].to_string().contains("no longer in the server"));
    // A transient failure proves nothing about staff status: do nothing.
    e.rest.fail("GET", "/guilds/*/members/*", 429, 0, "rate limited");
    let (sink, applied) = run(&e, &s, "955006000000000402", "955006000000000403", "discord.gg/abc").await;
    assert!(applied.is_none() && sink.stops().is_empty());
}

#[tokio::test]
async fn punishments_go_through_moderation_once_per_window() {
    let e = env(true);
    let seen: Arc<Mutex<Vec<feat_moderation::moderation_service::ModActionInput>>> = Arc::default();
    let rec = seen.clone();
    svc::set_mod_executor(
        &e.t,
        Arc::new(move |_app, input| {
            rec.lock().unwrap_or_else(|e| e.into_inner()).push(input);
            Box::pin(async { Err(feat_moderation::moderation_service::ModError::new(feat_moderation::moderation_service::ModCode::Hierarchy, "My role is too low.")) })
        }),
    );
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "ban" } } }), true);
    let (sink, applied) = run(&e, &s, UID, "955006000000000500", "discord.gg/abc").await;
    assert_eq!(sink.stops(), vec!["automod"]);
    assert_eq!(applied.and_then(|a| a.status), Some("failed"));
    let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert_eq!((c.action.as_str(), c.source.as_str(), c.delete_message_seconds), ("BAN", "automod", Some(3600)));
    assert_eq!(c.reason.as_deref(), Some("[Automod] Server Invites: invite discord.gg/abc in #general"));
    assert_eq!(c.log_fields.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), vec!["Rule", "Detail", "Content"]);
    let embed = &mod_log_posts(&e)[0]["embeds"][0];
    assert!(embed["title"].as_str().unwrap_or_default().contains("Message Deleted, Ban Not Applied"));
    assert!(embed["fields"].to_string().contains("HIERARCHY: My role is too low."));
    let notice = &e.rest.calls_to("POST", &format!("/channels/{CH}/messages"))[0].body;
    assert!(notice["content"].as_str().unwrap_or_default().contains("your message was removed: "), "a failed punishment is not announced");
    // The slot is taken: the next message is deleted, the punishment suppressed, nothing logged.
    let (_, applied) = run(&e, &s, UID, "955006000000000501", "discord.gg/abc").await;
    assert_eq!(applied.and_then(|a| a.status), Some("suppressed"));
    assert_eq!(seen.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    assert_eq!(mod_log_posts(&e).len(), 1);

    // A warn-only action keeps the message and never stops the chain.
    let w = env(true);
    svc::set_mod_executor(&w.t, Arc::new(|_app, _input| Box::pin(async { Err(feat_moderation::moderation_service::ModError::new(feat_moderation::moderation_service::ModCode::DiscordError, "boom")) })));
    let ws = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "warn" } } }), true);
    let (sink, _) = run(&w, &ws, UID, "955006000000000502", "discord.gg/abc").await;
    assert!(w.rest.calls_to("DELETE", "/channels/*/messages/*").is_empty());
    assert!(sink.stops().is_empty());
    assert_eq!(sink.action(), Some(AutomodAction { rule: "invites".into(), deleted: false }));
    assert!(mod_log_posts(&w)[0]["embeds"][0]["title"].as_str().unwrap_or_default().contains("Warn Not Applied"));
}

#[tokio::test]
async fn honeypot_posts_are_punished_with_every_rule_off_and_staff_are_spared() {
    let e = env(true);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let rec = seen.clone();
    svc::set_mod_executor(
        &e.t,
        Arc::new(move |_app, input| {
            rec.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{} {:?}", input.action, input.delete_message_seconds));
            Box::pin(async { Err(feat_moderation::moderation_service::ModError::new(feat_moderation::moderation_service::ModCode::NotPermitted, "no")) })
        }),
    );
    let s = settings_row(json!({ "version": 2, "rules": {}, "honeypot": { "channelIds": [CH] }, "exemptChannelIds": [CH] }), true);
    let (sink, _) = run(&e, &s, UID, "955006000000000600", "free nitro").await;
    assert_eq!(sink.stops(), vec!["automod"]);
    assert_eq!(sink.action(), Some(AutomodAction { rule: "honeypot".into(), deleted: true }));
    assert!(e.rest.calls_to("POST", &format!("/channels/{CH}/messages")).is_empty(), "no in-channel notice");
    assert_eq!(seen.lock().unwrap_or_else(|e| e.into_inner()).clone(), vec!["SOFTBAN Some(86400)"]);
    let embed = &mod_log_posts(&e)[0]["embeds"][0];
    assert!(embed["title"].as_str().unwrap_or_default().contains("Message Deleted, Softban Not Applied"));
    assert!(embed["fields"].to_string().contains("Honeypot"));
    // Staff, the owner and edits are spared.
    for (user, id) in [("955006000000000007", "955006000000000601"), (OWNER, "955006000000000602")] {
        assert!(run(&e, &s, user, id, "hi").await.1.is_none());
    }
    let edit = input(&e.t, UID, "955006000000000603", "hi");
    assert!(svc::run_automod(&e.t, &Sink::default(), Some(&s), &edit, true).await.is_none());
}

#[tokio::test]
async fn content_rules_through_the_pipeline() {
    let e = env(true);
    let one = |rules: Value| settings_row(json!({ "version": 2, "rules": rules, "badWords": ["ass"], "blockedDomains": ["evil.com"] }), false);
    let cases = [
        (json!({ "caps": { "action": "delete" } }), "THIS IS ALL CAPITALS HERE", true),
        (json!({ "caps": { "action": "delete" } }), "OK THANKS", false),
        (json!({ "emojis": { "action": "delete" } }), "😀😀😀😀😀😀😀😀😀😀", true),
        (json!({ "zalgo": { "action": "delete" } }), "Z\u{353}\u{354}\u{355}\u{351}\u{352}o", true),
        (json!({ "badWords": { "action": "delete" } }), "you ass", true),
        (json!({ "badWords": { "action": "delete" } }), "my assistant", false),
        (json!({ "links": { "action": "delete" } }), "see https://evil.com/x", true),
        (json!({ "links": { "action": "delete" } }), "see https://fine.org/x", false),
        (json!({ "links": { "action": "delete" } }), "go to node.js", false),
    ];
    for (i, (rules, content, deleted)) in cases.into_iter().enumerate() {
        let (sink, _) = run(&e, &one(rules), UID, &format!("95500600000000070{i}"), content).await;
        assert_eq!(!sink.stops().is_empty(), deleted, "{content}");
    }
    // Several matches act once with the strongest action.
    let both = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" }, "caps": { "action": "delete" } } }), false);
    let (sink, _) = run(&e, &both, UID, "955006000000000720", "DISCORD.GG/ABC NOW").await;
    assert_eq!(sink.stops().len(), 1);
    assert_eq!(sink.action().map(|a| a.rule), Some("invites".into()));
    // The guild's own vanity invite is not a violation.
    let v = {
        let rest = FakeRest::new();
        let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[guild(true).vanity_code("Mine")], &rest);
        Env { t, rest }
    };
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), false);
    assert!(run(&v, &s, UID, "955006000000000721", "join discord.gg/mine").await.1.is_none());
    // A forward is judged like the same text pasted (effectiveText).
    let mut fwd = input(&e.t, UID, "955006000000000722", "");
    fwd.message.snapshots = vec![svc::SnapshotView { content: "join discord.gg/abc".into(), attachments: vec![] }];
    let sink = Sink::default();
    assert!(svc::run_automod(&e.t, &sink, Some(&s), &fwd, false).await.is_some());
    assert_eq!(sink.action(), Some(AutomodAction { rule: "invites".into(), deleted: true }));
}

#[tokio::test]
async fn edits_run_only_content_rules_and_a_rule_the_old_text_broke_is_not_repeated() {
    let e = env(true);
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" }, "spam": { "action": "delete", "threshold": 3 }, "repeatedText": { "action": "delete", "threshold": 2 } } }), false);
    let edit_of = |old: Option<&str>, new: &str, id: &str| {
        let mut i = input(&e.t, UID, id, new);
        i.old_known = old.is_some();
        i.old_message = old.map(|o| svc::AutomodMessage { content: o.into(), ..i.message.clone() });
        i
    };
    let sink = Sink::default();
    assert!(svc::run_automod(&e.t, &sink, Some(&s), &edit_of(Some("harmless"), "join discord.gg/abc", "955006000000000800"), true).await.is_some());
    assert_eq!(sink.stops(), vec!["automod"]);
    let sink = Sink::default();
    assert!(svc::run_automod(&e.t, &sink, Some(&s), &edit_of(Some("join discord.gg/abc please"), "join discord.gg/abc thanks", "955006000000000801"), true).await.is_none());
    let sink = Sink::default();
    assert!(svc::run_automod(&e.t, &sink, Some(&s), &edit_of(None, "join discord.gg/abc", "955006000000000802"), true).await.is_some(), "an uncached old message cannot excuse the edit");
    for i in 0..4 {
        assert!(svc::run_automod(&e.t, &Sink::default(), Some(&s), &edit_of(Some("x"), "x", &format!("95500600000000081{i}")), true).await.is_none(), "spam and repeatedText never run on edits");
    }
    assert!(e.t.cache_backend.keys().iter().all(|k| !k.starts_with("automod:")));
}

#[tokio::test]
async fn anti_spam_and_repeated_text_windows_and_redis_down() {
    let e = env(true);
    let spam = settings_row(json!({ "version": 2, "rules": { "spam": { "action": "delete", "threshold": 3 } } }), false);
    let mut deleted = Vec::new();
    for (i, c) in ["a", "b", "c", "d"].iter().enumerate() {
        let (sink, _) = run(&e, &spam, UID, &format!("95500600000000090{i}"), c).await;
        deleted.push(!sink.stops().is_empty());
    }
    assert_eq!(deleted, vec![false, false, true, true], "the third message in the window and every later one");
    // A re-delivered event is never counted twice.
    let (sink, _) = run(&e, &spam, "955006000000000950", "955006000000000901", "x").await;
    assert!(sink.stops().is_empty());

    let rep = settings_row(json!({ "version": 2, "rules": { "repeatedText": { "action": "delete", "threshold": 2 } } }), false);
    let first = run(&e, &rep, "955006000000000960", "955006000000000910", "Buy   now").await.0;
    let second = run(&e, &rep, "955006000000000960", "955006000000000911", "buy NOW").await.0;
    let other = run(&e, &rep, "955006000000000960", "955006000000000912", "something else").await.0;
    assert_eq!([first.stops().len(), second.stops().len(), other.stops().len()], [0, 1, 0]);
    let before = e.t.cache_backend.keys().len();
    run(&e, &rep, "955006000000000960", "955006000000000913", "   ").await;
    assert_eq!(e.t.cache_backend.keys().len(), before, "no window for empty content");
    assert!(e.t.cache_backend.keys().contains(&svc::dup_key(G, "955006000000000960", "buy now")));

    // Redis down: both rules fail open.
    e.t.cache_backend.set_available(false);
    let both = settings_row(json!({ "version": 2, "rules": { "spam": { "action": "ban", "threshold": 3 }, "repeatedText": { "action": "ban", "threshold": 2 } } }), true);
    for i in 0..5 {
        assert!(run(&e, &both, "955006000000000970", &format!("95500600000000092{i}"), "same thing").await.1.is_none());
    }
}

#[tokio::test]
async fn the_hooks_run_through_the_runner_and_set_the_shared_state() {
    let e = env(true);
    e.t.settings_store.put(settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), false));
    e.t.install(registry());
    let message = fake_message("955006000000001000", G, CH, UID, "discord.gg/abc");
    let ctx = e
        .t
        .run_hooks(MessageCreate { guild_id: GuildId::new(G.parse().unwrap_or(1)), member: member_of(&e.t, UID), channel_id: message.channel_id, channel: None, message })
        .await;
    assert!(ctx.stopped());
    assert_eq!(ctx.state.automod_action(), Some(AutomodAction { rule: "invites".into(), deleted: true }));
    assert_eq!(ctx.state.get::<Value>("automodAction"), Some(json!({ "rule": "invites", "deleted": true })));
}

// ── The command against the in-memory settings store ──────────────────────────

const ADMIN: &str = "955006000000000008";

fn cmd_env() -> Env {
    let rest = FakeRest::new();
    let g = guild(true).role("955006000000000022", 3, Permissions::MANAGE_GUILD).member(ADMIN, &["955006000000000022"]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[g], &rest);
    t.install(registry());
    Env { t, rest }
}

async fn slash(e: &Env, group: Option<&str>, sub: &str, options: &[(&str, u8, Value)]) -> bot_core::interaction::RecordedCall {
    let mut f = FakeInteraction::slash("automod").guild(G).user(ADMIN).roles(&["955006000000000022"]).permissions(Permissions::MANAGE_GUILD);
    f = match group {
        Some(g) => f.group(g, sub),
        None => f.subcommand(sub),
    };
    for (n, k, v) in options {
        f = f.option(n, *k, v.clone());
    }
    let before = e.t.responses.calls().len();
    bot_core::router::route_interaction(&e.t, f.build()).await;
    let calls = e.t.responses.calls();
    assert!(calls.len() > before, "an answer");
    calls[calls.len() - 1].clone()
}

fn stored(e: &Env) -> svc::AutomodConfig {
    svc::normalize_config(e.t.settings_store.row(G).and_then(|r| r.automod_config).as_ref())
}

#[tokio::test]
async fn command_round_trip_through_the_settings_row() {
    use bot_core::command::option_type::{CHANNEL, INTEGER, ROLE, STRING};
    let e = cmd_env();
    let r = slash(&e, None, "status", &[]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Auto-Moderation"));
    assert!(r.embed_description().unwrap_or_default().contains("Status:** ❌ Off"));
    assert!(!e.t.responses.calls()[0].ephemeral(), "status is public");

    let r = slash(&e, None, "enable", &[]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Automod Enabled"));
    assert!(r.embed_description().unwrap_or_default().contains("No rules are active yet"));
    assert!(!r.embed_description().unwrap_or_default().contains("No rules are active. Use"));

    let r = slash(&e, None, "rule", &[("rule", STRING, json!("nope")), ("action", STRING, json!("delete"))]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Unknown Rule"));
    let r = slash(&e, None, "rule", &[("rule", STRING, json!("badWords")), ("action", STRING, json!("delete")), ("threshold", INTEGER, json!(5))]).await;
    assert!(r.embed_description().unwrap_or_default().contains("has no threshold"));
    let r = slash(&e, None, "rule", &[("rule", STRING, json!("caps")), ("action", STRING, json!("delete")), ("threshold", INTEGER, json!(20))]).await;
    assert!(r.embed_description().unwrap_or_default().contains("between 50 and 100"));
    let r = slash(&e, None, "rule", &[("rule", STRING, json!("caps")), ("action", STRING, json!("delete")), ("timeout", STRING, json!("10m"))]).await;
    assert!(r.embed_description().unwrap_or_default().contains("only applies to the **Delete & timeout** action"));
    let r = slash(&e, None, "rule", &[("rule", STRING, json!("caps")), ("action", STRING, json!("timeout")), ("timeout", STRING, json!("40d"))]).await;
    assert!(r.embed_description().unwrap_or_default().contains("between 1 minute and 28 days"));
    assert_eq!(stored(&e).rule("caps").action, "off");

    let r = slash(&e, None, "rule", &[
        ("rule", STRING, json!("Excessive Caps")), ("action", STRING, json!("timeout")), ("threshold", INTEGER, json!(80)),
        ("timeout", STRING, json!("2h")), ("roles", STRING, json!("only")), ("channels", STRING, json!("except")),
    ])
    .await;
    assert!(r.embed_title().unwrap_or_default().contains("Rule Updated"));
    assert!(r.embed_description().unwrap_or_default().contains("Delete & timeout · ≥ 80% capitals · 2 hours"));
    assert!(r.embed_description().unwrap_or_default().contains("applies to nobody"));
    let c = stored(&e);
    assert_eq!((c.rule("caps").timeout_seconds, c.rule("caps").threshold, c.rule("caps").role_mode.as_str()), (7200, Some(80), "only"));
    assert!(slash(&e, None, "rule", &[("rule", STRING, json!("caps")), ("action", STRING, json!("off"))]).await.embed_description().unwrap_or_default().contains("is now off"));
    assert!(slash(&e, None, "rule", &[("rule", STRING, json!("badWords")), ("action", STRING, json!("delete_warn"))]).await.embed_description().unwrap_or_default().contains("no words or patterns yet"));

    // words (ephemeral)
    let r = slash(&e, Some("words"), "add", &[("words", STRING, json!("Ass, cunt,a, ass,!!,bad phrase"))]).await;
    let d = r.embed_description().unwrap_or_default().to_string();
    assert!(d.contains("Added **3**: ||ass||, ||cunt||, ||bad phrase||"), "{d}");
    assert!(d.contains("ass (already listed)") && d.contains("a (too short)") && d.contains("!! (no letters or digits)"), "{d}");
    assert_eq!(stored(&e).bad_words, vec!["ass", "cunt", "bad phrase"]);
    let defers: Vec<_> = e.t.responses.calls().into_iter().filter(|c| c.method == "deferReply").collect();
    assert!(defers.last().is_some_and(|c| c.ephemeral()));
    assert!(slash(&e, Some("words"), "add", &[("words", STRING, json!("ass"))]).await.embed_title().unwrap_or_default().contains("Nothing Added"));
    let r = slash(&e, Some("words"), "list", &[]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Bad Words (3)"));
    assert!(slash(&e, Some("words"), "remove", &[("word", STRING, json!("CUNT"))]).await.embed_title().unwrap_or_default().contains("Word Removed"));
    assert!(slash(&e, Some("words"), "remove", &[("word", STRING, json!("zzz"))]).await.embed_title().unwrap_or_default().contains("Not Listed"));
    assert!(slash(&e, Some("words"), "add", &[("words", STRING, json!("_exact_"))]).await.embed_description().unwrap_or_default().contains("||\\_exact\\_||"));

    // patterns
    assert!(slash(&e, Some("patterns"), "add", &[("pattern", STRING, json!("(a+)+$"))]).await.embed_title().unwrap_or_default().contains("Invalid Pattern"));
    assert!(slash(&e, Some("patterns"), "add", &[("pattern", STRING, json!("a*"))]).await.embed_description().unwrap_or_default().contains("matches empty text"));
    let r = slash(&e, Some("patterns"), "add", &[("pattern", STRING, json!("fr[e3]{2}\\s*n[i1]tro"))]).await;
    assert!(r.embed_description().unwrap_or_default().contains("Pattern **#1**"));
    assert!(slash(&e, Some("patterns"), "add", &[("pattern", STRING, json!("fr[e3]{2}\\s*n[i1]tro"))]).await.embed_title().unwrap_or_default().contains("Already Listed"));
    assert!(slash(&e, Some("patterns"), "list", &[]).await.embed_description().unwrap_or_default().contains("**#1** `fr[e3]{2}"));
    assert!(slash(&e, Some("patterns"), "remove", &[("pattern", STRING, json!("1"))]).await.embed_title().unwrap_or_default().contains("Pattern Removed"));
    assert!(slash(&e, Some("patterns"), "remove", &[("pattern", STRING, json!("1"))]).await.embed_title().unwrap_or_default().contains("Not Listed"));

    // domains (public)
    let r = slash(&e, Some("domains"), "add", &[("domain", STRING, json!("https://WWW.Evil.com/x"))]).await;
    let d = r.embed_description().unwrap_or_default().to_string();
    assert!(d.contains("**evil.com** (and its subdomains) is now **blocked**") && d.contains("only blocked domains are removed") && d.contains("External Links** rule is off"), "{d}");
    assert!(slash(&e, Some("domains"), "add", &[("domain", STRING, json!("evil.com")), ("list", STRING, json!("allowed"))]).await.embed_description().unwrap_or_default().contains("removed from the blocked list"));
    assert!(slash(&e, Some("domains"), "add", &[("domain", STRING, json!("evil.com")), ("list", STRING, json!("allowed"))]).await.embed_title().unwrap_or_default().contains("Already Listed"));
    assert!(slash(&e, Some("domains"), "add", &[("domain", STRING, json!("not a domain"))]).await.embed_title().unwrap_or_default().contains("Invalid Domain"));
    let r = slash(&e, Some("domains"), "list", &[]).await;
    assert!(r.body.to_string().contains("evil.com") && r.embed_description().unwrap_or_default().contains("every link is blocked except allowed domains"));
    assert!(slash(&e, Some("domains"), "remove", &[("domain", STRING, json!("evil.com"))]).await.embed_title().unwrap_or_default().contains("Domain Removed"));
    assert!(slash(&e, Some("domains"), "remove", &[("domain", STRING, json!("evil.com"))]).await.embed_title().unwrap_or_default().contains("Not Listed"));

    // exemptions
    assert!(slash(&e, Some("exempt"), "add", &[]).await.embed_title().unwrap_or_default().contains("Nothing Selected"));
    assert!(slash(&e, Some("exempt"), "add", &[("role", ROLE, json!(G))]).await.embed_title().unwrap_or_default().contains("Invalid Role"));
    let r = slash(&e, Some("exempt"), "add", &[("role", ROLE, json!(MOD_ROLE)), ("channel", CHANNEL, json!(CH))]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Exemptions Updated"));
    let c = stored(&e);
    assert_eq!((c.exempt_role_ids.clone(), c.exempt_channel_ids.clone()), (vec![MOD_ROLE.to_string()], vec![CH.to_string()]));
    assert!(slash(&e, Some("exempt"), "add", &[("role", ROLE, json!(MOD_ROLE))]).await.embed_title().unwrap_or_default().contains("No Change"));
    assert!(slash(&e, Some("exempt"), "add", &[("role", ROLE, json!(MOD_ROLE)), ("rule", STRING, json!("links"))]).await.embed_description().unwrap_or_default().contains("External Links** ignores"));
    assert_eq!(stored(&e).rule("links").role_ids, vec![MOD_ROLE]);
    assert!(slash(&e, Some("exempt"), "add", &[("role", ROLE, json!(MOD_ROLE)), ("rule", STRING, json!("nope"))]).await.embed_title().unwrap_or_default().contains("Unknown Rule"));
    assert!(slash(&e, Some("exempt"), "remove", &[("role", ROLE, json!(MOD_ROLE)), ("channel", CHANNEL, json!(CH))]).await.embed_title().unwrap_or_default().contains("Exemptions Updated"));
    assert!(stored(&e).exempt_role_ids.is_empty());

    // honeypot: an existing channel gets the warning and the config.
    let r = slash(&e, Some("honeypot"), "add", &[("channel", CHANNEL, json!(CH)), ("action", STRING, json!("kick"))]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Honeypot Set"));
    assert_eq!((stored(&e).honeypot.channel_ids, stored(&e).honeypot.action), (vec![CH.to_string()], "kick".to_string()));
    let warning = e.rest.calls_to("POST", &format!("/channels/{CH}/messages"));
    assert!(warning.last().is_some_and(|c| c.body["embeds"][0]["description"].as_str().unwrap_or_default().contains("automatically kicked")));
    assert!(slash(&e, Some("honeypot"), "add", &[("timeout", STRING, json!("1h"))]).await.embed_description().unwrap_or_default().contains("only applies with `action:`"));
    assert!(slash(&e, Some("honeypot"), "remove", &[("channel", CHANNEL, json!(LOGCH))]).await.embed_title().unwrap_or_default().contains("Not a Honeypot"));
    assert!(slash(&e, Some("honeypot"), "remove", &[("channel", CHANNEL, json!(CH))]).await.embed_title().unwrap_or_default().contains("Honeypot Removed"));

    assert!(slash(&e, Some("words"), "remove", &[("word", STRING, json!("_exact_"))]).await.embed_description().unwrap_or_default().contains("||\\_exact\\_||"));

    // status shows the configuration; disable keeps the rules.
    let r = slash(&e, None, "status", &[]).await;
    assert!(r.embed_description().unwrap_or_default().contains("Status:** ✅ On"));
    assert!(r.body.to_string().contains("Bad Words** — Delete & warn · 2 words, 0 patterns"));
    assert!(slash(&e, None, "disable", &[]).await.embed_title().unwrap_or_default().contains("Automod Disabled"));
    assert_eq!(stored(&e).rule("badWords").action, "delete_warn");
    assert!(!e.t.settings_store.row(G).is_some_and(|r| r.automod_enabled));
}

#[tokio::test]
async fn a_legacy_config_is_shown_as_such_and_status_stays_within_6000() {
    let e = cmd_env();
    e.t.settings_store.put(bot_db::GuildSettings {
        guild_id: G.into(),
        automod_enabled: true,
        automod_config: Some(json!({ "filterInvites": true, "automodAction": "BAN", "badWords": ["Foo"], "filterBadWords": true })),
        ..Default::default()
    });
    let r = slash(&e, None, "status", &[]).await;
    let body = r.body.to_string();
    assert!(body.contains("old format") && body.contains("Server Invites** — Delete & ban") && body.contains("Bad Words** — Delete & ban · 1 words"));
    slash(&e, Some("words"), "add", &[("words", bot_core::command::option_type::STRING, json!("bar"))]).await;
    let raw = e.t.settings_store.row(G).and_then(|r| r.automod_config).unwrap_or_default();
    assert_eq!(raw["version"], 2);
    assert_eq!(raw["badWords"], json!(["foo", "bar"]));
    assert!(slash(&e, None, "status", &[]).await.body["embeds"][0].get("footer").is_none());

    let ids = |base: u64, n: u64| (0..n).map(|i| format!("955006{:012}", base + i)).collect::<Vec<_>>();
    let rules: serde_json::Map<String, Value> = svc::RULE_KEYS
        .iter()
        .map(|k| (k.to_string(), json!({ "action": "ban", "roleMode": "only", "roleIds": ids(6000, 50), "channelMode": "only", "channelIds": ids(7000, 50) })))
        .collect();
    let mut big = settings_row(
        json!({ "version": 2, "rules": rules, "badWords": (0..500).map(|i| format!("word{i}")).collect::<Vec<_>>(),
            "blockedDomains": (0..200).map(|i| format!("blocked{i}.com")).collect::<Vec<_>>(), "exemptRoleIds": ids(8000, 50), "exemptChannelIds": ids(9000, 50) }),
        true,
    );
    big.mod_log_channel_id = Some(LOGCH.into());
    e.t.settings_store.put(big);
    let r = slash(&e, None, "status", &[]).await;
    let embed = &r.body["embeds"][0];
    let l = |v: &Value| v.as_str().map(bot_util::js::utf16_len).unwrap_or(0);
    let fields = embed["fields"].as_array().cloned().unwrap_or_default();
    let total = l(&embed["title"]) + l(&embed["description"]) + l(&embed["footer"]["text"]) + fields.iter().map(|f| l(&f["name"]) + l(&f["value"])).sum::<usize>();
    assert!(total <= 6000, "{total}");
    assert!(fields.len() <= 25 && fields.iter().all(|f| (1..=1024).contains(&l(&f["value"]))));
    assert!(fields.iter().any(|f| f["name"] == "Rules"));
}

#[tokio::test]
async fn autocomplete_rules_are_synchronous_and_other_fields_answer_from_the_settings() {
    let e = cmd_env();
    let ac = |focus: &str, value: &str| {
        FakeInteraction::autocomplete("automod").guild(G).user(ADMIN).roles(&["955006000000000022"]).permissions(Permissions::MANAGE_GUILD).subcommand("rule").focused(focus, value).build()
    };
    bot_core::router::route_interaction(&e.t, ac("rule", "ca")).await;
    let choices = e.t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(choices.iter().map(|c| c["value"].clone()).collect::<Vec<_>>(), vec![json!("caps")]);
    bot_core::router::route_interaction(&e.t, ac("rule", "excessive")).await;
    let choices = e.t.responses.calls().last().map(|c| c.choices()).unwrap_or_default();
    assert_eq!(choices.iter().map(|c| c["value"].clone()).collect::<Vec<_>>(), vec![json!("caps"), json!("emojis"), json!("spoilers"), json!("mentions")]);
    assert_eq!(cmd::rule_choices("", None).len(), 10);
    let c = cfg(json!({ "version": 2, "badWords": ["alpha", "beta"], "badWordPatterns": ["abc", "x+y"], "blockedDomains": ["evil.com"] }));
    assert_eq!(cmd::list_choices("word", "al", &c).len(), 1);
    assert_eq!(cmd::list_choices("pattern", "2", &c).iter().map(|x| x.value.clone()).collect::<Vec<_>>(), vec![json!("2")]);
    assert_eq!(cmd::list_choices("domain", "", &c).iter().map(|x| x.name.clone().unwrap_or_default()).collect::<Vec<_>>()[0], "evil.com (blocked)");
    assert!(cmd::rule_choices("", Some(&c))[0].name.clone().unwrap_or_default().ends_with("— Off"));
}

#[tokio::test]
async fn collect_problems_lists_what_would_stop_automod() {
    let rest = FakeRest::new();
    let bare = FakeGuild::new(G, OWNER).member(TEST_BOT_ID, &[]).channel(CH, 0, "general", &[]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds_rest(&[bare], &rest);
    let gid = GuildId::new(G.parse().unwrap_or(1));
    let config = cfg(json!({ "version": 2, "rules": { "badWords": { "action": "ban" }, "spam": { "action": "timeout" }, "caps": { "action": "kick", "roleMode": "only" } } }));
    t.cache_backend.set_available(false);
    let s = bot_db::GuildSettings { guild_id: G.into(), automod_enabled: true, ..Default::default() };
    let text = cmd::collect_problems(&t, gid, Some(&s), &config, None).join("\n");
    for needle in ["Manage Messages", "Timeout Members", "Kick Members", "Ban Members", "Redis is unavailable", "Bad Words** has no words or patterns", "Excessive Caps** applies to nobody: its roles mode", "No mod-log channel"] {
        assert!(text.contains(needle), "{needle}");
    }
    let with_log = bot_db::GuildSettings { mod_log_channel_id: Some(LOGCH.into()), ..s.clone() };
    assert_eq!(cmd::collect_problems(&t, gid, Some(&with_log), &svc::normalize_config(None), None), vec![cmd::NO_RULES_PROBLEM]);
    let only = cmd::collect_problems(&t, gid, Some(&bot_db::GuildSettings { automod_enabled: false, ..s.clone() }), &config, Some("caps"));
    assert!(!only.iter().any(|p| p.contains("mod-log") || p.contains("No rules")));
    // The honeypot alone is active; its problems are listed.
    let hp = cfg(json!({ "version": 2, "rules": {}, "honeypot": { "channelIds": [CH, LOGCH] } }));
    let problems = cmd::collect_problems(&t, gid, Some(&with_log), &hp, None);
    assert!(!problems.iter().any(|p| p.contains("No rules are active")));
    assert!(problems.iter().any(|p| p.contains("Ban Members")));
    assert!(problems.iter().any(|p| p.contains("was deleted")));
    assert!(cmd::collect_honeypot_problems(&t, gid, &cfg(json!({ "version": 2, "honeypot": { "channelIds": [CH], "action": "kick" } }))).iter().any(|p| p.contains("Kick Members")));
    assert!(cmd::collect_honeypot_problems(&t, gid, &svc::normalize_config(None)).is_empty());
}

// ── Cross-language: the settings row and the shared Redis window ──────────────

#[tokio::test]
async fn js_written_configs_read_in_rust_and_rust_written_configs_read_in_js() {
    let Some(url) = bot_core::testing::database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    const GX: &str = "955006000000005006";
    let t = bot_core::testing::DbTestApp::new(&url).await.with_guilds([GX]);
    t.reset_guilds(&[GX]).await;
    let raw = json!({ "filterInvites": true, "automodAction": "KICK", "badWords": ["Foo", "Bar baz"], "massMentionThreshold": 7, "exemptRoleIds": [UID] });
    let js = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["cfgwrite", GX, &raw.to_string()]);
    assert!(js.get("error").is_none(), "{js}");
    let s = t.settings().get_settings_with(GX, true).await.expect("settings");
    assert!(s.automod_enabled);
    assert_eq!(s.automod_config.as_ref(), Some(&js["automodConfig"]));
    assert_eq!(svc::get_automod_config(&t, Some(&s)).config.to_json(), js["normalized"]);

    // Rust writes through the command's write_config; the JS reads the same config back.
    let (_, ()) = cmd::write_config(&t, GX, |c, _: &mut ()| {
        c.bad_words.push("qux".into());
        c.rule_mut("caps").action = "timeout".into();
        c.honeypot.channel_ids = vec![CH.into()];
        true
    })
    .await
    .expect("write");
    let back = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["cfgread", GX]);
    let mine = svc::normalize_config(t.settings().get_settings(GX).await.ok().and_then(|s| s.automod_config.clone()).as_ref());
    assert_eq!(back["normalized"], mine.to_json());
    assert_eq!(back["automodConfig"], mine.to_json(), "the stored value is the normalised v2 config");
    assert_eq!(back["normalized"]["badWords"], json!(["foo", "bar baz", "qux"]));
    t.reset_guilds(&[GX]).await;
}

#[tokio::test]
async fn the_anti_spam_window_is_shared_with_the_js() {
    let (Some(url), Some(redis)) = (bot_core::testing::database_url(), bot_core::testing::redis_url()) else {
        eprintln!("skipped: TEST_DATABASE_URL / TEST_REDIS_URL not set");
        return;
    };
    const GX: &str = "955006000000005007";
    let user = "955006000000005107";
    let t = bot_core::testing::DbTestApp::with_redis(&url, &redis).await.with_guilds([GX]);
    t.cache().del_one(&svc::rate_key(GX, user)).await;
    let js = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["spam", GX, user, "3"]);
    assert_eq!(js["key"], svc::rate_key(GX, user));
    // Rust adds the fourth message to the same window: the JS entries are counted.
    let config = cfg(json!({ "version": 2, "rules": { "spam": { "action": "delete", "threshold": 4 } } }));
    let m = AutomodMessage { id: "955006000000005999".into(), guild_id: GX.into(), author_id: user.into(), content: "x".into(), ..Default::default() };
    let hit = svc::check_spam(&t, &m, &config).await;
    assert_eq!(hit.and_then(|h| h.detail), Some("4 messages in 5 s".into()));
    t.cache().del_one(&svc::rate_key(GX, user)).await;
}

#[test]
fn input_of_a_real_message_reads_mentions_snapshots_and_attachments() {
    let raw = json!({
        "id": "955006000000009001", "channel_id": CH, "guild_id": G,
        "author": { "id": UID, "username": "someone", "discriminator": "0", "avatar": null },
        "content": "hey <@955006000000000009>", "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": true, "mentions": [{ "id": "955006000000000009", "username": "x", "discriminator": "0", "avatar": null }],
        "mention_roles": [MOD_ROLE], "attachments": [{ "id": "1", "filename": "SPOILER_a.png", "size": 1, "url": "https://x", "proxy_url": "https://x" }],
        "embeds": [], "pinned": false, "type": 0, "components": [],
        "message_snapshots": [{ "message": { "content": "||s|| fwd", "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "type": 0 } }]
    });
    let message: serenity::all::Message = serde_json::from_value(raw).expect("message");
    let m = AutomodMessage::of(&message, GuildId::new(G.parse().unwrap_or(1)));
    assert_eq!(m.effective_text(), "hey <@955006000000000009>\n||s|| fwd");
    assert_eq!(svc::count_mentions(&m), 3);
    assert_eq!(m.attachments, vec![svc::AttachmentView { spoiler: true }]);
    assert_eq!(m.author_tag, "someone");
    assert!(!m.system);
}

#[tokio::test]
async fn concurrent_word_additions_do_not_lose_updates_and_create_the_row() {
    let Some(url) = bot_core::testing::database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    const GX: &str = "955006000000005008";
    let t = bot_core::testing::DbTestApp::new(&url).await.with_guilds([GX]);
    t.reset_guilds(&[GX]).await; // no settings row: the first write creates it
    let mut handles = Vec::new();
    for w in ["alpha", "bravo", "charlie", "delta"] {
        let app = App::clone(&t);
        handles.push(tokio::spawn(async move {
            cmd::write_config(&app, GX, move |c, _: &mut ()| {
                c.bad_words.push(w.to_string());
                true
            })
            .await
            .map(|_| ())
        }));
    }
    for h in handles {
        h.await.expect("join").expect("write");
    }
    let s = t.settings().get_settings_with(GX, true).await.expect("settings");
    let mut words = svc::normalize_config(s.automod_config.as_ref()).bad_words;
    words.sort();
    assert_eq!(words, vec!["alpha", "bravo", "charlie", "delta"]);
    let js = run_node("rust/fixtures/gen/feat-automod-js.mjs", &["cfgread", GX]);
    assert_eq!(js["normalized"]["badWords"].as_array().map(Vec::len), Some(4));
    t.reset_guilds(&[GX]).await;
}

#[tokio::test]
async fn a_database_failure_is_reported_not_thrown() {
    use bot_core::command::option_type::STRING;
    let e = cmd_env();
    e.t.settings_store.set_available(false);
    let r = slash(&e, None, "rule", &[("rule", STRING, json!("caps")), ("action", STRING, json!("delete"))]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Could Not Save"));
    let r = slash(&e, None, "status", &[]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Settings Unavailable"));
    let r = slash(&e, Some("words"), "list", &[]).await;
    assert!(r.embed_title().unwrap_or_default().contains("Settings Unavailable"));
}

#[tokio::test]
async fn reset_automod_state_clears_the_slots_and_throttles() {
    let e = env(true);
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "delete" } } }), true);
    run(&e, &s, UID, "955006000000001100", "discord.gg/abc").await;
    run(&e, &s, UID, "955006000000001101", "discord.gg/abc").await;
    assert_eq!(mod_log_posts(&e).len(), 1, "throttled");
    svc::reset_automod_state(&e.t);
    assert!(svc::claim_punishment(&e.t, G, UID, 1, e.t.clock().now_ms()), "the slots are cleared");
    run(&e, &s, UID, "955006000000001102", "discord.gg/abc").await;
    assert_eq!(mod_log_posts(&e).len(), 2, "the throttle is cleared");
}

// ── Audit ─────────────────────────────────────────────────────────────────────

/// discord.js `MessageMentions` keeps a mentioned role only when `guild.roles.cache` has it, so `countMentions`
/// (`mentions.roles.size`) never counts an id the guild does not know.
#[tokio::test]
async fn mentioned_roles_count_only_the_guilds_cached_roles() {
    let e = env(true);
    let mut message = fake_message("955006000000000700", G, CH, UID, "<@&955006000000000021> <@&955006000000000099>");
    message.mention_roles = vec![
        serenity::all::RoleId::new(MOD_ROLE.parse().unwrap_or(1)),
        serenity::all::RoleId::new(955_006_000_000_000_099),
        serenity::all::RoleId::new(MOD_ROLE.parse().unwrap_or(1)),
    ];
    let ev = MessageCreate { guild_id: GuildId::new(G.parse().unwrap_or(1)), member: member_of(&e.t, UID), channel_id: message.channel_id, channel: None, message };
    let inp = AutomodInput::from_create(&e.t, &ev);
    assert_eq!(inp.message.mention_role_count, 1, "the unknown role id is not a mention");
    assert_eq!(svc::count_mentions(&inp.message), 1);
}

/// `buildReason(outcome, channel?.name ?? 'unknown')`: a channel automod only knows by its id is "#unknown", never "#".
#[tokio::test]
async fn an_uncached_channel_is_named_unknown_in_the_reason() {
    let e = env(true);
    let seen: Arc<Mutex<Vec<feat_moderation::moderation_service::ModActionInput>>> = Arc::default();
    let rec = seen.clone();
    svc::set_mod_executor(
        &e.t,
        Arc::new(move |_app, input| {
            rec.lock().unwrap_or_else(|e| e.into_inner()).push(input);
            Box::pin(async { Err(feat_moderation::moderation_service::ModError::new(feat_moderation::moderation_service::ModCode::DiscordError, "boom")) })
        }),
    );
    let s = settings_row(json!({ "version": 2, "rules": { "invites": { "action": "warn" } } }), false);
    let message = fake_message("955006000000000710", G, "955006000000000098", UID, "discord.gg/abc");
    let ev = MessageCreate { guild_id: GuildId::new(G.parse().unwrap_or(1)), member: member_of(&e.t, UID), channel_id: message.channel_id, channel: None, message };
    let inp = AutomodInput::from_create(&e.t, &ev);
    svc::run_automod(&e.t, &Sink::default(), Some(&s), &inp, false).await;
    let calls = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].reason.as_deref(), Some("[Automod] Server Invites: invite discord.gg/abc in #unknown"));
}

/// `/automod honeypot add channel:<not cached>`: discord.js checked the picked channel object from the interaction, so
/// the warning is posted; Rust fetches the channel (`guild.channels.fetch`) instead of calling it unusable.
#[tokio::test]
async fn honeypot_add_checks_a_picked_channel_that_is_not_cached() {
    use bot_core::command::option_type::CHANNEL;
    const LATE: &str = "955006000000000097";
    let e = cmd_env();
    e.rest.on("GET", &format!("/channels/{LATE}"), 200, json!({ "id": LATE, "type": 0, "guild_id": G, "name": "trap", "position": 0, "permission_overwrites": [] }));
    let r = slash(&e, Some("honeypot"), "add", &[("channel", CHANNEL, json!(LATE))]).await;
    let d = r.embed_description().unwrap_or_default().to_string();
    assert!(d.contains("A warning is posted in it."), "{d}");
    assert!(!d.contains("The warning was not posted"), "{d}");
    assert_eq!(e.rest.calls_to("POST", &format!("/channels/{LATE}/messages")).len(), 1);
}
