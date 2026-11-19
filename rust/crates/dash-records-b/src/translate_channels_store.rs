//! Port of `src/dashboard/collections/translateChannels.store.js` (the TranslateChannel table). Same effects as
//! /autotranslate: the per-server cap, turning the Auto-Translate module on when a channel is added (through the
//! settings service), and a refresh of the channel list in Redis (`guild:{g}:translateChannels`) after every write.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_core::settings::SettingsPatch;
use bot_db::{SelectBuilder, TRANSLATE_CHANNEL_INFO, TranslateChannel};
use dash_collections::collections::small::{MAX_TARGETS, describe_languages};
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx};
use feat_translation::translation_service as tr;
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R, js_slice, log_info, rec_at};

fn to_item(row: &TranslateChannel) -> Value {
    let mut target_langs = tr::normalize_target_list(&row.target_langs).valid;
    target_langs.truncate(MAX_TARGETS);
    let codes: Vec<Value> = target_langs.iter().map(|c| json!(c)).collect();
    json!({ "id": row.id, "channelId": row.channel_id, "targetLangs": target_langs, "languages": describe_languages(&codes) })
}

/// `String(x)` of a list entry.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => dash_schema::js::to_js_string(Some(other)),
    }
}

/// `cleanLanguages(input)`: canonical codes, or an error naming the language that is not known.
fn clean_languages(input: Option<&Value>) -> R<Vec<String>> {
    let inputs: Vec<String> = input.and_then(Value::as_array).map(|a| a.iter().map(js_string).collect()).unwrap_or_default();
    let list = tr::normalize_target_list(&inputs);
    if let Some(first) = list.invalid.first() {
        return Err(rec_at(RecordErrorCode::Invalid, format!("\"{}\" is not a language I know.", js_slice(first, 40)), "targetLangs"));
    }
    if list.valid.is_empty() {
        return Err(rec_at(RecordErrorCode::Invalid, "Pick at least one language.", "targetLangs"));
    }
    let mut valid = list.valid;
    valid.truncate(MAX_TARGETS);
    Ok(valid)
}

fn already_set() -> dash_collections::StoreError {
    rec_at(RecordErrorCode::Conflict, "That channel already has auto-translate. Edit its languages instead.", "channelId")
}

fn gone() -> RecordError {
    RecordError::not_found("That translated channel no longer exists.")
}

/// The `translateChannels` store.
pub struct TranslateChannelsStore {
    app: App,
}

impl TranslateChannelsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<TranslateChannel>> {
        SelectBuilder::new(&TRANSLATE_CHANNEL_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<TranslateChannel, _>(self.app.db())
            .await
            .oi()
    }
}

#[async_trait]
impl RecordStore for TranslateChannelsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let rows = SelectBuilder::new(&TRANSLATE_CHANNEL_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("createdAt", true)
            .oi()?
            .limit(limits::TRANSLATE_CHANNELS as i64)
            .build()
            .oi()?
            .fetch_all::<TranslateChannel, _>(self.app.db())
            .await
            .oi()?;
        Ok(ListAnswer::items(rows.iter().map(to_item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        Ok(self.find(guild_id, id).await?.as_ref().map(to_item))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let target_langs = clean_languages(data.get("targetLangs"))?;
        self.app.settings().ensure_guild_settings(guild_id).await.oi()?;
        let channel_id = data.get("channelId").map(js_string).unwrap_or_default();
        // The same per-guild lock as /autotranslate, so the cap holds when both save at once.
        let saved = match tr::save_translate_channel(&self.app, guild_id, &channel_id, &target_langs, false).await {
            Ok(s) => s,
            Err(e) if e.is_unique_violation() => return Err(already_set()),
            Err(e) => return Err(e).oi(),
        };
        let row = match saved {
            tr::SaveOutcome::Saved { row, .. } => row,
            tr::SaveOutcome::Exists => return Err(already_set()),
            tr::SaveOutcome::Limit { .. } => {
                return Err(RecordError::new(
                    RecordErrorCode::Limit,
                    format!("This server already has {} auto-translate channels (the maximum). Turn one off first.", limits::TRANSLATE_CHANNELS),
                )
                .into());
            }
        };
        // /autotranslate switches the module on when a channel is added; the dashboard does the same.
        let settings = self.app.settings().get_settings(guild_id).await.oi()?;
        if !settings.translation_enabled {
            self.app.settings().update_settings(guild_id, SettingsPatch::new().set("translationEnabled", true)).await.oi()?;
        }
        tr::refresh_translate_cache(&self.app, guild_id).await;
        log_info(format!("Dashboard auto-translate {} ({}) created in {guild_id} by {}", row.id, row.channel_id, ctx.user_id));
        Ok(Saved::item(to_item(&row)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, _ctx: &StoreCtx) -> R<Saved> {
        if data.contains_key("targetLangs") {
            let target_langs = clean_languages(data.get("targetLangs"))?;
            let count = bot_db::UpdateBuilder::new(&TRANSLATE_CHANNEL_INFO)
                .set("targetLangs", json!(target_langs))
                .oi()?
                .filter("id", json!(id))
                .oi()?
                .filter("guildId", json!(guild_id))
                .oi()?
                .without_returning()
                .build()
                .oi()?
                .execute(self.app.db())
                .await
                .oi()?;
            if count == 0 {
                return Err(gone().into());
            }
            tr::refresh_translate_cache(&self.app, guild_id).await;
        }
        let row = self.find(guild_id, id).await?.ok_or_else(gone)?;
        Ok(Saved::item(to_item(&row)))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        let count = bot_db::delete_where(&TRANSLATE_CHANNEL_INFO, [("id", json!(id)), ("guildId", json!(guild_id))])
            .oi()?
            .execute(self.app.db())
            .await
            .oi()?;
        if count == 0 {
            return Err(gone().into());
        }
        tr::refresh_translate_cache(&self.app, guild_id).await;
        log_info(format!("Dashboard auto-translate {id} deleted in {guild_id} by {}", ctx.user_id));
        Ok(())
    }
}
