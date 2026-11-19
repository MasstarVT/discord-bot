//! `GuildSettings: Default` against what Prisma wrote for a row created with only `guildId`, and the `ModuleFlags`
//! lookup on a typed row.

use bot_config::ModuleFlags;
use bot_db::GuildSettings;
use serde_json::Value;

fn baseline_min() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/mockdb/baseline.json");
    let all: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("baseline.json")).expect("JSON");
    all["GuildSettings"]["min"].clone()
}

#[test]
fn default_matches_prismas_defaults() {
    let min = baseline_min();
    let ours = serde_json::to_value(GuildSettings::default()).unwrap();
    let theirs = min.as_object().expect("the min row");
    let mut checked = 0;
    for (k, v) in theirs {
        if matches!(k.as_str(), "id" | "guildId" | "createdAt" | "updatedAt") {
            continue;
        }
        assert_eq!(&ours[k], v, "default of {k}");
        checked += 1;
    }
    assert!(checked > 100, "{checked} columns compared");
    let d = GuildSettings::default();
    assert!(bot_db::is_cuid(&d.id));
    assert!(d.guild_id.is_empty());
    let custom = GuildSettings { guild_id: "1".into(), xp_rate: 40, ..Default::default() };
    assert_eq!(custom.xp_rate, 40);
}

#[test]
fn module_flags_read_the_typed_row() {
    let s = GuildSettings { xp_enabled: true, mod_enabled: false, ..Default::default() };
    let as_json = serde_json::to_value(&s).unwrap();
    for m in bot_config::MODULES.iter() {
        assert_eq!(s.module_flag(m.flag), as_json[m.flag].as_bool(), "flag {}", m.flag);
    }
    assert_eq!(s.module_flag("xpEnabled"), Some(true));
    assert_eq!(s.module_flag("modEnabled"), Some(false));
    assert_eq!(s.module_flag("timezone"), None);
    assert_eq!(s.module_flag("nope"), None);
    assert!(bot_config::is_module_enabled_in(Some(&s), Some("leveling")));
}
