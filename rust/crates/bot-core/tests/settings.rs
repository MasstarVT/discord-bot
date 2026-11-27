//! settingsService.js: the cache marker, revival, the memo, generations, write-through, the failed-SET rule; the JS
//! cache fixture replayed; and with TEST_DATABASE_URL / TEST_REDIS_URL the PostgreSQL store and the cross-language
//! round trips (a row cached by the JS bot read by Rust, and the reverse).

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use bot_config::ModuleKey;
use bot_core::cache::{Cache, MemoryBackend, RedisBackend, RedisOptions};
use bot_core::clock::{ManualClock, SharedClock};
use bot_core::settings::{
    CACHE_VERSION_FIELD, MemorySettingsStore, PgSettingsStore, SETTINGS_CACHE_VERSION, SettingsMutator, SettingsPatch, SettingsPublish, SettingsService, SettingsStore,
    is_module_enabled, is_valid_cached_settings, revive_settings, settings_key, with_marker,
};
use bot_core::testing::load_golden;
use bot_db::{DbError, GuildSettings};
use serde_json::{Value, json};

struct Fixture {
    service: SettingsService,
    store: Arc<MemorySettingsStore>,
    backend: Arc<MemoryBackend>,
    clock: ManualClock,
}

fn fixture() -> Fixture {
    let clock = ManualClock::new(1_800_000_000_000);
    let shared: SharedClock = Arc::new(clock.clone());
    let backend = Arc::new(MemoryBackend::new(shared.clone()));
    let store = Arc::new(MemorySettingsStore::new());
    let service = SettingsService::new(store.clone(), Cache::new(backend.clone(), shared.clone()), shared);
    Fixture { service, store, backend, clock }
}

#[test]
fn is_valid_cached_settings_checks_shape_guild_and_marker() {
    assert!(!is_valid_cached_settings(&json!({ "guildId": "g", "modEnabled": false }), "g"), "old shape, no marker");
    assert!(is_valid_cached_settings(&json!({ "guildId": "g", CACHE_VERSION_FIELD: SETTINGS_CACHE_VERSION }), "g"));
    assert!(is_valid_cached_settings(&json!({ "guildId": "g", "_cacheV": 2.0 }), "g"), "2.0 === 2");
    assert!(!is_valid_cached_settings(&json!({ "guildId": "other", "_cacheV": 2 }), "g"));
    assert!(!is_valid_cached_settings(&json!({ "guildId": "g", "_cacheV": 1 }), "g"));
    assert!(!is_valid_cached_settings(&Value::Null, "g"));
    assert!(!is_valid_cached_settings(&json!("x"), "g"));
    assert!(!is_valid_cached_settings(&json!([]), "g"));
    assert_eq!(settings_key("42"), "guild:42:settings");
}

#[test]
fn the_js_cache_value_is_read_and_written_back_byte_identically() {
    let f = load_golden("bot-core/settings-cache.json");
    assert_eq!(f["version"], json!(SETTINGS_CACHE_VERSION));
    assert_eq!(f["ttl"], json!(300));
    for name in ["default", "full"] {
        let entry = &f["rows"][name];
        let raw = entry["raw"].as_str().unwrap();
        let guild_id = entry["guildId"].as_str().unwrap();
        assert_eq!(entry["key"], json!(settings_key(guild_id)));
        let hit: Value = serde_json::from_str(raw).unwrap();
        assert!(is_valid_cached_settings(&hit, guild_id), "{name}");
        let row = revive_settings(&hit).unwrap_or_else(|| panic!("{name} revives"));
        assert_eq!(row.guild_id, guild_id);
        assert_eq!(row.created_at.timestamp_millis(), chrono::DateTime::parse_from_rfc3339(hit["createdAt"].as_str().unwrap()).unwrap().timestamp_millis());
        let back = serde_json::to_string(&with_marker(&row)).unwrap();
        assert_eq!(back, raw, "{name}: what Rust writes is what the JS wrote");
    }
}

#[test]
fn revive_strips_the_marker_and_revives_dates() {
    let row = MemorySettingsStore::new_row("g", serde_json::Map::new()).unwrap();
    let mut cached = with_marker(&row);
    assert_eq!(cached[CACHE_VERSION_FIELD], json!(2));
    cached["createdAt"] = json!("2026-01-01T00:00:00.000Z");
    let revived = revive_settings(&cached).unwrap();
    assert_eq!(bot_db::format_js_date(&revived.created_at), "2026-01-01T00:00:00.000Z");
    assert!(!revived.to_json().as_object().unwrap().contains_key(CACHE_VERSION_FIELD));
    assert!(revive_settings(&Value::Null).is_none());
    assert!(revive_settings(&json!({ "guildId": "g", "_cacheV": 2 })).is_none(), "an unreadable row is a miss");
}

#[tokio::test]
async fn peek_is_memo_only_and_safe_returns_none_when_postgres_is_down() {
    let f = fixture();
    assert!(f.service.peek_settings("9550000000000999").is_none());
    f.store.set_available(false);
    assert!(f.service.get_settings_safe("9550000000000998").await.is_none());
    assert!(f.service.get_settings("9550000000000998").await.is_err());
}

#[tokio::test]
async fn get_settings_memoises_the_same_row_and_caches_it_with_the_marker() {
    let f = fixture();
    let s1 = f.service.get_settings("101").await.unwrap();
    assert_eq!(s1.guild_id, "101");
    assert!(s1.mod_enabled, "moderation defaults on");
    assert!(!s1.level_up_use_embed);
    assert!(Arc::ptr_eq(&f.service.get_settings("101").await.unwrap(), &s1), "memo returns the same row");
    assert!(Arc::ptr_eq(&f.service.peek_settings("101").unwrap(), &s1));
    let cached: Value = serde_json::from_str(&f.backend.raw("guild:101:settings").unwrap()).unwrap();
    assert_eq!(cached["_cacheV"], json!(2));
    assert_eq!(f.backend.pttl("guild:101:settings"), 300_000);
    let fresh = f.service.get_settings_with("101", true).await.unwrap();
    assert!(!Arc::ptr_eq(&fresh, &s1), "fresh skips the memo");
    // The memo expires after 30 s; then Redis answers (no store read: the store is down).
    f.clock.advance(30_000);
    assert!(f.service.peek_settings("101").is_none());
    f.store.set_available(false);
    assert_eq!(f.service.get_settings("101").await.unwrap().guild_id, "101");
}

#[tokio::test]
async fn a_cached_row_with_another_version_or_guild_is_a_miss_and_is_replaced() {
    let f = fixture();
    f.backend.put_raw("guild:7:settings", r#"{"guildId":"7","modEnabled":false,"_cacheV":1}"#, Some(300_000));
    let row = f.service.get_settings("7").await.unwrap();
    assert!(row.mod_enabled, "the stale copy was not used");
    let cached: Value = serde_json::from_str(&f.backend.raw("guild:7:settings").unwrap()).unwrap();
    assert_eq!(cached["_cacheV"], json!(2));
    f.backend.put_raw("guild:8:settings", r#"{"guildId":"9","_cacheV":2}"#, Some(300_000));
    assert_eq!(f.service.get_settings("8").await.unwrap().guild_id, "8");
}

#[tokio::test]
async fn update_settings_writes_through_and_drops_the_key_when_the_set_fails() {
    let f = fixture();
    let u = f.service.update_settings("g1", SettingsPatch::new().set("countingEnabled", true).set("modLogChannelId", "123")).await.unwrap();
    assert!(u.counting_enabled);
    assert!(Arc::ptr_eq(&f.service.peek_settings("g1").unwrap(), &u), "write-through memoises");
    let cached: Value = serde_json::from_str(&f.backend.raw("guild:g1:settings").unwrap()).unwrap();
    assert_eq!(cached["countingEnabled"], json!(true));
    assert_eq!(cached["_cacheV"], json!(SETTINGS_CACHE_VERSION));
    assert_eq!(f.store.row("g1").unwrap().mod_log_channel_id.as_deref(), Some("123"));

    // Redis timed out on the SET: the key is dropped so the old row cannot come back.
    f.backend.fail_ops(&["set"]);
    f.backend.clear_calls();
    let row = f.service.update_settings("g1", SettingsPatch::new().set("countingEnabled", false)).await.unwrap();
    assert!(!row.counting_enabled);
    let dels: Vec<_> = f.backend.calls().into_iter().filter(|c| c.op == "del").collect();
    assert_eq!(dels.len(), 1);
    assert_eq!(dels[0].args, vec!["guild:g1:settings"]);
    assert!(f.backend.raw("guild:g1:settings").is_none());

    // A successful SET leaves the key alone.
    f.backend.fail_ops(&[]);
    f.backend.clear_calls();
    f.service.update_settings("g1", SettingsPatch::new().set("countingEnabled", true)).await.unwrap();
    assert!(!f.backend.calls().iter().any(|c| c.op == "del"));

    // Unknown columns and wrong types are refused, like Prisma.
    assert!(f.service.update_settings("g1", SettingsPatch::new().set("noSuchColumn", 1)).await.is_err());
    assert!(f.service.update_settings("g1", SettingsPatch::new().set("countingEnabled", "yes")).await.is_err());
}

#[tokio::test]
async fn update_settings_with_is_a_read_modify_write_and_publishes_the_same_way() {
    let f = fixture();
    let out = f
        .service
        .update_settings_with("g2", |cur: GuildSettings| {
            let mut ids = cur.mod_role_ids.clone();
            ids.push("2".into());
            Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
        })
        .await
        .unwrap();
    assert_eq!(out.mod_role_ids, vec!["2"]);
    // A failed SET drops the key, and the memo still has the new row.
    f.backend.fail_ops(&["set"]);
    f.service
        .update_settings_with("g2", |cur: GuildSettings| {
            let mut ids = cur.mod_role_ids.clone();
            ids.push("3".into());
            Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
        })
        .await
        .unwrap();
    assert!(f.backend.raw("guild:g2:settings").is_none());
    f.store.set_available(false);
    assert_eq!(f.service.get_settings("g2").await.unwrap().mod_role_ids, vec!["2", "3"], "from the memo");
    f.store.set_available(true);
    f.backend.fail_ops(&[]);

    // A throwing mutator rolls back and its error arrives unchanged; an empty result changes nothing.
    #[derive(Debug, thiserror::Error)]
    #[error("validation says no")]
    struct Refused;
    let err = f.service.update_settings_with("g2", |_| Err(Refused.into())).await.unwrap_err();
    assert!(err.downcast_ref::<Refused>().is_some());
    let same = f.service.update_settings_with("g2", |_| Ok(SettingsPatch::new())).await.unwrap();
    assert_eq!(same.mod_role_ids, vec!["2", "3"]);
    // JSON columns.
    let j = f
        .service
        .update_settings_with("g2", |_| Ok(SettingsPatch::new().set("automodConfig", json!({ "rules": { "caps": { "roleIds": ["x"] } } }))))
        .await
        .unwrap();
    assert_eq!(j.automod_config, Some(json!({ "rules": { "caps": { "roleIds": ["x"] } } })));
}

#[tokio::test]
async fn concurrent_read_modify_writes_all_land() {
    let f = fixture();
    let mut tasks = Vec::new();
    for i in 0..10 {
        let s = f.service.clone();
        tasks.push(tokio::spawn(async move {
            s.update_settings_with("g3", move |cur: GuildSettings| {
                let mut ids = cur.mod_role_ids.clone();
                ids.push(format!("c{i}"));
                Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
            })
            .await
            .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let ids = f.store.row("g3").unwrap().mod_role_ids;
    assert_eq!(ids.len(), 10);
    assert_eq!(ids.iter().collect::<std::collections::HashSet<_>>().len(), 10);
}

#[tokio::test]
async fn invalidate_drops_memo_ensured_and_the_key() {
    let f = fixture();
    f.service.get_settings("g4").await.unwrap();
    f.service.invalidate_settings("g4").await;
    assert!(f.service.peek_settings("g4").is_none());
    assert!(f.backend.raw("guild:g4:settings").is_none());
    assert_eq!(f.service.get_settings_safe("g4").await.unwrap().guild_id, "g4");
}

/// A store whose `find` waits for a signal, to stage a slow reader.
struct SlowStore {
    inner: Arc<MemorySettingsStore>,
    gate: tokio::sync::Semaphore,
}

#[async_trait]
impl SettingsStore for SlowStore {
    async fn find(&self, guild_id: &str) -> Result<Option<GuildSettings>, DbError> {
        let row = self.inner.find(guild_id).await;
        let _permit = self.gate.acquire().await;
        row
    }
    async fn create_if_missing(&self, guild_id: &str) -> Result<GuildSettings, DbError> {
        self.inner.create_if_missing(guild_id).await
    }
    async fn upsert(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        self.inner.upsert(guild_id, data).await
    }
    async fn update(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        self.inner.update(guild_id, data).await
    }
    async fn update_with(&self, guild_id: &str, mutator: SettingsMutator, publish: SettingsPublish) -> anyhow::Result<GuildSettings> {
        self.inner.update_with(guild_id, mutator, publish).await
    }
}

/// A store that records when the row is published and when the write commits, and can fail the commit.
struct CommitStore {
    inner: Arc<MemorySettingsStore>,
    backend: Arc<MemoryBackend>,
    events: Arc<std::sync::Mutex<Vec<String>>>,
    fail_commit: bool,
}

#[async_trait]
impl SettingsStore for CommitStore {
    async fn find(&self, guild_id: &str) -> Result<Option<GuildSettings>, DbError> {
        self.inner.find(guild_id).await
    }
    async fn create_if_missing(&self, guild_id: &str) -> Result<GuildSettings, DbError> {
        self.inner.create_if_missing(guild_id).await
    }
    async fn upsert(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        self.inner.upsert(guild_id, data).await
    }
    async fn update(&self, guild_id: &str, data: &SettingsPatch) -> Result<GuildSettings, DbError> {
        self.inner.update(guild_id, data).await
    }
    async fn update_with(&self, guild_id: &str, mutator: SettingsMutator, publish: SettingsPublish) -> anyhow::Result<GuildSettings> {
        let row = self.inner.update_with(guild_id, mutator, publish).await?;
        // What Redis holds at the moment of the commit.
        let cached: Option<Value> = self.backend.raw(&settings_key(guild_id)).map(|r| serde_json::from_str(&r).unwrap());
        let ids = cached.map(|c| c["modRoleIds"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect::<Vec<_>>().join(","));
        self.events.lock().unwrap().push(format!("commit, redis has {}", ids.unwrap_or_else(|| "nothing".into())));
        if self.fail_commit {
            anyhow::bail!("commit failed");
        }
        Ok(row)
    }
}

fn commit_fixture(fail_commit: bool) -> (SettingsService, Arc<MemoryBackend>, Arc<std::sync::Mutex<Vec<String>>>) {
    let clock: SharedClock = Arc::new(ManualClock::new(1_800_000_000_000));
    let backend = Arc::new(MemoryBackend::new(clock.clone()));
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = CommitStore { inner: Arc::new(MemorySettingsStore::new()), backend: backend.clone(), events: events.clone(), fail_commit };
    (SettingsService::new(Arc::new(store), Cache::new(backend.clone(), clock.clone()), clock), backend, events)
}

fn push_role(id: &'static str) -> impl FnOnce(GuildSettings) -> anyhow::Result<SettingsPatch> + Send + 'static {
    move |cur: GuildSettings| {
        let mut ids = cur.mod_role_ids.clone();
        ids.push(id.into());
        Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
    }
}

#[tokio::test]
async fn update_settings_with_publishes_before_the_commit_so_writers_reach_redis_in_commit_order() {
    let (service, _backend, events) = commit_fixture(false);
    service.update_settings_with("g6", push_role("a")).await.unwrap();
    service.update_settings_with("g6", push_role("b")).await.unwrap();
    assert_eq!(*events.lock().unwrap(), vec!["commit, redis has a", "commit, redis has a,b"]);
    assert_eq!(service.peek_settings("g6").unwrap().mod_role_ids, vec!["a", "b"]);
}

#[tokio::test]
async fn update_settings_with_drops_the_published_row_when_the_commit_fails() {
    let (service, backend, _events) = commit_fixture(true);
    let err = service.update_settings_with("g7", push_role("x")).await.unwrap_err();
    assert_eq!(err.to_string(), "commit failed");
    assert!(backend.raw(&settings_key("g7")).is_none(), "the uncommitted row is not left in Redis");
    assert!(service.peek_settings("g7").is_none(), "nor in the memo");
}

#[tokio::test]
async fn a_slow_reader_never_memoises_a_row_older_than_a_finished_write() {
    let clock: SharedClock = Arc::new(ManualClock::new(1_800_000_000_000));
    let inner = Arc::new(MemorySettingsStore::new());
    inner.put(MemorySettingsStore::new_row("g5", serde_json::Map::new()).unwrap());
    let slow = Arc::new(SlowStore { inner: inner.clone(), gate: tokio::sync::Semaphore::new(0) });
    let backend = Arc::new(MemoryBackend::new(clock.clone()));
    let service = SettingsService::new(slow.clone(), Cache::new(backend, clock.clone()), clock);
    let reader = {
        let s = service.clone();
        tokio::spawn(async move { s.get_settings("g5").await.unwrap() })
    };
    tokio::time::sleep(std::time::Duration::from_millis(20)).await; // the reader has read the old row
    let written = service.update_settings("g5", SettingsPatch::new().set("countingEnabled", true)).await.unwrap();
    slow.gate.add_permits(1);
    let stale = reader.await.unwrap();
    assert!(!stale.counting_enabled, "the slow reader got the row it read");
    assert!(Arc::ptr_eq(&service.peek_settings("g5").unwrap(), &written), "but the memo keeps the newer write");
}

/// JS clears `loading` in `promise.then(clear, clear)`, whoever awaits. A starter dropped mid-load (a timeout) must not
/// leave the finished load behind: past the memo, a read then has to load the new row, not get the old one back.
#[tokio::test]
async fn a_cancelled_first_reader_does_not_pin_a_stale_load() {
    let manual = ManualClock::new(1_800_000_000_000);
    let clock: SharedClock = Arc::new(manual.clone());
    let inner = Arc::new(MemorySettingsStore::new());
    inner.put(MemorySettingsStore::new_row("g6", serde_json::Map::new()).unwrap());
    let slow = Arc::new(SlowStore { inner: inner.clone(), gate: tokio::sync::Semaphore::new(0) });
    let backend = Arc::new(MemoryBackend::new(clock.clone()));
    let service = SettingsService::new(slow.clone(), Cache::new(backend.clone(), clock.clone()), clock);
    let first = tokio::time::timeout(std::time::Duration::from_millis(20), service.get_settings("g6")).await;
    assert!(first.is_err(), "the first reader timed out and was dropped");
    slow.gate.add_permits(10);
    let second = service.get_settings("g6").await.unwrap();
    assert!(!second.counting_enabled);
    // The row changes behind the service (another process), the Redis copy expires with the memo.
    let mut row = inner.row("g6").unwrap();
    row.counting_enabled = true;
    inner.put(row);
    manual.advance(301_000);
    let third = service.get_settings("g6").await.unwrap();
    assert!(third.counting_enabled, "a fresh load, not the finished load of the cancelled reader");
}

#[test]
fn module_gate_on_a_settings_row() {
    let mut row = MemorySettingsStore::new_row("g", serde_json::Map::new()).unwrap();
    assert!(is_module_enabled(Some(&row), Some(ModuleKey::Moderation)));
    assert!(!is_module_enabled(Some(&row), Some(ModuleKey::Automod)));
    assert!(is_module_enabled(None, None), "no module: on");
    assert!(!is_module_enabled(None, Some(ModuleKey::Moderation)), "no settings: off");
    row.xp_enabled = true;
    assert!(is_module_enabled(Some(&row), Some(ModuleKey::Leveling)), "leveling is the xpEnabled flag");
}

// ── PostgreSQL and the cross-language round trips ─────────────────────────────

fn id(n: u32) -> String {
    format!("95500000000{n:05}")
}

async fn cleanup(pool: &bot_db::PgPool, ids: &[String]) {
    sqlx::query(r#"DELETE FROM "GuildSettings" WHERE "guildId" = ANY($1)"#).bind(ids).execute(pool).await.unwrap();
}

#[tokio::test]
async fn db_settings_get_update_update_with_and_ensure() {
    let Some(url) = common::database_url() else { return };
    let pool = common::pool(&url).await;
    let clock: SharedClock = Arc::new(bot_core::clock::SystemClock);
    let backend = Arc::new(MemoryBackend::new(clock.clone()));
    let service = SettingsService::new(Arc::new(PgSettingsStore::new(pool.clone())), Cache::new(backend, clock.clone()), clock);
    let gid = id(101);
    let gid2 = id(102);
    cleanup(&pool, &[gid.clone(), gid2.clone()]).await;

    let s1 = service.get_settings(&gid).await.unwrap();
    assert_eq!(s1.guild_id, gid);
    assert!(s1.mod_enabled);
    assert!(!s1.level_up_use_embed, "new guilds keep the MEE6 plain-text default");
    assert!(Arc::ptr_eq(&service.get_settings(&gid).await.unwrap(), &s1));
    service.ensure_guild_settings(&gid).await.unwrap();
    service.ensure_guild_settings(&gid).await.unwrap();

    let u = service.update_settings(&gid, SettingsPatch::new().set("automodEnabled", true).set("modLogChannelId", "123")).await.unwrap();
    assert!(u.automod_enabled);
    let (log,): (Option<String>,) = sqlx::query_as(r#"SELECT "modLogChannelId" FROM "GuildSettings" WHERE "guildId" = $1"#).bind(&gid).fetch_one(&pool).await.unwrap();
    assert_eq!(log.as_deref(), Some("123"));
    assert_eq!(service.get_settings_with(&gid, true).await.unwrap().mod_log_channel_id.as_deref(), Some("123"));

    let pushed = service
        .update_settings_with(&gid, |cur: GuildSettings| {
            let mut ids = cur.mod_role_ids.clone();
            ids.push("r1".into());
            Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
        })
        .await
        .unwrap();
    assert_eq!(pushed.mod_role_ids, vec!["r1"]);
    service
        .update_settings_with(&gid, |_| Ok(SettingsPatch::new().set("automodConfig", json!({ "rules": { "caps": { "roleIds": ["x"] } } }))))
        .await
        .unwrap();
    assert_eq!(service.get_settings_with(&gid, true).await.unwrap().automod_config, Some(json!({ "rules": { "caps": { "roleIds": ["x"] } } })));

    // No lost updates: ten concurrent read-modify-writes all land.
    let mut tasks = Vec::new();
    for i in 0..10 {
        let s = service.clone();
        let g = gid.clone();
        tasks.push(tokio::spawn(async move {
            s.update_settings_with(&g, move |cur: GuildSettings| {
                let mut ids = cur.mod_role_ids.clone();
                ids.push(format!("c{i}"));
                Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
            })
            .await
            .unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let (all,): (Vec<String>,) = sqlx::query_as(r#"SELECT "modRoleIds" FROM "GuildSettings" WHERE "guildId" = $1"#).bind(&gid).fetch_one(&pool).await.unwrap();
    assert_eq!(all.len(), 11);

    #[derive(Debug, thiserror::Error)]
    #[error("validation says no")]
    struct Refused;
    let err = service.update_settings_with(&gid, |_| Err(Refused.into())).await.unwrap_err();
    assert!(err.downcast_ref::<Refused>().is_some());
    assert_eq!(service.update_settings_with(&gid, |_| Ok(SettingsPatch::new())).await.unwrap().mod_role_ids.len(), 11);

    // Missing rows are created by update and updateSettingsWith alike.
    assert!(service.update_settings(&gid2, SettingsPatch::new().set("xpEnabled", true)).await.unwrap().xp_enabled);
    cleanup(&pool, std::slice::from_ref(&gid2)).await;
    service.invalidate_settings(&gid2).await;
    let created = service.update_settings_with(&gid2, |_| Ok(SettingsPatch::new().set("autoroleIds", json!(["a"])))).await.unwrap();
    assert_eq!(created.autorole_ids, vec!["a"]);

    service.invalidate_settings(&gid).await;
    assert!(service.peek_settings(&gid).is_none());
    assert_eq!(service.get_settings_safe(&gid).await.unwrap().guild_id, gid);
    cleanup(&pool, &[gid, gid2]).await;
}

#[tokio::test]
async fn db_every_raw_settings_statement_executes() {
    let Some(url) = common::database_url() else { return };
    let pool = common::pool(&url).await;
    let mut tx = pool.begin().await.unwrap();
    for (name, sql) in bot_core::settings::RAW_SQL {
        let r = if sql.contains("INSERT") {
            sqlx::query(*sql).bind(bot_db::new_uuid()).bind("0").execute(&mut *tx).await.map(|_| ())
        } else {
            sqlx::query(*sql).bind("0").execute(&mut *tx).await.map(|_| ())
        };
        r.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!sql.replace("now() AT TIME ZONE 'UTC'", "").contains("now()"), "{name}: bare now()");
    }
    tx.rollback().await.unwrap();
}

async fn redis_service(pool: &bot_db::PgPool, url: &str) -> (SettingsService, RedisBackend) {
    let backend = RedisBackend::connect(RedisOptions::new(Some(url))).unwrap();
    assert!(backend.wait_ready(5000).await);
    let clock: SharedClock = Arc::new(bot_core::clock::SystemClock);
    let service = SettingsService::new(Arc::new(PgSettingsStore::new(pool.clone())), Cache::new(Arc::new(backend.clone()), clock.clone()), clock);
    (service, backend)
}

#[tokio::test]
async fn js_caches_a_row_and_rust_reads_it_from_redis() {
    let (Some(db), Some(redis)) = (common::database_url(), common::redis_url()) else { return };
    let pool = common::pool(&db).await;
    let gid = id(501);
    cleanup(&pool, std::slice::from_ref(&gid)).await;
    // The JS bot loads (creates) the row and caches it.
    let js = common::run_js(&["settings-update", &gid, r#"{"countingEnabled":true,"modRoleIds":["r1","r2"],"automodConfig":{"rules":{"caps":{"on":true}}}}"#]);
    assert!(js.get("error").is_none(), "{js}");
    let (service, backend) = redis_service(&pool, &redis).await;
    // Change the database behind the cache's back: Rust must answer from the JS-written Redis copy.
    sqlx::query(r#"UPDATE "GuildSettings" SET "countingEnabled" = false WHERE "guildId" = $1"#).bind(&gid).execute(&pool).await.unwrap();
    let row = service.get_settings(&gid).await.unwrap();
    assert!(row.counting_enabled, "read from the JS cache entry");
    assert_eq!(row.mod_role_ids, vec!["r1", "r2"]);
    assert_eq!(row.automod_config, Some(json!({ "rules": { "caps": { "on": true } } })));
    assert_eq!(row.to_json(), revive_settings(&js["row"]).unwrap().to_json(), "the same row the JS returned");
    backend.disconnect();
    cleanup(&pool, &[gid]).await;
}

#[tokio::test]
async fn rust_writes_a_row_and_js_reads_it_from_redis() {
    let (Some(db), Some(redis)) = (common::database_url(), common::redis_url()) else { return };
    let pool = common::pool(&db).await;
    let gid = id(502);
    cleanup(&pool, std::slice::from_ref(&gid)).await;
    let (service, backend) = redis_service(&pool, &redis).await;
    let row = service
        .update_settings(&gid, SettingsPatch::new().set("welcomeEnabled", true).set("embedColor", 0x123456).set("timezone", "Europe/Berlin"))
        .await
        .unwrap();
    // Change the database behind the cache's back: the JS must answer from the Rust-written Redis copy.
    sqlx::query(r#"UPDATE "GuildSettings" SET "welcomeEnabled" = false WHERE "guildId" = $1"#).bind(&gid).execute(&pool).await.unwrap();
    let js = common::run_js(&["settings-memo", &gid]);
    assert!(js.get("error").is_none(), "{js}");
    assert_eq!(js["row"]["welcomeEnabled"], json!(true), "the JS read the Rust cache entry");
    assert_eq!(js["row"]["embedColor"], json!(0x123456));
    let ours = row.to_json();
    assert_eq!(js["row"], ours, "JSON-equal rows");
    assert_eq!(js["raw"].as_str().unwrap(), serde_json::to_string(&with_marker(&row)).unwrap());
    backend.disconnect();
    cleanup(&pool, &[gid]).await;
}

/// Prisma sends no UPDATE for `updateSettings(guildId, {})` (an upsert with empty `update`) or an empty
/// `updateSettingsWith` result: the row and its `updatedAt` stay as they were.
#[tokio::test]
async fn db_an_empty_update_leaves_updated_at_alone() {
    let Some(url) = common::database_url() else { return };
    let pool = common::pool(&url).await;
    let clock: SharedClock = Arc::new(bot_core::clock::SystemClock);
    let backend = Arc::new(MemoryBackend::new(clock.clone()));
    let store = PgSettingsStore::new(pool.clone());
    let service = SettingsService::new(Arc::new(store.clone()), Cache::new(backend, clock.clone()), clock);
    let gid = id(131);
    cleanup(&pool, std::slice::from_ref(&gid)).await;
    let first = service.update_settings(&gid, SettingsPatch::new().set("automodEnabled", true)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let again = service.update_settings(&gid, SettingsPatch::new()).await.unwrap();
    assert_eq!(again.updated_at, first.updated_at, "upsert with update: {{}}");
    assert!(again.automod_enabled);
    let updated = store.update(&gid, &SettingsPatch::new()).await.unwrap();
    assert_eq!(updated.updated_at, first.updated_at, "update with data: {{}}");
    let with = service.update_settings_with(&gid, |_| Ok(SettingsPatch::new())).await.unwrap();
    assert_eq!(with.updated_at, first.updated_at);
    // A new guild with an empty patch is created.
    let gid2 = id(132);
    cleanup(&pool, std::slice::from_ref(&gid2)).await;
    let created = service.update_settings(&gid2, SettingsPatch::new()).await.unwrap();
    assert_eq!(created.guild_id, gid2);
    cleanup(&pool, &[gid, gid2]).await;
}
