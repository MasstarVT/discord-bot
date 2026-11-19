//! Port of `src/dashboard/collections/starboards.store.js` (the Starboard table). Create, edit and delete go through
//! feat-starboard-stats (create_board, update_board, delete_board: cap and row locks inside, the board cache in Redis
//! invalidated after every write), exactly like /starboard.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{InsertBuilder, STARBOARD_INFO, SelectBuilder, Starboard, TxOptions};
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError};
use feat_starboard_stats::starboard_service as sb;
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R, log_info, now_ms, rec_at, recheck};

const SELF_STAR_CONFLICT: &str = "Self-stars are counted on this board, so they cannot also be removed.";

/// `toItem(row, reposts)`.
pub fn to_item(row: &Starboard, reposts: i64) -> Value {
    json!({
        "id": row.id,
        "name": row.name,
        "channelId": row.channel_id,
        "emojis": row.emojis,
        "threshold": row.threshold,
        "selfStar": row.self_star,
        "removeSelfStars": row.remove_self_stars,
        "ignoreBots": row.ignore_bots,
        "removeBelowThreshold": row.remove_below_threshold,
        "autoReact": row.auto_react,
        "maxMessageAgeDays": row.max_message_age_days,
        "allowNsfw": row.allow_nsfw,
        "ignoredChannelIds": row.ignored_channel_ids,
        "enabled": row.enabled,
        "reposts": reposts,
    })
}

fn name_taken(name: &str) -> StoreError {
    rec_at(RecordErrorCode::Conflict, format!("A starboard named \"{name}\" already exists."), "name")
}

fn js_string(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(other) => dash_schema::js::to_js_string(Some(other)),
        None => "undefined".into(),
    }
}

fn clean_name(name: Option<&Value>) -> R<String> {
    sb::normalize_board_name(&js_string(name))
        .ok_or_else(|| rec_at(RecordErrorCode::Invalid, "Board names may use letters a-z, digits, - and _ (max 32).", "name"))
}

/// Entries are unicode emojis, `<:name:id>` or bare ids; the stored form is the emoji itself or the custom emoji's id.
fn clean_emojis(entries: Option<&Value>) -> R<Vec<String>> {
    let list: Vec<String> = entries.and_then(Value::as_array).map(|a| a.iter().map(|v| js_string(Some(v))).collect()).unwrap_or_default();
    if list.is_empty() {
        return Ok(Vec::new()); // every emoji counts
    }
    sb::parse_emoji_list(&list.join(" "), &[]).map_err(|e| rec_at(RecordErrorCode::Invalid, e, "emojis"))
}

/// Age-restricted sources need an age-restricted repost channel (checked against the live channel list).
async fn assert_nsfw_channel(ctx: &StoreCtx, guild_id: &str, channel_id: &str) -> R<()> {
    let Some(discord) = crate::common::discord_with(ctx, dash_collections::DiscordMethod::GetGuildResources) else { return Ok(()) };
    let resources = discord
        .get_guild_resources(guild_id, false)
        .await
        .map_err(|_| RecordError::discord("Could not check whether the repost channel is age-restricted. Try again in a moment."))?;
    if let Some(channel) = resources.channel(channel_id)
        && !channel.nsfw
    {
        return Err(rec_at(
            RecordErrorCode::Invalid,
            "To repost from age-restricted channels, the repost channel must be age-restricted too. Mark it as age-restricted in Discord first.",
            "allowNsfw",
        ));
    }
    Ok(())
}

/// `mapError(err, name)`: the service's typed errors as the admin sees them.
fn map_error(err: sb::BoardError, name: &str) -> StoreError {
    if err.is_unique_violation() {
        return name_taken(name);
    }
    match err {
        sb::BoardError::Limit => {
            RecordError::new(RecordErrorCode::Limit, format!("A server can have at most {} starboards. Delete one first.", limits::STARBOARDS)).into()
        }
        sb::BoardError::NotFound => RecordError::not_found("That starboard no longer exists.").into(),
        sb::BoardError::IgnoreLimit => rec_at(
            RecordErrorCode::Limit,
            format!("A starboard can ignore at most {} channels.", sb::MAX_IGNORED_CHANNELS),
            "ignoredChannelIds",
        ),
        sb::BoardError::SettingsConflict => rec_at(RecordErrorCode::Invalid, SELF_STAR_CONFLICT, "removeSelfStars"),
        sb::BoardError::Db(e) => StoreError::Internal(e.into()),
        sb::BoardError::Other(e) => StoreError::Internal(e),
    }
}

fn bool_of(v: &Value) -> Option<bool> {
    v.as_bool()
}

fn int_of(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// starboardService.createBoard(guildId, data), with `ignoredChannelIds` (the Rust BoardPatch has no field for it):
/// the GuildSettings row lock, the cap, the insert of every set field, then the board cache invalidation.
async fn create_board(app: &App, guild_id: &str, fields: Vec<(&'static str, Value)>) -> Result<Starboard, sb::BoardError> {
    app.settings().ensure_guild_settings(guild_id).await.map_err(|e| sb::BoardError::Other(anyhow::anyhow!("{e}")))?;
    let gid = guild_id.to_string();
    let row = bot_db::transaction(app.db(), TxOptions::TEN_SECONDS, move |tx| {
        Box::pin(async move {
            sqlx::query(sb::LOCK_GUILD_SETTINGS_SQL).bind(gid.clone()).execute(&mut *tx).await?;
            let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "Starboard" WHERE "guildId" = $1"#).bind(gid.clone()).fetch_one(&mut *tx).await?;
            if n >= limits::STARBOARDS as i64 {
                return Err(sb::BoardError::Limit);
            }
            let mut ins = InsertBuilder::new(&STARBOARD_INFO).value("guildId", json!(gid))?;
            for (k, v) in fields {
                ins = ins.value(k, v)?;
            }
            let row: Starboard = ins.build()?.fetch_one(&mut *tx).await?;
            Ok(row)
        })
    })
    .await?;
    sb::invalidate_boards(app, guild_id).await;
    Ok(row)
}

/// The `starboards` store.
pub struct StarboardsStore {
    app: App,
}

impl StarboardsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<Starboard>> {
        SelectBuilder::new(&STARBOARD_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<Starboard, _>(self.app.db())
            .await
            .oi()
    }

    async fn get_item(&self, guild_id: &str, id: &str) -> R<Option<Value>> {
        let Some(row) = self.find(guild_id, id).await? else { return Ok(None) };
        let reposts: i64 =
            sqlx::query_scalar(r#"SELECT COUNT(*)::bigint FROM "StarboardEntry" WHERE "starboardId" = $1 AND "boardMessageId" IS NOT NULL"#)
                .bind(id)
                .fetch_one(self.app.db())
                .await
                .oi()?;
        Ok(Some(to_item(&row, reposts)))
    }
}

/// The create fields in the order the JS object lists them (undefined ones left out).
const CREATE_KEYS: [&str; 10] = [
    "threshold", "selfStar", "removeSelfStars", "ignoreBots", "removeBelowThreshold", "autoReact", "maxMessageAgeDays", "allowNsfw",
    "ignoredChannelIds", "enabled",
];

#[async_trait]
impl RecordStore for StarboardsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let rows = sb::list_boards_with_counts(&self.app, guild_id).await.oi()?;
        Ok(ListAnswer::items(rows.iter().map(|r| to_item(&r.board, r.reposts)).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        self.get_item(guild_id, id).await
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let name = clean_name(data.get("name"))?;
        let emojis = clean_emojis(data.get("emojis"))?;
        let channel_id = js_string(data.get("channelId"));
        if crate::common::truthy(data.get("allowNsfw")) {
            assert_nsfw_channel(ctx, guild_id, &channel_id).await?;
        }
        let mut fields: Vec<(&'static str, Value)> = vec![("name", json!(name))];
        if let Some(c) = data.get("channelId") {
            fields.push(("channelId", c.clone()));
        }
        fields.push(("emojis", json!(emojis)));
        for key in CREATE_KEYS {
            if let Some(v) = data.get(key) {
                fields.push((key, v.clone()));
            }
        }
        match create_board(&self.app, guild_id, fields).await {
            Ok(row) => {
                log_info(format!("Dashboard starboard {} created in {guild_id} by {}", row.id, ctx.user_id));
                Ok(Saved::item(to_item(&row, 0)))
            }
            Err(err) => Err(map_error(err, &name)),
        }
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let current = self.find(guild_id, id).await?.ok_or_else(|| RecordError::not_found("That starboard no longer exists."))?;
        // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
        recheck("starboards", &data, &to_item(&current, 0), now_ms(&self.app))?;

        let mut patch = sb::BoardPatch::default();
        if let Some(v) = data.get("channelId") {
            patch.channel_id = Some(js_string(Some(v)));
        }
        if let Some(v) = data.get("threshold") {
            patch.threshold = int_of(v);
        }
        macro_rules! flag {
            ($key:literal, $field:ident) => {
                if let Some(v) = data.get($key) {
                    patch.$field = bool_of(v);
                }
            };
        }
        flag!("selfStar", self_star);
        flag!("removeSelfStars", remove_self_stars);
        flag!("ignoreBots", ignore_bots);
        flag!("removeBelowThreshold", remove_below_threshold);
        flag!("autoReact", auto_react);
        flag!("allowNsfw", allow_nsfw);
        flag!("enabled", enabled);
        if let Some(v) = data.get("maxMessageAgeDays") {
            patch.max_message_age_days = int_of(v);
        }
        if data.contains_key("name") {
            patch.name = Some(clean_name(data.get("name"))?);
        }
        if data.contains_key("emojis") {
            patch.emojis = Some(clean_emojis(data.get("emojis"))?);
        }

        let allow_nsfw = patch.allow_nsfw.unwrap_or(current.allow_nsfw);
        if allow_nsfw && (patch.allow_nsfw.is_some() || patch.channel_id.is_some()) {
            let channel = patch.channel_id.clone().unwrap_or_else(|| current.channel_id.clone());
            assert_nsfw_channel(ctx, guild_id, &channel).await?;
        }

        // The service edits the ignore list one channel at a time: apply the difference pairwise, the first pair with the rest.
        let wanted: Vec<String> = match data.get("ignoredChannelIds").filter(|v| !v.is_null()) {
            Some(v) => v.as_array().map(|a| a.iter().map(|x| js_string(Some(x))).collect()).unwrap_or_default(),
            None => current.ignored_channel_ids.clone(),
        };
        let mut adds: std::collections::VecDeque<String> = wanted.iter().filter(|c| !current.ignored_channel_ids.contains(c)).cloned().collect();
        let mut removes: std::collections::VecDeque<String> =
            current.ignored_channel_ids.iter().filter(|c| !wanted.contains(c)).cloned().collect();
        let err_name = patch.name.clone().unwrap_or_else(|| current.name.clone());
        let mut step = patch;
        step.add_ignored = adds.pop_front();
        step.remove_ignored = removes.pop_front();
        loop {
            let has_fields = !step.fields().is_empty() || step.add_ignored.is_some() || step.remove_ignored.is_some();
            if has_fields {
                sb::update_board(&self.app, guild_id, id, step).await.map_err(|e| map_error(e, &err_name))?;
            }
            if adds.is_empty() && removes.is_empty() {
                break;
            }
            step = sb::BoardPatch { add_ignored: adds.pop_front(), remove_ignored: removes.pop_front(), ..Default::default() };
        }
        log_info(format!("Dashboard starboard {id} updated in {guild_id} by {}", ctx.user_id));
        Ok(Saved::item(self.get_item(guild_id, id).await?.unwrap_or(Value::Null)))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        if sb::delete_board(&self.app, guild_id, id).await.oi()? == 0 {
            return Err(RecordError::not_found("That starboard no longer exists.").into());
        }
        log_info(format!("Dashboard starboard {id} deleted in {guild_id} by {}", ctx.user_id));
        Ok(())
    }
}
