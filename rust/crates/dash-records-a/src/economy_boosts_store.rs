//! Port of `src/dashboard/collections/economyBoosts.store.js`: the `economyBoosts` collection (the EconomyBoost table).
//! Create and delete use economyService (createBoost, deleteBoost), which keep the cap and the case-insensitive unique
//! name under a lock on the GuildSettings row. The dashboard edits name, bonus, role and the on/off switch under the
//! same lock. The period is create-only. Boosts are not cached anywhere.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{BoostPeriod, ECONOMY_BOOST_INFO, EconomyBoost, UpdateBuilder};
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError};
use feat_economy::economy_service::{self as eco, EconomyCode, NewBoost};
use serde_json::{Map, Value, json};

use crate::common::{TX_OPTS, TxError, int, internal, lock_guild_settings, row_json, str_at};

/// The `economyBoosts` store.
pub struct EconomyBoostsStore {
    app: App,
}

impl EconomyBoostsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

const PERIOD_TEXT: [(&str, &str); 4] = [("ONE_TIME", "one-time"), ("DAILY", "daily"), ("WEEKLY", "weekly"), ("MONTHLY", "monthly")];

/// `toItem(row)`: a boost row (with `_count.uses`) as an item.
pub fn boost_item(row: &Value) -> Value {
    let period = row.get("period").and_then(Value::as_str).unwrap_or("");
    let period_text = PERIOD_TEXT.iter().find(|(k, _)| *k == period).map_or(period, |(_, v)| *v);
    let role = row.get("roleId").filter(|v| !v.is_null()).cloned();
    let off = row.get("enabled") == Some(&Value::Bool(false));
    let percent = row.get("percent").cloned().unwrap_or(Value::Null);
    let mut parts = vec![format!("+{}%", dash_schema::js::to_js_string(Some(&percent))), period_text.to_string()];
    parts.push(if role.as_ref().is_some_and(|r| dash_schema::js::truthy(Some(r))) { "one role".into() } else { "everyone".into() });
    if off {
        parts.push("off".into());
    }
    json!({
        "id": row.get("id"),
        "name": row.get("name"),
        "percent": percent,
        "period": row.get("period"),
        "roleId": role.unwrap_or(Value::Null),
        "enabled": !off,
        "uses": row.get("_count").and_then(|c| c.get("uses")).filter(|v| !v.is_null()).cloned().unwrap_or(json!(0)),
        "summary": parts.join(" · "),
    })
}

/// `checkName(name)`: validateBoostName, refused at `name`.
fn check_name(name: &str) -> Result<String, StoreError> {
    eco::validate_boost_name(name).map_err(|reason| RecordError::invalid(reason).with_path("name").into())
}

fn name_taken(name: &str) -> RecordError {
    RecordError::new(RecordErrorCode::Conflict, format!("A boost named \"{name}\" already exists.")).with_path("name")
}

/// `mapError(err, name)`.
fn map_error(err: anyhow::Error, name: &str) -> StoreError {
    if let Some(e) = eco::economy_error(&err)
        && e.code == EconomyCode::Invalid
    {
        return match e.reason() {
            Some("limit") => RecordError::new(RecordErrorCode::Limit, format!("This server already has {} boosts (the maximum).", limits::ECONOMY_BOOSTS)).into(),
            Some("name-taken") => name_taken(name).into(),
            _ => RecordError::invalid("That boost is not valid.").into(),
        };
    }
    internal(err)
}

const WITH_USES_SQL: &str = r#"SELECT b.*, (SELECT COUNT(*)::bigint FROM "EconomyBoostUse" u WHERE u."boostId" = b."id") AS "uses"
  FROM "EconomyBoost" b WHERE b."id" = $1 AND b."guildId" = $2 LIMIT 1"#;

/// `withUses(id, guildId)`: the row with `_count.uses`.
async fn with_uses(app: &App, id: &str, guild_id: &str) -> Result<Option<Value>, StoreError> {
    let row = sqlx::query(WITH_USES_SQL).bind(id).bind(guild_id).fetch_optional(app.db()).await.map_err(internal)?;
    let Some(row) = row else { return Ok(None) };
    let boost: EconomyBoost = sqlx::FromRow::from_row(&row).map_err(internal)?;
    let uses: i64 = sqlx::Row::try_get(&row, "uses").map_err(internal)?;
    let mut v = row_json(&boost);
    v["_count"] = json!({ "uses": uses });
    Ok(Some(v))
}

#[async_trait]
impl RecordStore for EconomyBoostsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let rows = eco::list_boosts(&self.app, guild_id).await.map_err(internal)?;
        Ok(ListAnswer::items(rows.iter().map(|r| boost_item(&row_json(r))).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        Ok(with_uses(&self.app, id, guild_id).await?.as_ref().map(boost_item))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let name = check_name(str_at(&data, "name").unwrap_or(""))?;
        let period = str_at(&data, "period").and_then(BoostPeriod::parse);
        let created_by = if ctx.user_id.is_empty() { "dashboard".to_string() } else { ctx.user_id.clone() };
        let result = async {
            let Some(period) = period else { return Err(eco::EconomyError::with(EconomyCode::Invalid, json!({})).into()) };
            let new = NewBoost {
                name: name.clone(),
                percent: int(data.get("percent")).unwrap_or(0),
                period,
                role_id: str_at(&data, "roleId").map(str::to_string),
                created_by,
            };
            let mut row = eco::create_boost(&self.app, guild_id, new).await?;
            // createBoost always starts a boost on; /economy-admin boost toggle is the only way to turn it off.
            if data.get("enabled") == Some(&Value::Bool(false))
                && let Some(off) = eco::set_boost_enabled(&self.app, guild_id, &row.id, false).await?
            {
                row = off;
            }
            Ok::<_, anyhow::Error>(row)
        }
        .await;
        match result {
            Ok(row) => {
                tracing::info!(
                    "Dashboard coin boost \"{}\" +{}% ({}) created in {guild_id} by {}",
                    row.name,
                    row.percent,
                    row.period.as_str(),
                    ctx.user_id
                );
                let mut v = row_json(&row);
                v["_count"] = json!({ "uses": 0 });
                Ok(Saved::item(boost_item(&v)))
            }
            Err(e) => Err(map_error(e, &name)),
        }
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, _ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let mut patch: Vec<(&'static str, Value)> = Vec::new();
        let mut patch_name: Option<String> = None;
        if let Some(n) = data.get("name") {
            let n = check_name(n.as_str().unwrap_or(""))?;
            patch.push(("name", json!(n)));
            patch_name = Some(n);
        }
        for key in ["percent", "roleId", "enabled"] {
            if let Some(v) = data.get(key) {
                patch.push((key, v.clone()));
            }
        }
        let (g, i, pn) = (guild_id.to_string(), id.to_string(), patch_name.clone());
        let out = bot_db::transaction(self.app.db(), TX_OPTS, move |tx| {
            Box::pin(async move {
                lock_guild_settings(tx, &g).await?;
                let found: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "EconomyBoost" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
                    .bind(&i)
                    .bind(&g)
                    .fetch_optional(&mut *tx)
                    .await?;
                if found.is_none() {
                    return Err(RecordError::not_found("That boost no longer exists.").into());
                }
                if let Some(n) = &pn {
                    let clash: Option<(String,)> = sqlx::query_as(
                        r#"SELECT "id" FROM "EconomyBoost" WHERE "guildId" = $1 AND "id" <> $2 AND "name" ILIKE $3 LIMIT 1"#,
                    )
                    .bind(&g)
                    .bind(&i)
                    .bind(n)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if clash.is_some() {
                        return Err(name_taken(n).into());
                    }
                }
                if !patch.is_empty() {
                    UpdateBuilder::new(&ECONOMY_BOOST_INFO).set_all(patch)?.filter("id", json!(i))?.without_returning().build()?.execute(&mut *tx).await?;
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
        match with_uses(&self.app, id, guild_id).await? {
            Some(row) => Ok(Saved::item(boost_item(&row))),
            None => Err(StoreError::Internal(anyhow::anyhow!("Cannot read properties of null (reading 'id')"))),
        }
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
        if !eco::delete_boost(&self.app, guild_id, id).await.map_err(internal)? {
            return Err(RecordError::not_found("That boost no longer exists.").into());
        }
        tracing::info!("Dashboard coin boost {id} deleted in {guild_id} by {}", ctx.user_id);
        Ok(())
    }
}
