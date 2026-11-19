//! The dynamic INSERT / UPDATE / SELECT builders: SQL text, typed binds, defaults and errors.

use bot_db::query::{SelectBuilder, delete_where};
use bot_db::schema::ScalarKind;
use bot_db::{
    DbError, GUILD_SETTINGS_INFO, GuildSettings, InsertBuilder, LockMode, MODELS, POLL_INFO, SCHEDULED_JOB_INFO, SqlValue,
    UpdateBuilder, is_cuid, to_sql_value,
};
use serde_json::{Value, json};

fn returning_all() -> String {
    GuildSettings::COLUMNS.iter().map(|c| format!("\"{}\"", c.name)).collect::<Vec<_>>().join(", ")
}

#[test]
fn update_by_field_name_sets_updated_at_and_casts_enums() {
    let q = UpdateBuilder::new(&GUILD_SETTINGS_INFO)
        .set("levelingEnabled", json!(true))
        .unwrap()
        .set("modRoleIds", json!(["1", "2"]))
        .unwrap()
        .set("xpMultiplier", json!(1.5))
        .unwrap()
        .set("modVisibility", json!("ANONYMOUS"))
        .unwrap()
        .set("automodConfig", Value::Null)
        .unwrap()
        .filter("guildId", json!("123"))
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(
        q.sql,
        format!(
            "UPDATE \"GuildSettings\" SET \"levelingEnabled\" = $1, \"modRoleIds\" = $2, \"xpMultiplier\" = $3, \
             \"modVisibility\" = $4::\"ModVisibility\", \"automodConfig\" = $5, \"updatedAt\" = $6 WHERE \"guildId\" = $7 RETURNING {}",
            returning_all()
        )
    );
    assert_eq!(q.args.len(), 7);
    assert_eq!(q.args[0], SqlValue::Bool(Some(true)));
    assert_eq!(q.args[1], SqlValue::TextArray(vec!["1".into(), "2".into()]));
    assert_eq!(q.args[2], SqlValue::Float(Some(1.5)));
    assert_eq!(q.args[3], SqlValue::Enum { type_name: "ModVisibility", value: Some("ANONYMOUS".into()) });
    // Json null on a `Json?` column is SQL NULL (Prisma.DbNull), typed as jsonb.
    assert_eq!(q.args[4], SqlValue::Json(None));
    assert!(matches!(q.args[5], SqlValue::Timestamp(Some(_))));
    assert_eq!(q.args[6], SqlValue::Text(Some("123".into())));
    assert!(q.arguments().is_ok());
}

#[test]
fn update_errors() {
    let b = || UpdateBuilder::new(&GUILD_SETTINGS_INFO);
    assert!(matches!(b().set("noSuchColumn", json!(1)), Err(DbError::UnknownColumn { .. })));
    assert!(matches!(b().set("guildId", Value::Null), Err(DbError::InvalidValue { .. })));
    assert!(matches!(b().set("levelingEnabled", json!("yes")), Err(DbError::InvalidValue { .. })));
    assert!(matches!(b().set("modVisibility", json!("SECRET")), Err(DbError::InvalidValue { .. })));
    assert!(matches!(b().set("modRoleIds", json!([1])), Err(DbError::InvalidValue { .. })));
    assert!(matches!(b().set("modRoleIds", Value::Null), Err(DbError::InvalidValue { .. })));
    assert!(matches!(b().set("levelingEnabled", json!(true)).unwrap().set("levelingEnabled", json!(false)), Err(DbError::DuplicateColumn(_))));
    // No WHERE clause: refused rather than updating every guild.
    assert!(matches!(b().set("levelingEnabled", json!(true)).unwrap().build(), Err(DbError::EmptyWhere(_))));
}

#[test]
fn update_clamps_int_columns_and_honours_an_explicit_updated_at() {
    let int_col = GuildSettings::COLUMNS.iter().find(|c| c.kind == ScalarKind::Int && !c.list && !c.nullable).unwrap();
    let q = UpdateBuilder::new(&GUILD_SETTINGS_INFO)
        .set(int_col.name, json!(1e12))
        .unwrap()
        .set("updatedAt", json!("2026-01-02T03:04:05.678Z"))
        .unwrap()
        .filter("guildId", json!("1"))
        .unwrap()
        .without_returning()
        .build()
        .unwrap();
    assert_eq!(q.sql, format!("UPDATE \"GuildSettings\" SET \"{}\" = $1, \"updatedAt\" = $2 WHERE \"guildId\" = $3", int_col.name));
    assert_eq!(q.args[0], SqlValue::Int(Some(i32::MAX)));
    let SqlValue::Timestamp(Some(ts)) = &q.args[1] else { panic!() };
    assert_eq!(ts.and_utc().timestamp_millis(), 1767323045678);
    assert!(UpdateBuilder::new(&GUILD_SETTINGS_INFO).set(int_col.name, json!(1.5)).is_err());
}

#[test]
fn insert_applies_prisma_client_defaults() {
    let q = InsertBuilder::new(&GUILD_SETTINGS_INFO).value("guildId", json!("42")).unwrap().on_conflict_do_nothing(&["guildId"]).unwrap().build().unwrap();
    assert_eq!(
        q.sql,
        format!(
            "INSERT INTO \"GuildSettings\" (\"id\", \"guildId\", \"createdAt\", \"updatedAt\") VALUES ($1, $2, $3, $4) \
             ON CONFLICT (\"guildId\") DO NOTHING RETURNING {}",
            returning_all()
        )
    );
    let SqlValue::Text(Some(id)) = &q.args[0] else { panic!("id") };
    assert!(is_cuid(id), "{id}");
    assert_eq!(q.args[1], SqlValue::Text(Some("42".into())));
    assert!(matches!(q.args[2], SqlValue::Timestamp(Some(_))));
    assert!(matches!(q.args[3], SqlValue::Timestamp(Some(_))));
}

#[test]
fn insert_requires_columns_without_default_and_supports_upsert() {
    // ScheduledJob needs type, runAt, ... : a missing required column is an error, like Prisma.
    let err = InsertBuilder::new(&SCHEDULED_JOB_INFO).build().unwrap_err();
    assert!(matches!(err, DbError::MissingColumn { .. }), "{err}");

    let q = InsertBuilder::new(&GUILD_SETTINGS_INFO)
        .value("guildId", json!("7"))
        .unwrap()
        .value("levelingEnabled", json!(false))
        .unwrap()
        .on_conflict_update(&["guildId"], [("levelingEnabled", json!(false))])
        .unwrap()
        .without_returning()
        .build()
        .unwrap();
    assert_eq!(
        q.sql,
        "INSERT INTO \"GuildSettings\" (\"id\", \"guildId\", \"createdAt\", \"updatedAt\", \"levelingEnabled\") VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (\"guildId\") DO UPDATE SET \"levelingEnabled\" = $6, \"updatedAt\" = $7"
    );
    // Omitted list column without a default (Poll.options) is left to the database, like Prisma.
    let q = InsertBuilder::new(&POLL_INFO)
        .values([
            ("guildId", json!("1")),
            ("channelId", json!("2")),
            ("authorId", json!("3")),
            ("question", json!("Q?")),
            ("endsAt", json!("2026-01-02T03:04:05.678Z")),
        ])
        .unwrap()
        .without_returning()
        .build()
        .unwrap();
    assert!(q.sql.starts_with("INSERT INTO \"Poll\" (\"id\", \"guildId\", \"channelId\", \"authorId\", \"question\", \"endsAt\", \"createdAt\", \"updatedAt\")"), "{}", q.sql);
}

#[test]
fn select_and_delete_builders() {
    let q = SelectBuilder::new(&SCHEDULED_JOB_INFO)
        .filter("status", json!("PENDING"))
        .unwrap()
        .filter("guildId", Value::Null)
        .unwrap()
        .order_by("runAt", true)
        .unwrap()
        .limit(5)
        .lock(LockMode::ForUpdateSkipLocked)
        .build()
        .unwrap();
    let cols = SCHEDULED_JOB_INFO.select_list();
    assert_eq!(
        q.sql,
        format!(
            "SELECT {cols} FROM \"ScheduledJob\" WHERE \"status\" = $1::\"JobStatus\" AND \"guildId\" IS NULL ORDER BY \"runAt\" ASC LIMIT 5 FOR UPDATE SKIP LOCKED"
        )
    );
    assert_eq!(q.args.len(), 1);
    assert!(matches!(SelectBuilder::new(&SCHEDULED_JOB_INFO).lock(LockMode::ForUpdate).build(), Err(DbError::EmptyWhere(_))));
    let d = delete_where(&POLL_INFO, [("id", json!("x"))]).unwrap();
    assert_eq!(d.sql, "DELETE FROM \"Poll\" WHERE \"id\" = $1");
    assert!(delete_where(&POLL_INFO, []).is_err());
}

#[test]
fn every_column_of_every_model_converts_its_own_js_value() {
    // The JS-produced sample rows: every value converts to a bind of the right variant for its column.
    let p = format!("{}/../../fixtures/golden/bot-db/rows.json", env!("CARGO_MANIFEST_DIR"));
    let rows: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    for m in MODELS {
        let full: Value = serde_json::from_str(rows[m.name]["full"].as_str().unwrap()).unwrap();
        for c in m.columns {
            let v = to_sql_value(c, &full[c.field]).unwrap_or_else(|e| panic!("{}.{}: {e}", m.name, c.name));
            let ok = match (c.kind, c.list, &v) {
                (ScalarKind::String, false, SqlValue::Text(Some(_))) => true,
                (ScalarKind::String, true, SqlValue::TextArray(_)) => true,
                (ScalarKind::Int, false, SqlValue::Int(Some(_))) => true,
                (ScalarKind::Int, true, SqlValue::IntArray(_)) => true,
                (ScalarKind::Float, false, SqlValue::Float(Some(_))) => true,
                (ScalarKind::Boolean, false, SqlValue::Bool(Some(_))) => true,
                (ScalarKind::DateTime, false, SqlValue::Timestamp(Some(_))) => true,
                (ScalarKind::DateTime, true, SqlValue::TimestampArray(_)) => true,
                (ScalarKind::Json, false, SqlValue::Json(Some(_))) => true,
                (ScalarKind::Enum(t), false, SqlValue::Enum { type_name, value: Some(_) }) => t == *type_name,
                _ => false,
            };
            assert!(ok, "{}.{}: {v:?}", m.name, c.name);
        }
    }
}
