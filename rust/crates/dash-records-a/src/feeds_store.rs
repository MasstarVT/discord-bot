//! Port of `src/dashboard/collections/feeds.store.js`: the `feeds` collection, the Feed rows /feed writes. Adding
//! resolves and probes the source with the same resolveFeedSource (SSRF-safe fetches; the probe seeds the feed so no
//! backlog is posted) and schedules `feed.poll` (dedupe `feed.poll:{id}`) like /feed add; enabling and disabling follow
//! /feed enable|disable. The cap is guarded by a per-guild advisory lock (`feed-create:{guildId}`).

use std::sync::Arc;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_core::scheduler::CancelJobs;
use bot_db::{FEED_INFO, Feed, FeedType, InsertBuilder};
use dash_collections::collections::embeds::{member_for, ping_role_problem};
use dash_collections::collections::feeds::{FeedInput, feed_item, parse_feed_source};
use dash_collections::{
    ActionAnswer, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError,
};
use feat_feeds::feed_service::{
    Probe, SEEN_CAP, SourceInput, SourceResult, base_interval_ms, describe_fetch_error, feed_job_key, feed_status, fetch_feed_source,
    parse_message_option, resolve_feed_source, schedule_feed_job, source_link,
};
use serde_json::{Map, Value, json};

use crate::common::{TxError, advisory_lock, internal, row_json, truthy};

/// The `feeds` store.
pub struct FeedsStore {
    app: App,
    /// Like /feed test: one check per feed every 15 s (per dashboard process).
    test_throttle: bot_util::async_util::Throttle,
}

impl FeedsStore {
    pub fn new(app: App) -> Self {
        let test_throttle = bot_core::clock::clock_throttle(app.clock(), 15_000);
        Self { app, test_throttle }
    }
}

fn not_found() -> StoreError {
    RecordError::not_found("That feed no longer exists.").into()
}

fn limit_error() -> RecordError {
    RecordError::new(RecordErrorCode::Limit, format!("This server already follows {} feeds, the maximum. Remove one first.", limits::FEEDS))
}

/// `toItem(row)`.
pub fn feed_row_item(row: &Feed) -> Value {
    let (icon, text) = feed_status(row);
    let link = source_link(row.r#type, &row.source, &row.fetch_url);
    let url = link.strip_prefix('<').unwrap_or(&link);
    let url = url.strip_suffix('>').unwrap_or(url);
    feed_item(&row_json(row), &format!("{icon} {text}"), Some(url))
}

async fn find_row(app: &App, guild_id: &str, id: &str) -> Result<Feed, StoreError> {
    sqlx::query_as::<_, Feed>(r#"SELECT * FROM "Feed" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)?
        .ok_or_else(not_found)
}

async fn load_resources(ctx: &StoreCtx, guild_id: &str) -> Result<Arc<DiscordResources>, StoreError> {
    let Some(discord) = crate::common::discord_with(ctx.discord.as_ref(), dash_collections::DiscordMethod::GetGuildResources) else {
        return Err(RecordError::unavailable("The server cannot be read from the dashboard right now.").into());
    };
    discord
        .get_guild_resources(guild_id, false)
        .await
        .map_err(|_| RecordError::discord("The server could not be loaded from Discord. Try again in a moment.").into())
}

async fn check_ping_role(guild_id: &str, role_id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
    let resources = load_resources(ctx, guild_id).await?;
    let member = member_for(ctx, Some(&resources));
    match ping_role_problem(Some(&resources), role_id, member.as_ref()) {
        Some(problem) => Err(RecordError::invalid(problem).with_path("pingRoleId").into()),
        None => Ok(()),
    }
}

/// `messageOf(raw)`: `\n` typed literally becomes a new line, blank means the type's default (null).
fn message_of(raw: Option<&Value>) -> Result<Option<String>, StoreError> {
    let text = match raw {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => Some(dash_schema::js::to_js_string(Some(v))),
    };
    parse_message_option(text.as_deref()).map_err(|e| RecordError::invalid(e).with_path("messageTemplate").into())
}

fn source_input(input: FeedInput) -> SourceInput {
    match input {
        FeedInput::Url(v) => SourceInput::Url(v),
        FeedInput::ChannelId(v) => SourceInput::ChannelId(v),
        FeedInput::Name(v) => SourceInput::Name(v),
        FeedInput::Actor(v) => SourceInput::Actor(v),
        FeedInput::Query(v) => SourceInput::Query(v),
    }
}

async fn count_feeds(app: &App, guild_id: &str) -> Result<i64, StoreError> {
    let (n,): (i64,) =
        sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Feed" WHERE "guildId" = $1"#).bind(guild_id).fetch_one(app.db()).await.map_err(internal)?;
    Ok(n)
}

#[async_trait]
impl RecordStore for FeedsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let rows = sqlx::query_as::<_, Feed>(r#"SELECT * FROM "Feed" WHERE "guildId" = $1 ORDER BY "createdAt" ASC LIMIT $2"#)
            .bind(guild_id)
            .bind(limits::FEEDS as i64)
            .fetch_all(self.app.db())
            .await
            .map_err(internal)?;
        Ok(ListAnswer::items(rows.iter().map(feed_row_item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        match find_row(&self.app, guild_id, id).await {
            Ok(row) => Ok(Some(feed_row_item(&row))),
            Err(StoreError::Record(e)) if e.code == RecordErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let app = &self.app;
        let type_text = data.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let source_raw = match data.get("source") {
            None | Some(Value::Null) => String::new(),
            Some(v) => dash_schema::js::to_js_string(Some(v)),
        };
        let input = parse_feed_source(&type_text, &source_raw).map_err(|m| StoreError::from(RecordError::invalid(m).with_path("source")))?;
        let message_template = message_of(data.get("messageTemplate"))?;
        if truthy(data.get("includeReposts")) && type_text != "BLUESKY" {
            return Err(RecordError::invalid("Only Bluesky feeds can include reposts.").with_path("includeReposts").into());
        }
        if let Some(role) = data.get("pingRoleId").filter(|v| truthy(Some(v))) {
            check_ping_role(guild_id, role.as_str().unwrap_or_default(), ctx).await?;
        }
        // Fast cap check before any network request (enforced again under the lock).
        if count_feeds(app, guild_id).await? >= limits::FEEDS as i64 {
            return Err(limit_error().into());
        }
        let Some(ty) = FeedType::parse(&type_text) else { return Err(RecordError::invalid("That kind of feed is not supported.").with_path("source").into()) };
        let r = match resolve_feed_source(app, ty, &source_input(input)).await {
            Ok(r) => r,
            Err(refusal) => {
                let message = refusal.message.replace("**", "").replace('`', "");
                return Err(RecordError::invalid(format!("{}: {message}", refusal.title)).with_path("source").into());
            }
        };
        let channel_id = data.get("channelId").cloned().unwrap_or(Value::Null);
        let dup: Option<(String,)> =
            sqlx::query_as(r#"SELECT "id" FROM "Feed" WHERE "guildId" = $1 AND "type" = $2::"FeedType" AND "source" = $3 AND "channelId" = $4 LIMIT 1"#)
                .bind(guild_id)
                .bind(&type_text)
                .bind(&r.source)
                .bind(channel_id.as_str().unwrap_or_default())
                .fetch_optional(app.db())
                .await
                .map_err(internal)?;
        if dup.is_some() {
            return Err(RecordError::new(RecordErrorCode::Conflict, "This feed is already posted in that channel.").with_path("source").into());
        }
        app.settings().ensure_guild_settings(guild_id).await.map_err(internal)?;
        let now = bot_db::format_js_date(&chrono::DateTime::from_timestamp_millis(app.clock().now_ms()).unwrap_or_default());
        let enabled = data.get("enabled") != Some(&Value::Bool(false));
        let (seeded, seen, etag, last_modified, last_error) = match &r.probe {
            Probe::Ok { items, etag, last_modified } => {
                let mut ids: Vec<String> = Vec::new();
                for i in items {
                    if !ids.contains(&i.id) {
                        ids.push(i.id.clone());
                    }
                }
                ids.truncate(SEEN_CAP);
                (true, ids, json!(etag), json!(last_modified), Value::Null)
            }
            Probe::Failed(e) => (false, Vec::new(), Value::Null, Value::Null, json!(describe_fetch_error(ty, e))),
        };
        let created_by = if ctx.user_id.is_empty() { "0".to_string() } else { ctx.user_id.clone() };
        let values: Vec<(&str, Value)> = vec![
            ("guildId", json!(guild_id)),
            ("type", json!(type_text)),
            ("source", json!(r.source)),
            ("displayName", json!(bot_util::text::clamp_text(&r.display_name, 100))),
            ("fetchUrl", json!(r.fetch_url)),
            ("channelId", channel_id),
            ("pingRoleId", crate::common::nn(data.get("pingRoleId"))),
            ("messageTemplate", json!(message_template)),
            ("includeReposts", json!(type_text == "BLUESKY" && data.get("includeReposts") == Some(&Value::Bool(true)))),
            ("createdBy", json!(created_by)),
            ("initialized", json!(seeded)),
            ("seenIds", json!(seen)),
            ("etag", etag),
            ("lastModified", last_modified),
            ("lastCheckedAt", json!(now)),
            ("lastSuccessAt", if seeded { json!(now) } else { Value::Null }),
            ("lastError", last_error),
            ("enabled", json!(enabled)),
            ("disabledReason", if enabled { Value::Null } else { json!("Paused by an admin") }),
        ];
        let g = guild_id.to_string();
        let out = bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
            Box::pin(async move {
                advisory_lock(tx, &format!("feed-create:{g}")).await?;
                let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Feed" WHERE "guildId" = $1"#).bind(&g).fetch_one(&mut *tx).await?;
                if n >= limits::FEEDS as i64 {
                    return Err(limit_error().into());
                }
                let row: Feed = InsertBuilder::new(&FEED_INFO).values(values)?.build()?.fetch_one(&mut *tx).await?;
                Ok::<_, TxError>(row)
            })
        })
        .await;
        let row = match out {
            Ok(row) => row,
            Err(e) if e.is_unique() => {
                return Err(RecordError::new(RecordErrorCode::Conflict, "This feed is already posted in that channel.").with_path("source").into());
            }
            Err(e) => return Err(e.into()),
        };
        if enabled {
            // The first check: a full interval after a seeding probe, two minutes when the probe failed (as /feed add).
            let delay = if row.initialized { base_interval_ms(row.r#type) } else { 2 * 60_000 };
            let run_at = chrono::DateTime::from_timestamp_millis(app.clock().now_ms() + delay).unwrap_or_default();
            if let Err(err) = schedule_feed_job(app, &row.guild_id, &row.id, run_at).await {
                // The row exists, so the add is reported as done; the bot's guildReady repair schedules it.
                tracing::error!("Dashboard: could not schedule feed {}: {err:#}", row.id);
            }
        }
        Ok(Saved::item(feed_row_item(&row)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let app = &self.app;
        let row = find_row(app, guild_id, id).await?;
        let cur = row_json(&row);
        let same = |k: &str, v: &Value| dash_schema::js::strict_eq(v, cur.get(k).unwrap_or(&Value::Null));
        let mut patch: Vec<(&'static str, Value)> = Vec::new();
        let mut new_channel: Option<String> = None;
        if let Some(ch) = data.get("channelId")
            && !same("channelId", ch)
        {
            patch.push(("channelId", ch.clone()));
            new_channel = ch.as_str().map(str::to_string);
            if row.failure_count == 0 {
                patch.push(("lastError", Value::Null)); // it was a channel problem
            }
        }
        if let Some(role) = data.get("pingRoleId")
            && !same("pingRoleId", role)
        {
            if truthy(Some(role)) {
                check_ping_role(guild_id, role.as_str().unwrap_or_default(), ctx).await?;
            }
            patch.push(("pingRoleId", role.clone()));
        }
        if data.contains_key("messageTemplate") {
            let message = message_of(data.get("messageTemplate"))?;
            if message != row.message_template {
                patch.push(("messageTemplate", json!(message)));
            }
        }
        if let Some(rep) = data.get("includeReposts")
            && !same("includeReposts", rep)
        {
            if row.r#type != FeedType::Bluesky {
                return Err(RecordError::invalid("Only Bluesky feeds can include reposts.").with_path("includeReposts").into());
            }
            patch.push(("includeReposts", rep.clone()));
        }
        let mut job: Option<bool> = None;
        if let Some(en) = data.get("enabled")
            && !same("enabled", en)
        {
            if truthy(Some(en)) {
                let channel_id = new_channel.clone().unwrap_or_else(|| row.channel_id.clone());
                let resources = load_resources(ctx, guild_id).await?;
                if !resources.channels.iter().any(|c| c.id == channel_id) {
                    return Err(RecordError::invalid("Its channel no longer exists. Pick a new channel first.").with_path("channelId").into());
                }
                // /feed enable: the first check re-seeds, so items published while it was off are skipped.
                patch.retain(|(k, _)| *k != "lastError");
                patch.extend([
                    ("enabled", json!(true)),
                    ("failureCount", json!(0)),
                    ("lastError", Value::Null),
                    ("disabledReason", Value::Null),
                    ("initialized", json!(false)),
                ]);
                job = Some(true);
            } else {
                patch.extend([("enabled", json!(false)), ("disabledReason", json!("Paused by an admin"))]);
                job = Some(false);
            }
        }
        if patch.is_empty() {
            return Ok(Saved::item(feed_row_item(&row)));
        }
        let q = bot_db::UpdateBuilder::new(&FEED_INFO)
            .set_all(patch)
            .and_then(|b| b.filter("id", json!(id)))
            .and_then(|b| b.filter("guildId", json!(guild_id)))
            .map(bot_db::UpdateBuilder::without_returning)
            .and_then(bot_db::UpdateBuilder::build)
            .map_err(internal)?;
        match q.execute(app.db()).await {
            Ok(0) => return Err(not_found()),
            Ok(_) => {}
            Err(e) if e.is_unique_violation() => {
                return Err(RecordError::new(RecordErrorCode::Conflict, "That channel already has this feed.").with_path("channelId").into());
            }
            Err(e) => return Err(internal(e)),
        }
        let updated = find_row(app, guild_id, id).await?;
        match job {
            Some(true) => {
                let run_at = chrono::DateTime::from_timestamp_millis(app.clock().now_ms() + 10_000).unwrap_or_default();
                schedule_feed_job(app, &updated.guild_id, &updated.id, run_at).await.map_err(internal)?;
            }
            Some(false) => {
                // scheduler.js cancelJobs never throws (a database error is logged and counts as 0 cancelled).
                app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(feed_job_key(id)), ..Default::default() }).await;
            }
            None => {}
        }
        Ok(Saved::item(feed_row_item(&updated)))
    }

    async fn remove(&self, guild_id: &str, id: &str, _ctx: &StoreCtx) -> Result<(), StoreError> {
        let n = sqlx::query(r#"DELETE FROM "Feed" WHERE "id" = $1 AND "guildId" = $2"#)
            .bind(id)
            .bind(guild_id)
            .execute(self.app.db())
            .await
            .map_err(internal)?
            .rows_affected();
        if n == 0 {
            return Err(not_found());
        }
        // scheduler.js cancelJobs never throws: the feed is gone, so the remove succeeds even when the job delete fails.
        self.app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(feed_job_key(id)), ..Default::default() }).await;
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["test"]
    }

    /// /feed test: a fresh, unconditional fetch; it may refresh the shared fetch cache and never writes the row.
    async fn action(&self, guild_id: &str, id: &str, action_id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        if action_id != "test" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        let row = find_row(&self.app, guild_id, id).await?;
        if !self.test_throttle.allow(&row.id) {
            return Err(RecordError::invalid("This feed was just checked. Try again in a few seconds.").into());
        }
        let item = feed_row_item(&row);
        let message = match fetch_feed_source(&self.app, &row, false, false).await {
            SourceResult::Err { error, .. } => format!("The check failed: {}. Nothing was changed.", describe_fetch_error(row.r#type, &error)),
            SourceResult::Ok { items: all, .. } => {
                let items: Vec<_> = all.iter().filter(|i| row.include_reposts || !i.is_repost).collect();
                match items.first() {
                    None => "The feed answered but has no items to show yet.".to_string(),
                    Some(newest) => {
                        let t = bot_util::js::trim(&newest.title);
                        let title = bot_util::text::clamp_text(if t.is_empty() { "untitled" } else { t }, 150);
                        let url = match newest.url.as_deref().filter(|u| !u.is_empty()) {
                            Some(u) => format!(" ({})", bot_util::text::clamp_text(u, 300)),
                            None => String::new(),
                        };
                        format!("The feed works: {} item(s). Newest: \"{title}\"{url}.", all.len())
                    }
                }
            }
        };
        Ok(ActionAnswer { item: Some(item), message: Some(message) })
    }
}
