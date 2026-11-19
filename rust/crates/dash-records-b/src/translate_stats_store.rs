//! Port of `src/dashboard/collections/translateStats.store.js`: reads the TranslateStat counters and the
//! TranslateChannel rows of a server. Read-only; the bot writes the counters (feat-translation translate_stats_service).

use async_trait::async_trait;
use bot_core::App;
use bot_db::{SelectBuilder, TRANSLATE_CHANNEL_INFO, TRANSLATE_STAT_INFO, TranslateChannel, TranslateStat};
use dash_collections::collections::translate_stats::build_stat_items;
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordStore, Saved, StoreCtx};
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R};

/// Counter rows read per server (channels x languages x via x direction stays far below this).
pub const MAX_ROWS: i64 = 20_000;
const READ_ONLY: &str = "Translation activity is counted by the bot and cannot be changed here.";

/// The `translateStats` store.
pub struct TranslateStatsStore {
    app: App,
}

impl TranslateStatsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn load(&self, guild_id: &str, channel_id: Option<&str>) -> R<Vec<Value>> {
        let mut stats = SelectBuilder::new(&TRANSLATE_STAT_INFO).filter("guildId", json!(guild_id)).oi()?;
        let mut channels = SelectBuilder::new(&TRANSLATE_CHANNEL_INFO).filter("guildId", json!(guild_id)).oi()?;
        if let Some(c) = channel_id {
            stats = stats.filter("channelId", json!(c)).oi()?;
            channels = channels.filter("channelId", json!(c)).oi()?;
        }
        let stats = stats.limit(MAX_ROWS).build().oi()?;
        let channels = channels.limit(bot_config::tables::limits::TRANSLATE_CHANNELS as i64).build().oi()?;
        let (rows, configured) = tokio::try_join!(
            stats.fetch_all::<TranslateStat, _>(self.app.db()),
            channels.fetch_all::<TranslateChannel, _>(self.app.db()),
        )
        .oi()?;
        // STAT_SELECT / { channelId, targetLangs }.
        let rows: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "channelId": r.channel_id, "via": r.via, "direction": r.direction, "lang": r.lang, "count": r.count,
                    "firstAt": bot_db::format_js_date(&r.first_at), "lastAt": bot_db::format_js_date(&r.last_at),
                })
            })
            .collect();
        let configured: Vec<Value> = configured.iter().map(|c| json!({ "channelId": c.channel_id, "targetLangs": c.target_langs })).collect();
        Ok(build_stat_items(&rows, &configured))
    }
}

#[async_trait]
impl RecordStore for TranslateStatsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        Ok(ListAnswer::items(self.load(guild_id, None).await?))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        // `channelId ? { guildId, channelId } : { guildId }`: an empty id reads the whole server.
        let channel = (!id.is_empty()).then_some(id);
        Ok(self.load(guild_id, channel).await?.into_iter().next())
    }

    async fn create(&self, _guild_id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> R<Saved> {
        Err(RecordError::invalid(READ_ONLY).into())
    }

    async fn update(&self, _guild_id: &str, _id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> R<Saved> {
        Err(RecordError::invalid(READ_ONLY).into())
    }

    async fn remove(&self, _guild_id: &str, _id: &str, _ctx: &StoreCtx) -> R<()> {
        Err(RecordError::invalid(READ_ONLY).into())
    }
}
