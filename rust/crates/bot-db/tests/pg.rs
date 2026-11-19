//! Tests against a real PostgreSQL. Skipped unless TEST_DATABASE_URL is set:
//!   TEST_DATABASE_URL='postgresql://postgres:password@localhost:5432/discordbot_test' cargo test -p bot-db --test pg
//! Everything happens in a fresh schema (`botdb_test_<random>`) that is dropped at the end.

use std::collections::{HashMap, HashSet};

use bot_db::client::parse_database_url_with;
use bot_db::migrate::{migrate_deploy, migrations, read_applied, status};
use bot_db::query::SelectBuilder;
use bot_db::schema::{ClientDefault, ScalarKind};
use bot_db::{
    DbError, GUILD_SETTINGS_INFO, GuildSettings, InsertBuilder, MODELS, SQL_NOW, TxOptions, UpdateBuilder, advisory_xact_lock,
    fetch_all_json, lock_guild_settings, model_info, sql_utc, sql_utc_param, transaction, try_advisory_xact_lock,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

fn golden(path: &str) -> Value {
    let p = format!("{}/../../fixtures/golden/{path}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

/// TEST_DATABASE_URL with `schema=` replaced.
fn url_with_schema(base: &str, schema: &str) -> String {
    let mut u = url::Url::parse(base).expect("TEST_DATABASE_URL");
    let pairs: Vec<(String, String)> = u.query_pairs().filter(|(k, _)| k != "schema").map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    u.set_query(None);
    {
        let mut q = u.query_pairs_mut();
        for (k, v) in &pairs {
            q.append_pair(k, v);
        }
        q.append_pair("schema", schema);
    }
    u.to_string()
}

struct TestDb {
    pool: PgPool,
    schema: String,
}

async fn setup() -> Option<TestDb> {
    let Ok(base) = std::env::var("TEST_DATABASE_URL") else {
        println!("skipped: TEST_DATABASE_URL not set");
        return None;
    };
    let schema = format!("botdb_test_{}", &bot_db::new_cuid()[13..]);
    let cfg = parse_database_url_with(&url_with_schema(&base, &schema), false, std::path::Path::new("prisma")).unwrap();
    let pool = bot_db::connect_with(&cfg).await.expect("connect");
    Some(TestDb { pool, schema })
}

async fn teardown(db: TestDb) {
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", db.schema))).execute(&db.pool).await;
    db.pool.close().await;
}

/// Models in an order where every referenced (parent) model comes first.
fn insertion_order(dmmf: &Value) -> Vec<String> {
    let models = dmmf["models"].as_array().unwrap();
    let mut deps: HashMap<String, HashSet<String>> = HashMap::new();
    for m in models {
        let name = m["name"].as_str().unwrap().to_string();
        let set = deps.entry(name.clone()).or_default();
        for f in m["fields"].as_array().unwrap() {
            if f["kind"] == "object" && f["relationFromFields"].as_array().is_some_and(|a| !a.is_empty()) {
                let parent = f["type"].as_str().unwrap().to_string();
                if parent != name {
                    set.insert(parent);
                }
            }
        }
    }
    let mut order = Vec::new();
    let mut done: HashSet<String> = HashSet::new();
    while order.len() < deps.len() {
        let before = order.len();
        for m in models {
            let name = m["name"].as_str().unwrap();
            if !done.contains(name) && deps[name].iter().all(|p| done.contains(p)) {
                done.insert(name.to_string());
                order.push(name.to_string());
            }
        }
        assert!(order.len() > before, "relation cycle");
    }
    order
}

#[tokio::test]
async fn migrations_rows_schema_and_helpers_against_postgres() {
    let Some(db) = setup().await else { return };
    let result = run(&db).await;
    teardown(db).await;
    result.unwrap();
}

async fn run(db: &TestDb) -> Result<(), DbError> {
    let pool = &db.pool;
    let schema = db.schema.as_str();

    // ── Migrations: an empty database gets everything; a second run has nothing to do. ──
    let report = migrate_deploy(pool, schema).await?;
    assert_eq!(report.applied, migrations().iter().map(|m| m.name.to_string()).collect::<Vec<_>>());
    assert!(migrate_deploy(pool, schema).await?.applied.is_empty());
    assert!(status(pool, schema).await?.pending.is_empty());
    let mut conn = pool.acquire().await?;
    let rows = read_applied(&mut conn, schema).await?;
    drop(conn);
    assert_eq!(rows.len(), migrations().len());
    for (row, m) in rows.iter().zip(migrations()) {
        // Recorded the way `prisma migrate deploy` records it, so Prisma sees an up-to-date database.
        assert_eq!(row.migration_name, m.name);
        assert_eq!(row.checksum, m.checksum());
        assert!(row.finished_at.is_some());
        assert!(row.rolled_back_at.is_none());
        assert_eq!(row.applied_steps_count, 1);
        assert_eq!(row.id.len(), 36);
    }

    // ── information_schema agrees with the generated COLUMNS. ──
    for info in MODELS {
        let cols = sqlx::query(
            "SELECT column_name, is_nullable, udt_name, column_default FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2",
        )
        .bind(schema)
        .bind(info.table)
        .fetch_all(pool)
        .await?;
        let mut db_cols: HashMap<String, (String, String, Option<String>)> = HashMap::new();
        for r in cols {
            db_cols.insert(r.try_get("column_name")?, (r.try_get("is_nullable")?, r.try_get("udt_name")?, r.try_get("column_default")?));
        }
        assert_eq!(db_cols.len(), info.columns.len(), "{}: column count", info.name);
        for c in info.columns {
            let (nullable, udt, default) = db_cols.get(c.name).unwrap_or_else(|| panic!("{}.{} missing in the database", info.name, c.name));
            let expected_udt = if c.list { format!("_{}", c.kind.pg_udt_name()) } else { c.kind.pg_udt_name().to_string() };
            assert_eq!(udt, &expected_udt, "{}.{}: type", info.name, c.name);
            if !c.list {
                assert_eq!(nullable == "YES", c.nullable, "{}.{}: nullability", info.name, c.name);
            }
            if matches!(c.client_default, ClientDefault::Database | ClientDefault::Now) {
                assert!(default.is_some(), "{}.{}: database default", info.name, c.name);
            }
            if matches!(c.client_default, ClientDefault::Cuid | ClientDefault::Uuid | ClientDefault::UpdatedAt | ClientDefault::None) && !c.list {
                assert!(default.is_none(), "{}.{}: unexpected database default {default:?}", info.name, c.name);
            }
        }
    }

    // ── One row of every model: inserted from the JS sample row, read back JSON-equal. ──
    let dmmf = golden("prisma-dmmf.json");
    let rows = golden("bot-db/rows.json");
    let mut inserted: HashMap<String, Value> = HashMap::new();
    for name in insertion_order(&dmmf) {
        let info = model_info(&name).unwrap();
        let mut row: Value = serde_json::from_str(rows[&name]["full"].as_str().unwrap()).unwrap();
        // Foreign keys point at the parent row inserted earlier.
        let m = dmmf["models"].as_array().unwrap().iter().find(|m| m["name"] == name.as_str()).unwrap();
        for f in m["fields"].as_array().unwrap() {
            let from = f["relationFromFields"].as_array();
            if f["kind"] != "object" || from.is_none_or(|a| a.is_empty()) {
                continue;
            }
            let parent = &inserted[f["type"].as_str().unwrap()];
            for (fk, pk) in from.unwrap().iter().zip(f["relationToFields"].as_array().unwrap()) {
                row[fk.as_str().unwrap()] = parent[pk.as_str().unwrap()].clone();
            }
        }
        let mut b = InsertBuilder::new(info);
        for c in info.columns {
            b = b.value(c.name, row[c.field].clone())?;
        }
        let q = b.build()?;
        let back = fetch_all_json(&name, &q, pool).await.unwrap()?;
        assert_eq!(back.len(), 1, "{name}");
        assert_eq!(back[0], row, "{name}: INSERT ... RETURNING");
        // And a plain SELECT by primary key.
        let mut s = SelectBuilder::new(info);
        for pk in info.primary_key {
            s = s.filter(pk, row[*pk].clone())?;
        }
        let back = fetch_all_json(&name, &s.build()?, pool).await.unwrap()?;
        assert_eq!(back, vec![row.clone()], "{name}: SELECT");
        inserted.insert(name, row);
    }

    // ── The dynamic update builder against the real table. ──
    let gs = &inserted["GuildSettings"];
    let guild_id = gs["guildId"].as_str().unwrap().to_string();
    let before: GuildSettings = GuildSettings::from_json(gs)?;
    let updated: GuildSettings = UpdateBuilder::new(&GUILD_SETTINGS_INFO)
        .set("levelingEnabled", json!(!before.leveling_enabled))?
        .set("modVisibility", json!("PRIVATE"))?
        .set("automodConfig", Value::Null)?
        .set("modRoleIds", json!(["a", "b"]))?
        .filter("guildId", json!(guild_id))?
        .build()?
        .fetch_one(pool)
        .await?;
    assert_eq!(updated.leveling_enabled, !before.leveling_enabled);
    assert_eq!(updated.mod_visibility, bot_db::ModVisibility::Private);
    assert_eq!(updated.automod_config, None);
    assert_eq!(updated.mod_role_ids, vec!["a", "b"]);
    assert!(updated.updated_at > before.updated_at);
    // An insert with only the required columns gets Prisma's client defaults.
    let fresh: GuildSettings = InsertBuilder::new(&GUILD_SETTINGS_INFO).value("guildId", json!("fresh-guild"))?.build()?.fetch_one(pool).await?;
    assert!(bot_db::is_cuid(&fresh.id));
    assert!((bot_db::now_utc() - fresh.created_at).num_seconds().abs() < 60);
    // ON CONFLICT DO NOTHING returns no row for an existing guild.
    let again: Option<GuildSettings> = InsertBuilder::new(&GUILD_SETTINGS_INFO)
        .value("guildId", json!("fresh-guild"))?
        .on_conflict_do_nothing(&["guildId"])?
        .build()?
        .fetch_optional(pool)
        .await?;
    assert!(again.is_none());
    // Prisma sends no UPDATE for empty data: `upsert({ update: {} })` returns the existing row as it is (also for a
    // model without @updatedAt, where DO NOTHING would return no row), `update({ data: {} })` only reads it, and
    // `updateMany({ data: {} })` changes nothing (count 0).
    let kept: GuildSettings = InsertBuilder::new(&GUILD_SETTINGS_INFO)
        .value("guildId", json!("fresh-guild"))?
        .on_conflict_update(&["guildId"], Vec::<(&str, Value)>::new())?
        .build()?
        .fetch_one(pool)
        .await?;
    assert_eq!((kept.id.as_str(), kept.updated_at), (fresh.id.as_str(), fresh.updated_at));
    let read: GuildSettings = UpdateBuilder::new(&GUILD_SETTINGS_INFO).filter("guildId", json!("fresh-guild"))?.build()?.fetch_one(pool).await?;
    assert_eq!(read.updated_at, fresh.updated_at);
    let none = UpdateBuilder::new(&GUILD_SETTINGS_INFO).filter("guildId", json!("fresh-guild"))?.without_returning().build()?.execute(pool).await?;
    assert_eq!(none, 0);
    let afk = model_info("AfkStatus").expect("AfkStatus");
    let first: bot_db::AfkStatus = InsertBuilder::new(afk)
        .value("guildId", json!("fresh-guild"))?
        .value("userId", json!("1"))?
        .value("reason", json!("lunch"))?
        .build()?
        .fetch_one(pool)
        .await?;
    let upserted: bot_db::AfkStatus = InsertBuilder::new(afk)
        .value("guildId", json!("fresh-guild"))?
        .value("userId", json!("1"))?
        .on_conflict_update(&["guildId", "userId"], Vec::<(&str, Value)>::new())?
        .build()?
        .fetch_one(pool)
        .await?;
    assert_eq!(upserted.reason.as_deref(), Some("lunch"));
    assert_eq!(upserted.since, first.since);

    // ── seed.js: creates the test guild once, then skips. ──
    let first = bot_db::seed::seed(pool).await?;
    assert!(matches!(first, bot_db::seed::SeedOutcome::Seeded(_)), "{first:?}");
    assert!(matches!(bot_db::seed::seed(pool).await?, bot_db::seed::SeedOutcome::Skipped(_)));

    // ── Transactions and locks. ──
    let gid = guild_id.clone();
    let locked = transaction(pool, TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            let found = lock_guild_settings(tx, &gid).await?;
            advisory_xact_lock(tx, &format!("infraction:{gid}")).await?;
            assert!(try_advisory_xact_lock(tx, "free-key").await?);
            Ok::<_, DbError>(found)
        })
    })
    .await?;
    assert!(locked);
    // A rolled-back transaction leaves nothing behind.
    let r: Result<(), DbError> = transaction(pool, TxOptions::default(), |tx| {
        Box::pin(async move {
            InsertBuilder::new(&GUILD_SETTINGS_INFO).value("guildId", json!("rolled-back"))?.without_returning().build()?.execute(&mut *tx).await?;
            Err(DbError::Migration("abort".into()))
        })
    })
    .await;
    assert!(r.is_err());
    let n: i64 = sqlx::query_scalar(r#"SELECT count(*) FROM "GuildSettings" WHERE "guildId" = 'rolled-back'"#).fetch_one(pool).await?;
    assert_eq!(n, 0);

    // ── SQL_NOW and sqlUtc are UTC whatever the session time zone is (test/db.test.js D2, D3). ──
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL TimeZone = 'America/New_York'").execute(&mut *tx).await?;
    let now: chrono::NaiveDateTime = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {SQL_NOW} AS now"))).fetch_one(&mut *tx).await?;
    assert!((now.and_utc() - chrono::Utc::now()).num_seconds().abs() < 5);
    let sample = bot_db::serde_js::from_epoch_ms(1767323045006).unwrap();
    let ts: chrono::NaiveDateTime =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {} AS ts", sql_utc(1)))).bind(sql_utc_param(&sample)).fetch_one(&mut *tx).await?;
    assert_eq!(ts.and_utc(), sample);
    tx.rollback().await?;

    // ── Enum decode through sqlx's type lookup (quoted, case-sensitive type names). ──
    let v: bot_db::ModVisibility = sqlx::query_scalar(r#"SELECT 'ANONYMOUS'::"ModVisibility""#).fetch_one(pool).await?;
    assert_eq!(v, bot_db::ModVisibility::Anonymous);
    let _ = ScalarKind::Json;
    Ok(())
}
