//! The generated models against Prisma's own data model (prisma-dmmf.json, read at test time) and against what the JS
//! bot's JSON.stringify writes (rust/fixtures/golden/bot-db/*.json, produced by rust/fixtures/gen/bot-db-json.mjs).

use bot_db::schema::{ClientDefault, ScalarKind};
use bot_db::serde_js::{format_js_date, from_epoch_ms, parse_js_date};
use bot_db::{ENUMS, GuildSettings, MODELS, Model, model_info, reserialize_json};
use serde_json::{Value, json};

fn golden(path: &str) -> Value {
    let p = format!("{}/../../fixtures/golden/{path}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p}: {e}"))).expect("valid JSON")
}

fn dmmf() -> Value {
    golden("prisma-dmmf.json")
}

fn scalar_fields(model: &Value) -> Vec<&Value> {
    model["fields"].as_array().unwrap().iter().filter(|f| f["kind"] != "object").collect()
}

#[test]
fn every_model_and_field_is_generated_with_the_right_metadata() {
    let d = dmmf();
    let models = d["models"].as_array().unwrap();
    assert_eq!(models.len(), 58);
    assert_eq!(MODELS.len(), models.len());
    for (i, m) in models.iter().enumerate() {
        let name = m["name"].as_str().unwrap();
        let info = model_info(name).unwrap_or_else(|| panic!("model {name} missing"));
        assert_eq!(MODELS[i].name, name, "schema order");
        assert_eq!(info.table, m["dbName"].as_str().unwrap_or(name));
        let fields = scalar_fields(m);
        assert_eq!(info.columns.len(), fields.len(), "{name}: column count");
        for (f, c) in fields.iter().zip(info.columns) {
            let fname = f["name"].as_str().unwrap();
            let ctx = format!("{name}.{fname}");
            assert_eq!(c.field, fname, "{ctx}");
            assert_eq!(c.name, f["dbName"].as_str().unwrap_or(fname), "{ctx}");
            let ty = f["type"].as_str().unwrap();
            let expected_kind = if f["kind"] == "enum" {
                ScalarKind::Enum(ENUMS.iter().find(|e| e.name == ty).expect("enum exists").name)
            } else {
                match ty {
                    "String" => ScalarKind::String,
                    "Int" => ScalarKind::Int,
                    "BigInt" => ScalarKind::BigInt,
                    "Float" => ScalarKind::Float,
                    "Boolean" => ScalarKind::Boolean,
                    "DateTime" => ScalarKind::DateTime,
                    "Json" => ScalarKind::Json,
                    other => panic!("{ctx}: unexpected type {other}"),
                }
            };
            assert_eq!(c.kind, expected_kind, "{ctx}");
            assert_eq!(c.list, f["isList"] == true, "{ctx}: list");
            assert_eq!(c.nullable, f["isRequired"] == false, "{ctx}: nullable");
            assert_eq!(c.has_default, f["hasDefaultValue"] == true, "{ctx}: has_default");
            assert_eq!(c.is_id, f["isId"] == true, "{ctx}: is_id");
            assert_eq!(c.is_unique, f["isUnique"] == true, "{ctx}: is_unique");
            assert_eq!(c.updated_at, f["isUpdatedAt"] == true, "{ctx}: updated_at");
            let default = &f["default"];
            let expected_default = if f["isUpdatedAt"] == true {
                ClientDefault::UpdatedAt
            } else if default.is_object() {
                match default["name"].as_str().unwrap() {
                    "cuid" => ClientDefault::Cuid,
                    "uuid" => ClientDefault::Uuid,
                    "now" => ClientDefault::Now,
                    _ => ClientDefault::Database,
                }
            } else if f["hasDefaultValue"] == true {
                ClientDefault::Database
            } else {
                ClientDefault::None
            };
            assert_eq!(c.client_default, expected_default, "{ctx}: client default");
            if f["hasDefaultValue"] == true && !default.is_object() {
                let lit: Value = serde_json::from_str(c.default_json.expect("literal default")).unwrap();
                assert_eq!(&lit, default, "{ctx}: default literal");
            } else {
                assert!(c.default_json.is_none(), "{ctx}");
            }
        }
        // Primary key.
        let pk: Vec<&str> = if m["primaryKey"].is_object() {
            m["primaryKey"]["fields"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect()
        } else {
            fields.iter().filter(|f| f["isId"] == true).map(|f| f["name"].as_str().unwrap()).collect()
        };
        assert_eq!(info.primary_key, pk.as_slice(), "{name}: primary key");
    }
}

#[test]
fn every_enum_and_value_is_generated() {
    let d = dmmf();
    let enums = d["enums"].as_array().unwrap();
    assert_eq!(enums.len(), 28);
    assert_eq!(ENUMS.len(), enums.len());
    for e in enums {
        let name = e["name"].as_str().unwrap();
        let info = ENUMS.iter().find(|x| x.name == name).unwrap_or_else(|| panic!("enum {name}"));
        let values: Vec<&str> = e["values"].as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(info.values, values.as_slice(), "{name}");
    }
    // The Rust enum types themselves: labels, parse and serde agree.
    use bot_db::{InfractionType, StatKind, TriggerMatchType};
    assert_eq!(InfractionType::ALL.len(), 9);
    assert_eq!(TriggerMatchType::StartsWith.as_str(), "STARTS_WITH");
    assert_eq!(TriggerMatchType::parse("ENDS_WITH"), Some(TriggerMatchType::EndsWith));
    assert_eq!(TriggerMatchType::parse("ends_with"), None);
    assert_eq!(serde_json::to_value(StatKind::PeakOnline).unwrap(), json!("PEAK_ONLINE"));
    assert_eq!(serde_json::from_value::<StatKind>(json!("ROLE_MEMBERS")).unwrap(), StatKind::RoleMembers);
    assert_eq!(StatKind::TYPE_NAME, "StatKind");
    for k in StatKind::ALL {
        assert_eq!(k.as_str().parse::<StatKind>().unwrap(), *k);
    }
}

#[test]
fn enum_sql_type_names_are_quoted_for_case_sensitive_lookup() {
    use sqlx::TypeInfo;
    // Prisma creates the types quoted ("ModVisibility"); sqlx resolves names with to_regtype, which folds case
    // unless the name is quoted.
    let ti = <bot_db::ModVisibility as sqlx::Type<sqlx::Postgres>>::type_info();
    assert_eq!(ti.name(), "\"ModVisibility\"");
}

/// Rows written by the JS bot read back and write out byte-identical (key order, dates, floats, Json, arrays).
#[test]
fn js_rows_round_trip_byte_for_byte() {
    let rows = golden("bot-db/rows.json");
    let rows = rows.as_object().unwrap();
    assert_eq!(rows.len(), 58);
    for (model, variants) in rows {
        let full = variants["full"].as_str().unwrap();
        let nulls = variants["nulls"].as_str().unwrap();
        let out = reserialize_json(model, full).expect("known model").unwrap_or_else(|e| panic!("{model} full: {e}"));
        assert_eq!(out, full, "{model}: full row");
        let out = reserialize_json(model, nulls).expect("known model").unwrap_or_else(|e| panic!("{model} nulls: {e}"));
        assert_eq!(out, nulls, "{model}: row with nulls");
        // Extra keys (the settings cache's `_cacheV`) are ignored.
        let out = reserialize_json(model, variants["withExtra"].as_str().unwrap()).unwrap().unwrap();
        assert_eq!(out, full, "{model}: extra key ignored");
        // Absent optional fields read as null.
        let out = reserialize_json(model, variants["missing"].as_str().unwrap()).unwrap().unwrap_or_else(|e| panic!("{model} missing: {e}"));
        assert_eq!(out, nulls, "{model}: absent optional fields");
    }
    assert!(reserialize_json("NoSuchModel", "{}").is_none());
}

/// Every field has the right Rust type and nullability: a value of the wrong JSON type is rejected, `null` is
/// rejected on a required field and accepted on an optional one.
#[test]
fn every_field_rejects_wrong_types_and_respects_nullability() {
    let d = dmmf();
    let rows = golden("bot-db/rows.json");
    for m in d["models"].as_array().unwrap() {
        let model = m["name"].as_str().unwrap();
        let base: Value = serde_json::from_str(rows[model]["full"].as_str().unwrap()).unwrap();
        for f in scalar_fields(m) {
            let fname = f["name"].as_str().unwrap();
            let ty = f["type"].as_str().unwrap();
            let list = f["isList"] == true;
            let required = f["isRequired"] == true;
            let try_with = |v: Value| {
                let mut row = base.clone();
                row[fname] = v;
                reserialize_json(model, &row.to_string()).unwrap()
            };
            if ty != "Json" {
                let wrong = match (ty, list) {
                    (_, true) => json!({"not": "a list"}),
                    ("String", _) => json!(5),
                    ("Int" | "BigInt" | "Float", _) => json!("x"),
                    ("Boolean", _) => json!("true"),
                    ("DateTime", _) => json!(true),
                    _ if f["kind"] == "enum" => json!("NOT_A_VALUE"),
                    other => panic!("{other:?}"),
                };
                assert!(try_with(wrong).is_err(), "{model}.{fname}: wrong type accepted");
            }
            if !list && ty != "Json" {
                let r = try_with(Value::Null);
                assert_eq!(r.is_ok(), !required, "{model}.{fname}: null handling");
            }
            if ty == "Int" && !list {
                // A float written for an Int (JS has one number type) reads when integral.
                let r = try_with(json!(7.0)).unwrap();
                assert!(r.contains(&format!("\"{fname}\":7")), "{model}.{fname}");
                assert!(try_with(json!(7.5)).is_err(), "{model}.{fname}: fraction accepted");
            }
        }
    }
}

#[test]
fn guild_settings_flag_and_json_view() {
    let rows = golden("bot-db/rows.json");
    let raw = rows["GuildSettings"]["full"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(raw).unwrap();
    let gs = GuildSettings::from_json(&parsed).unwrap();
    assert_eq!(gs.to_json(), parsed);
    let n = GuildSettings::BOOLEAN_COLUMNS.len();
    assert!(n > 50, "{n}");
    for col in GuildSettings::BOOLEAN_COLUMNS {
        assert_eq!(gs.flag(col), parsed[*col].as_bool(), "{col}");
    }
    assert_eq!(gs.flag("guildId"), None);
    assert_eq!(gs.flag("noSuchColumn"), None);
    assert_eq!(gs.flag("levelingEnabled"), Some(gs.leveling_enabled));
    // Every Boolean column of the DMMF is covered.
    let d = dmmf();
    let m = d["models"].as_array().unwrap().iter().find(|m| m["name"] == "GuildSettings").unwrap();
    let bools: Vec<&str> = scalar_fields(m).into_iter().filter(|f| f["type"] == "Boolean").map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(GuildSettings::BOOLEAN_COLUMNS, bools.as_slice());
    // A value that is not JSON for the model is an error (the caller treats it as a cache miss).
    assert!(GuildSettings::from_json(&json!({"guildId": 5})).is_err());
    assert!(GuildSettings::from_json_str("not json").is_err());
    assert_eq!(<GuildSettings as Model>::info().name, "GuildSettings");
    assert_eq!(GuildSettings::TABLE, "GuildSettings");
    assert_eq!(GuildSettings::COLUMNS.len(), scalar_fields(m).len());
    assert_eq!(GuildSettings::COLUMNS.len(), 197);
}

#[test]
fn dates_format_exactly_like_js() {
    let d = golden("bot-db/dates.json");
    for case in d["format"].as_array().unwrap() {
        let ms = case["ms"].as_f64().unwrap() as i64;
        // chrono stops at year +-262143; JS goes to +-275760 (8.64e15 ms). Only those extremes may be missing.
        let Some(dt) = from_epoch_ms(ms) else {
            assert!(ms.abs() > 8_200_000_000_000_000, "{ms} should be representable");
            continue;
        };
        assert_eq!(format_js_date(&dt), case["json"].as_str().unwrap(), "{ms}");
        // And they read back.
        assert_eq!(parse_js_date(case["json"].as_str().unwrap()), Some(dt), "{ms}");
    }
}

#[test]
fn dates_parse_like_js() {
    let d = golden("bot-db/dates.json");
    for case in d["parse"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = parse_js_date(input).map(|d| format_js_date(&d));
        assert_eq!(got.as_deref(), case["iso"].as_str(), "{input:?}");
    }
    // Epoch milliseconds are read too.
    let v: bot_db::JsDate = serde_json::from_value(json!(1767323045678i64)).unwrap();
    assert_eq!(format_js_date(&v.0), "2026-01-02T03:04:05.678Z");
}

#[test]
fn floats_and_bigints_serialise_like_js() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct F {
        #[serde(with = "bot_db::serde_js::float")]
        f: f64,
        #[serde(with = "bot_db::serde_js::bigint")]
        b: i64,
        #[serde(with = "bot_db::serde_js::bigint_opt", default)]
        o: Option<i64>,
    }
    let s = |f: f64, b: i64| serde_json::to_string(&F { f, b, o: None }).unwrap();
    assert_eq!(s(1.0, 1), r#"{"f":1,"b":1,"o":null}"#);
    assert_eq!(s(-0.0, 0), r#"{"f":0,"b":0,"o":null}"#);
    assert_eq!(s(1.5, -5), r#"{"f":1.5,"b":-5,"o":null}"#);
    assert_eq!(s(0.1, 9007199254740991), r#"{"f":0.1,"b":9007199254740991,"o":null}"#);
    // Beyond 2^53 a BigInt cannot be a JS number: written as a string, read from either.
    assert_eq!(s(f64::NAN, 9007199254740993), r#"{"f":null,"b":"9007199254740993","o":null}"#);
    let back: F = serde_json::from_str(r#"{"f":2,"b":"9007199254740993","o":"12"}"#).unwrap();
    assert_eq!((back.f, back.b, back.o), (2.0, 9007199254740993, Some(12)));
}

#[test]
fn cuid_and_uuid_shapes() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2000 {
        let id = bot_db::new_cuid();
        assert_eq!(id.len(), 25, "{id}");
        assert!(id.starts_with('c'), "{id}");
        assert!(bot_db::is_cuid(&id), "{id}");
        assert!(seen.insert(id));
    }
    // Ids generated later sort after earlier ones (timestamp prefix), like Prisma's.
    let a = bot_db::new_cuid();
    std::thread::sleep(std::time::Duration::from_millis(3));
    let b = bot_db::new_cuid();
    assert!(a[1..9] <= b[1..9]);
    let u = bot_db::new_uuid();
    assert_eq!(u.len(), 36);
    assert_eq!(&u[14..15], "4");
    assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    let now = bot_db::now_utc();
    assert_eq!(now.timestamp_subsec_nanos() % 1_000_000, 0);
}
