//! Loading and caching (the per-guild memo, the Redis copy, the generation that guards against stale loaders), the
//! reads and the row-locked writes of `automationService.js`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use bot_core::App;
use bot_db::{AUTOMATION_INFO, Automation, InsertBuilder, PgPool, SelectBuilder, UpdateBuilder};
use bot_util::async_util::Throttle;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::pure::*;
use super::tables::*;

/// `AutomationLite`: what the dispatcher needs of an enabled automation, with normalised arrays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutomationLite {
    pub id: String,
    pub name: String,
    pub trigger: String,
    #[serde(rename = "cooldownSeconds", default)]
    pub cooldown_seconds: i64,
    #[serde(default)]
    pub conditions: Vec<Value>,
    #[serde(default)]
    pub actions: Vec<Value>,
}

/// The enabled automations of a guild by trigger (JS object key order).
pub type ByTrigger = IndexMap<String, Vec<AutomationLite>>;

/// The cached value of `guild:{g}:automations`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAutomations {
    v: Value,
    #[serde(rename = "byTrigger")]
    by_trigger: ByTrigger,
}

type LoadFuture = Shared<BoxFuture<'static, Option<Arc<ByTrigger>>>>;

/// The module-scope state of the JS service, one per app.
pub struct AutomationState {
    memo: Mutex<IndexMap<String, (i64, Arc<ByTrigger>)>>,
    generation: Mutex<HashMap<String, u64>>,
    inflight: Mutex<HashMap<String, LoadFuture>>,
    pub(crate) local_cooldowns: Mutex<LocalExpiringSet>,
    pub(crate) local_reposts: Mutex<LocalExpiringSet>,
    pub(crate) causes: Mutex<CauseTracker>,
    pub(crate) budget: Mutex<RunBudget>,
    pub(crate) guild_queue: bot_core::sync::KeyedMutex,
    pub(crate) pending: Mutex<HashMap<String, usize>>,
    /// `createThrottle(600_000)`, on the app clock.
    pub(crate) warn_throttle: Throttle,
    /// `createThrottle(60_000)`, on the app clock.
    pub(crate) chain_throttle: Throttle,
}

impl Default for AutomationState {
    fn default() -> Self {
        Self {
            memo: Mutex::new(IndexMap::new()),
            generation: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            local_cooldowns: Mutex::new(LocalExpiringSet::default()),
            local_reposts: Mutex::new(LocalExpiringSet::default()),
            causes: Mutex::new(CauseTracker::default()),
            budget: Mutex::new(RunBudget::default()),
            guild_queue: bot_core::sync::KeyedMutex::new(),
            pending: Mutex::new(HashMap::new()),
            warn_throttle: Throttle::new(600_000),
            chain_throttle: Throttle::new(60_000),
        }
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn state(app: &App) -> Arc<AutomationState> {
    app.ext::<AutomationState>()
}

pub(crate) fn now_ms(app: &App) -> i64 {
    app.clock().now_ms()
}

/// `warnThrottle(key)` on the app clock.
pub(crate) fn warn_allowed(app: &App, key: &str) -> bool {
    state(app).warn_throttle.allow_at(key, now_ms(app))
}

fn generation_of(st: &AutomationState, guild_id: &str) -> u64 {
    lock(&st.generation).get(guild_id).copied().unwrap_or(0)
}

fn set_memo(app: &App, guild_id: &str, by_trigger: Arc<ByTrigger>) {
    let st = state(app);
    let mut memo = lock(&st.memo);
    memo.shift_remove(guild_id);
    memo.insert(guild_id.to_string(), (now_ms(app), by_trigger));
    while memo.len() > MEMO_MAX_GUILDS {
        memo.shift_remove_index(0);
    }
}

fn trigger_string(t: &bot_db::AutomationTrigger) -> String {
    serde_json::to_value(t).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn lite_from_row(row: &Automation) -> Option<AutomationLite> {
    let actions = normalize_items(&row.actions, ListKind::Action);
    if actions.is_empty() {
        return None;
    }
    Some(AutomationLite {
        id: row.id.clone(),
        name: row.name.clone(),
        trigger: trigger_string(&row.trigger),
        cooldown_seconds: i64::from(row.cooldown_seconds),
        conditions: normalize_items(&row.conditions, ListKind::Condition),
        actions,
    })
}

/// The enabled rows of a guild, oldest first.
async fn enabled_rows(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<Automation>> {
    Ok(SelectBuilder::new(&AUTOMATION_INFO)
        .filter("guildId", json!(guild_id))?
        .filter("enabled", json!(true))?
        .order_by("createdAt", true)?
        .build()?
        .fetch_all::<Automation, _>(pool)
        .await?)
}

async fn load_automations(app: App, guild_id: String, generation: u64) -> Option<Arc<ByTrigger>> {
    let key = automations_cache_key(&guild_id);
    // Degrade decision: a Redis read failure (a miss) reads Postgres, i.e. it fails open to the database.
    if let Some(raw) = app.cache().get(&key).await
        && raw.get("v").and_then(Value::as_f64) == Some(1.0)
        && raw.get("byTrigger").is_some_and(Value::is_object)
        && let Ok(cached) = serde_json::from_value::<CachedAutomations>(raw)
    {
        let by = Arc::new(cached.by_trigger);
        if generation_of(&state(&app), &guild_id) == generation {
            set_memo(&app, &guild_id, by.clone());
        }
        return Some(by);
    }
    let rows = match enabled_rows(app.db(), &guild_id).await {
        Ok(r) => r,
        Err(e) => {
            // Degrade decision: Postgres down -> nothing runs (fail-closed). Logged once per guild per 10 min.
            if warn_allowed(&app, &format!("db:{guild_id}")) {
                bot_util::log_error!(format!("automations: could not load automations for {guild_id}"), bot_core::error::log_arg(&e));
            }
            return None;
        }
    };
    let mut by: ByTrigger = IndexMap::new();
    for row in &rows {
        if let Some(lite) = lite_from_row(row) {
            by.entry(lite.trigger.clone()).or_default().push(lite);
        }
    }
    let by = Arc::new(by);
    // A loader that started before an invalidation must not write stale rows back into the memo or Redis.
    if generation_of(&state(&app), &guild_id) == generation {
        set_memo(&app, &guild_id, by.clone());
        app.cache().set(&key, &json!({ "v": 1, "byTrigger": &*by }), CACHE_TTL_SECONDS).await;
    }
    Some(by)
}

/// `getAutomations(guildId)`: the enabled automations by trigger; `None` when Postgres is unreachable. Never fails.
pub async fn get_automations(app: &App, guild_id: &str) -> Option<Arc<ByTrigger>> {
    let st = state(app);
    if let Some((at, by)) = lock(&st.memo).get(guild_id)
        && now_ms(app) - at < MEMO_TTL_MS
    {
        return Some(by.clone());
    }
    let generation = generation_of(&st, guild_id);
    let key = format!("{guild_id}:{generation}");
    let fut = {
        let mut inflight = lock(&st.inflight);
        match inflight.get(&key) {
            Some(f) => f.clone(),
            None => {
                let (app2, g2, k2) = (app.clone(), guild_id.to_string(), key.clone());
                let f = async move {
                    let out = load_automations(app2.clone(), g2, generation).await;
                    lock(&state(&app2).inflight).remove(&k2);
                    out
                }
                .boxed()
                .shared();
                inflight.insert(key, f.clone());
                f
            }
        }
    };
    fut.await
}

/// `peekHasTrigger(guildId, trigger)`: false only when a fresh memo has nothing for the trigger; unknown or stale is
/// true.
pub fn peek_has_trigger(app: &App, guild_id: &str, trigger: &str) -> bool {
    let st = state(app);
    let memo = lock(&st.memo);
    let Some((at, by)) = memo.get(guild_id) else { return true };
    if now_ms(app) - at >= MEMO_TTL_MS {
        return true;
    }
    by.get(trigger).is_some_and(|l| !l.is_empty())
}

/// `invalidateAutomations(guildId)`: drops the memo and the Redis key and bumps the generation. Never fails.
pub async fn invalidate_automations(app: &App, guild_id: &str) {
    let st = state(app);
    *lock(&st.generation).entry(guild_id.to_string()).or_insert(0) += 1;
    lock(&st.memo).shift_remove(guild_id);
    app.cache().del_one(&automations_cache_key(guild_id)).await;
}

/// `__setAutomationMemo(guildId, byTrigger)`: the test seam.
pub fn set_automation_memo(app: &App, guild_id: &str, by_trigger: ByTrigger) {
    set_memo(app, guild_id, Arc::new(by_trigger));
}

// ── Reads ────────────────────────────────────────────────────────────────────

/// A row with normalised arrays (`hydrate`); invalid stored elements are dropped and listed.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationRecord {
    pub id: String,
    pub guild_id: String,
    pub name: String,
    pub trigger: String,
    pub conditions: Vec<Value>,
    pub actions: Vec<Value>,
    pub enabled: bool,
    pub cooldown_seconds: i64,
    pub button_seq: i64,
    pub uses: i64,
    pub created_by: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub invalid_conditions: Vec<InvalidItem>,
    pub invalid_actions: Vec<InvalidItem>,
}

impl AutomationRecord {
    /// `hydrate(row)`.
    pub fn hydrate(row: Automation) -> Self {
        let c = normalize_list(&row.conditions, ListKind::Condition, false);
        let a = normalize_list(&row.actions, ListKind::Action, false);
        Self {
            trigger: trigger_string(&row.trigger),
            id: row.id,
            guild_id: row.guild_id,
            name: row.name,
            conditions: c.items,
            actions: a.items,
            enabled: row.enabled,
            cooldown_seconds: i64::from(row.cooldown_seconds),
            button_seq: i64::from(row.button_seq),
            uses: i64::from(row.uses),
            created_by: row.created_by,
            created_at: row.created_at,
            updated_at: row.updated_at,
            invalid_conditions: c.invalid,
            invalid_actions: a.invalid,
        }
    }

    /// The row as JSON with Prisma's field names (what `toExportObject` reads).
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "guildId": self.guild_id, "name": self.name, "trigger": self.trigger, "conditions": self.conditions,
            "actions": self.actions, "enabled": self.enabled, "cooldownSeconds": self.cooldown_seconds, "buttonSeq": self.button_seq,
            "uses": self.uses, "createdBy": self.created_by,
            "createdAt": bot_db::format_js_date(&self.created_at), "updatedAt": bot_db::format_js_date(&self.updated_at),
        })
    }
}

/// `getAutomationByName(guildId, name)`.
pub async fn get_automation_by_name(pool: &PgPool, guild_id: &str, name: &str) -> anyhow::Result<Option<AutomationRecord>> {
    let row: Option<Automation> = SelectBuilder::new(&AUTOMATION_INFO)
        .filter("guildId", json!(guild_id))?
        .filter("name", json!(name))?
        .build()?
        .fetch_optional(pool)
        .await?;
    Ok(row.map(AutomationRecord::hydrate))
}

/// One row of [`search_automations`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutomationSearchRow {
    pub name: String,
    pub trigger: String,
    pub enabled: bool,
}

/// `searchAutomations(guildId, q)`: Prisma's insensitive `contains` (ILIKE, unescaped), by name, at most 25. Never
/// fails.
pub async fn search_automations(pool: &PgPool, guild_id: &str, q: &str) -> Vec<AutomationSearchRow> {
    let sql = r#"SELECT "name", "trigger"::text AS "trigger", "enabled" FROM "Automation" WHERE "guildId" = $1 AND "name" ILIKE $2 ORDER BY "name" ASC LIMIT 25"#;
    match bot_db::sqlx::query_as::<_, (String, String, bool)>(sql).bind(guild_id).bind(format!("%{q}%")).fetch_all(pool).await {
        Ok(rows) => rows.into_iter().map(|(name, trigger, enabled)| AutomationSearchRow { name, trigger, enabled }).collect(),
        Err(e) => {
            bot_util::log_debug!(format!("automations: search failed ({e})"));
            Vec::new()
        }
    }
}

/// `searchSavedEmbedNames(guildId, q)`: saved-embed names through the embeds contract. Never fails.
pub async fn search_saved_embed_names(pool: &PgPool, guild_id: &str, q: &str) -> Vec<String> {
    feat_embeds::embed_service::search_saved_embeds(pool, guild_id, q, 25).await.into_iter().map(|r| r.name).collect()
}

/// One row of [`list_automations`].
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationListRow {
    pub name: String,
    pub trigger: String,
    pub enabled: bool,
    pub conditions: Vec<Value>,
    pub actions: Vec<Value>,
    pub uses: i64,
}

async fn all_rows(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<Automation>> {
    Ok(SelectBuilder::new(&AUTOMATION_INFO)
        .filter("guildId", json!(guild_id))?
        .order_by("createdAt", true)?
        .limit(bot_config::limits::AUTOMATIONS as i64)
        .build()?
        .fetch_all::<Automation, _>(pool)
        .await?)
}

/// `listAutomations(guildId)`: oldest first, at most `LIMITS.AUTOMATIONS`.
pub async fn list_automations(pool: &PgPool, guild_id: &str) -> anyhow::Result<Vec<AutomationListRow>> {
    Ok(all_rows(pool, guild_id)
        .await?
        .into_iter()
        .map(|r| AutomationListRow {
            name: r.name.clone(),
            trigger: trigger_string(&r.trigger),
            enabled: r.enabled,
            conditions: normalize_items(&r.conditions, ListKind::Condition),
            actions: normalize_items(&r.actions, ListKind::Action),
            uses: i64::from(r.uses),
        })
        .collect())
}

/// One entry of [`list_automation_refs`].
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationRef {
    pub id: String,
    pub name: String,
    pub actions: Vec<Value>,
}

/// `listAutomationRefs(guildId)`: every automation with its normalised actions. Never fails.
pub async fn list_automation_refs(pool: &PgPool, guild_id: &str) -> Vec<AutomationRef> {
    match all_rows(pool, guild_id).await {
        Ok(rows) => rows.into_iter().map(|r| AutomationRef { actions: normalize_items(&r.actions, ListKind::Action), id: r.id, name: r.name }).collect(),
        Err(e) => {
            bot_util::log_debug!(format!("automations: listing refs failed ({e})"));
            Vec::new()
        }
    }
}

/// One automation button of a guild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationButton {
    pub automation_id: String,
    pub automation_name: String,
    pub index: i64,
    pub label: String,
}

/// `listAutomationButtons(guildId)`: every automation button of the guild. Never fails.
pub async fn list_automation_buttons(pool: &PgPool, guild_id: &str) -> Vec<AutomationButton> {
    let mut out = Vec::new();
    for r in list_automation_refs(pool, guild_id).await {
        for a in &r.actions {
            if let Some(index) = automation_button_index(a) {
                let label = a.get("button").map(|b| sfield(b, "label").to_string()).unwrap_or_default();
                out.push(AutomationButton { automation_id: r.id.clone(), automation_name: r.name.clone(), index, label });
            }
        }
    }
    out
}

// ── Writes ───────────────────────────────────────────────────────────────────

/// The fields of a create / replace (`data`).
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationInput {
    pub name: Option<String>,
    pub trigger: String,
    pub conditions: Vec<Value>,
    pub actions: Vec<Value>,
    pub enabled: bool,
    pub cooldown_seconds: i64,
}

struct Prepared {
    trigger: String,
    conditions: Vec<Value>,
    actions: Vec<Value>,
    cooldown_seconds: i64,
    enabled: bool,
}

/// `prepareWrite(data)`: normalises and validates a create / replace.
fn prepare_write(data: &AutomationInput) -> Result<Prepared, AutomationError> {
    let Some(trigger) = trigger_key(&data.trigger) else { return Err(AutomationError::new("Unknown trigger.")) };
    let c = normalize_list(&Value::Array(data.conditions.clone()), ListKind::Condition, false);
    if let Some(inv) = c.invalid.first() {
        return Err(AutomationError::new(format!("Condition {}: {}", inv.index, inv.error)));
    }
    let a = normalize_list(&Value::Array(data.actions.clone()), ListKind::Action, false);
    if let Some(inv) = a.invalid.first() {
        return Err(AutomationError::new(format!("Action {}: {}", inv.index, inv.error)));
    }
    let errors = validate_automation_shape(trigger, &c.items, &a.items, &json!(data.cooldown_seconds));
    if !errors.is_empty() {
        return Err(AutomationError::new(errors.join("\n")));
    }
    Ok(Prepared {
        trigger: trigger.into(),
        enabled: data.enabled && !a.items.is_empty(),
        conditions: c.items,
        actions: a.items,
        cooldown_seconds: data.cooldown_seconds,
    })
}

fn rewrite_self_refs(conditions: &[Value], from: &str, to: &str) -> Vec<Value> {
    conditions
        .iter()
        .map(|c| {
            if c.get("type").and_then(Value::as_str) != Some("BUTTON_IS") {
                return c.clone();
            }
            let mut c = c.clone();
            if let Some(bs) = c.get_mut("buttons").and_then(Value::as_array_mut) {
                for b in bs.iter_mut() {
                    if b.get("automationId").and_then(Value::as_str) == Some(from)
                        && let Some(m) = b.as_object_mut()
                    {
                        m.insert("automationId".into(), Value::String(to.into()));
                    }
                }
            }
            c
        })
        .collect()
}

fn has_self_ref(conditions: &[Value], id: &str) -> bool {
    conditions.iter().any(|c| {
        c.get("type").and_then(Value::as_str) == Some("BUTTON_IS")
            && c.get("buttons").and_then(Value::as_array).is_some_and(|bs| bs.iter().any(|b| b.get("automationId").and_then(Value::as_str) == Some(id)))
    })
}

async fn lock_one(tx: &mut bot_db::sqlx::PgConnection, sql: &'static str, a: &str, b: &str) -> anyhow::Result<Option<String>> {
    let row: Option<(String,)> = bot_db::sqlx::query_as(sql).bind(a).bind(b).fetch_optional(&mut *tx).await.map_err(bot_db::DbError::from)?;
    Ok(row.map(|r| r.0))
}

async fn find_by_id(tx: &mut bot_db::sqlx::PgConnection, id: &str) -> anyhow::Result<Option<Automation>> {
    Ok(SelectBuilder::new(&AUTOMATION_INFO).filter("id", json!(id))?.build()?.fetch_optional(&mut *tx).await?)
}

/// `createAutomation(guildId, data, createdBy, { selfRefId })`: creates under the per-guild cap. BUTTON_IS entries
/// that carry `self_ref_id` (the exported id of a JSON import) are rewritten to the new row's id in the same
/// transaction. Fails with an [`AutomationError`] (invalid, `NAME`, `LIMIT`) or a unique violation for a taken name.
pub async fn create_automation(app: &App, guild_id: &str, data: &AutomationInput, created_by: &str, self_ref_id: Option<&str>) -> anyhow::Result<AutomationRecord> {
    if created_by.is_empty() {
        anyhow::bail!("createAutomation: createdBy is required");
    }
    let Some(name) = data.name.as_deref().and_then(normalize_automation_name) else {
        return Err(AutomationError::with_code(error_texts::NAME, ErrorCode::Name).into());
    };
    let w = prepare_write(data)?;
    app.settings().ensure_guild_settings(guild_id).await?;
    let (g, creator, self_ref) = (guild_id.to_string(), created_by.to_string(), self_ref_id.map(str::to_string));
    let row: Automation = bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            bot_db::sqlx::query(LOCK_GUILD_SQL).bind(&g).execute(&mut *tx).await.map_err(bot_db::DbError::from)?;
            let (count,): (i64,) = bot_db::sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Automation" WHERE "guildId" = $1"#)
                .bind(&g)
                .fetch_one(&mut *tx)
                .await
                .map_err(bot_db::DbError::from)?;
            if count >= bot_config::limits::AUTOMATIONS as i64 {
                return Err(AutomationError::with_code(error_texts::limit(), ErrorCode::Limit).into());
            }
            let created: Automation = InsertBuilder::new(&AUTOMATION_INFO)
                .values([
                    ("guildId", json!(g)),
                    ("name", json!(name)),
                    ("trigger", json!(w.trigger)),
                    ("conditions", Value::Array(w.conditions.clone())),
                    ("actions", Value::Array(w.actions.clone())),
                    ("enabled", json!(w.enabled)),
                    ("cooldownSeconds", json!(w.cooldown_seconds)),
                    ("buttonSeq", json!(highest_button(&w.actions))),
                    ("createdBy", json!(creator)),
                ])?
                .build()?
                .fetch_one(&mut *tx)
                .await?;
            if let Some(from) = self_ref.filter(|s| !s.is_empty() && has_self_ref(&w.conditions, s)) {
                let rewritten = rewrite_self_refs(&w.conditions, &from, &created.id);
                let updated: Automation = UpdateBuilder::new(&AUTOMATION_INFO)
                    .set("conditions", Value::Array(rewritten))?
                    .filter("id", json!(created.id))?
                    .build()?
                    .fetch_one(&mut *tx)
                    .await?;
                return Ok(updated);
            }
            Ok::<_, anyhow::Error>(created)
        })
    })
    .await?;
    invalidate_automations(app, guild_id).await;
    Ok(AutomationRecord::hydrate(row))
}

/// The columns a mutation changes (Prisma field names) and what it returns to the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Mutation<R> {
    pub data: Vec<(&'static str, Value)>,
    pub result: R,
}

impl<R> Mutation<R> {
    pub fn new(data: Vec<(&'static str, Value)>, result: R) -> Self {
        Self { data, result }
    }
}

fn patched<'a>(data: &'a [(&'static str, Value)], key: &str) -> Option<&'a Value> {
    data.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn as_list(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

/// `mutateAutomation(guildId, name, mutator)`: a read-modify-write of one automation under a row lock. The mutator is
/// pure and synchronous: it gets the row with normalised arrays and returns the columns to change (none = no write)
/// and a result. Fails with an [`AutomationError`] (`NOTFOUND`, an invalid shape, or the mutator's own) or a unique
/// violation on a rename to a taken name.
pub async fn mutate_automation<R, F>(app: &App, guild_id: &str, name: &str, mutator: F) -> anyhow::Result<(AutomationRecord, R)>
where
    F: FnOnce(&AutomationRecord) -> anyhow::Result<Mutation<R>> + Send + 'static,
    R: Send + 'static,
{
    let (g, n) = (guild_id.to_string(), name.to_string());
    let (row, result) = bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            let id = lock_one(tx, LOCK_BY_NAME_SQL, &g, &n).await?;
            let row = match id {
                Some(id) => find_by_id(tx, &id).await?,
                None => None,
            };
            let Some(row) = row else { return Err(AutomationError::with_code(error_texts::not_found(&n), ErrorCode::NotFound).into()) };
            let current = AutomationRecord::hydrate(row.clone());
            let Mutation { data, result } = mutator(&current)?;
            let trigger = patched(&data, "trigger").and_then(Value::as_str).unwrap_or(&current.trigger).to_string();
            let conditions = patched(&data, "conditions").map_or_else(|| current.conditions.clone(), as_list);
            let actions = patched(&data, "actions").map_or_else(|| current.actions.clone(), as_list);
            let cooldown = patched(&data, "cooldownSeconds").cloned().unwrap_or_else(|| json!(current.cooldown_seconds));
            let errors = validate_automation_shape(&trigger, &conditions, &actions, &cooldown);
            if !errors.is_empty() {
                return Err(AutomationError::new(errors.join("\n")).into());
            }
            if data.is_empty() {
                return Ok((row, result));
            }
            let updated: Automation =
                UpdateBuilder::new(&AUTOMATION_INFO).set_all(data.iter().map(|(k, v)| (*k, v.clone())))?.filter("id", json!(row.id))?.build()?.fetch_one(&mut *tx).await?;
            Ok::<_, anyhow::Error>((updated, result))
        })
    })
    .await?;
    invalidate_automations(app, guild_id).await;
    Ok((AutomationRecord::hydrate(row), result))
}

/// `replaceAutomation(guildId, id, data)`: an import with overwrite. `uses` and `createdBy` are kept and `buttonSeq`
/// only grows.
pub async fn replace_automation(app: &App, guild_id: &str, id: &str, data: &AutomationInput) -> anyhow::Result<AutomationRecord> {
    let w = prepare_write(data)?;
    let new_name = match &data.name {
        None => None,
        Some(n) => match normalize_automation_name(n) {
            Some(v) => Some(v),
            None => return Err(AutomationError::with_code(error_texts::NAME, ErrorCode::Name).into()),
        },
    };
    let (g, id) = (guild_id.to_string(), id.to_string());
    let row: Automation = bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
        Box::pin(async move {
            let locked = lock_one(tx, LOCK_BY_ID_SQL, &g, &id).await?;
            let found = match locked {
                Some(id) => find_by_id(tx, &id).await?,
                None => None,
            };
            let Some(found) = found else {
                return Err(AutomationError::with_code(error_texts::not_found(new_name.as_deref().unwrap_or("?")), ErrorCode::NotFound).into());
            };
            let mut set: Vec<(&str, Value)> = Vec::new();
            if let Some(n) = new_name.filter(|n| *n != found.name) {
                set.push(("name", json!(n)));
            }
            set.extend([
                ("trigger", json!(w.trigger)),
                ("conditions", Value::Array(w.conditions)),
                ("actions", Value::Array(w.actions.clone())),
                ("enabled", json!(w.enabled)),
                ("cooldownSeconds", json!(w.cooldown_seconds)),
                ("buttonSeq", json!(i64::from(found.button_seq).max(highest_button(&w.actions)))),
            ]);
            let updated: Automation = UpdateBuilder::new(&AUTOMATION_INFO).set_all(set)?.filter("id", json!(found.id))?.build()?.fetch_one(&mut *tx).await?;
            Ok::<_, anyhow::Error>(updated)
        })
    })
    .await?;
    invalidate_automations(app, guild_id).await;
    Ok(AutomationRecord::hydrate(row))
}

/// `deleteAutomation(guildId, id)`: the number of rows deleted.
pub async fn delete_automation(app: &App, guild_id: &str, id: &str) -> anyhow::Result<u64> {
    let n = bot_db::query::delete_where(&AUTOMATION_INFO, [("guildId", json!(guild_id)), ("id", json!(id))])?.execute(app.db()).await?;
    invalidate_automations(app, guild_id).await;
    Ok(n)
}

/// `bumpUses(id)`: the fire-and-forget run counter (the Int column is clamped; no updatedAt change).
pub fn bump_uses(app: &App, id: &str) {
    let (pool, id) = (app.db().clone(), id.to_string());
    tokio::spawn(async move {
        if let Err(e) = bot_db::sqlx::query(USES_SQL).bind(&id).execute(&pool).await {
            bot_util::log_debug!(format!("automations: uses bump failed ({e})"));
        }
    });
}
