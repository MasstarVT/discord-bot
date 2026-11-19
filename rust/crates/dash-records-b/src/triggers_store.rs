//! Port of `src/dashboard/collections/triggers.store.js`: the AutoTrigger rows /tag trigger writes, through the same
//! tagService helpers (create_trigger_row_with: per-guild advisory lock and cap; write_trigger: optimistic update;
//! validate_trigger_pattern: safe regexes). Every write drops the shared Redis list (invalidate_trigger_cache).

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{AUTO_TRIGGER_INFO, AutoTrigger, SelectBuilder};
use dash_collections::collections::triggers::trigger_item;
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx};
use feat_tags::tag_service as tags;
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R, actor_id, rec_at, row_json};

fn not_found() -> RecordError {
    RecordError::not_found("That auto-responder no longer exists.")
}

const FIELDS: [&str; 9] =
    ["trigger", "matchType", "caseSensitive", "response", "cooldown", "deleteMessage", "channelIds", "exemptRoleIds", "enabled"];

fn item(row: &AutoTrigger) -> Value {
    trigger_item(&row_json(row))
}

fn check_pattern(trigger: &str, match_type: &str, case_sensitive: bool) -> R<()> {
    tags::validate_trigger_pattern(trigger, match_type, case_sensitive).map_err(|reason| rec_at(RecordErrorCode::Invalid, reason, "trigger"))
}

/// The `triggers` store.
pub struct TriggersStore {
    app: App,
}

impl TriggersStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<AutoTrigger>> {
        SelectBuilder::new(&AUTO_TRIGGER_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<AutoTrigger, _>(self.app.db())
            .await
            .oi()
    }
}

fn match_type_str(row: &AutoTrigger) -> String {
    match row_json(row).get("matchType") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

#[async_trait]
impl RecordStore for TriggersStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        // Creation order: the #numbers /tag trigger list shows, and the order in which triggers are tried.
        let rows = SelectBuilder::new(&AUTO_TRIGGER_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("createdAt", true)
            .oi()?
            .order_by("id", true)
            .oi()?
            .limit(limits::AUTO_TRIGGERS as i64)
            .build()
            .oi()?
            .fetch_all::<AutoTrigger, _>(self.app.db())
            .await
            .oi()?;
        Ok(ListAnswer::items(rows.iter().map(item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        Ok(self.find(guild_id, id).await?.as_ref().map(item))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        check_pattern(
            data.get("trigger").and_then(Value::as_str).unwrap_or(""),
            data.get("matchType").and_then(Value::as_str).unwrap_or(""),
            data.get("caseSensitive") == Some(&Value::Bool(true)),
        )?;
        self.app.settings().ensure_guild_settings(guild_id).await.oi()?;
        let mut fields: tags::Patch = FIELDS.iter().filter_map(|k| data.get(*k).map(|v| (*k, v.clone()))).collect();
        fields.push(("createdBy", json!(actor_id(ctx))));
        let row = match tags::create_trigger_row_with(&self.app, guild_id, fields).await {
            Ok(r) => r,
            Err(err) => {
                if tags::as_tag_error(&err).is_some_and(|e| e.code == "LIMIT") {
                    return Err(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("This server already has {} auto-responders, the maximum. Delete one first.", limits::AUTO_TRIGGERS),
                    )
                    .into());
                }
                return Err(err.into());
            }
        };
        tags::invalidate_trigger_cache(&self.app, guild_id).await;
        Ok(Saved::item(item(&row)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, _ctx: &StoreCtx) -> R<Saved> {
        let row = self.find(guild_id, id).await?.ok_or_else(not_found)?;
        let patch: tags::Patch = FIELDS.iter().filter_map(|k| data.get(*k).map(|v| (*k, v.clone()))).collect();
        if patch.is_empty() {
            return Ok(Saved::item(item(&row)));
        }
        let get = |k: &str| patch.iter().find(|(p, _)| *p == k).map(|(_, v)| v).filter(|v| !v.is_null());
        if ["trigger", "matchType", "caseSensitive"].iter().any(|k| patch.iter().any(|(p, _)| p == k)) {
            let trigger = get("trigger").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| row.trigger.clone());
            let match_type = get("matchType").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| match_type_str(&row));
            let case_sensitive = get("caseSensitive").map_or(row.case_sensitive, |v| v == &Value::Bool(true));
            check_pattern(&trigger, &match_type, case_sensitive)?;
        }
        let updated = match tags::write_trigger(&self.app, guild_id, &row, patch).await {
            Ok(r) => r,
            Err(err) => {
                if tags::as_tag_error(&err).is_some_and(|e| e.code == "CONFLICT") {
                    if self.find(guild_id, id).await?.is_none() {
                        // writeTrigger answers null for a row deleted mid-update; the JS drops the cache before its not_found.
                        tags::invalidate_trigger_cache(&self.app, guild_id).await;
                        return Err(not_found().into());
                    }
                    return Err(RecordError::new(
                        RecordErrorCode::Conflict,
                        "Someone else changed this auto-responder meanwhile. Reload it and try again.",
                    )
                    .into());
                }
                return Err(err.into());
            }
        };
        tags::invalidate_trigger_cache(&self.app, guild_id).await;
        Ok(Saved::item(item(&updated)))
    }

    async fn remove(&self, guild_id: &str, id: &str, _ctx: &StoreCtx) -> R<()> {
        let count = bot_db::delete_where(&AUTO_TRIGGER_INFO, [("id", json!(id)), ("guildId", json!(guild_id))])
            .oi()?
            .execute(self.app.db())
            .await
            .oi()?;
        if count == 0 {
            return Err(not_found().into());
        }
        tags::invalidate_trigger_cache(&self.app, guild_id).await;
        Ok(())
    }
}
