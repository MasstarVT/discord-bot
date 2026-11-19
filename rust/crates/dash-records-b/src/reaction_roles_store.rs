//! Port of `src/dashboard/collections/reactionRoles.store.js`: the same ReactionRoleGroup / ReactionRoleItem rows and
//! panel index as /reactionrole (feat-reactionroles). The dashboard only writes the database (under a per-guild advisory
//! lock for the panel cap); everything that touches Discord is a task run in the bot:
//!   rr.publish  post a new panel or bring the posted one up to date   (create, edit)
//!   rr.repost   post again, in another channel or after the message was deleted   (the "Repost" action)
//!   rr.delete   delete the panel and its message   (delete)
//!   rr.roleTemplate  create a role template's roles and fill the saved empty panel   (create from a template)

use std::collections::HashSet;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{InsertBuilder, REACTION_ROLE_GROUP_INFO, REACTION_ROLE_ITEM_INFO, ReactionRoleItem, SelectBuilder, UpdateBuilder};
use dash_collections::collections::reaction_roles::{DEFAULT_TITLE, UNPOSTED_MESSAGE_ID, diff_panel_items, emoji_key, normalize_items, panel_item};
use dash_collections::errors::TaskStatus;
use dash_collections::{
    ActionAnswer, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError,
};
use feat_reactionroles::reaction_role_service as rr;
use feat_reactionroles::role_templates::get_role_template;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::common::{
    OrInternal, R, custom_emoji_id, is_unique, load_resources, lock, log_info, now, now_ms, rec_at, recheck, row_json, run_after_save, run_task,
    sentence,
};

/// Raw statements (test/db.test.js runs each one): (name, SQL, sample bind).
pub const RAW_SQL: [(&str, &str, &str); 2] = [
    ("dashboard.reactionRoles.lockGuild", crate::common::LOCK_SQL, "rrpanel:0"),
    ("dashboard.reactionRoles.lockItems", crate::common::LOCK_SQL, "rritems:0"),
];

fn not_found() -> RecordError {
    RecordError::not_found("That panel no longer exists.")
}

fn require_tasks(ctx: &StoreCtx) -> R<()> {
    if ctx.can_run_tasks() { Ok(()) } else { Err(RecordError::unavailable("Panels cannot be posted from the dashboard right now.").into()) }
}

fn emoji_ids(items: &[Value]) -> Vec<String> {
    items.iter().filter_map(|i| i.get("emoji").and_then(Value::as_str).and_then(custom_emoji_id).map(str::to_string)).collect()
}

/// The server's roles and emojis (read again, fresh, when a custom emoji of the form is missing from the cached list).
async fn resources_for(ctx: &StoreCtx, guild_id: &str, items: &[Value]) -> R<Arc<DiscordResources>> {
    load_resources(
        ctx,
        guild_id,
        "The roles of this server cannot be checked from the dashboard right now.",
        "The server could not be loaded from Discord. Try again in a moment.",
        &emoji_ids(items),
    )
    .await
}

/// A Unicode emoji must be one the bot can put on a panel (parseEmojiInput: exactly one emoji).
fn assert_usable_emojis(items: &[Value]) -> R<()> {
    for (i, item) in items.iter().enumerate() {
        let Some(emoji) = item.get("emoji").filter(|v| crate::common::truthy(Some(v))) else { continue };
        let text = emoji.as_str().unwrap_or_default();
        if custom_emoji_id(text).is_none() && !rr::is_unicode_emoji(text) {
            return Err(rec_at(
                RecordErrorCode::Invalid,
                format!("\"{text}\" cannot be used as a panel emoji. Pick a standard emoji or one of this server's emojis."),
                &format!("items.{i}.emoji"),
            ));
        }
    }
    Ok(())
}

fn str_or_none(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

/// `rr.draftItem(groupId, item, position)` as the insert of a ReactionRoleItem row.
fn item_insert(group_id: &str, item: &Value, position: i64) -> anyhow::Result<bot_db::BuiltQuery> {
    let new = rr::NewItem {
        role_id: item.get("roleId").and_then(Value::as_str).unwrap_or_default().to_string(),
        label: item.get("label").and_then(Value::as_str).unwrap_or_default().to_string(),
        emoji: str_or_none(item.get("emoji")),
        style: item.get("style").and_then(Value::as_i64).map(|s| s as i32),
        description: str_or_none(item.get("description")),
    };
    let d = rr::draft_item(group_id, &new, position as i32)?;
    Ok(InsertBuilder::new(&REACTION_ROLE_ITEM_INFO)
        .values([
            ("groupId", json!(group_id)),
            ("customId", json!(d.custom_id)),
            ("roleId", json!(d.role_id)),
            ("label", json!(d.label)),
            ("emoji", json!(d.emoji)),
            ("style", json!(d.style)),
            ("description", json!(d.description)),
            ("position", json!(d.position)),
        ])?
        .build()?)
}

/// The `reactionRoles` store.
pub struct ReactionRolesStore {
    app: App,
}

impl ReactionRolesStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn load_row(&self, guild_id: &str, id: &str) -> R<rr::store::Panel> {
        rr::get_panel(self.app.db(), guild_id, id).await.oi()?.ok_or_else(|| not_found().into())
    }

    async fn get_item(&self, guild_id: &str, id: &str) -> R<Option<Value>> {
        Ok(rr::get_panel(self.app.db(), guild_id, id).await.oi()?.map(|p| panel_item(&row_json(&p))))
    }

    /// The bot creates (or reuses) the template's roles, links them, fills the saved empty panel and posts it. When it
    /// refuses, the empty panel is deleted again so nothing half-made stays in the list.
    async fn create_from_template(&self, ctx: &StoreCtx, guild_id: &str, id: &str, template: &str) -> R<Saved> {
        let out = match run_task(ctx, "rr.roleTemplate", json!({ "groupId": id, "template": template })).await {
            Ok(o) => o,
            Err(StoreError::Record(e)) if matches!(e.code, RecordErrorCode::Failed | RecordErrorCode::Unavailable) => {
                rr::delete_panel(self.app.db(), guild_id, id).await.oi()?;
                rr::invalidate_panel_index(&self.app, guild_id).await;
                return Err(RecordError::new(e.code, format!("Nothing was posted: {}", sentence(&e.message))).into());
            }
            Err(e) => return Err(e),
        };
        let message = if out.status == TaskStatus::Pending {
            "The bot is still creating the roles and posting the panel. Refresh in a moment to see it.".to_string()
        } else {
            out.message
        };
        Ok(Saved::with_message(self.get_item(guild_id, id).await?.unwrap_or(Value::Null), Some(message)))
    }
}

#[async_trait]
impl RecordStore for ReactionRolesStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let groups: Vec<bot_db::ReactionRoleGroup> = SelectBuilder::new(&REACTION_ROLE_GROUP_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("createdAt", false)
            .oi()?
            .limit(limits::REACTION_ROLE_PANELS as i64)
            .build()
            .oi()?
            .fetch_all(self.app.db())
            .await
            .oi()?;
        let ids: Vec<String> = groups.iter().map(|g| g.id.clone()).collect();
        let items: Vec<ReactionRoleItem> =
            sqlx::query_as(r#"SELECT * FROM "ReactionRoleItem" WHERE "groupId" = ANY($1) ORDER BY "position" ASC"#).bind(&ids).fetch_all(self.app.db()).await.oi()?;
        let out = groups
            .into_iter()
            .map(|group| {
                let mine: Vec<ReactionRoleItem> = items.iter().filter(|i| i.group_id == group.id).cloned().collect();
                panel_item(&row_json(&rr::store::Panel { group, items: mine }))
            })
            .collect();
        Ok(ListAnswer::items(out))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        self.get_item(guild_id, id).await
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        require_tasks(ctx)?;
        let mut data = data;
        // A role template: the template's panel, whose roles the bot creates (rr.roleTemplate), so the role list is ignored.
        let template = data.get("template").and_then(Value::as_str).and_then(get_role_template);
        if let Some(t) = &template {
            let panel = &t.panel;
            let title = data.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
            let trimmed = bot_util::js::trim(&title);
            let keep_title = !trimmed.is_empty() && trimmed != DEFAULT_TITLE;
            data.insert("type".into(), json!(panel.kind));
            data.insert("layout".into(), json!(panel.layout));
            data.insert("items".into(), json!([]));
            data.insert("maxRoles".into(), Value::Null);
            data.insert("title".into(), if keep_title { json!(title) } else { json!(panel.title) });
            if data.get("description").is_none_or(Value::is_null) {
                data.insert("description".into(), json!(panel.description));
            }
            if data.get("placeholder").is_none_or(Value::is_null) {
                match panel.placeholder {
                    Some(p) => data.insert("placeholder".into(), json!(p)),
                    None => data.remove("placeholder"),
                };
            }
        }
        let layout = data.get("layout").and_then(Value::as_str).unwrap_or_default().to_string();
        let kind = data.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
        let raw_items = data.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let items = if raw_items.is_empty() {
            Vec::new()
        } else {
            let res = resources_for(ctx, guild_id, &raw_items).await?;
            normalize_items(&raw_items, Some(&layout), Some(&res), &HashSet::new())?
        };
        assert_usable_emojis(&items)?;
        let id = bot_db::new_uuid();
        self.app.settings().ensure_guild_settings(guild_id).await.oi()?;

        let title = data.get("title").and_then(Value::as_str).unwrap_or_default();
        let title = if bot_util::js::trim(title).is_empty() { DEFAULT_TITLE.to_string() } else { bot_util::js::trim(title).to_string() };
        let group = InsertBuilder::new(&REACTION_ROLE_GROUP_INFO)
            .values([
                ("id", json!(id)),
                ("guildId", json!(guild_id)),
                ("channelId", data.get("channelId").cloned().unwrap_or(Value::Null)),
                // Not posted yet: the bot posts it right after and moves the row to the real message.
                ("messageId", json!(UNPOSTED_MESSAGE_ID)),
                ("messageMissingAt", json!(bot_db::format_js_date(&now(&self.app)))),
                ("title", json!(title)),
                ("description", data.get("description").cloned().unwrap_or(Value::Null)),
                ("type", json!(kind)),
                ("layout", json!(layout)),
                ("maxRoles", if kind == "NORMAL" { data.get("maxRoles").cloned().unwrap_or(Value::Null) } else { Value::Null }),
                ("plainText", json!(data.get("plainText") == Some(&Value::Bool(true)))),
                ("messageContent", data.get("messageContent").cloned().unwrap_or(Value::Null)),
                ("attached", json!(false)),
                ("placeholder", if layout == "SELECT" { data.get("placeholder").cloned().unwrap_or(Value::Null) } else { Value::Null }),
            ])
            .oi()?
            .without_returning()
            .build()
            .oi()?;
        let inserts = items.iter().enumerate().map(|(p, item)| item_insert(&id, item, p as i64)).collect::<anyhow::Result<Vec<_>>>().oi()?;
        let key = format!("rrpanel:{guild_id}");
        let gid = guild_id.to_string();
        let result = bot_db::transaction(self.app.db(), bot_db::TxOptions::new(2_000, 10_000), move |tx| {
            Box::pin(async move {
                lock(&mut *tx, &key).await?;
                let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "ReactionRoleGroup" WHERE "guildId" = $1"#)
                    .bind(&gid)
                    .fetch_one(&mut *tx)
                    .await?;
                if n >= limits::REACTION_ROLE_PANELS as i64 {
                    return Err(anyhow::Error::from(RecordError::new(
                        RecordErrorCode::Limit,
                        format!("This server already has {} reaction role panels, the maximum. Delete one first.", limits::REACTION_ROLE_PANELS),
                    )));
                }
                group.execute(&mut *tx).await?;
                for q in inserts {
                    q.execute(&mut *tx).await?;
                }
                Ok::<(), anyhow::Error>(())
            })
        })
        .await;
        if let Err(err) = result {
            if let Some(e) = err.downcast_ref::<RecordError>() {
                return Err(e.clone().into());
            }
            if is_unique(&err) {
                return Err(RecordError::new(
                    RecordErrorCode::Conflict,
                    "That panel could not be saved because one of its roles is already in use. Try again.",
                )
                .into());
            }
            return Err(err.into());
        }
        rr::invalidate_panel_index(&self.app, guild_id).await;
        log_info(format!("Dashboard reaction role panel {id} created in {guild_id} by {}", ctx.user_id));

        if let Some(t) = template {
            return self.create_from_template(ctx, guild_id, &id, t.key).await;
        }
        let posted = run_after_save(
            ctx,
            "rr.publish",
            json!({ "groupId": id, "mode": "post" }),
            "The panel was saved, but it was not posted:",
            "Fix that, close this window, then use \"Repost\" on the panel in the list.",
        )
        .await?;
        // The bot's note (where it posted, any reaction it could not add) replaces the plain "Created" toast.
        Ok(Saved::with_message(self.get_item(guild_id, &id).await?.unwrap_or(Value::Null), Some(posted.message)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        require_tasks(ctx)?;
        let row = self.load_row(guild_id, id).await?;
        let row_value = row_json(&row);
        // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
        recheck("reactionRoles", &data, &panel_item(&row_value), now_ms(&self.app))?;

        let mut patch = Map::new();
        for key in ["title", "description", "messageContent", "plainText", "type"] {
            if let Some(v) = data.get(key) {
                let current = row_value.get(key).cloned().unwrap_or(Value::Null);
                if !dash_schema::js::strict_eq(v, &current) && !(v.is_null() && current.is_null()) {
                    patch.insert(key.into(), v.clone());
                }
            }
        }
        if let Some(t) = patch.get("title").cloned() {
            let s = t.as_str().unwrap_or_default();
            let trimmed = bot_util::js::trim(s);
            patch.insert("title".into(), json!(if trimmed.is_empty() { DEFAULT_TITLE } else { trimmed }));
        }
        let kind_now = patch.get("type").and_then(Value::as_str).map_or_else(|| row.kind().to_string(), str::to_string);
        if kind_now != "NORMAL" {
            // Like /reactionrole edit: the role limit only exists in Normal mode.
            if row.group.max_roles.is_some() {
                patch.insert("maxRoles".into(), Value::Null);
            }
        } else if data.contains_key("maxRoles") {
            let want = data.get("maxRoles").cloned().unwrap_or(Value::Null);
            if !dash_schema::js::strict_eq(&want, &json!(row.group.max_roles)) {
                patch.insert("maxRoles".into(), want);
            }
        }
        if row.layout() == "SELECT" && data.contains_key("placeholder") {
            let want = data.get("placeholder").cloned().unwrap_or(Value::Null);
            if !dash_schema::js::strict_eq(&want, &json!(row.group.placeholder)) {
                patch.insert("placeholder".into(), want);
            }
        }

        let mut diff = None;
        if data.contains_key("items") {
            let held: HashSet<String> = row.items.iter().filter_map(|i| emoji_key(Some(&json!(i.emoji)))).collect();
            let raw_items = data.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let res = resources_for(ctx, guild_id, &raw_items).await?;
            let next = normalize_items(&raw_items, Some(row.layout()), Some(&res), &held)?;
            assert_usable_emojis(&next)?;
            let (gid, pid) = (guild_id.to_string(), id.to_string());
            let result = bot_db::transaction(self.app.db(), bot_db::TxOptions::new(2_000, 10_000), move |tx| {
                Box::pin(async move {
                    lock(&mut *tx, &format!("rritems:{pid}")).await?;
                    let exists: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "ReactionRoleGroup" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
                        .bind(&pid)
                        .bind(&gid)
                        .fetch_optional(&mut *tx)
                        .await?;
                    if exists.is_none() {
                        return Err(anyhow::Error::from(not_found()));
                    }
                    // The rows as they are now, under the lock.
                    let stored: Vec<ReactionRoleItem> = SelectBuilder::new(&REACTION_ROLE_ITEM_INFO)
                        .filter("groupId", json!(pid))?
                        .order_by("position", true)?
                        .build()?
                        .fetch_all(&mut *tx)
                        .await?;
                    let stored: Vec<Value> = stored.iter().map(row_json).collect();
                    let plan = diff_panel_items(&stored, &next);
                    for gone in &plan.remove {
                        bot_db::delete_where(&REACTION_ROLE_ITEM_INFO, [("id", gone.get("id").cloned().unwrap_or(Value::Null))])?
                            .execute(&mut *tx)
                            .await?;
                    }
                    for u in &plan.update {
                        let data = u.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
                        UpdateBuilder::new(&REACTION_ROLE_ITEM_INFO)
                            .set_all(data.iter().map(|(k, v)| (k.as_str(), v.clone())))?
                            .filter("id", u.get("id").cloned().unwrap_or(Value::Null))?
                            .without_returning()
                            .build()?
                            .execute(&mut *tx)
                            .await?;
                    }
                    for c in &plan.create {
                        let position = c.get("position").and_then(Value::as_i64).unwrap_or(0);
                        item_insert(&pid, c, position)?.execute(&mut *tx).await?;
                    }
                    Ok::<_, anyhow::Error>(plan)
                })
            })
            .await;
            match result {
                Ok(plan) => diff = Some(plan),
                Err(err) => {
                    if let Some(e) = err.downcast_ref::<RecordError>() {
                        return Err(e.clone().into());
                    }
                    if is_unique(&err) {
                        return Err(rec_at(
                            RecordErrorCode::Conflict,
                            "One of those roles was just added to the panel by someone else. Reload and try again.",
                            "items",
                        ));
                    }
                    return Err(err.into());
                }
            }
        }
        if !patch.is_empty() {
            let p = rr::store::PanelPatch {
                title: patch.get("title").and_then(Value::as_str).map(str::to_string),
                description: patch.get("description").map(|v| str_or_none(Some(v))),
                message_content: patch.get("messageContent").map(|v| str_or_none(Some(v))),
                plain_text: patch.get("plainText").and_then(Value::as_bool),
                kind: patch.get("type").and_then(Value::as_str).map(str::to_string),
                placeholder: patch.get("placeholder").map(|v| str_or_none(Some(v))),
                max_roles: patch.get("maxRoles").map(|v| v.as_i64().map(|n| n as i32)),
                layout: None,
                channel_id: None,
            };
            if rr::update_panel(self.app.db(), guild_id, id, &p).await.oi()?.is_none() {
                return Err(not_found().into());
            }
        }
        let changed = diff.as_ref().is_some_and(|d| d.changed);
        if changed || !patch.is_empty() {
            rr::invalidate_panel_index(&self.app, guild_id).await;
        }

        // The posted message follows the saved panel. An attached message is never edited: only its reactions change. A new
        // role mode or limit also reaches the bot's panel index through the task, so it applies at once.
        let message_changed =
            ["title", "description", "messageContent", "plainText", "type", "maxRoles", "placeholder"].iter().any(|k| patch.contains_key(*k));
        let unposted = row.group.message_id == UNPOSTED_MESSAGE_ID;
        let mut synced = None;
        if changed || (!row.group.attached && message_changed) || patch.contains_key("type") || patch.contains_key("maxRoles") || unposted {
            let removed = diff.as_ref().map(|d| d.removed_emojis.clone()).unwrap_or_default();
            synced = Some(
                run_after_save(
                    ctx,
                    "rr.publish",
                    json!({ "groupId": id, "mode": "sync", "removedEmojis": removed }),
                    "Your changes were saved, but the posted panel was not fully updated:",
                    "Use \"Repost\" to post the panel again.",
                )
                .await?,
            );
        }
        log_info(format!("Dashboard reaction role panel {id} updated in {guild_id} by {}", ctx.user_id));
        Ok(Saved::with_message(self.get_item(guild_id, id).await?.unwrap_or(Value::Null), synced.map(|s| s.message)))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        let row = self.load_row(guild_id, id).await?;
        if row.group.message_id == UNPOSTED_MESSAGE_ID {
            // Never posted: there is no message to delete and no reaction to take back, so the bot is not needed.
            rr::delete_panel(self.app.db(), guild_id, id).await.oi()?;
            rr::invalidate_panel_index(&self.app, guild_id).await;
        } else {
            require_tasks(ctx)?;
            run_task(ctx, "rr.delete", json!({ "groupId": id })).await?;
        }
        log_info(format!("Dashboard reaction role panel {id} deleted in {guild_id} by {}", ctx.user_id));
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["repost"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<ActionAnswer> {
        if action_id != "repost" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        // Posts the panel again (another channel, or after its message was deleted): `{ channelId }`.
        require_tasks(ctx)?;
        self.load_row(guild_id, id).await?;
        let channel = data.get("channelId").filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null);
        let out = run_task(ctx, "rr.repost", json!({ "groupId": id, "channelId": channel })).await?;
        Ok(ActionAnswer { item: self.get_item(guild_id, id).await?, message: Some(sentence(&out.message)) })
    }
}
