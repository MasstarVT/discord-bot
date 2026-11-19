//! Port of `src/dashboard/collections/statChannels.store.js` (the StatChannel table). Same rules as /stats: the
//! per-server and role-counter caps, one counter per kind (per role for Role members), validate_stat_template for the
//! name, and the recurring `stats.refresh` job (scheduled through stats_service), so the bot renames the channel on its
//! next run. Edit and delete reuse edit_template and delete_stat_channel(keep_channel: true).

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{InsertBuilder, STAT_CHANNEL_INFO, SelectBuilder, StatChannel, StatKind, TxOptions};
use bot_util::placeholder_parser::PlaceholderGuild;
use dash_collections::collections::small::{kind_info, normalize_stat_template};
use dash_collections::{
    ActionAnswer, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError,
};
use feat_starboard_stats::stats_service as stats;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::common::{OrInternal, R, is_unique, locale_int, log_info, now_ms, rec_at, row_json};

/// `TX_OPTS` (`{ timeout: 10_000, maxWait: 10_000 }`).
const TX_OPTS: TxOptions = TxOptions::TEN_SECONDS;

fn kind_str(row: &StatChannel) -> String {
    row_json(row).get("kind").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// `toItem(row)`.
pub fn to_item(row: &StatChannel) -> Value {
    let kind = kind_str(row);
    json!({
        "id": row.id,
        "channelId": row.channel_id,
        "kind": kind,
        "roleId": row.role_id,
        "template": row.template,
        "kindLabel": kind_info(&kind).map_or(kind.clone(), |(l, _)| l.to_string()),
        "lastValue": row.last_value,
        "peakValue": row.peak_value,
        "lastRenamedAt": row.last_renamed_at.as_ref().map(bot_db::format_js_date),
    })
}

/// The server's name, member count and role names, for rendering the template; None when Discord cannot be asked.
async fn guild_facts(ctx: &StoreCtx, guild_id: &str) -> Option<Arc<DiscordResources>> {
    let discord = crate::common::discord_with(ctx, dash_collections::DiscordMethod::GetGuildResources)?;
    discord.get_guild_resources(guild_id, false).await.ok()
}

/// `validateStatTemplate` with a plain guild object (the server tokens only need a name and a count).
fn clean_template(template: &str, kind: &str, role_id: Option<&str>, facts: Option<&DiscordResources>, guild_id: &str) -> R<String> {
    let role_name = if kind == "ROLE_MEMBERS" {
        Some(facts.and_then(|f| role_id.and_then(|r| f.role(r))).map_or("role".to_string(), |r| r.name.clone()))
    } else {
        None
    };
    let guild = PlaceholderGuild {
        id: guild_id.to_string(),
        name: Some(facts.map_or("Server".to_string(), |f| f.guild.name.clone())),
        member_count: Some(facts.and_then(|f| f.guild.member_count).unwrap_or(0) as f64),
        ..Default::default()
    };
    let Ok(stat_kind) = serde_json::from_value::<StatKind>(json!(kind)) else {
        return Err(rec_at(RecordErrorCode::Invalid, "Unknown counter type.", "kind"));
    };
    stats::validate_stat_template(template, stat_kind, role_name.as_deref(), Some(&guild))
        .map_err(|e| rec_at(RecordErrorCode::Invalid, e, "template"))
}

fn conflict_counter_err(channel_id: &str) -> RecordError {
    RecordError::new(RecordErrorCode::Conflict, format!("<#{channel_id}> already shows this counter. Edit that one instead.")).with_path("kind")
}

fn channel_taken_err() -> RecordError {
    RecordError::new(RecordErrorCode::Conflict, "That channel already shows a counter.").with_path("channelId")
}

fn channel_taken() -> StoreError {
    channel_taken_err().into()
}

fn gone() -> RecordError {
    RecordError::not_found("That counter no longer exists.")
}

/// `String(x)`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => dash_schema::js::to_js_string(Some(other)),
    }
}

/// The `statChannels` store.
pub struct StatChannelsStore {
    app: App,
}

impl StatChannelsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<StatChannel>> {
        SelectBuilder::new(&STAT_CHANNEL_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<StatChannel, _>(self.app.db())
            .await
            .oi()
    }
}

#[async_trait]
impl RecordStore for StatChannelsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let rows = SelectBuilder::new(&STAT_CHANNEL_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("createdAt", true)
            .oi()?
            .limit(limits::STAT_CHANNELS as i64)
            .build()
            .oi()?
            .fetch_all::<StatChannel, _>(self.app.db())
            .await
            .oi()?;
        Ok(ListAnswer::items(rows.iter().map(to_item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        Ok(self.find(guild_id, id).await?.as_ref().map(to_item))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let kind = data.get("kind").map(js_string).unwrap_or_default();
        let channel_id = data.get("channelId").map(js_string).unwrap_or_default();
        let role_id: Option<String> =
            if kind == "ROLE_MEMBERS" { data.get("roleId").filter(|v| crate::common::truthy(Some(v))).map(js_string) } else { None };
        if kind == "ROLE_MEMBERS" && role_id.is_none() {
            return Err(rec_at(RecordErrorCode::Invalid, "Pick the role to count for \"Role members\".", "roleId"));
        }
        if role_id.as_deref() == Some(guild_id) {
            return Err(rec_at(RecordErrorCode::Invalid, "Use the Members counter to count everyone.", "roleId"));
        }

        let facts = guild_facts(ctx, guild_id).await;
        let is_list = serde_json::from_value::<StatKind>(json!(kind)).is_ok_and(stats::is_list_kind);
        let member_count = facts.as_ref().and_then(|f| f.guild.member_count).unwrap_or(0);
        if is_list && member_count > stats::MEMBER_LIST_MAX_MEMBERS as i64 {
            return Err(rec_at(
                RecordErrorCode::Invalid,
                format!(
                    "This counter needs the full member list, which the bot only loads for servers with up to {} members. Use Members instead.",
                    locale_int(stats::MEMBER_LIST_MAX_MEMBERS as i64)
                ),
                "kind",
            ));
        }
        let wanted = match data.get("template").filter(|v| !v.is_null()) {
            Some(t) => js_string(t),
            None => kind_info(&kind).map(|(_, t)| t.to_string()).unwrap_or_default(),
        };
        let template = clean_template(&wanted, &kind, role_id.as_deref(), facts.as_deref(), guild_id)?;

        self.app.settings().ensure_guild_settings(guild_id).await.oi()?;
        let (g, k, c, r, t) = (guild_id.to_string(), kind.clone(), channel_id.clone(), role_id.clone(), template);
        let result = bot_db::transaction(self.app.db(), TX_OPTS, move |tx| {
            Box::pin(async move {
                // lockGuildSettingsSql(guildId) of levelRoles.store.js.
                sqlx::query(bot_db::tx::LOCK_GUILD_SETTINGS_SQL).bind(&g).execute(&mut *tx).await.map_err(bot_db::DbError::from)?;
                let rows: Vec<(String, StatKind, Option<String>)> =
                    sqlx::query_as(r#"SELECT "channelId", "kind", "roleId" FROM "StatChannel" WHERE "guildId" = $1"#)
                        .bind(&g)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(bot_db::DbError::from)?;
                let rows: Vec<(String, String, Option<String>)> =
                    rows.into_iter().map(|(ch, kd, ro)| (ch, json!(kd).as_str().unwrap_or_default().to_string(), ro)).collect();
                if rows.len() >= limits::STAT_CHANNELS {
                    return Err(anyhow::Error::from(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("A server can have at most {} counter channels. Remove one first.", limits::STAT_CHANNELS),
                    )));
                }
                if k == "ROLE_MEMBERS" && rows.iter().filter(|(_, kd, _)| kd == "ROLE_MEMBERS").count() >= limits::ROLE_STAT_CHANNELS {
                    return Err(anyhow::Error::from(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("A server can have at most {} role counters. Remove one first.", limits::ROLE_STAT_CHANNELS),
                    )));
                }
                if let Some((dup_channel, _, _)) = rows.iter().find(|(_, kd, ro)| *kd == k && (k != "ROLE_MEMBERS" || *ro == r)) {
                    return Err(anyhow::Error::from(conflict_counter_err(dup_channel)));
                }
                if rows.iter().any(|(ch, _, _)| *ch == c) {
                    return Err(anyhow::Error::from(channel_taken_err()));
                }
                let q = InsertBuilder::new(&STAT_CHANNEL_INFO)
                    .values([
                        ("guildId", json!(g)),
                        ("channelId", json!(c)),
                        ("kind", json!(k)),
                        ("roleId", json!(r)),
                        ("template", json!(t)),
                        ("lastValue", Value::Null),
                        ("lastRenamedAt", Value::Null),
                        ("peakValue", Value::Null),
                    ])?
                    .build()?;
                Ok::<StatChannel, anyhow::Error>(q.fetch_one(&mut *tx).await?)
            })
        })
        .await;
        let row = match result {
            Ok(r) => r,
            Err(err) => {
                if let Some(e) = err.downcast_ref::<RecordError>() {
                    return Err(e.clone().into());
                }
                if is_unique(&err) {
                    return Err(channel_taken());
                }
                return Err(err.into());
            }
        };
        // The first value arrives with the next refresh of the guild's recurring job.
        stats::ensure_refresh_job(&self.app, guild_id, Some(now_ms(&self.app) + stats::MIN_NEXT_RUN_MS)).await;
        log_info(format!("Dashboard stat channel {} ({kind}) created in {guild_id} by {}", row.id, ctx.user_id));
        Ok(Saved::item(to_item(&row)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let row = self.find(guild_id, id).await?.ok_or_else(gone)?;
        let Some(wanted) = data.get("template") else { return Ok(Saved::item(to_item(&row))) };
        let kind = kind_str(&row);
        // Empty = the default name of the counter type.
        let wanted = if wanted.is_null() { kind_info(&kind).map(|(_, t)| t.to_string()).unwrap_or_default() } else { js_string(wanted) };
        let facts = guild_facts(ctx, guild_id).await;
        let template = clean_template(&wanted, &kind, row.role_id.as_deref(), facts.as_deref(), guild_id)?;
        if normalize_stat_template(Some(&json!(template))) != row.template
            && let Err(err) = stats::edit_template(&self.app, guild_id, &row, &template).await
        {
            if err.downcast_ref::<stats::StatsError>().is_some_and(|e| e.code == "NOT_FOUND") {
                return Err(gone().into());
            }
            return Err(err.into());
        }
        let mut item = to_item(&row);
        item["template"] = json!(template);
        Ok(Saved::item(item))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        let row = self.find(guild_id, id).await?.ok_or_else(gone)?;
        stats::delete_stat_channel(&self.app, guild_id, &row, true, "").await.oi()?;
        log_info(format!("Dashboard stat channel {id} removed in {guild_id} by {}; the Discord channel was kept", ctx.user_id));
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["refresh"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> R<ActionAnswer> {
        if action_id != "refresh" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        // /stats refresh: pulls the guild's recurring job forward; the bot's shard does the renaming.
        let row = self.find(guild_id, id).await?.ok_or_else(gone)?;
        let settings = self.app.settings().get_settings(guild_id).await.oi()?;
        if !bot_core::settings::is_module_enabled_by_name(Some(&settings), Some("stats")) {
            return Err(RecordError::invalid(
                "The Statistics Channels module is turned off, so counters are not updated. Turn it on first.",
            )
            .into());
        }
        if !stats::pull_refresh(&self.app, guild_id, stats::EDIT_PULL_DELAY_MS).await {
            return Err(RecordError::unavailable("Could not schedule the update. Try again in a moment.").into());
        }
        Ok(ActionAnswer::new(to_item(&row), "The bot will update the counters within about a minute."))
    }
}
