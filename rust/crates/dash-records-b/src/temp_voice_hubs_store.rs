//! Port of `src/dashboard/collections/tempVoiceHubs.store.js` (the TempVoiceHub table). Create and edit go through
//! feat-tempvoice (create_hub under a lock on the GuildSettings row, update_hub), with the same checks as /voice-hub:
//! the name format, the hub being an existing voice channel that is neither a hub nor a temporary channel, and the
//! per-server cap. Deleting a hub is refused while temporary channels from it are open.

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{SelectBuilder, TEMP_VOICE_HUB_INFO, TempVoiceHub};
use bot_util::placeholder_parser::{PlaceholderGuild, PlaceholderMember, PlaceholderUser};
use dash_collections::collections::small::DEFAULT_NAME_FORMAT;
use dash_collections::{ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError};
use feat_tempvoice::temp_voice_service as tv;
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R, log_info, rec_at, truthy};

const SAMPLE_USER_ID: &str = "100000000000000001";

/// `toItem(row)` with `row._count.channels`.
pub fn to_item(row: &TempVoiceHub, channels: i64) -> Value {
    let bitrate = row.bitrate.filter(|b| *b != 0).map(|b| ((f64::from(b) / 1000.0 + 0.5).floor()).clamp(8.0, 384.0) as i64);
    json!({
        "id": row.id,
        "channelId": row.channel_id,
        "categoryId": row.category_id,
        "nameFormat": row.name_format,
        "userLimit": row.user_limit,
        "bitrate": bitrate,
        "keepAliveMinutes": row.keep_alive_minutes,
        "ownershipLockMinutes": row.ownership_lock_minutes,
        "syncPermissions": row.sync_permissions,
        "createTextChannel": row.create_text_channel,
        "textChannelMembersOnly": row.text_channel_members_only,
        "ignoredRoleIds": row.ignored_role_ids,
        "moderatorRoleIds": row.moderator_role_ids,
        "activeChannels": channels,
    })
}

/// `String(x)`.
fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => dash_schema::js::to_js_string(Some(other)),
    }
}

/// `cleanNameFormat(format, guildName)`: the service's check of a name format, rendered with a sample member so
/// `{user}` shows up as the mention it would be.
fn clean_name_format(format: Option<&Value>, guild_name: Option<&str>) -> R<String> {
    let text = format.filter(|v| !v.is_null()).map_or_else(|| DEFAULT_NAME_FORMAT.to_string(), js_string);
    let ctx = tv::NameContext {
        member: Some(PlaceholderMember {
            display_name: Some("Sample".into()),
            user: Some(PlaceholderUser { id: SAMPLE_USER_ID.into(), username: Some("sample".into()), bot: false, ..Default::default() }),
            ..Default::default()
        }),
        guild: Some(PlaceholderGuild { id: "0".into(), name: Some(guild_name.unwrap_or("Server").to_string()), ..Default::default() }),
    };
    if let tv::NameFormatCheck::Err(error) = tv::validate_name_format(Some(&text), &ctx) {
        return Err(rec_at(RecordErrorCode::Invalid, error, "nameFormat"));
    }
    Ok(bot_util::js::trim(&text).to_string())
}

async fn guild_name_of(ctx: &StoreCtx, guild_id: &str) -> Option<String> {
    let discord = crate::common::discord_with(ctx, dash_collections::DiscordMethod::GetGuildResources)?;
    discord.get_guild_resources(guild_id, false).await.ok().map(|r| r.guild.name.clone())
}

fn already_hub() -> StoreError {
    rec_at(RecordErrorCode::Conflict, "That channel is already a hub. Edit it instead.", "channelId")
}

fn hub_limit() -> StoreError {
    RecordError::new(RecordErrorCode::Limit, format!("A server can have at most {} hubs. Delete one first.", limits::TEMPVOICE_HUBS)).into()
}

/// `data.bitrate * 1000` (kbps in the form, bps in the row).
fn times_1000(v: &Value) -> Value {
    if let Some(n) = v.as_i64() {
        return json!(n * 1000);
    }
    let x = v.as_f64().unwrap_or(f64::NAN) * 1000.0;
    if x.fract() == 0.0 && x.is_finite() { json!(x as i64) } else { json!(x) }
}

/// The `tempVoiceHubs` store.
pub struct TempVoiceHubsStore {
    app: App,
}

impl TempVoiceHubsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<TempVoiceHub>> {
        SelectBuilder::new(&TEMP_VOICE_HUB_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<TempVoiceHub, _>(self.app.db())
            .await
            .oi()
    }

    /// `_count: { select: { channels: true } }`.
    async fn channel_count(&self, hub_id: &str) -> R<i64> {
        sqlx::query_scalar::<_, i64>(r#"SELECT COUNT(*)::bigint FROM "TempVoiceChannel" WHERE "hubId" = $1"#)
            .bind(hub_id)
            .fetch_one(self.app.db())
            .await
            .oi()
    }

    async fn get_item(&self, guild_id: &str, id: &str) -> R<Option<Value>> {
        let Some(row) = self.find(guild_id, id).await? else { return Ok(None) };
        let n = self.channel_count(&row.id).await?;
        Ok(Some(to_item(&row, n)))
    }
}

#[async_trait]
impl RecordStore for TempVoiceHubsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let hubs = tv::list_hubs(self.app.db(), guild_id).await.oi()?;
        Ok(ListAnswer::items(hubs.iter().take(limits::TEMPVOICE_HUBS).map(|h| to_item(&h.hub, h.channels)).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        self.get_item(guild_id, id).await
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let name_format = clean_name_format(data.get("nameFormat"), guild_name_of(ctx, guild_id).await.as_deref())?;
        if tv::count_hubs(self.app.db(), guild_id).await.oi()? >= limits::TEMPVOICE_HUBS as i64 {
            return Err(hub_limit());
        }
        let channel_id = data.get("channelId").map(js_string).unwrap_or_default();
        if tv::find_hub_by_channel(self.app.db(), guild_id, &channel_id).await.oi()?.is_some() {
            return Err(already_hub());
        }
        if tv::is_temp_channel(self.app.db(), &channel_id).await.oi()? {
            return Err(rec_at(RecordErrorCode::Invalid, "That is a temporary channel and cannot be a hub.", "channelId"));
        }
        let mut hub: tv::HubData = vec![("channelId", json!(channel_id)), ("nameFormat", json!(name_format))];
        for key in [
            "userLimit", "keepAliveMinutes", "ownershipLockMinutes", "syncPermissions", "createTextChannel", "textChannelMembersOnly",
            "ignoredRoleIds", "moderatorRoleIds",
        ] {
            if let Some(v) = data.get(key) {
                hub.push((key, v.clone()));
            }
        }
        if truthy(data.get("categoryId")) {
            hub.push(("categoryId", data["categoryId"].clone()));
        }
        if truthy(data.get("bitrate")) {
            hub.push(("bitrate", times_1000(&data["bitrate"])));
        }
        match tv::create_hub(&self.app, guild_id, hub).await {
            Ok(row) => {
                log_info(format!("Dashboard voice hub {} ({}) created in {guild_id} by {}", row.id, row.channel_id, ctx.user_id));
                Ok(Saved::item(to_item(&row, 0)))
            }
            Err(err) if err.downcast_ref::<tv::HubLimitError>().is_some() => Err(hub_limit()),
            Err(err) if tv::is_duplicate_hub(&err) => Err(already_hub()),
            Err(err) => Err(err.into()),
        }
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let mut patch: tv::HubData = Vec::new();
        if let Some(v) = data.get("categoryId") {
            patch.push(("categoryId", v.clone()));
        }
        if data.contains_key("nameFormat") {
            let name = clean_name_format(data.get("nameFormat"), guild_name_of(ctx, guild_id).await.as_deref())?;
            patch.push(("nameFormat", json!(name)));
        }
        if let Some(v) = data.get("bitrate") {
            patch.push(("bitrate", if v.is_null() { Value::Null } else { times_1000(v) }));
        }
        for key in [
            "userLimit", "keepAliveMinutes", "ownershipLockMinutes", "syncPermissions", "createTextChannel", "textChannelMembersOnly",
            "ignoredRoleIds", "moderatorRoleIds",
        ] {
            if let Some(v) = data.get(key) {
                patch.push((key, v.clone()));
            }
        }
        let exists = if patch.is_empty() {
            self.find(guild_id, id).await?.is_some()
        } else {
            tv::update_hub(&self.app, guild_id, id, patch).await.oi()?.is_some()
        };
        if !exists {
            return Err(RecordError::not_found("That hub no longer exists.").into());
        }
        Ok(Saved::item(self.get_item(guild_id, id).await?.unwrap_or(Value::Null)))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        let Some(hub) = self.find(guild_id, id).await? else {
            return Err(RecordError::not_found("That hub no longer exists.").into());
        };
        let open = self.channel_count(&hub.id).await?;
        if open > 0 {
            let what = if open == 1 { " is" } else { "s are" };
            return Err(RecordError::invalid(format!(
                "{open} temporary channel{what} still open from this hub. Deleting the hub now would leave them behind in Discord. \
                 Wait until they close, or use /voice-hub delete in Discord, which removes them too."
            ))
            .into());
        }
        bot_db::delete_where(&TEMP_VOICE_HUB_INFO, [("id", json!(id)), ("guildId", json!(guild_id))])
            .oi()?
            .execute(self.app.db())
            .await
            .oi()?;
        log_info(format!("Dashboard voice hub {id} deleted in {guild_id} by {}; the Discord channel was kept", ctx.user_id));
        Ok(())
    }
}
