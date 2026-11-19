//! Every exported value of src/config/*.js against rust/fixtures/golden/config.json (generated from the JS).
//! Values are compared as serialized JSON strings so object key order is checked too.

use bot_config::*;
use serde_json::{Map, Value, json};

fn golden() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/golden/config.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("read config.json")).expect("parse config.json")
}

fn assert_same(section: &str, name: &str, rust: Value) {
    let g = golden();
    let expected = &g[section][name];
    assert!(!expected.is_null(), "{section}.{name} missing from the golden file");
    assert_eq!(
        serde_json::to_string(&rust).unwrap(),
        serde_json::to_string(expected).unwrap(),
        "{section}.{name} differs from the JS"
    );
}

#[test]
fn golden_has_no_untested_exports() {
    let g = golden();
    let tested: &[(&str, &[&str])] = &[
        ("constants", &["COIN_MAX", "COLORS", "DEFAULT_MODULE_SETTINGS", "LIMITS", "PERMISSION_LEVELS", "XP_DEFAULTS", "XP_MAX"]),
        ("customIds", &["CUSTOM_ID_MAX_LENGTH", "CUSTOM_ID_PREFIXES"]),
        ("hookPoints", &["HOOK_ORDER", "INTERNAL_HOOK_POINTS", "SYNTHETIC_HOOK_POINTS"]),
        ("jobTypes", &["INTERACTIVE_JOB_TYPES", "JOB_TYPES", "RECURRING_JOB_TYPES"]),
        ("modules", &["MODULES", "MODULE_FLAGS", "MODULE_NAMES"]),
    ];
    let sections: Vec<&str> = g.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(sections, tested.iter().map(|(s, _)| *s).collect::<Vec<_>>());
    for (section, names) in tested {
        let keys: Vec<&str> = g[*section].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(&keys, names, "{section}: exported names");
    }
}

// ── constants.js ─────────────────────────────────────────────────────────────

#[test]
fn constants_scalars() {
    assert_same("constants", "XP_MAX", json!(XP_MAX));
    assert_same("constants", "COIN_MAX", json!(COIN_MAX));
}

#[test]
fn constants_colors() {
    let m: Map<String, Value> = COLORS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    assert_same("constants", "COLORS", Value::Object(m));
    assert_eq!(colors::SUCCESS, 0x57f287);
}

#[test]
fn constants_permission_levels() {
    let m: Map<String, Value> = PermissionLevel::ALL.iter().map(|l| (l.as_str().to_string(), json!(l.as_u8()))).collect();
    assert_same("constants", "PERMISSION_LEVELS", Value::Object(m));
    assert!(PermissionLevel::Owner > PermissionLevel::Admin && PermissionLevel::Moderator > PermissionLevel::Member);
    for l in PermissionLevel::ALL {
        assert_eq!(PermissionLevel::from_u8(l.as_u8()), Some(l));
        assert_eq!(PermissionLevel::from_name(l.as_str()), Some(l));
    }
    assert_eq!(PermissionLevel::from_u8(4), None);
}

#[test]
fn constants_limits() {
    let m: Map<String, Value> = LIMITS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    assert_same("constants", "LIMITS", Value::Object(m));
    assert_eq!(limits::SAVED_EMBEDS, 500);
    assert_eq!(limits::COUNTING_MATH_LENGTH, 100);
}

#[test]
fn constants_default_module_settings() {
    let m: Map<String, Value> = DEFAULT_MODULE_SETTINGS.iter().map(|(k, v)| (k.as_str().to_string(), json!(v))).collect();
    assert_same("constants", "DEFAULT_MODULE_SETTINGS", Value::Object(m));
    assert!(default_module_setting(ModuleKey::Moderation));
    assert!(!default_module_setting(ModuleKey::Automod));
}

#[test]
fn constants_xp_defaults_and_formula() {
    let d = XP_DEFAULTS;
    assert_same(
        "constants",
        "XP_DEFAULTS",
        json!({ "rate": d.rate, "min": d.min, "max": d.max, "cooldown": d.cooldown, "levelUpChannel": d.level_up_channel }),
    );
    // xpForLevel = 5 * level^2 + 50 * level + 100
    assert_eq!(xp_for_level(0), 100);
    assert_eq!(xp_for_level(1), 155);
    assert_eq!(xp_for_level(10), 1100);
    assert_eq!(xp_for_level(100), 55100);
}

// ── customIds.js ─────────────────────────────────────────────────────────────

#[test]
fn custom_id_registry() {
    assert_same("customIds", "CUSTOM_ID_MAX_LENGTH", json!(CUSTOM_ID_MAX_LENGTH));
    let m: Map<String, Value> = CUSTOM_ID_PREFIXES
        .iter()
        .map(|p| {
            let types: Vec<&str> = p.types.iter().map(|t| t.as_str()).collect();
            (p.prefix.to_string(), json!({ "owner": p.owner, "types": types, "kind": p.kind.as_str() }))
        })
        .collect();
    assert_same("customIds", "CUSTOM_ID_PREFIXES", Value::Object(m));
}

// ── hookPoints.js ────────────────────────────────────────────────────────────

#[test]
fn hook_order() {
    let m: Map<String, Value> = HOOK_ORDER.iter().map(|(e, ids)| (e.as_str().to_string(), json!(ids))).collect();
    assert_same("hookPoints", "HOOK_ORDER", Value::Object(m));
    let internal: Vec<&str> = INTERNAL_HOOK_POINTS.iter().map(|e| e.as_str()).collect();
    assert_same("hookPoints", "INTERNAL_HOOK_POINTS", json!(internal));
    let synthetic: Vec<&str> = SYNTHETIC_HOOK_POINTS.iter().map(|e| e.as_str()).collect();
    assert_same("hookPoints", "SYNTHETIC_HOOK_POINTS", json!(synthetic));
    // The enum and the table are in the same order (HookPoint::order indexes by discriminant).
    for (i, e) in HookPoint::ALL.iter().enumerate() {
        assert_eq!(HOOK_ORDER[i].0, *e);
        assert_eq!(e.order(), HOOK_ORDER[i].1);
    }
}

// ── jobTypes.js ──────────────────────────────────────────────────────────────

#[test]
fn job_types() {
    let m: Map<String, Value> = JOB_TYPES
        .iter()
        .map(|j| {
            let mut o = Map::new();
            o.insert("owner".into(), json!(j.owner));
            o.insert("recurring".into(), json!(j.recurring));
            o.insert("dedupe".into(), json!(j.dedupe));
            if j.interactive {
                o.insert("interactive".into(), json!(true));
            }
            (j.job_type.as_str().to_string(), Value::Object(o))
        })
        .collect();
    assert_same("jobTypes", "JOB_TYPES", Value::Object(m));
    let rec: Vec<&str> = RECURRING_JOB_TYPES.iter().map(|j| j.as_str()).collect();
    assert_same("jobTypes", "RECURRING_JOB_TYPES", json!(rec));
    let inter: Vec<&str> = INTERACTIVE_JOB_TYPES.iter().map(|j| j.as_str()).collect();
    assert_same("jobTypes", "INTERACTIVE_JOB_TYPES", json!(inter));
    for (i, j) in JobType::ALL.iter().enumerate() {
        assert_eq!(JOB_TYPES[i].job_type, *j);
        assert_eq!(j.info().job_type, *j);
    }
    // RECURRING/INTERACTIVE are the filtered keys, in JOB_TYPES order.
    let derived: Vec<JobType> = JobType::ALL.into_iter().filter(|j| j.is_recurring()).collect();
    assert_eq!(derived, RECURRING_JOB_TYPES.to_vec());
    let derived: Vec<JobType> = JobType::ALL.into_iter().filter(|j| j.is_interactive()).collect();
    assert_eq!(derived, INTERACTIVE_JOB_TYPES.to_vec());
}

// ── modules.js ───────────────────────────────────────────────────────────────

#[test]
fn modules_registry() {
    let m: Map<String, Value> = MODULES
        .iter()
        .map(|m| {
            (
                m.key.as_str().to_string(),
                json!({ "flag": m.flag, "label": m.label, "emoji": m.emoji, "mee6": m.mee6, "configCommand": m.config_command, "default": m.default }),
            )
        })
        .collect();
    assert_same("modules", "MODULES", Value::Object(m));
    // Guard: object key order is really preserved (serde_json `preserve_order`), so the string compares check order.
    let g = golden();
    let first: Vec<&str> = g["modules"]["MODULES"].as_object().unwrap().keys().take(2).map(String::as_str).collect();
    assert_eq!(first, ["moderation", "automod"]);
    assert_same("modules", "MODULE_NAMES", json!(MODULE_NAMES.as_slice()));
    let flags: Map<String, Value> = module_flags().map(|(k, f)| (k.to_string(), json!(f))).collect();
    assert_same("modules", "MODULE_FLAGS", Value::Object(flags));
    for (i, k) in ModuleKey::ALL.iter().enumerate() {
        assert_eq!(MODULES[i].key, *k);
        assert_eq!(k.info().key, *k);
    }
}
