//! Port of `src/dashboard/collections/shopItems.store.js` (the ShopItem table). Writes go through feat-economy
//! (create_item, update_item, delete_item), which keep the per-server cap and the case-insensitive name unique under a
//! lock on the GuildSettings row, exactly like /economy-admin item. Shop items are not cached anywhere.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{SHOP_ITEM_INFO, SelectBuilder, ShopItem};
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError};
use feat_economy::economy_service as eco;
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R, locale_int, log_info, now_ms, rec_at, recheck, row_json};

fn type_text(t: &str) -> Option<&'static str> {
    match t {
        "STATIC" => Some("collectible"),
        "USABLE" => Some("usable"),
        "ROLE" => Some("role"),
        _ => None,
    }
}

fn type_of(row: &ShopItem) -> String {
    row_json(row).get("type").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// `summaryOf(row)`: "1,000 · role · stock 3 · hidden".
fn summary_of(row: &ShopItem) -> String {
    let ty = type_of(row);
    let mut parts = vec![locale_int(i64::from(row.price)), type_text(&ty).map_or(ty.clone(), str::to_string)];
    if let Some(stock) = row.stock {
        parts.push(if stock == 0 { "sold out".to_string() } else { format!("stock {}", locale_int(i64::from(stock))) });
    }
    if !row.enabled {
        parts.push("hidden".to_string());
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// `toItem(row)`.
pub fn to_item(row: &ShopItem) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "price": row.price,
        "type": type_of(row),
        "roleId": row.role_id,
        "description": row.description,
        "imageUrl": row.image_url,
        "useOnBuy": row.use_on_buy,
        "destroyOnUse": row.destroy_on_use,
        "canBuyMultiple": row.can_buy_multiple,
        "maxPerUser": row.max_per_user,
        "stock": row.stock,
        "buyMessage": row.buy_message,
        "useMessage": row.use_message,
        "enabled": row.enabled,
        "summary": summary_of(row),
        "createdAt": bot_db::format_js_date(&row.created_at),
    })
}

fn check_name(name: Option<&Value>) -> R<String> {
    let s = match name {
        Some(Value::String(s)) => s.clone(),
        Some(other) => dash_schema::js::to_js_string(Some(other)),
        None => "undefined".to_string(),
    };
    eco::validate_item_name(&s).map_err(|reason| rec_at(RecordErrorCode::Invalid, reason, "name"))
}

/// `mapError(err, name)`: the service's EconomyError as the admin sees it.
fn map_error(err: anyhow::Error, name: &str) -> StoreError {
    let Some(e) = eco::economy_error(&err) else { return StoreError::Internal(err) };
    match e.code {
        eco::EconomyCode::ItemGone => RecordError::not_found("That item no longer exists.").into(),
        eco::EconomyCode::Invalid if e.reason() == Some("limit") => {
            RecordError::new(RecordErrorCode::Limit, format!("This server already has {} shop items (the maximum).", limits::SHOP_ITEMS)).into()
        }
        eco::EconomyCode::Invalid => rec_at(RecordErrorCode::Conflict, format!("An item named \"{name}\" already exists."), "name"),
        _ => StoreError::Internal(err),
    }
}

/// The `shopItems` store.
pub struct ShopItemsStore {
    app: App,
}

impl ShopItemsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<ShopItem>> {
        SelectBuilder::new(&SHOP_ITEM_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<ShopItem, _>(self.app.db())
            .await
            .oi()
    }
}

#[async_trait]
impl RecordStore for ShopItemsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let rows = eco::list_items(&self.app, guild_id, false, true).await.oi()?;
        Ok(ListAnswer::items(rows.iter().map(to_item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        Ok(self.find(guild_id, id).await?.as_ref().map(to_item))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let name = check_name(data.get("name"))?;
        let mut item = Map::new();
        item.insert("name".into(), json!(name));
        // Keys the request does not set stay out (undefined), so the column defaults apply.
        let mut copy = |key: &str, value: Option<Value>| {
            if let Some(v) = value {
                item.insert(key.into(), v);
            }
        };
        copy("price", data.get("price").cloned());
        copy("type", data.get("type").cloned());
        let is_role = data.get("type").and_then(Value::as_str) == Some("ROLE");
        copy("roleId", if is_role { data.get("roleId").cloned() } else { Some(Value::Null) });
        for key in [
            "description", "imageUrl", "useOnBuy", "destroyOnUse", "canBuyMultiple", "maxPerUser", "stock", "buyMessage", "useMessage", "enabled",
        ] {
            copy(key, data.get(key).cloned());
        }
        match eco::create_item(&self.app, guild_id, item).await {
            Ok(row) => {
                log_info(format!("Dashboard shop item {} created in {guild_id} by {}", row.id, ctx.user_id));
                Ok(Saved::item(to_item(&row)))
            }
            Err(err) => Err(map_error(err, &name)),
        }
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, _ctx: &StoreCtx) -> R<Saved> {
        let current = self.find(guild_id, id).await?.ok_or_else(|| RecordError::not_found("That item no longer exists."))?;
        // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
        recheck("shopItems", &data, &to_item(&current), now_ms(&self.app))?;
        let mut patch = data;
        if patch.contains_key("name") {
            let name = check_name(patch.get("name"))?;
            patch.insert("name".into(), json!(name));
        }
        // Changing away from Role drops the role, as /economy-admin item edit does.
        if let Some(t) = patch.get("type")
            && t.as_str() != Some("ROLE")
            && type_of(&current) == "ROLE"
        {
            patch.insert("roleId".into(), Value::Null);
        }
        if patch.is_empty() {
            return Ok(Saved::item(to_item(&current)));
        }
        let name = patch.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| current.name.clone());
        match eco::update_item(&self.app, guild_id, id, patch).await {
            Ok(row) => Ok(Saved::item(to_item(&row))),
            Err(err) => Err(map_error(err, &name)),
        }
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        if self.find(guild_id, id).await?.is_none() {
            return Err(RecordError::not_found("That item no longer exists.").into());
        }
        let copies = eco::delete_item(&self.app, guild_id, id).await.oi()?;
        log_info(format!("Dashboard shop item {id} deleted ({copies} copies) in {guild_id} by {}", ctx.user_id));
        Ok(())
    }
}
