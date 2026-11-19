//! Ported from test/foundation.test.js and test/registry.test.js (the config-table cases).

use bot_config::*;
use serde_json::json;
use std::collections::HashSet;

// ── MODULES, DEFAULT_MODULE_SETTINGS and the schema agree ────────────────────

#[test]
fn modules_defaults_and_schema_agree() {
    let mut a: Vec<&str> = MODULES.iter().map(|m| m.key.as_str()).collect();
    let mut b: Vec<&str> = DEFAULT_MODULE_SETTINGS.iter().map(|(k, _)| k.as_str()).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../prisma/schema.prisma");
    let schema = std::fs::read_to_string(path).expect("read prisma/schema.prisma");
    let start = schema.find("model GuildSettings {").expect("model GuildSettings");
    let body = &schema[start..];
    let body = &body[..body.find("\n}").expect("end of GuildSettings")];
    for m in &MODULES {
        assert_eq!(m.default, default_module_setting(m.key), "{} default", m.key);
        // `\n\s+<flag>\s+Boolean\s+@default\((true|false)\)`
        let decl = body.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            if words.next() != Some(m.flag) || words.next() != Some("Boolean") {
                return None;
            }
            match words.next() {
                Some("@default(true)") => Some(true),
                Some("@default(false)") => Some(false),
                _ => None,
            }
        });
        let decl = decl.unwrap_or_else(|| panic!("{} is declared in GuildSettings as Boolean @default(...)", m.flag));
        assert_eq!(decl, m.default, "{} schema default", m.flag);
    }
    assert_eq!(ModuleKey::Leveling.flag(), "xpEnabled");
    assert_eq!(module_flags().find(|(k, _)| *k == "leveling"), Some(("leveling", "xpEnabled")));
    let flags: HashSet<&str> = MODULES.iter().map(|m| m.flag).collect();
    assert_eq!(flags.len(), MODULE_NAMES.len(), "flags are unique");
    assert_eq!(MODULE_NAMES.len(), 42);
}

// ── isModuleEnabled, getModule and isModuleKey ───────────────────────────────

#[test]
fn is_module_enabled_semantics() {
    let none: Option<&serde_json::Value> = None;
    let empty = json!({});
    assert!(is_module_enabled_in(Some(&empty), None), "null key is core");
    assert!(is_module_enabled_in(none, None), "a null key is core");
    assert!(!is_module_enabled_in(none, Some("moderation")), "null settings");
    assert!(!is_module_enabled_in(Some(&json!({ "modEnabled": true })), Some("nope")), "unknown key");
    assert!(is_module_enabled_in(Some(&json!({ "modEnabled": true })), Some("moderation")));
    assert!(!is_module_enabled_in(Some(&json!({ "modEnabled": false })), Some("moderation")));
    assert!(!is_module_enabled_in(Some(&json!({ "modEnabled": 1 })), Some("moderation")), "strictly true");
    assert!(is_module_enabled_in(Some(&json!({ "xpEnabled": true })), Some("leveling")));
    assert!(!is_module_enabled_in(Some(&json!({ "levelingEnabled": true })), Some("leveling")), "levelingEnabled stays unread");

    // Closure form: the lookup is not called for a null key, and receives the flag column.
    assert!(is_module_enabled(None, |_| panic!("not called for a null key")));
    assert!(is_module_enabled(Some("leveling"), |flag| (flag == "xpEnabled").then_some(true)));
    assert!(!is_module_enabled(Some("moderation"), |_| None));
    assert!(!is_module_enabled(Some("toString"), |_| Some(true)));
    assert!(is_module_key_enabled(None, |_| None));
    assert!(is_module_key_enabled(Some(ModuleKey::Games), |f| (f == "gamesEnabled").then_some(true)));
    assert!(!is_module_key_enabled(Some(ModuleKey::Games), |_| Some(false)));

    assert!(get_module("toString").is_none(), "no prototype keys");
    assert_eq!(get_module("games").unwrap().flag, "gamesEnabled");
    assert!(is_module_key("games"));
    assert!(!is_module_key("constructor"));
    assert!(!is_module_key("Games"), "case-sensitive");
}

#[test]
fn string_enums_round_trip() {
    for k in ModuleKey::ALL {
        assert_eq!(k.as_str().parse::<ModuleKey>().unwrap(), k);
        assert_eq!(serde_json::to_value(k).unwrap(), json!(k.as_str()));
        assert_eq!(serde_json::from_value::<ModuleKey>(json!(k.as_str())).unwrap(), k);
        assert_eq!(k.to_string(), k.as_str());
    }
    for j in JobType::ALL {
        assert_eq!(j.as_str().parse::<JobType>().unwrap(), j);
        assert_eq!(serde_json::from_value::<JobType>(json!(j.as_str())).unwrap(), j);
    }
    for e in HookPoint::ALL {
        assert_eq!(e.as_str().parse::<HookPoint>().unwrap(), e);
    }
    assert!("nope".parse::<ModuleKey>().is_err());
    assert!(serde_json::from_value::<JobType>(json!("toString")).is_err());
    assert_eq!(JobType::LevelsVoiceSweep.as_str(), "levels.voiceSweep");
    assert_eq!(ComponentType::from_name("select"), Some(ComponentType::Select));
    assert_eq!(CustomIdKind::from_name("field"), Some(CustomIdKind::Field));
}

// ── HOOK_ORDER ids are well-formed and hookIndex works ───────────────────────

fn is_hook_part(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[test]
fn hook_order_well_formed_and_hook_index() {
    for (event, ids) in &HOOK_ORDER {
        let set: HashSet<&&str> = ids.iter().collect();
        assert_eq!(set.len(), ids.len(), "{event}: no duplicate ids");
        for id in *ids {
            // ^[a-z0-9-]+(:[a-z0-9-]+)?$
            let ok = match id.split_once(':') {
                Some((a, b)) => is_hook_part(a) && is_hook_part(b),
                None => is_hook_part(id),
            };
            assert!(ok, "{event}: {id}");
        }
    }
    assert_eq!(INTERNAL_HOOK_POINTS.map(|e| e.as_str()), ["levelUp", "economyPurchase"]);
    assert_eq!(SYNTHETIC_HOOK_POINTS.map(|e| e.as_str()), ["guildReady"]);
    assert!(HookPoint::LevelUp.is_internal() && HookPoint::GuildReady.is_synthetic());
    assert_eq!(hook_index("messageCreate", "automod"), Some(0));
    assert_eq!(hook_index("messageCreate", "custom-commands:triggers"), Some(7));
    assert_eq!(hook_index("messageCreate", "nope"), None);
    assert_eq!(hook_index("noSuchEvent", "automod"), None);
    assert_eq!(hook_index("toString", "automod"), None);
}

// ── CUSTOM_ID_PREFIXES and buildCustomId ─────────────────────────────────────

#[test]
fn custom_id_prefixes_well_formed() {
    let mut seen = HashSet::new();
    for p in &CUSTOM_ID_PREFIXES {
        // ^[a-z][a-z0-9-]*$
        let mut b = p.prefix.bytes();
        assert!(b.next().is_some_and(|c| c.is_ascii_lowercase()), "{}", p.prefix);
        assert!(b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'), "{}", p.prefix);
        assert!(!p.owner.is_empty(), "{}", p.prefix);
        assert!(!p.types.is_empty(), "{}", p.prefix);
        assert!(seen.insert(p.prefix), "duplicate {}", p.prefix);
    }
}

#[test]
fn build_and_parse_custom_ids() {
    assert_eq!(CUSTOM_ID_MAX_LENGTH, 100);
    assert_eq!(build_custom_id("rr", &["g1", "r2"]).unwrap(), "rr:g1:r2");
    assert_eq!(build_custom_id::<&str>("noop", &[]).unwrap(), "noop");
    assert_eq!(custom_id!("noop").unwrap(), "noop");
    assert_eq!(custom_id!("rr", 123u64, "r2").unwrap(), "rr:123:r2");
    assert_eq!(parse_custom_id("rr:g1:r2"), ParsedCustomId { prefix: "rr", params: vec!["g1", "r2"] });
    assert_eq!(parse_custom_id("noop"), ParsedCustomId { prefix: "noop", params: vec![] });
    assert_eq!(parse_custom_id(""), ParsedCustomId { prefix: "", params: vec![] });
    assert_eq!(parse_custom_id("a::b:"), ParsedCustomId { prefix: "a", params: vec!["", "b", ""] });
    assert_eq!(prefix_info("rr").unwrap().owner, "reaction-roles");
    assert!(prefix_info("nope").is_none());
    assert!(prefix_info("toString").is_none());

    let err = |r: Result<String, CustomIdError>| r.unwrap_err().to_string();
    assert_eq!(err(build_custom_id::<&str>("nope", &[])), "Unknown customId prefix \"nope\"");
    assert_eq!(err(build_custom_id::<&str>("no-such-prefix", &[])), "Unknown customId prefix \"no-such-prefix\"");
    assert_eq!(err(build_custom_id("welcome-title", &["x"])), "customId prefix \"welcome-title\" is a field id and takes no params");
    assert_eq!(err(build_custom_id("rr", &["a:b"])), "customId param \"a:b\" contains ':'");
    let long = "x".repeat(100);
    assert_eq!(err(build_custom_id("rr", &[long.as_str()])), "customId is 103 characters (max 100)");
    assert_eq!(build_custom_id("rr", &["x".repeat(96)]).unwrap().len(), 99);
    assert_eq!(build_custom_id("rr", &["x".repeat(97)]).unwrap().len(), 100);
    // Length counts UTF-16 code units like JS: an emoji outside the BMP counts 2.
    assert!(build_custom_id("rr", &["😀".repeat(48)]).is_ok()); // 3 + 96 = 99
    assert!(build_custom_id("rr", &["😀".repeat(49)]).is_err()); // 3 + 98 = 101
    assert_eq!(build_custom_id::<&str>("welcome-title", &[]).unwrap(), "welcome-title");
}

// ── JOB_TYPES ────────────────────────────────────────────────────────────────

#[test]
fn job_types_well_formed() {
    for j in &JOB_TYPES {
        // ^[a-z][a-z0-9-]*\.[a-z][a-zA-Z0-9.-]*$
        let t = j.job_type.as_str();
        let (head, tail) = t.split_once('.').unwrap_or_else(|| panic!("{t}"));
        let mut h = head.bytes();
        assert!(h.next().is_some_and(|c| c.is_ascii_lowercase()) && h.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'), "{t}");
        let mut tl = tail.bytes();
        assert!(tl.next().is_some_and(|c| c.is_ascii_lowercase()) && tl.all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-'), "{t}");
        assert!(!j.owner.is_empty(), "{t}: owner");
    }
    assert!(RECURRING_JOB_TYPES.contains(&JobType::FeedPoll) && !RECURRING_JOB_TYPES.contains(&JobType::ModUnban));
    assert!(is_job_type("mod.unban"));
    assert!(!is_job_type("toString"));
    assert_eq!(INTERACTIVE_JOB_TYPES, [JobType::DashboardTask]);
}
