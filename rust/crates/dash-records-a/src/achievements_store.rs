//! Port of `src/dashboard/collections/achievements.store.js`: the `achievements` collection (the Achievement table,
//! tiers as JSON). Mirrors /achievement-config: the same tier normalisation (achievementService), the same progress
//! fix-ups when tiers change, and the achievement index in Redis (`guild:{g}:achievements`) is rebuilt after every
//! write. The cap and the unique name are checked in a transaction under a lock on the GuildSettings row.

use std::collections::HashMap;

use async_trait::async_trait;
use bot_config::ModuleKey;
use bot_config::tables::limits;
use bot_core::App;
use bot_core::settings::is_module_enabled;
use bot_db::{ACHIEVEMENT_INFO, Achievement, InsertBuilder, UpdateBuilder};
use dash_collections::collections::small::{NAME_MAX, check_achievement, trigger_label};
use dash_collections::{CheckCtx, ListAnswer, ListOpts, Mode, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError};
use feat_achievements::achievement_service::{
    Reward, Tier, delete_achievement, list_achievements, normalize_achievement_name, normalize_rewards, normalize_tiers, refresh_achievement_cache,
    rewards_json, serialize_tiers, tiers_json, validate_tiers,
};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::common::{TX_OPTS, TxError, internal, lock_guild_settings, truthy};

/// `achievementLockSql(id, guildId)`.
pub const ACHIEVEMENT_LOCK_SQL: &str = r#"SELECT 1 AS ok FROM "Achievement" WHERE "id" = $1 AND "guildId" = $2 FOR UPDATE"#;

/// The `achievements` store.
pub struct AchievementsStore {
    app: App,
}

impl AchievementsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

// ── Tiers: flat dashboard shape <-> the stored { goal, rewards } ──────────────

/// `tierToWire(tier)`.
pub fn tier_to_wire(tier: &Tier) -> Value {
    let role = |f: fn(&Reward) -> Option<&String>| tier.rewards.iter().find_map(f).map_or(Value::Null, |r| json!(r));
    let amount = |f: fn(&Reward) -> Option<i64>| tier.rewards.iter().find_map(f).map_or(Value::Null, |n| json!(n));
    json!({
        "goal": tier.goal,
        "roleAdd": role(|r| if let Reward::RoleAdd(x) = r { Some(x) } else { None }),
        "roleRemove": role(|r| if let Reward::RoleRemove(x) = r { Some(x) } else { None }),
        "xp": amount(|r| if let Reward::Xp(n) = r { Some(*n) } else { None }),
        "coins": amount(|r| if let Reward::Coins(n) = r { Some(*n) } else { None }),
    })
}

/// `tierFromWire(tier)`: `{ goal, rewards }` (rewards normalised).
pub fn tier_from_wire(tier: &Value) -> Value {
    let mut rewards = Vec::new();
    if truthy(tier.get("roleAdd")) {
        rewards.push(json!({ "type": "ROLE_ADD", "roleId": tier["roleAdd"] }));
    }
    if truthy(tier.get("roleRemove")) {
        rewards.push(json!({ "type": "ROLE_REMOVE", "roleId": tier["roleRemove"] }));
    }
    if truthy(tier.get("xp")) {
        rewards.push(json!({ "type": "XP", "amount": tier["xp"] }));
    }
    if truthy(tier.get("coins")) {
        rewards.push(json!({ "type": "COINS", "amount": tier["coins"] }));
    }
    let mut m = Map::new();
    if let Some(g) = tier.get("goal") {
        m.insert("goal".into(), g.clone());
    }
    m.insert("rewards".into(), rewards_json(&normalize_rewards(&Value::Array(rewards))));
    Value::Object(m)
}

/// `toItem(row, members)`.
pub fn achievement_item(row: &Achievement, members: i64) -> Value {
    let trigger = row.trigger.as_str();
    json!({
        "id": row.id,
        "name": row.name,
        "description": row.description,
        "emoji": row.emoji,
        "trigger": trigger,
        "tiers": normalize_tiers(&row.tiers).iter().map(tier_to_wire).collect::<Vec<_>>(),
        "trackPastProgress": row.track_past_progress,
        "deadline": row.deadline.map(|d| bot_db::format_js_date(&d)),
        "enabled": row.enabled,
        "triggerLabel": trigger_label(trigger).unwrap_or(trigger),
        "members": members,
    })
}

fn name_taken(name: &str) -> RecordError {
    RecordError::new(RecordErrorCode::Conflict, format!("An achievement named \"{name}\" already exists.")).with_path("name")
}

/// `cleanName(name)`.
fn clean_name(name: Option<&Value>) -> Result<String, StoreError> {
    let s = match name {
        None | Some(Value::Null) => String::new(),
        Some(v) => dash_schema::js::to_js_string(Some(v)),
    };
    normalize_achievement_name(Some(&s))
        .ok_or_else(|| RecordError::invalid(format!("Names must be 1 to {NAME_MAX} characters.")).with_path("name").into())
}

/// `cleanTiers(tiers, trigger)`: the stored tiers, or RecordError('invalid') at `tiers`.
fn clean_tiers(tiers: &[Value], trigger: &str) -> Result<Vec<Tier>, RecordError> {
    let wire: Vec<Value> = tiers.iter().map(tier_from_wire).collect();
    let stored = serialize_tiers(&Value::Array(wire));
    let verdict = validate_tiers(&tiers_json(&stored), trigger);
    match verdict {
        Err(e) => Err(RecordError::invalid(e).with_path("tiers")),
        Ok(()) if stored.len() != tiers.len() => Err(RecordError::invalid("Goals must go up from tier to tier.").with_path("tiers")),
        Ok(()) => Ok(stored),
    }
}

fn list_of(v: Option<&Value>) -> &[Value] {
    v.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `assertRewardsAvailable(guildId, tiers, stored)`: XP and coin rewards only work while their module is on; only new or
/// changed rewards count.
async fn assert_rewards_available(app: &App, guild_id: &str, tiers: &[Value], stored: &[Tier]) -> Result<(), StoreError> {
    let before: Vec<Value> = stored.iter().map(tier_to_wire).collect();
    let added = |key: &str| {
        tiers.iter().enumerate().any(|(i, t)| {
            truthy(t.get(key))
                && match before.get(i).and_then(|b| b.get(key)) {
                    None => true,
                    Some(b) => !dash_schema::js::strict_eq(t.get(key).unwrap_or(&Value::Null), b),
                }
        })
    };
    let (wants_xp, wants_coins) = (added("xp"), added("coins"));
    if !wants_xp && !wants_coins {
        return Ok(());
    }
    let settings = app.settings().get_settings(guild_id).await.map_err(internal)?;
    if wants_xp && !is_module_enabled(Some(&settings), Some(ModuleKey::Leveling)) {
        return Err(RecordError::invalid("XP rewards need the Levels module. Turn it on first.").with_path("tiers").into());
    }
    if wants_coins && !is_module_enabled(Some(&settings), Some(ModuleKey::Economy)) {
        return Err(RecordError::invalid("Coin rewards need the Economy module. Turn it on first.").with_path("tiers").into());
    }
    Ok(())
}

/// `updateMany` of AchievementProgress (Prisma sets `updatedAt` too).
async fn progress_update(tx: &mut PgConnection, set: &str, filter: &str, id: &str, n: Option<i32>) -> Result<(), TxError> {
    let sql = format!(r#"UPDATE "AchievementProgress" SET {set}, "updatedAt" = {} WHERE "achievementId" = $1{filter}"#, bot_db::SQL_NOW);
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id);
    if let Some(n) = n {
        q = q.bind(n);
    }
    q.execute(&mut *tx).await?;
    Ok(())
}

/// `applyProgressFixups(tx, achievementId, before, after)`: the progress fix-ups of a tier change, by position.
async fn apply_progress_fixups(tx: &mut PgConnection, id: &str, before: &[Tier], after: &[Tier]) -> Result<(), TxError> {
    if after.len() > before.len() {
        progress_update(tx, r#""completedAt" = NULL"#, r#" AND "completedAt" IS NOT NULL"#, id, None).await?;
    } else if after.len() < before.len() {
        let n = after.len() as i32;
        progress_update(tx, r#""tier" = $2"#, r#" AND "tier" > $2"#, id, Some(n)).await?;
        progress_update(tx, &format!(r#""completedAt" = {}"#, bot_db::SQL_NOW), r#" AND "tier" >= $2 AND "completedAt" IS NULL"#, id, Some(n)).await?;
        progress_update(tx, r#""almostNotified" = false"#, r#" AND "tier" = $2"#, id, Some(n)).await?;
    }
    for i in 0..before.len().min(after.len()) {
        if before[i].goal != after[i].goal {
            progress_update(tx, r#""almostNotified" = false"#, r#" AND "tier" = $2"#, id, Some(i as i32)).await?;
        }
    }
    Ok(())
}

async fn find(app: &App, guild_id: &str, id: &str) -> Result<Option<Achievement>, StoreError> {
    sqlx::query_as::<_, Achievement>(r#"SELECT * FROM "Achievement" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)
}

/// `getItem(guildId, id)` (the member count is not filtered by guild, as in the JS).
async fn get_item(app: &App, guild_id: &str, id: &str) -> Result<Option<Value>, StoreError> {
    let Some(row) = find(app, guild_id, id).await? else { return Ok(None) };
    let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "AchievementProgress" WHERE "achievementId" = $1"#)
        .bind(id)
        .fetch_one(app.db())
        .await
        .map_err(internal)?;
    Ok(Some(achievement_item(&row, n)))
}

/// `new Date(v)` of a deadline: the ISO text (Prisma refuses an invalid date).
fn date_value(v: Option<&Value>) -> Result<Value, StoreError> {
    if !truthy(v) {
        return Ok(Value::Null);
    }
    let ms = match v {
        Some(Value::String(s)) => bot_util::js::date_parse(s),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        _ => f64::NAN,
    };
    bot_util::js::to_iso_string(ms).map(Value::String).ok_or_else(|| StoreError::Internal(anyhow::anyhow!("Invalid Date")))
}

#[async_trait]
impl RecordStore for AchievementsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let rows = list_achievements(self.app.db(), guild_id).await.map_err(internal)?;
        let groups: Vec<(String, i64)> = sqlx::query_as(
            r#"SELECT "achievementId", COUNT(*)::bigint FROM "AchievementProgress" WHERE "guildId" = $1 GROUP BY "achievementId""#,
        )
        .bind(guild_id)
        .fetch_all(self.app.db())
        .await
        .map_err(internal)?;
        let counts: HashMap<String, i64> = groups.into_iter().collect();
        Ok(ListAnswer::items(
            rows.iter().take(limits::ACHIEVEMENTS).map(|r| achievement_item(r, counts.get(&r.id).copied().unwrap_or(0))).collect(),
        ))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        get_item(&self.app, guild_id, id).await
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let name = clean_name(data.get("name"))?;
        let trigger = data.get("trigger").and_then(Value::as_str).unwrap_or("").to_string();
        let wire = list_of(data.get("tiers")).to_vec();
        let tiers = clean_tiers(&wire, &trigger)?;
        assert_rewards_available(&self.app, guild_id, &wire, &[]).await?;
        self.app.settings().ensure_guild_settings(guild_id).await.map_err(internal)?;
        let mut values: Vec<(&str, Value)> = vec![
            ("guildId", json!(guild_id)),
            ("name", json!(name)),
            ("description", crate::common::nn(data.get("description"))),
            ("emoji", crate::common::nn(data.get("emoji"))),
            ("trigger", json!(trigger)),
            ("deadline", date_value(data.get("deadline"))?),
            ("tiers", tiers_json(&tiers)),
        ];
        // trackPastProgress / enabled: undefined takes the column default (Prisma leaves them out).
        for k in ["trackPastProgress", "enabled"] {
            if let Some(v) = data.get(k) {
                values.push((k, v.clone()));
            }
        }
        let (g, n) = (guild_id.to_string(), name.clone());
        let out = bot_db::transaction(self.app.db(), TX_OPTS, move |tx| {
            Box::pin(async move {
                lock_guild_settings(tx, &g).await?;
                let (count,): (i64,) =
                    sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Achievement" WHERE "guildId" = $1"#).bind(&g).fetch_one(&mut *tx).await?;
                if count >= limits::ACHIEVEMENTS as i64 {
                    return Err(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("A server can have at most {} achievements. Delete one first.", limits::ACHIEVEMENTS),
                    )
                    .into());
                }
                let clash: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "Achievement" WHERE "guildId" = $1 AND "name" ILIKE $2 LIMIT 1"#)
                    .bind(&g)
                    .bind(&n)
                    .fetch_optional(&mut *tx)
                    .await?;
                if clash.is_some() {
                    return Err(name_taken(&n).into());
                }
                let row: Achievement = InsertBuilder::new(&ACHIEVEMENT_INFO).values(values)?.build()?.fetch_one(&mut *tx).await?;
                Ok::<_, TxError>(row)
            })
        })
        .await;
        let row = match out {
            Ok(row) => row,
            Err(e) if e.is_unique() => return Err(name_taken(&name).into()),
            Err(e) => return Err(e.into()),
        };
        refresh_achievement_cache(&self.app, guild_id).await;
        tracing::info!("Dashboard achievement {} created in {guild_id} by {}", row.id, ctx.user_id);
        Ok(Saved::item(achievement_item(&row, 0)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let mut patch: Vec<(&'static str, Value)> = Vec::new();
        let mut patch_name: Option<String> = None;
        if data.contains_key("name") {
            let n = clean_name(data.get("name"))?;
            patch.push(("name", json!(n)));
            patch_name = Some(n);
        }
        for k in ["description", "emoji", "trackPastProgress"] {
            if let Some(v) = data.get(k) {
                patch.push((k, v.clone()));
            }
        }
        if data.contains_key("deadline") {
            patch.push(("deadline", date_value(data.get("deadline"))?));
        }
        if let Some(v) = data.get("enabled") {
            patch.push(("enabled", v.clone()));
        }
        let wire: Option<Vec<Value>> = data.contains_key("tiers").then(|| list_of(data.get("tiers")).to_vec());
        if let Some(w) = &wire {
            let existing: Option<(Value,)> = sqlx::query_as(r#"SELECT "tiers" FROM "Achievement" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
                .bind(id)
                .bind(guild_id)
                .fetch_optional(self.app.db())
                .await
                .map_err(internal)?;
            let Some((tiers,)) = existing else { return Err(RecordError::not_found("That achievement no longer exists.").into()) };
            assert_rewards_available(&self.app, guild_id, w, &normalize_tiers(&tiers)).await?;
        }
        let (g, i, d) = (guild_id.to_string(), id.to_string(), data.clone());
        let now_ms = self.app.clock().now_ms() as f64;
        let pn = patch_name.clone();
        let out = bot_db::transaction(self.app.db(), TX_OPTS, move |tx| {
            Box::pin(async move {
                sqlx::query(ACHIEVEMENT_LOCK_SQL).bind(&i).bind(&g).fetch_all(&mut *tx).await?;
                let current = sqlx::query_as::<_, Achievement>(r#"SELECT * FROM "Achievement" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
                    .bind(&i)
                    .bind(&g)
                    .fetch_optional(&mut *tx)
                    .await?;
                let Some(current) = current else { return Err(RecordError::not_found("That achievement no longer exists.").into()) };
                // The cross-field rules again, against the row as it is now.
                let current_item = achievement_item(&current, 0);
                let check = CheckCtx { mode: Mode::Update, current: current_item.as_object(), now_ms };
                if let Some(p) = check_achievement(&d, &check) {
                    return Err(RecordError::invalid(p.message).with_path(p.path).into());
                }
                if let Some(n) = &pn {
                    let clash: Option<(String,)> =
                        sqlx::query_as(r#"SELECT "id" FROM "Achievement" WHERE "guildId" = $1 AND "id" <> $2 AND "name" ILIKE $3 LIMIT 1"#)
                            .bind(&g)
                            .bind(&i)
                            .bind(n)
                            .fetch_optional(&mut *tx)
                            .await?;
                    if clash.is_some() {
                        return Err(name_taken(n).into());
                    }
                }
                if let Some(w) = &wire {
                    let tiers = clean_tiers(w, current.trigger.as_str())?;
                    apply_progress_fixups(tx, &i, &normalize_tiers(&current.tiers), &tiers).await?;
                    patch.push(("tiers", tiers_json(&tiers)));
                }
                if !patch.is_empty() {
                    UpdateBuilder::new(&ACHIEVEMENT_INFO).set_all(patch)?.filter("id", json!(i))?.without_returning().build()?.execute(&mut *tx).await?;
                }
                Ok::<_, TxError>(())
            })
        })
        .await;
        match out {
            Ok(()) => {}
            Err(e) if e.is_unique() => return Err(name_taken(patch_name.as_deref().unwrap_or("")).into()),
            Err(e) => return Err(e.into()),
        }
        refresh_achievement_cache(&self.app, guild_id).await;
        tracing::info!("Dashboard achievement {id} updated in {guild_id} by {}", ctx.user_id);
        Ok(Saved::item(get_item(&self.app, guild_id, id).await?.unwrap_or(Value::Null)))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
        if delete_achievement(self.app.db(), guild_id, id).await.map_err(internal)? == 0 {
            return Err(RecordError::not_found("That achievement no longer exists.").into());
        }
        refresh_achievement_cache(&self.app, guild_id).await;
        tracing::info!("Dashboard achievement {id} deleted in {guild_id} by {}", ctx.user_id);
        Ok(())
    }
}
