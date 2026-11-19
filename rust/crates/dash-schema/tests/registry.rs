//! The settings registry: the cases of test/dashboard.test.js ("settings registry", "settings registry agrees with the
//! bot services", the catalog checks that come from fields.js), checks against the Rust foundation (LIMITS, MODULES,
//! the Prisma data model), the hook table, JS number semantics, and a database test: the GuildSettings rows the JS
//! toolchain seeded, read through bot-db, give what the JS readSettingsValues gives for the rows Prisma reads back.

use std::path::PathBuf;

use bot_config::constants::limits;
use bot_config::modules::{MODULE_NAMES, get_module};
use dash_schema::fields::{self, constants, registry, settings_field};
use dash_schema::schema::{self, Field};
use dash_schema::{check_cross_fields, read_settings_values, to_stored_changes, validate_settings_changes};
use serde_json::{Map, Value, json};

const CH_TEXT: &str = "300000000000000001";
const RL_LOW: &str = "400000000000000001";
const RL_MID: &str = "400000000000000003";

fn golden(name: &str) -> Value {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden").join(name);
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))).unwrap()
}

fn valid(changes: Value) -> Map<String, Value> {
    match validate_settings_changes(&json!({ "changes": changes })) {
        Ok(v) => v.changes,
        Err(e) => panic!("refused {changes}: {} at {:?}", e.message, e.path),
    }
}

/// The GuildSettings fields of the Prisma data model: name -> DMMF field.
fn guild_settings_columns() -> (Map<String, Value>, Map<String, Value>) {
    let dmmf = golden("prisma-dmmf.json");
    let model = dmmf["models"].as_array().unwrap().iter().find(|m| m["name"] == "GuildSettings").unwrap();
    let columns = model["fields"].as_array().unwrap().iter().filter(|f| f["kind"] != "object").map(|f| (f["name"].as_str().unwrap().to_string(), f.clone())).collect();
    let enums = dmmf["enums"].as_array().unwrap().iter().map(|e| (e["name"].as_str().unwrap().to_string(), e["values"].clone())).collect();
    (columns, enums)
}

fn all_groups() -> Vec<(String, &'static fields::Group)> {
    let mut out: Vec<(String, &'static fields::Group)> = fields::general_groups().iter().map(|g| ("general".to_string(), g)).collect();
    for (key, groups) in &registry().module_groups {
        out.extend(groups.iter().map(|g| (key.clone(), g)));
    }
    out
}

#[test]
fn every_group_lints_clean_and_every_field_sits_in_exactly_one_group() {
    let mut keys: Vec<&str> = registry().module_groups.keys().map(String::as_str).collect();
    let mut names: Vec<&str> = MODULE_NAMES.to_vec();
    keys.sort_unstable();
    names.sort_unstable();
    assert_eq!(keys, names);
    let mut seen: Vec<(String, String)> = Vec::new();
    for (where_, g) in all_groups() {
        assert_eq!(schema::lint_descriptors(&g.fields, &format!("{where_}/{}/", g.id)), Vec::<String>::new(), "{where_}/{}", g.id);
        assert!(!g.id.is_empty() && g.id.chars().next().unwrap().is_ascii_lowercase() && g.id.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
        assert!(!g.title.is_empty() && !g.fields.is_empty(), "{where_}/{}", g.id);
        for d in &g.fields {
            assert!(!seen.iter().any(|(f, _)| f == d.field()), "{} is in two groups", d.field());
            seen.push((d.field().to_string(), format!("{where_}/{}", g.id)));
            assert!(d.description().is_some_and(|s| s.encode_utf16().count() >= 10), "{} needs a description", d.field());
            assert_eq!(settings_field(d.field()), Some(d));
        }
    }
    for (key, groups) in &registry().module_groups {
        let ids: std::collections::HashSet<&str> = groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids.len(), groups.len(), "{key}: duplicate group id");
    }
    // Module flags are the page switch, not a group field; everything else is in a group.
    let flags: Vec<&str> = MODULE_NAMES.iter().map(|k| get_module(k).unwrap().flag).collect();
    let mut all: Vec<String> = seen.iter().map(|(f, _)| f.clone()).chain(flags.iter().map(|f| f.to_string())).collect();
    let mut ours: Vec<String> = registry().settings_fields.keys().cloned().collect();
    all.sort();
    ours.sort();
    assert_eq!(ours, all);
    for flag in &flags {
        assert_eq!(settings_field(flag).unwrap().ty(), "boolean");
    }
    let general: Vec<&str> = fields::general_groups()[0].fields.iter().map(Field::field).collect();
    assert_eq!(general, ["timezone", "embedColor", "customPrefix"]);
}

#[test]
fn module_flags_mirror_the_module_registry() {
    for key in MODULE_NAMES {
        let m = get_module(key).unwrap();
        let d = settings_field(m.flag).unwrap();
        assert_eq!(d.label(), Some(m.label), "{key}");
        assert_eq!(d.default_value(), Some(&json!(m.default)), "{key}");
        assert_eq!(d.description(), Some(format!("Turns the {} module on or off.", m.label).as_str()));
        assert_eq!(fields::settings_column(m.flag), Some(m.flag));
    }
}

#[test]
fn every_field_is_stored_in_a_real_guild_settings_column_of_a_matching_type() {
    let (columns, enums) = guild_settings_columns();
    for (field, desc) in &registry().settings_fields {
        let column = fields::settings_column(field).unwrap();
        let col = columns.get(column).unwrap_or_else(|| panic!("{field} -> {column} is not a GuildSettings column"));
        let (ty, list, optional) = (col["type"].as_str().unwrap(), col["isList"] == true, col["isRequired"] != true);
        if field == "logEvents" {
            assert!(list && ty == "String" && desc.ty() == "multiEnum");
            continue;
        }
        if column != field {
            assert_eq!(ty, "Json", "{field}: a virtual field must live in a Json column");
            continue;
        }
        let t = desc.ty();
        if ty == "Json" {
            assert!(["object", "objectList", "embed", "template"].contains(&t), "{field}: Json column, {t} field");
        } else if list {
            assert!(["channels", "roles", "multiEnum", "stringList"].contains(&t), "{field}: list column, {t} field");
        } else if ty == "Boolean" {
            assert_eq!(t, "boolean", "{field}");
        } else if ty == "Float" {
            assert_eq!(t, "float", "{field}");
        } else if ty == "Int" {
            assert!(t == "int" || (t == "color" && desc.string("format") == Some("int")), "{field}");
        } else if let Some(values) = enums.get(ty) {
            assert_eq!(t, "enum", "{field}");
            let mut ours: Vec<String> = desc.option_values().iter().map(|v| v.as_str().unwrap().to_string()).collect();
            let mut theirs: Vec<String> = values.as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap().to_string()).collect();
            ours.sort();
            theirs.sort();
            assert_eq!(ours, theirs, "{field}: options = enum {ty}");
        } else {
            assert_eq!(ty, "String", "{field}");
            assert!(["text", "timezone", "channel", "role", "emoji", "url"].contains(&t), "{field}: String column, {t} field");
        }
        if !optional {
            assert!(!desc.nullable(), "{field}: the column is not nullable");
        }
    }
}

#[test]
fn descriptor_defaults_match_the_schema_defaults() {
    let (columns, _) = guild_settings_columns();
    let mut checked = 0;
    for (field, desc) in &registry().settings_fields {
        if fields::settings_column(field) != Some(field.as_str()) {
            continue;
        }
        let col = &columns[field.as_str()];
        let Some(def) = col.get("default") else { continue };
        // Like the JS test: Boolean, Int, Float, String and enum columns (a Json @default is its text).
        if col["isList"] == true || def.is_object() || col["type"] == "Json" || col["type"] == "DateTime" {
            continue;
        }
        checked += 1;
        let ours = desc.default_value().unwrap();
        assert_eq!(ours.as_f64().map_or(ours.clone(), |n| json!(n)), def.as_f64().map_or(def.clone(), |n| json!(n)), "{field}");
    }
    assert!(checked >= 60, "only {checked} defaults checked");
}

#[test]
fn every_column_is_editable_or_not_exposed_and_bot_managed_state_never_is_editable() {
    let (columns, _) = guild_settings_columns();
    let exposed: std::collections::HashSet<&str> = registry().settings_columns.values().map(String::as_str).collect();
    let not_exposed = &registry().not_exposed;
    for column in columns.keys() {
        let hidden = not_exposed.contains_key(column);
        assert_ne!(exposed.contains(column.as_str()), hidden, "{column}");
        if hidden {
            assert!(not_exposed[column].len() > 5, "{column}: give a reason");
        }
    }
    for column in not_exposed.keys() {
        assert!(columns.contains_key(column), "{column} is not a column");
    }
    for field in ["captchaMessageId", "captchaLockdownPrior", "captchaChannelId", "levelingEnabled", "xpRate", "guildId", "id", "automodConfig"] {
        assert!(settings_field(field).is_none(), "{field} must not be a field");
    }
    for c in &registry().json_columns {
        assert_eq!(columns[c.as_str()]["type"], "Json", "{c}");
        assert!(fields::is_json_column(c));
    }
}

#[test]
fn list_caps_come_from_limits() {
    let caps: [(&str, usize); 16] = [
        ("modRoleIds", limits::MOD_ROLES),
        ("immuneRoleIds", limits::IMMUNE_ROLES),
        ("infractionEscalation", limits::ESCALATION_TIERS),
        ("autoroleIds", limits::AUTOROLES),
        ("noXpRoleIds", limits::NOXP_ROLES),
        ("noXpChannelIds", limits::NOXP_CHANNELS),
        ("xpBoostRoles", limits::XP_BOOST_ROLES),
        ("timezoneRoles", limits::TIMEZONE_ROLES),
        ("logIgnoredChannelIds", limits::LOG_IGNORED_CHANNELS),
        ("economyGameChannelIds", limits::ECONOMY_GAME_CHANNELS),
        ("automodBadWords", limits::AUTOMOD_WORDS),
        ("automodBadWordPatterns", limits::AUTOMOD_PATTERNS),
        ("automodBlockedDomains", limits::AUTOMOD_DOMAINS),
        ("automodAllowedDomains", limits::AUTOMOD_DOMAINS),
        ("automodExemptRoleIds", limits::AUTOMOD_EXEMPTIONS),
        ("automodExemptChannelIds", limits::AUTOMOD_EXEMPTIONS),
    ];
    for (field, max) in caps {
        assert_eq!(settings_field(field).unwrap().number("maxItems"), Some(max as f64), "{field}");
    }
    for (key, rule) in &constants().automod_rules {
        let d = settings_field(&fields::rule_field_name(key)).unwrap_or_else(|| panic!("{key}"));
        assert_eq!(d.child("roleIds").unwrap().number("maxItems"), Some(limits::AUTOMOD_EXEMPTIONS as f64));
        assert_eq!(d.child("threshold").is_some(), rule.threshold.is_some(), "{key} threshold");
    }
    assert_eq!(constants().automod_honeypot.max_channels as usize, limits::AUTOMOD_HONEYPOTS);
    assert_eq!(settings_field("automodHoneypot").unwrap().child("channelIds").unwrap().number("maxItems"), Some(limits::AUTOMOD_HONEYPOTS as f64));
    for field in ["economyStartingBalance", "economyMaxBalance", "economyMaxBet", "economyMaxPayout"] {
        assert_eq!(settings_field(field).unwrap().number("max"), Some(f64::from(bot_config::constants::COIN_MAX)), "{field}");
    }
    // Roles the bot hands out must pass the assignability check.
    for field in ["autoroleIds", "muteRoleId", "captchaRoleId", "birthdayRoleId", "liveRoleId"] {
        assert!(settings_field(field).unwrap().assignable(), "{field}");
    }
    for field in ["modRoleIds", "immuneRoleIds", "noXpRoleIds", "liveRoleRequiredRoleId", "liveRoleAnnouncePingRoleId"] {
        assert!(!settings_field(field).unwrap().assignable(), "{field}");
    }
}

#[test]
fn message_fields_offer_their_extra_placeholders() {
    assert_eq!(settings_field("levelUpMessage").unwrap().get("placeholders"), Some(&json!(["{player}", "{player.name}", "{level}", "{rank}", "{xp}", "{mention}"])));
    assert_eq!(settings_field("birthdayMessageAge").unwrap().get("placeholders"), Some(&json!(["{age}"])));
    let ach = settings_field("achievementMessage").unwrap().get("placeholders").unwrap().as_array().unwrap();
    for t in ["{player}", "{achievement}", "{tier}", "{rewards}"] {
        assert!(ach.contains(&json!(t)), "{t}");
    }
}

#[test]
fn public_descriptors_carry_no_server_hooks_and_survive_json() {
    for (field, desc) in &registry().settings_fields {
        let public = schema::public_descriptor(desc);
        let round: Value = serde_json::from_str(&public.to_string()).unwrap();
        assert_eq!(round.to_string(), public.to_string(), "{field}");
        for key in ["read", "write", "check", "normalize"] {
            assert!(public.get(key).is_none(), "{field}.{key}");
        }
    }
    // A descriptor built in Rust keeps its key order and loses server-only keys.
    let d = Field::from_json(json!({ "field": "x", "type": "object", "label": "X", "check": "server", "fields": [
        { "field": "a", "type": "int", "min": 0, "max": 5, "label": "A", "normalize": true },
    ], "default": {} }))
    .unwrap();
    assert_eq!(
        schema::public_descriptor(&d).to_string(),
        r#"{"field":"x","type":"object","label":"X","fields":[{"field":"a","type":"int","min":0,"max":5,"label":"A"}],"default":{}}"#
    );
}

#[test]
fn the_hooks_are_exactly_those_of_the_js_descriptors() {
    for field in registry().settings_fields.keys() {
        let ours = fields::hook_names(field);
        let theirs: Vec<&str> = registry().js_hooks.get(field).map(|l| l.iter().map(String::as_str).collect()).unwrap_or_default();
        assert_eq!(ours, theirs, "{field}");
    }
}

#[test]
fn one_request_can_carry_every_field() {
    assert!(read_settings_values(None).len() <= fields::max_settings_changes());
    assert_eq!(fields::max_settings_changes(), 300);
}

#[test]
fn read_settings_values_every_field_defaults_and_effective_texts_for_an_empty_row() {
    let v = read_settings_values(None);
    let keys: Vec<&String> = v.keys().collect();
    assert_eq!(keys, registry().settings_fields.keys().collect::<Vec<_>>());
    let c = constants();
    assert_eq!(v["timezone"], "UTC");
    assert_eq!(v["customPrefix"], "!");
    assert_eq!(v["embedColor"], Value::Null);
    assert_eq!(v["welcomeMessage"], c.default_texts["welcomeMessage"].as_str());
    assert_eq!(v["levelUpMessage"], c.default_texts["levelUpMessage"].as_str());
    assert_eq!(v["welcomeEmbed"]["title"], c.default_embeds["welcomeEmbed"]["title"]);
    assert_eq!(v["welcomeDmEmbed"], Value::Null);
    assert_eq!(v["automodAllowedDomains"], json!(c.automod_defaults.allowed_domains));
    assert_eq!(v["automodRuleCaps"]["threshold"], 70);
    assert_eq!(v["automodRuleCaps"]["action"], "off");
    assert!(v["automodRuleInvites"].get("threshold").is_none());
    assert_eq!(v["captchaTemplate"]["embed"]["title"], c.default_captcha_panel["embed"]["title"]);
    assert_eq!(v["captchaVerifyButton"], json!({ "label": "Verify", "style": "SUCCESS" }));
    assert_eq!(v["rankCardStyle"], c.default_rank_card_style);
    assert_eq!(v["welcomeCardStyle"]["enabled"], true);
    assert_eq!(v["infractionEscalation"], json!([]));
    assert_eq!(v["economyMaxBalance"], 1_000_000_000);
}

#[test]
fn read_settings_values_stored_shapes_legacy_text_rules_and_junk() {
    let row = json!({
        "welcomeMessage": null, "welcomeEmbed": { "title": "Custom", "card": false }, "welcomeUseEmbed": true,
        "goodbyeMessage": null, "goodbyeUseEmbed": true, "levelUpMessage": "   ",
        "disabledLogEvents": ["voiceJoin", "notAnEvent", "messageDelete"], "timezone": "Not/AZone", "currencyEmoji": "x",
        "automodConfig": { "version": 2, "rules": { "caps": { "action": "kick", "threshold": 80 } }, "badWords": ["ok", "x", "fine word"], "allowedDomains": [] },
        "infractionEscalation": [{ "id": 2, "warnCount": 5, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": null },
            { "warnCount": 3, "action": "BAN", "duration": 600 }],
        "captchaTemplate": { "content": "Press it", "embed": null, "buttons": [], "verifyButton": { "label": "Go", "style": "DANGER" } },
    });
    let v = read_settings_values(Some(&row));
    assert_eq!(v["welcomeMessage"], "", "a custom embed without stored text sends no text line");
    assert_eq!(v["welcomeCardStyle"]["enabled"], false, "the legacy card flag");
    assert_eq!(v["goodbyeMessage"], "");
    assert_eq!(v["levelUpMessage"], constants().default_texts["levelUpMessage"].as_str());
    let events: Vec<&String> = constants().log_events.keys().filter(|k| *k != "voiceJoin" && *k != "messageDelete").collect();
    assert_eq!(v["logEvents"], json!(events));
    assert_eq!(v["timezone"], "UTC");
    assert_eq!(v["currencyEmoji"], "🪙");
    assert_eq!(v["automodRuleCaps"]["action"], "kick");
    assert_eq!(v["automodRuleCaps"]["threshold"], 80);
    assert_eq!(v["automodBadWords"], json!(["ok", "fine word"]), "one bad entry never hides the rest");
    assert_eq!(v["automodAllowedDomains"], json!([]), "an explicit empty allowlist stays empty");
    assert_eq!(
        v["infractionEscalation"],
        json!([
            { "warnCount": 3, "withinSeconds": null, "action": "TEMPBAN", "durationSeconds": 600 },
            { "warnCount": 5, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
        ])
    );
    assert_eq!(v["captchaTemplate"]["content"], "Press it");
    assert_eq!(v["captchaVerifyButton"], json!({ "label": "Go", "style": "DANGER" }));
}

#[test]
fn validate_settings_changes_normalises_like_the_commands() {
    let r = validate_settings_changes(&json!({ "changes": {
        "timezone": "america/new_york", "customPrefix": "OFF", "embedColor": "#5865f2", "xpMultiplier": 1.234,
        "automodBadWords": ["  BAD   word ", "bad word", "ｆｕｌｌ"], "automodBlockedDomains": ["https://www.Example.com/x", "example.com"],
        "logEvents": ["voiceJoin", "messageDelete"], "currencyName": "gold    bars", "levelUpMessage": "  ", "birthdayMessage": "  Hi {user}  ",
        "welcomeMessage": "  keep spaces  ", "welcomeEmbed": { "title": "", "description": "" },
        "welcomeCardStyle": { "enabled": true, "background": "#AABBCC", "backgroundUrl": null, "textColor": null, "accentColor": null, "overlayOpacity": 0.456 },
        "xpBoostRoles": [{ "roleId": RL_LOW, "multiplier": 1.255 }],
        "timezoneRoles": [{ "roleId": RL_MID, "zone": "america/chicago" }, { "roleId": RL_LOW, "zone": "Asia/Calcutta" }],
    } }))
    .unwrap();
    let c = &r.changes;
    assert_eq!(c["timezone"], "America/New_York");
    assert_eq!(c["customPrefix"], "");
    assert_eq!(c["embedColor"], 0x5865f2);
    assert_eq!(c["xpMultiplier"], 1.23);
    assert_eq!(c["automodBadWords"], json!(["bad word", "full"]));
    assert_eq!(c["automodBlockedDomains"], json!(["example.com"]));
    assert_eq!(c["logEvents"], json!(["messageDelete", "voiceJoin"]));
    let stored = to_stored_changes(&valid(json!({ "logEvents": c["logEvents"] })), None);
    let off: Vec<&String> = constants().log_events.keys().filter(|k| *k != "messageDelete" && *k != "voiceJoin").collect();
    assert_eq!(stored["disabledLogEvents"], json!(off), "stored as the switched-off list");
    assert_eq!(c["currencyName"], "gold bars");
    assert_eq!(c["levelUpMessage"], Value::Null, "blank = the default text");
    assert_eq!(c["birthdayMessage"], "Hi {user}");
    assert_eq!(c["welcomeMessage"], "  keep spaces  ", "welcome texts are stored as typed");
    assert_eq!(c["welcomeEmbed"], Value::Null, "an empty embed means the default");
    assert_eq!(c["welcomeCardStyle"]["background"], "#AABBCC");
    assert_eq!(c["xpBoostRoles"], json!([{ "roleId": RL_LOW, "multiplier": 1.25 }]));
    assert_eq!(c["timezoneRoles"], json!([{ "roleId": RL_MID, "zone": "America/Chicago" }, { "roleId": RL_LOW, "zone": "Asia/Kolkata" }]));
    let mut roles: Vec<(String, String)> = r.refs.roles.iter().map(|x| (x.id.clone(), x.path.clone())).collect();
    roles.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        roles,
        [
            (RL_MID.to_string(), "timezoneRoles.0.roleId".to_string()),
            (RL_LOW.to_string(), "timezoneRoles.1.roleId".to_string()),
            (RL_LOW.to_string(), "xpBoostRoles.0.roleId".to_string()),
        ]
    );
}

#[test]
fn validate_settings_changes_refuses_what_the_commands_refuse_naming_the_path() {
    let tier = |o: Value| {
        let mut t = json!({ "warnCount": 3, "withinSeconds": null, "action": "TIMEOUT", "durationSeconds": 600 });
        t.as_object_mut().unwrap().extend(o.as_object().unwrap().clone());
        t
    };
    let mod_roles: Vec<String> = (0..26).map(|i| (400_000_000_000_000_100_u64 + i).to_string()).collect();
    let zones: Vec<Value> = (0..26).map(|i| json!({ "roleId": format!("7000000000000000{i:02}"), "zone": "UTC" })).collect();
    let tiers: Vec<Value> = (0..11).map(|i| tier(json!({ "warnCount": i + 1 }))).collect();
    let cases: Vec<(Value, &str)> = vec![
        (json!({ "xpMin": 0 }), "xpMin"),
        (json!({ "xpMax": 1001 }), "xpMax"),
        (json!({ "xpCooldown": 4 }), "xpCooldown"),
        (json!({ "voiceXpRate": 51 }), "voiceXpRate"),
        (json!({ "birthdayHour": 24 }), "birthdayHour"),
        (json!({ "autoroleDelay": 604_801 }), "autoroleDelay"),
        (json!({ "captchaTimeoutMinutes": 3 }), "captchaTimeoutMinutes"),
        (json!({ "dailyMax": 1_000_001 }), "dailyMax"),
        (json!({ "economyMaxBalance": 0 }), "economyMaxBalance"),
        (json!({ "embedColor": 0 }), "embedColor"),
        (json!({ "customPrefix": "a b" }), "customPrefix"),
        (json!({ "customPrefix": "!!!!!!" }), "customPrefix"),
        (json!({ "customPrefix": "<@" }), "customPrefix"),
        (json!({ "timezone": "EST" }), "timezone"),
        (json!({ "timezone": "+02:00" }), "timezone"),
        (json!({ "currencyName": "a*b" }), "currencyName"),
        (json!({ "currencyName": "" }), "currencyName"),
        (json!({ "currencyEmoji": "x" }), "currencyEmoji"),
        (json!({ "currencyEmoji": "1️⃣" }), "currencyEmoji"),
        (json!({ "achievementMessage": "{rewards}" }), "achievementMessage"),
        (json!({ "levelUpMessage": "x".repeat(1001) }), "levelUpMessage"),
        (json!({ "welcomeMessage": "x".repeat(2001) }), "welcomeMessage"),
        (json!({ "automodBadWords": ["fine", "!"] }), "automodBadWords.1"),
        (json!({ "automodBadWordPatterns": [".*"] }), "automodBadWordPatterns.0"),
        (json!({ "automodBadWordPatterns": ["ok", "(a+)+$"] }), "automodBadWordPatterns.1"),
        (json!({ "automodBlockedDomains": ["not a domain"] }), "automodBlockedDomains.0"),
        (json!({ "automodRuleCaps": { "action": "off", "threshold": 49 } }), "automodRuleCaps.threshold"),
        (json!({ "automodRuleInvites": { "action": "kick", "threshold": 3 } }), "automodRuleInvites.threshold"),
        (json!({ "automodRuleSpam": { "action": "explode" } }), "automodRuleSpam.action"),
        (json!({ "automodRuleSpam": { "timeoutSeconds": 59 } }), "automodRuleSpam.timeoutSeconds"),
        (json!({ "logEvents": ["messageDelete", "nope"] }), "logEvents.1"),
        (json!({ "infractionEscalation": [tier(json!({ "action": "KICK", "durationSeconds": 60 }))] }), "infractionEscalation.0.durationSeconds"),
        (json!({ "infractionEscalation": [tier(json!({ "durationSeconds": null }))] }), "infractionEscalation.0.durationSeconds"),
        (json!({ "infractionEscalation": [tier(json!({ "durationSeconds": 2_419_201 }))] }), "infractionEscalation.0.durationSeconds"),
        (json!({ "infractionEscalation": [tier(json!({ "action": "TEMPBAN", "durationSeconds": null }))] }), "infractionEscalation.0.durationSeconds"),
        (json!({ "infractionEscalation": [tier(json!({})), tier(json!({ "action": "BAN", "durationSeconds": null }))] }), "infractionEscalation.1.warnCount"),
        (json!({ "infractionEscalation": [tier(json!({ "withinSeconds": 59 }))] }), "infractionEscalation.0.withinSeconds"),
        (json!({ "infractionEscalation": tiers }), "infractionEscalation"),
        (json!({ "xpBoostRoles": [{ "roleId": RL_LOW, "multiplier": 2 }, { "roleId": RL_LOW, "multiplier": 3 }] }), "xpBoostRoles.1.roleId"),
        (json!({ "xpBoostRoles": [{ "roleId": RL_LOW, "multiplier": 1 }] }), "xpBoostRoles.0.multiplier"),
        (json!({ "timezoneRoles": [{ "roleId": RL_LOW, "zone": "UTC" }, { "roleId": RL_LOW, "zone": "Asia/Tokyo" }] }), "timezoneRoles.1.roleId"),
        (json!({ "timezoneRoles": [{ "roleId": RL_LOW, "zone": "EST" }] }), "timezoneRoles.0.zone"),
        (json!({ "timezoneRoles": [{ "roleId": RL_LOW, "zone": "Mars/Olympus" }] }), "timezoneRoles.0.zone"),
        (json!({ "timezoneRoles": zones }), "timezoneRoles"),
        (json!({ "welcomeCardStyle": { "overlayOpacity": 1.5 } }), "welcomeCardStyle.overlayOpacity"),
        (json!({ "welcomeCardStyle": { "backgroundUrl": "ftp://x.example/a.png" } }), "welcomeCardStyle.backgroundUrl"),
        (json!({ "welcomeCardStyle": { "v": 1 } }), "welcomeCardStyle.v"),
        (json!({ "rankCardStyle": { "overlayOpacity": 0.95 } }), "rankCardStyle.overlayOpacity"),
        (json!({ "rankCardStyle": { "backgroundUrl": format!("https://x.example/{}", "a".repeat(500)) } }), "rankCardStyle.backgroundUrl"),
        (json!({ "captchaVerifyButton": { "label": "", "style": "SUCCESS" } }), "captchaVerifyButton.label"),
        (json!({ "captchaVerifyButton": { "label": "Go", "style": "PINK" } }), "captchaVerifyButton.style"),
        (json!({ "captchaTemplate": { "content": null, "embed": null, "buttons": [] } }), "captchaTemplate"),
        (json!({ "welcomeChannelId": null }), "welcomeChannelId"),
        (json!({ "welcomeChannelId": "general" }), "welcomeChannelId"),
        (json!({ "modRoleIds": mod_roles }), "modRoleIds"),
        (json!({ "levelingEnabled": true }), "levelingEnabled"),
        (json!({ "captchaChannelId": CH_TEXT }), "captchaChannelId"),
    ];
    for (changes, path) in cases {
        let e = validate_settings_changes(&json!({ "changes": changes })).expect_err(&format!("accepted {changes}"));
        assert_eq!(e.path.as_deref(), Some(path), "{changes}: {}", e.message);
        assert!(!e.message.is_empty());
    }
    let many: Map<String, Value> = (0..=fields::max_settings_changes()).map(|i| (format!("f{i}"), json!(1))).collect();
    for body in [
        Value::Null,
        json!([]),
        json!({}),
        json!({ "changes": [] }),
        json!({ "changes": {} }),
        json!({ "changes": { "modEnabled": true }, "extra": 1 }),
        serde_json::from_str(r#"{"changes":{"__proto__":true}}"#).unwrap(),
        json!({ "changes": many }),
    ] {
        assert!(validate_settings_changes(&body).is_err(), "{}", body.to_string().chars().take(80).collect::<String>());
    }
}

#[test]
fn check_cross_fields_enforces_the_multi_column_rules_only_for_changed_fields() {
    let base = json!({ "xpMin": 15, "xpMax": 25, "dailyMin": 50, "dailyMax": 120, "workMin": 100, "workMax": 300, "economyStartingBalance": 1000,
        "economyMaxBalance": 1_000_000_000, "levelUpMode": "CURRENT", "levelUpChannelId": null, "achievementAnnounceMode": "CURRENT",
        "achievementChannelId": null, "modRoleIds": [RL_MID], "immuneRoleIds": [], "muteRoleId": null, "liveRoleId": RL_LOW, "liveRoleRequiredRoleId": null });
    let check = |patch: Value, changes: Value| {
        let mut row = base.clone();
        row.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
        check_cross_fields(Some(&row), Some(changes.as_object().unwrap())).map(|p| p.path)
    };
    assert_eq!(check(json!({}), json!({})), None);
    assert_eq!(check(json!({ "xpMin": 30 }), json!({ "xpMin": 30 })).as_deref(), Some("xpMin"));
    assert_eq!(check(json!({ "xpMax": 10 }), json!({ "xpMax": 10 })).as_deref(), Some("xpMax"));
    assert_eq!(check(json!({ "xpMin": 30, "xpMax": 40 }), json!({ "xpMin": 30, "xpMax": 40 })), None);
    assert_eq!(check(json!({ "xpMin": 30 }), json!({ "modEnabled": false })), None, "an old inconsistency never blocks an unrelated save");
    assert_eq!(check(json!({ "dailyMin": 200 }), json!({ "dailyMin": 200 })).as_deref(), Some("dailyMin"));
    assert_eq!(check(json!({ "workMax": 50 }), json!({ "workMax": 50 })).as_deref(), Some("workMax"));
    assert_eq!(check(json!({ "economyMaxBalance": 500 }), json!({ "economyMaxBalance": 500 })).as_deref(), Some("economyMaxBalance"));
    assert_eq!(check(json!({ "levelUpMode": "CHANNEL" }), json!({ "levelUpMode": "CHANNEL" })).as_deref(), Some("levelUpMode"));
    assert_eq!(check(json!({ "levelUpMode": "CHANNEL", "levelUpChannelId": CH_TEXT }), json!({ "levelUpMode": "CHANNEL", "levelUpChannelId": CH_TEXT })), None);
    assert_eq!(check(json!({ "achievementAnnounceMode": "CHANNEL" }), json!({ "achievementAnnounceMode": "CHANNEL" })).as_deref(), Some("achievementAnnounceMode"));
    assert_eq!(check(json!({ "captchaRoleId": RL_MID }), json!({ "captchaRoleId": RL_MID })).as_deref(), Some("captchaRoleId"), "a moderator role");
    assert_eq!(check(json!({ "captchaRoleId": RL_LOW, "muteRoleId": RL_LOW }), json!({ "captchaRoleId": RL_LOW })).as_deref(), Some("captchaRoleId"), "the mute role");
    assert_eq!(check(json!({ "captchaRoleId": RL_LOW }), json!({ "captchaRoleId": RL_LOW })), None);
    assert_eq!(check(json!({ "liveRoleRequiredRoleId": RL_LOW }), json!({ "liveRoleRequiredRoleId": RL_LOW })).as_deref(), Some("liveRoleRequiredRoleId"));
    let mute = json!([{ "warnCount": 3, "withinSeconds": null, "action": "MUTE", "durationSeconds": null }]);
    assert_eq!(check(json!({ "infractionEscalation": mute }), json!({ "infractionEscalation": mute })).as_deref(), Some("infractionEscalation.0.action"));
    assert_eq!(check(json!({ "infractionEscalation": mute, "muteRoleId": RL_LOW }), json!({ "infractionEscalation": mute })), None);
    let short = json!([{ "warnCount": 3, "withinSeconds": null, "action": "MUTE", "durationSeconds": 3600 }]);
    assert_eq!(check(json!({ "infractionEscalation": short }), json!({ "infractionEscalation": short })), None, "a short mute is a timeout");
    let both = json!({ "version": 2, "blockedDomains": ["a.com"], "allowedDomains": ["a.com"] });
    assert_eq!(
        check(json!({ "automodConfig": both }), json!({ "automodBlockedDomains": ["a.com"], "automodAllowedDomains": ["a.com"] })).as_deref(),
        Some("automodAllowedDomains")
    );
}

#[test]
fn to_stored_changes_merges_json_parts_into_the_stored_row() {
    let row = json!({
        "automodConfig": { "version": 2, "rules": { "caps": { "action": "kick", "threshold": 80 } }, "badWords": ["old"], "allowedDomains": ["tenor.com", "b.com"],
            "honeypot": { "channelIds": [CH_TEXT], "action": "ban", "timeoutSeconds": 600 } },
        "infractionEscalation": [
            { "id": 4, "warnCount": 3, "withinSeconds": null, "action": "TIMEOUT", "durationSeconds": 600, "countFrom": "2026-01-01T00:00:00.000Z" },
            { "id": 7, "warnCount": 9, "withinSeconds": null, "action": "BAN", "durationSeconds": null, "countFrom": null },
        ],
        "captchaTemplate": { "content": "Hello", "embed": null, "buttons": [], "verifyButton": { "label": "Go", "style": "DANGER" } },
    });
    let row_obj = row.as_object().unwrap().clone();
    let row = Some(&row);

    // Escalation: a tier with the same warnings and window keeps its id and countFrom, new ones get the next id; sorted.
    let tiers = to_stored_changes(
        &valid(json!({ "infractionEscalation": [
            { "warnCount": 5, "withinSeconds": 3600, "action": "BAN", "durationSeconds": null },
            { "warnCount": 3, "withinSeconds": null, "action": "KICK", "durationSeconds": null },
        ] })),
        row,
    );
    assert_eq!(
        tiers["infractionEscalation"],
        json!([
            { "id": 4, "warnCount": 3, "withinSeconds": null, "action": "KICK", "durationSeconds": null, "countFrom": "2026-01-01T00:00:00.000Z" },
            { "id": 8, "warnCount": 5, "withinSeconds": 3600, "action": "BAN", "durationSeconds": null, "countFrom": null },
        ])
    );

    // Automod: one rule changes, the rest of the config stays; a blocked domain leaves the allowlist.
    let rule = json!({ "action": "delete", "threshold": 75, "timeoutSeconds": 600, "roleMode": "except", "roleIds": [], "channelMode": "only", "channelIds": [CH_TEXT] });
    let stored = to_stored_changes(&valid(json!({ "automodRuleCaps": rule, "automodBlockedDomains": ["b.com"] })), row);
    assert_eq!(stored.keys().collect::<Vec<_>>(), ["automodConfig"]);
    let cfg = &stored["automodConfig"];
    assert_eq!(cfg["rules"]["caps"], rule);
    assert_eq!(cfg["rules"]["spam"]["action"], "off");
    assert_eq!(cfg["badWords"], json!(["old"]));
    assert_eq!(cfg["blockedDomains"], json!(["b.com"]));
    assert_eq!(cfg["allowedDomains"], json!(["tenor.com"]), "moved to the blocked list");
    assert_eq!(cfg["version"], 2);
    assert_eq!(cfg["honeypot"], json!({ "channelIds": [CH_TEXT], "action": "ban", "timeoutSeconds": 600 }), "saving another automod field keeps the honeypot");
    let honeypot = json!({ "channelIds": [], "action": "timeout", "timeoutSeconds": 3600 });
    let hp = to_stored_changes(&valid(json!({ "automodHoneypot": honeypot })), row);
    assert_eq!(hp["automodConfig"]["honeypot"], honeypot);
    assert_eq!(hp["automodConfig"]["badWords"], json!(["old"]));
    let together = to_stored_changes(&valid(json!({ "automodBlockedDomains": ["b.com"], "automodAllowedDomains": ["b.com"] })), row);
    let mut merged = row_obj.clone();
    merged.extend(together);
    let changes = json!({ "automodBlockedDomains": [], "automodAllowedDomains": [] });
    assert!(check_cross_fields(Some(&Value::Object(merged)), Some(changes.as_object().unwrap())).is_some());

    // Captcha: the message and the Verify button share the column without overwriting each other.
    let button = &to_stored_changes(&valid(json!({ "captchaVerifyButton": { "label": "Verify me", "style": "PRIMARY" } })), row)["captchaTemplate"];
    assert_eq!(button["content"], "Hello");
    assert_eq!(button["verifyButton"], json!({ "label": "Verify me", "style": "PRIMARY" }));
    let message = &to_stored_changes(&valid(json!({ "captchaTemplate": { "content": "New text", "embed": null, "buttons": [] } })), row)["captchaTemplate"];
    assert_eq!(message["content"], "New text");
    assert_eq!(message["verifyButton"], json!({ "label": "Go", "style": "DANGER" }));

    // Card styles and embeds are stored the way the commands write them.
    let card = &to_stored_changes(
        &valid(json!({ "welcomeCardStyle": {
            "enabled": false, "background": "#AABBCC", "backgroundUrl": "https://img.example/bg.png", "textColor": null, "accentColor": "#112233", "overlayOpacity": 0.5,
        } })),
        None,
    )["welcomeCardStyle"];
    assert_eq!(
        card.to_string(),
        json!({ "v": 1, "enabled": false, "background": "#aabbcc", "backgroundUrl": "https://img.example/bg.png", "textColor": null,
            "accentColor": "#112233", "overlayOpacity": 0.5 })
        .to_string()
    );
    let embed = to_stored_changes(&valid(json!({ "welcomeEmbed": { "title": "Hi", "description": "there", "footer": { "text": "f", "iconUrl": null }, "timestamp": false } })), None);
    assert_eq!(embed["welcomeEmbed"], json!({ "title": "Hi", "description": "there", "footer": { "text": "f" } }));
    let mut rank = constants().default_rank_card_style.clone();
    rank["overlayOpacity"] = json!(0.33);
    assert_eq!(to_stored_changes(&valid(json!({ "rankCardStyle": { "overlayOpacity": 0.333 } })), None)["rankCardStyle"], rank);
}

#[test]
fn numbers_follow_js_semantics() {
    // 20.0 is the integer 20 in JS, and is written without a fraction.
    let c = valid(json!({ "xpMin": 20.0, "xpMultiplier": 2.0, "embedColor": 255.0 }));
    assert_eq!(Value::Object(c).to_string(), r#"{"xpMin":20,"xpMultiplier":2,"embedColor":255}"#);
    assert!(validate_settings_changes(&json!({ "changes": { "xpMin": 20.5 } })).is_err());
    // Object.keys puts array-index keys first: the first problem is the unknown "5".
    let body: Value = serde_json::from_str(r#"{"changes":{"xpMin":0,"5":1}}"#).unwrap();
    assert_eq!(validate_settings_changes(&body).unwrap_err().path.as_deref(), Some("5"));
    assert_eq!(dash_schema::js::string_to_number(" 600 "), 600.0);
    assert_eq!(dash_schema::js::string_to_number("0x10"), 16.0);
    assert!(dash_schema::js::string_to_number("6e").is_nan());
    assert_eq!(dash_schema::js::round2(1.005), 1.0);
    assert_eq!(dash_schema::js::round2(2.675), 2.68);
}

#[test]
fn an_empty_required_text_says_so() {
    let d = Field::from_json(json!({ "field": "name", "type": "text", "label": "Name", "maxLength": 50 })).unwrap();
    let e = schema::validate_value(&d, &Value::Null, None, &mut dash_schema::Refs::default()).unwrap_err();
    assert_eq!(e.message, "\"Name\" cannot be empty.");
}

#[test]
fn restated_constants_agree_with_the_foundation() {
    let c = constants();
    assert_eq!(c.tier_actions.iter().map(|a| a.value.as_str()).collect::<Vec<_>>(), ["TIMEOUT", "MUTE", "KICK", "TEMPBAN", "BAN"]);
    assert_eq!(c.automod_honeypot.max_channels as usize, limits::AUTOMOD_HONEYPOTS);
    assert_eq!(fields::automod_rule_keys().len(), 10);
    assert_eq!(c.number_limits.timeout_max_seconds, 2_419_200);
    assert_eq!(c.automod_defaults.max_timeout_seconds, c.number_limits.timeout_max_seconds);
    assert_eq!(c.log_events.len(), 32);
    // The legacy zone names of /settings timezone (bot-util restates the same table).
    for (old, new) in bot_util::timezones::LEGACY_TZ_NAMES {
        assert_eq!(fields::canonical_zone(old), *new, "{old}");
    }
    assert_eq!(fields::canonical_zone("etc/gmt"), "UTC");
    assert_eq!(fields::canonical_zone("europe/berlin"), "Europe/Berlin");
    // The verify button colours are the captcha service's BUTTON_STYLES.
    let styles: Vec<&str> =
        settings_field("captchaVerifyButton").unwrap().child("style").unwrap().option_values().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(styles, ["SUCCESS", "PRIMARY", "SECONDARY", "DANGER"]);
    // The modules with nothing but their flag have no groups.
    assert_eq!(fields::module_groups("reactionroles").unwrap().len(), 0);
}

// ── Database: JS-seeded rows, read through bot-db ───────────────────────────────────────────────────────────

/// `validateSettingsChanges({ changes: readSettingsValues(row) })` and the columns it would store.
fn resave(row: &Value) -> Value {
    match validate_settings_changes(&json!({ "changes": read_settings_values(Some(row)) })) {
        Ok(v) => json!({ "ok": true, "stored": to_stored_changes(&v.changes, Some(row)) }),
        Err(e) => json!({ "ok": false, "message": e.message, "path": e.path }),
    }
}

#[test]
fn resaving_what_was_read_matches_the_js_for_every_sample_row() {
    let rows = golden("dash-schema/rows.json");
    for (name, case) in rows.as_object().unwrap() {
        assert_eq!(resave(&case["row"]).to_string(), case["resave"].to_string(), "{name}");
    }
}

#[tokio::test]
async fn seeded_rows_read_through_bot_db_give_what_the_js_reads() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    let pool = bot_db::client::connect(&url).await.expect("connect");
    let rows = golden("dash-schema/rows.json");
    for (name, guild) in [("seedMin", "900000000000000001"), ("seedFull", "900000000000000002")] {
        let gs: bot_db::models::GuildSettings = sqlx::query_as(r#"SELECT * FROM "GuildSettings" WHERE "guildId" = $1"#)
            .bind(guild)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("{guild}: {e}"));
        let row = dash_schema::row_json(&gs);
        let values = Value::Object(read_settings_values(Some(&row)));
        assert_eq!(values.to_string(), rows[name]["values"].to_string(), "{name}: readSettingsValues");
        let refs = dash_schema::collect_settings_refs(Some(&row));
        assert_eq!(json!({ "channels": refs.channels, "roles": refs.roles }), rows[name]["refs"], "{name}: refs");
        assert_eq!(json!(dash_schema::collect_settings_ref_keys(Some(&row))), rows[name]["refKeys"], "{name}: ref keys");
        // Saving every value as read back: the same answer and the same columns as the JS.
        assert_eq!(resave(&row).to_string(), rows[name]["resave"].to_string(), "{name}: resave");
    }
    pool.close().await;
}
