//! Port of `src/dashboard/collections/embeds.store.js`: the `embeds` collection. The same SavedEmbed rows, cache keys
//! and rules as /embed (embedService), with the posted message written through the dashboard's Discord REST client
//! (ctx.discord). The cap is guarded by a per-guild advisory lock (`savedembed:{guildId}`), and a template change edits
//! Discord first and saves after.

use std::sync::Arc;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_core::custom_embed::{RenderOptions, normalize_template, render_template};
use bot_db::{GuildSettings, InsertBuilder, SAVED_EMBED_INFO, SavedEmbed, UpdateBuilder};
use dash_collections::collections::embeds::{
    DiscordOp, FailureInfo, SLUG_MESSAGE, discord_codes, discord_failure, embed_item, member_for, render_context_for, rest_allowed_mentions,
    split_pingable_roles, to_rest_message,
};
use dash_collections::{
    ActionAnswer, ChannelCheck, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore,
    SEND_NEEDS, Saved, StoreCtx, StoreError, VIEW_NEEDS, is_channel_exempt, require_channel_access,
};
use feat_embeds::embed_service::{
    EMBED_CACHE_MISS_TTL_SECONDS, EMBED_CACHE_TTL_SECONDS, embed_cache_key, extract_role_mention_ids, normalize_embed_name,
    resolve_default_color, validate_for_save,
};
use serde_json::{Map, Value, json};

use crate::common::{TxError, advisory_lock, discord_with, internal, row_json};
use dash_collections::DiscordMethod;

/// The `embeds` store.
pub struct EmbedsStore {
    app: App,
}

impl EmbedsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

fn not_found() -> StoreError {
    RecordError::not_found("That saved embed no longer exists.").into()
}

fn item(row: &SavedEmbed) -> Value {
    embed_item(&row_json(row))
}

async fn find_row(app: &App, guild_id: &str, id: &str) -> Result<SavedEmbed, StoreError> {
    sqlx::query_as::<_, SavedEmbed>(r#"SELECT * FROM "SavedEmbed" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)?
        .ok_or_else(not_found)
}

/// `checkTemplate(raw)`: validateForSave on a template (the /embed write rules).
fn check_template(raw: &Value) -> Result<Value, StoreError> {
    let template = normalize_template(raw);
    let errors = validate_for_save(&template);
    if let Some(first) = errors.first() {
        let button = first.strip_prefix("Button ").and_then(|rest| {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<i64>().ok()
        });
        let path = match button {
            Some(n) => format!("template.buttons.{}", n - 1),
            None => "template".into(),
        };
        return Err(RecordError::invalid(first.clone()).with_path(path).into());
    }
    Ok(template)
}

/// `requireDiscord(ctx, method)`: unavailable when there is no client or it lacks `method`.
fn require_discord(ctx: &StoreCtx, method: DiscordMethod) -> Result<&Arc<dyn dash_collections::DashboardDiscord>, StoreError> {
    discord_with(ctx.discord.as_ref(), method)
        .ok_or_else(|| RecordError::unavailable("Posting messages from the dashboard is not available right now.").into())
}

async fn load_resources(ctx: &StoreCtx, guild_id: &str) -> Result<Arc<DiscordResources>, StoreError> {
    let discord = require_discord(ctx, DiscordMethod::GetGuildResources)?;
    discord
        .get_guild_resources(guild_id, false)
        .await
        .map_err(|_| RecordError::discord("The server could not be loaded from Discord. Try again in a moment.").into())
}

fn channel_name<'a>(resources: &'a DiscordResources, id: Option<&str>) -> Option<&'a str> {
    id.and_then(|id| resources.channel(id)).map(|c| c.name.as_str())
}

/// `requireStoredChannelAccess(ctx, row, path)`: View Channel in the channel the copy was posted in.
async fn require_stored_channel_access(ctx: &StoreCtx, row: &SavedEmbed, path: Option<&str>) -> Result<(), StoreError> {
    let Some(ch) = &row.channel_id else { return Ok(()) };
    let mut check = ChannelCheck::new(ch.clone()).needs(&VIEW_NEEDS).optional();
    if let Some(p) = path {
        check = check.path(p);
    }
    require_channel_access(ctx, &[check], "").await
}

/// `requireAutomationChannelAccess(ctx, guildId, name)`: the user needs access to every channel an automation posts
/// this embed in.
async fn require_automation_channel_access(app: &App, ctx: &StoreCtx, guild_id: &str, name: &str) -> Result<(), StoreError> {
    if is_channel_exempt(ctx.member.as_ref()) {
        return Ok(());
    }
    let rows: Vec<(Value,)> = sqlx::query_as(r#"SELECT "actions" FROM "Automation" WHERE "guildId" = $1 LIMIT $2"#)
        .bind(guild_id)
        .bind(limits::AUTOMATIONS as i64)
        .fetch_all(app.db())
        .await
        .map_err(internal)?;
    let mut channels: Vec<String> = Vec::new();
    for (actions,) in rows {
        for a in actions.as_array().into_iter().flatten() {
            let embed = a.get("embedName");
            let named = embed.and_then(Value::as_str).is_some_and(|e| e == name || normalize_embed_name(e).as_deref() == Some(name));
            if a.get("type").and_then(Value::as_str) == Some("SEND_EMBED")
                && let Some(ch) = a.get("channelId").filter(|c| crate::common::truthy(Some(c))).and_then(Value::as_str)
                && named
                && !channels.iter().any(|c| c == ch)
            {
                channels.push(ch.to_string());
            }
        }
    }
    let checks: Vec<ChannelCheck> = channels.into_iter().map(|c| ChannelCheck::new(c).needs(&SEND_NEEDS).path("template").optional()).collect();
    require_channel_access(ctx, &checks, "An automation posts this embed there, so editing it needs access to that channel. ").await
}

/// How a template is rendered for Discord (`mode` of renderForDiscord).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Publish,
    Edit,
    Plain,
}

/// `renderForDiscord(template, resources, { channelId, ctx, settings, mode })`: a REST message body and the role
/// mentions that will not ping. 'publish' lets `<@&id>` mentions the admin typed ping when the role is mentionable or
/// the dashboard user may mention every role; any other mode pings nobody.
pub fn render_for_discord(
    template: &Value,
    resources: &DiscordResources,
    channel_id: &str,
    ctx: &StoreCtx,
    settings: Option<&GuildSettings>,
    mode: RenderMode,
) -> Result<(Value, Vec<String>), StoreError> {
    let user_id = (!ctx.user_id.is_empty()).then_some(ctx.user_id.as_str());
    let render_ctx = render_context_for(Some(resources), Some(channel_id), ctx.user.as_ref(), user_id);
    let opts = RenderOptions { default_color: Some(resolve_default_color(settings)), ..Default::default() };
    let Some(rendered) = render_template(template, &render_ctx, opts) else {
        return Err(RecordError::invalid("This embed renders empty. Add a title, description, field or text.").with_path("template").into());
    };
    let rendered = rendered.to_json();
    if mode != RenderMode::Publish {
        return Ok((to_rest_message(Some(&rendered), mode == RenderMode::Edit, Some(rest_allowed_mentions(false, &[]))), Vec::new()));
    }
    // The role ids come from the text the admin wrote, not from the rendered text.
    let authored = extract_role_mention_ids(normalize_template(template).get("content").and_then(Value::as_str));
    let member = member_for(ctx, Some(resources));
    let (allowed, refused) = split_pingable_roles(&authored, Some(resources), member.as_ref());
    Ok((to_rest_message(Some(&rendered), false, Some(rest_allowed_mentions(true, &allowed))), refused))
}

/// What happened to the posted copy (`editPosted`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edited {
    Done,
    MessageGone,
    ChannelGone,
}

fn gone_patch(outcome: Edited) -> Vec<(&'static str, Value)> {
    match outcome {
        Edited::ChannelGone => vec![("messageId", Value::Null), ("channelId", Value::Null)],
        _ => vec![("messageId", Value::Null)],
    }
}

fn gone_text(outcome: Edited) -> &'static str {
    match outcome {
        Edited::ChannelGone => {
            "The channel it was posted in was deleted, so this embed is a draft again. Post it again with \"Post in a channel\"."
        }
        _ => "The posted message was deleted in Discord, so this embed is a draft again. Post it again with \"Post in a channel\".",
    }
}

/// `editPosted(guildId, row, template, ctx)`: edits the posted copy; a deleted message or channel makes it a draft.
async fn edit_posted(app: &App, guild_id: &str, row: &SavedEmbed, template: &Value, ctx: &StoreCtx) -> Result<Edited, StoreError> {
    let discord = require_discord(ctx, DiscordMethod::EditMessage)?.clone();
    let res = load_resources(ctx, guild_id).await?;
    let settings = app.settings().get_settings_safe(guild_id).await;
    let channel_id = row.channel_id.clone().unwrap_or_default();
    let message_id = row.message_id.clone().unwrap_or_default();
    let (body, _) = render_for_discord(template, &res, &channel_id, ctx, settings.as_deref(), RenderMode::Edit)?;
    match discord.edit_message(&channel_id, &message_id, body).await {
        Ok(_) => Ok(Edited::Done),
        Err(err) => {
            if err.discord_code == Some(discord_codes::UNKNOWN_CHANNEL) {
                return Ok(Edited::ChannelGone);
            }
            if err.discord_code == Some(discord_codes::UNKNOWN_MESSAGE) || err.discord_code == Some(discord_codes::CANNOT_EDIT_OTHERS) {
                return Ok(Edited::MessageGone);
            }
            if err.code == "not_found" && err.discord_code.is_none() {
                return Ok(Edited::MessageGone);
            }
            let info = FailureInfo { op: DiscordOp::Edit, channel_name: channel_name(&res, Some(&channel_id)), channel_id: Some(&channel_id), embeds: true };
            Err(discord_failure(StoreError::Discord(err), &info))
        }
    }
}

/// `updateMany({ where: { id, guildId, [messageId] }, data })`: the number of rows changed.
async fn update_many(app: &App, guild_id: &str, id: &str, message_id: Option<&str>, data: Vec<(&'static str, Value)>) -> Result<u64, StoreError> {
    let mut b = UpdateBuilder::new(&SAVED_EMBED_INFO).set_all(data).and_then(|b| b.filter("id", json!(id))).and_then(|b| b.filter("guildId", json!(guild_id)));
    if let Some(m) = message_id {
        b = b.and_then(|b| b.filter("messageId", json!(m)));
    }
    let q = b.map(UpdateBuilder::without_returning).and_then(UpdateBuilder::build).map_err(internal)?;
    q.execute(app.db()).await.map_err(internal)
}

#[async_trait]
impl RecordStore for EmbedsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let rows = sqlx::query_as::<_, SavedEmbed>(r#"SELECT * FROM "SavedEmbed" WHERE "guildId" = $1 ORDER BY "name" ASC LIMIT $2"#)
            .bind(guild_id)
            .bind(limits::SAVED_EMBEDS as i64)
            .fetch_all(self.app.db())
            .await
            .map_err(internal)?;
        Ok(ListAnswer::items(rows.iter().map(item).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        match find_row(&self.app, guild_id, id).await {
            Ok(row) => Ok(Some(item(&row))),
            Err(StoreError::Record(e)) if e.code == RecordErrorCode::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let name = data.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        if normalize_embed_name(&name).as_deref() != Some(name.as_str()) {
            return Err(RecordError::invalid(SLUG_MESSAGE).with_path("name").into());
        }
        let template = check_template(data.get("template").unwrap_or(&Value::Null))?;
        self.app.settings().ensure_guild_settings(guild_id).await.map_err(internal)?;
        let created_by = if ctx.user_id.is_empty() { "0".to_string() } else { ctx.user_id.clone() };
        let (g, n, tpl) = (guild_id.to_string(), name.clone(), template.clone());
        let out = bot_db::transaction(self.app.db(), bot_db::TxOptions::default(), move |tx| {
            Box::pin(async move {
                advisory_lock(tx, &format!("savedembed:{g}")).await?;
                let (count,): (i64,) =
                    sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "SavedEmbed" WHERE "guildId" = $1"#).bind(&g).fetch_one(&mut *tx).await?;
                if count >= limits::SAVED_EMBEDS as i64 {
                    return Err(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("This server already has {} saved embeds, the maximum. Delete one first.", limits::SAVED_EMBEDS),
                    )
                    .into());
                }
                let row: SavedEmbed = InsertBuilder::new(&SAVED_EMBED_INFO)
                    .values([("guildId", json!(g)), ("name", json!(n)), ("template", tpl), ("createdBy", json!(created_by))])?
                    .build()?
                    .fetch_one(&mut *tx)
                    .await?;
                Ok::<_, TxError>(row)
            })
        })
        .await;
        let row = match out {
            Ok(row) => row,
            Err(e) if e.is_unique() => {
                return Err(RecordError::new(RecordErrorCode::Conflict, format!("An embed called \"{name}\" already exists.")).with_path("name").into());
            }
            Err(e) => return Err(e.into()),
        };
        // Write-through, like createSavedEmbed: Automations read the template through this key.
        self.app.cache().set(&embed_cache_key(guild_id, &name), &template, EMBED_CACHE_TTL_SECONDS).await;
        Ok(Saved::item(item(&row)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let app = &self.app;
        let row = find_row(app, guild_id, id).await?;
        let Some(raw) = data.get("template") else { return Ok(Saved::item(item(&row))) };
        let template = check_template(raw)?;
        let mut patch: Vec<(&'static str, Value)> = vec![("template", template.clone())];
        require_automation_channel_access(app, ctx, guild_id, &row.name).await?;
        if row.message_id.is_some() && row.channel_id.is_some() {
            require_stored_channel_access(ctx, &row, Some("template")).await?;
            // Discord first: a refused edit (missing permission, outage) saves nothing and says why.
            let outcome = edit_posted(app, guild_id, &row, &template, ctx).await?;
            if outcome != Edited::Done {
                patch.extend(gone_patch(outcome));
            }
        }
        if update_many(app, guild_id, id, None, patch).await? == 0 {
            return Err(not_found());
        }
        app.cache().set(&embed_cache_key(guild_id, &row.name), &template, EMBED_CACHE_TTL_SECONDS).await;
        Ok(Saved::item(item(&find_row(app, guild_id, id).await?)))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<(), StoreError> {
        let app = &self.app;
        let row = find_row(app, guild_id, id).await?;
        // As /embed delete: a saved embed that points at a channel needs access to it, even when only kept as a hint.
        require_stored_channel_access(ctx, &row, None).await?;
        let n = sqlx::query(r#"DELETE FROM "SavedEmbed" WHERE "id" = $1 AND "guildId" = $2"#)
            .bind(id)
            .bind(guild_id)
            .execute(app.db())
            .await
            .map_err(internal)?
            .rows_affected();
        if n == 0 {
            return Err(not_found());
        }
        // A short negative entry, like deleteSavedEmbedAndMessage. The posted message stays (no longer tracked).
        app.cache().set(&embed_cache_key(guild_id, &row.name), &json!({ "__none": true }), EMBED_CACHE_MISS_TTL_SECONDS).await;
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["publish", "refresh", "unpublish"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        match action_id {
            "publish" => self.publish(guild_id, id, &data, ctx).await,
            "refresh" => self.refresh(guild_id, id, ctx).await,
            "unpublish" => self.unpublish(guild_id, id, ctx).await,
            _ => Err(RecordError::not_found("Unknown action.").into()),
        }
    }
}

impl EmbedsStore {
    /// Posts the embed in a channel (or moves it there): `{ channelId, deletePrevious }`.
    async fn publish(&self, guild_id: &str, id: &str, data: &Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        let app = &self.app;
        let discord = require_discord(ctx, DiscordMethod::SendMessage)?.clone();
        let row = find_row(app, guild_id, id).await?;
        let resources = load_resources(ctx, guild_id).await?;
        let target = data.get("channelId").and_then(Value::as_str).and_then(|c| resources.channel(c)).cloned();
        let Some(channel) = target else {
            return Err(RecordError::invalid("That channel does not exist in this server any more. Pick another one.").with_path("channelId").into());
        };
        let delete_previous = data.get("deletePrevious") != Some(&Value::Bool(false));
        // As /embed publish: View + Send in the target, and View where the previous copy is deleted.
        let previous = match (&row.channel_id, &row.message_id) {
            (Some(c), Some(m)) => Some((c.clone(), m.clone())),
            _ => None,
        };
        let mut checks = vec![ChannelCheck::new(channel.id.clone()).needs(&SEND_NEEDS).path("channelId")];
        if let Some((pc, _)) = &previous
            && delete_previous
            && *pc != channel.id
        {
            checks.push(ChannelCheck::new(pc.clone()).needs(&VIEW_NEEDS).path("deletePrevious").optional());
        }
        require_channel_access(ctx, &checks, "").await?;
        let settings = app.settings().get_settings_safe(guild_id).await;
        let (body, refused) = render_for_discord(&row.template, &resources, &channel.id, ctx, settings.as_deref(), RenderMode::Publish)?;
        let has_embeds = body.get("embeds").and_then(Value::as_array).is_some_and(|e| !e.is_empty());
        let sent = match discord.send_message(&channel.id, body).await {
            Ok(s) => s,
            Err(err) => {
                let info = FailureInfo { op: DiscordOp::Send, channel_name: Some(&channel.name), channel_id: Some(&channel.id), embeds: has_embeds };
                return Err(discord_failure(StoreError::Discord(err), &info));
            }
        };
        let n = update_many(app, guild_id, id, None, vec![("channelId", json!(channel.id)), ("messageId", json!(sent.id))]).await?;
        if n == 0 {
            // Deleted while posting: take the untracked copy down again.
            let _ = discord.delete_message(&channel.id, &sent.id).await;
            return Err(not_found());
        }
        let mut notes: Vec<String> = Vec::new();
        if let Some((pc, pm)) = &previous
            && *pm != sent.id
        {
            let where_ = match channel_name(&resources, Some(pc)).filter(|n| !n.is_empty()) {
                Some(n) => format!("#{n}"),
                None => "its old channel".into(),
            };
            if delete_previous {
                match discord.delete_message(pc, pm).await {
                    Ok(()) => notes.push(format!("The previous copy in {where_} was deleted.")),
                    Err(err) => {
                        if err.code != "not_found" {
                            let info = FailureInfo { op: DiscordOp::Delete, channel_name: channel_name(&resources, Some(pc)), channel_id: Some(pc), embeds: true };
                            let why = discord_failure(StoreError::Discord(err), &info).to_string();
                            notes.push(format!("{why} The previous copy in {where_} stays; delete it by hand."));
                        }
                    }
                }
            } else {
                notes.push(format!("The previous copy in {where_} stays but is no longer updated."));
            }
        }
        if !refused.is_empty() {
            let names: Vec<String> = refused
                .iter()
                .map(|rid| match resources.role(rid).map(|r| r.name.as_str()).filter(|n| !n.is_empty()) {
                    Some(n) => format!("@{n}"),
                    None => rid.clone(),
                })
                .collect();
            notes.push(format!("Not pinged (not mentionable, and you lack the Mention @everyone permission): {}.", names.join(", ")));
        }
        let mut message = vec![format!("Posted in #{}.", channel.name)];
        message.extend(notes);
        Ok(ActionAnswer { item: Some(item(&find_row(app, guild_id, id).await?)), message: Some(message.join(" ")) })
    }

    /// Edits the posted copy to the saved message again (/embed update).
    async fn refresh(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        let app = &self.app;
        let row = find_row(app, guild_id, id).await?;
        let (Some(_), Some(message_id)) = (&row.channel_id, &row.message_id) else {
            return Err(RecordError::invalid("This embed is not posted anywhere. Post it with \"Post in a channel\".").into());
        };
        require_stored_channel_access(ctx, &row, None).await?;
        let outcome = edit_posted(app, guild_id, &row, &normalize_template(&row.template), ctx).await?;
        if outcome != Edited::Done {
            update_many(app, guild_id, id, Some(message_id), gone_patch(outcome)).await?;
            return Ok(ActionAnswer { item: Some(item(&find_row(app, guild_id, id).await?)), message: Some(gone_text(outcome).into()) });
        }
        Ok(ActionAnswer { item: Some(item(&find_row(app, guild_id, id).await?)), message: Some("The posted message was updated.".into()) })
    }

    /// Deletes the posted copy and keeps the saved embed as a draft (the channel is kept as a hint).
    async fn unpublish(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        let app = &self.app;
        let discord = require_discord(ctx, DiscordMethod::DeleteMessage)?.clone();
        let row = find_row(app, guild_id, id).await?;
        let (Some(channel_id), Some(message_id)) = (&row.channel_id, &row.message_id) else {
            return Err(RecordError::invalid("This embed is not posted anywhere.").into());
        };
        require_stored_channel_access(ctx, &row, None).await?;
        if let Err(err) = discord.delete_message(channel_id, message_id).await {
            // 404 (message or channel gone) counts as done.
            if err.code != "not_found" {
                let info = FailureInfo { op: DiscordOp::Delete, channel_name: None, channel_id: Some(channel_id), embeds: true };
                return Err(discord_failure(StoreError::Discord(err), &info));
            }
        }
        update_many(app, guild_id, id, Some(message_id), vec![("messageId", Value::Null)]).await?;
        Ok(ActionAnswer {
            item: Some(item(&find_row(app, guild_id, id).await?)),
            message: Some("The posted message was deleted. The embed is a draft again.".into()),
        })
    }
}
