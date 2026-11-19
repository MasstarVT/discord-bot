//! Port of `src/dashboard/collections/tags.store.js`: the CustomTag rows /tag writes, through the same tagService
//! helpers (create_tag_row_with takes the per-guild advisory lock and the cap; the actions JSON and the content/useEmbed
//! mirror columns are written together). Role actions and ping roles follow the /tag rules for the dashboard user.

use std::collections::HashMap;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{CUSTOM_TAG_INFO, CustomTag, SelectBuilder, UpdateBuilder};
use dash_collections::channel_access::{ChannelCheck, SEND_NEEDS, require_channel_access};
use dash_collections::collections::embeds::{assign_role_problem, member_for, ping_role_problem};
use dash_collections::collections::tags::{action_to_form, action_to_stored, tag_item};
use dash_collections::{DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx};
use feat_tags::tag_service as tags;
use serde_json::{Map, Value, json};

use crate::common::{OrInternal, R, actor_id, is_unique, load_resources, rec_at, row_json};

fn not_found() -> RecordError {
    RecordError::not_found("That custom command no longer exists.")
}

const COLUMNS: [&str; 7] = ["description", "enabled", "allowRoleIds", "denyRoleIds", "allowChannelIds", "denyChannelIds", "deleteInvocation"];

/// `getEffectiveActions(row)` as stored JSON (logs the unknown-format warning like the JS).
fn effective(app: &App, row: &CustomTag) -> Vec<Value> {
    tags::warn_unknown_actions_format(app, row);
    tags::get_effective_actions(row).iter().map(tags::TagAction::to_json).collect()
}

fn to_item(app: &App, row: &CustomTag) -> Value {
    tag_item(&row_json(row), &effective(app, row))
}

fn list_of(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn is_role_action(t: Option<&str>) -> bool {
    matches!(t, Some("addRole" | "removeRole" | "toggleRole"))
}

/// The /tag action add role rules for every role the form adds: role actions need a role the dashboard user could hand
/// out (assignRoleProblem), ping roles must be pingable by them (pingRoleProblem). Roles the stored command already
/// had are kept without a check, so an unrelated edit never fails on them.
async fn check_roles(guild_id: &str, actions: &[Value], before: &[Value], ctx: &StoreCtx) -> R<()> {
    let old_roles: Vec<&Value> = before.iter().filter_map(|a| a.get("roleId")).filter(|r| crate::common::truthy(Some(r))).collect();
    let old_pings: Vec<Value> = before.iter().flat_map(|a| list_of(a.get("pingRoleIds"))).collect();
    // (path, role id, assign?)
    let mut checks: Vec<(String, String, bool)> = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        let ty = a.get("type").and_then(Value::as_str);
        if is_role_action(ty) {
            let role = a.get("roleId").cloned().unwrap_or(Value::Null);
            if !old_roles.contains(&&role) {
                checks.push((format!("actions.{i}.roleId"), js_string(&role), true));
            }
        } else if ty != Some("dm") {
            for (k, id) in list_of(a.get("pingRoleIds")).iter().enumerate() {
                if old_pings.contains(id) {
                    continue;
                }
                checks.push((format!("actions.{i}.pingRoleIds.{k}"), js_string(id), false));
            }
        }
    }
    if checks.is_empty() {
        return Ok(());
    }
    let resources = load_resources(
        ctx,
        guild_id,
        "Roles cannot be checked from the dashboard right now.",
        "The server's roles could not be loaded from Discord. Try again in a moment.",
        &[],
    )
    .await?;
    let res: &DiscordResources = &resources;
    let member = member_for(ctx, Some(res));
    for (path, id, assign) in checks {
        let problem =
            if assign { assign_role_problem(Some(res), &id, member.as_ref()) } else { ping_role_problem(Some(res), &id, member.as_ref()) };
        if let Some(p) = problem {
            return Err(rec_at(RecordErrorCode::Invalid, p, &path));
        }
    }
    Ok(())
}

fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => dash_schema::js::to_js_string(Some(other)),
    }
}

/// The /tag action add rule for a send action: the dashboard user must be able to view and post in the channel. Only a
/// send action the stored command already had, exactly as it is, is kept without a check.
async fn check_channels(actions: &[Value], before: &[Value], ctx: &StoreCtx) -> R<()> {
    // Form and stored actions compared through the same projection (actionToStored), so only real changes count; each
    // stored action keeps one form action (a copy added next to it is new).
    let mut kept: HashMap<String, i64> = HashMap::new();
    for a in before {
        *kept.entry(action_to_stored(&action_to_form(a)).to_string()).or_insert(0) += 1;
    }
    let mut checks = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        let key = action_to_stored(a).to_string();
        let is_kept = match kept.get_mut(&key) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        };
        if !is_kept && a.get("type").and_then(Value::as_str) == Some("send") && crate::common::truthy(a.get("channelId")) {
            checks.push(ChannelCheck::new(js_string(&a["channelId"])).needs(&SEND_NEEDS).path(format!("actions.{i}.channelId")));
        }
    }
    require_channel_access(ctx, &checks, "").await
}

/// `actionColumns(formActions)`: the actions JSON plus the legacy mirror columns.
fn action_columns(form_actions: &[Value]) -> Vec<(&'static str, Value)> {
    let stored: Vec<Value> = form_actions.iter().map(action_to_stored).collect();
    // syncLegacyColumns(stored): content and useEmbed follow the primary message action ({} when there is none).
    let primary = stored.iter().find(|a| matches!(a.get("type").and_then(Value::as_str), Some("reply" | "send")));
    let content = primary.and_then(|a| a.get("responses")).and_then(Value::as_array).and_then(|r| r.first()).filter(|v| !v.is_null()).cloned();
    let use_embed = primary.map(|a| a.get("embed") == Some(&Value::Bool(true)));
    vec![("actions", Value::Array(stored)), ("content", content.unwrap_or_else(|| json!(""))), ("useEmbed", json!(use_embed.unwrap_or(false)))]
}

fn scope_str(row: &CustomTag) -> String {
    row_json(row).get("cooldownScope").and_then(Value::as_str).unwrap_or("NONE").to_string()
}

/// The `tags` store.
pub struct TagsStore {
    app: App,
}

impl TagsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn find(&self, guild_id: &str, id: &str) -> R<Option<CustomTag>> {
        SelectBuilder::new(&CUSTOM_TAG_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional::<CustomTag, _>(self.app.db())
            .await
            .oi()
    }

    async fn find_row(&self, guild_id: &str, id: &str) -> R<CustomTag> {
        self.find(guild_id, id).await?.ok_or_else(|| not_found().into())
    }
}

#[async_trait]
impl RecordStore for TagsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let rows = SelectBuilder::new(&CUSTOM_TAG_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("name", true)
            .oi()?
            .limit(limits::CUSTOM_TAGS as i64)
            .build()
            .oi()?
            .fetch_all::<CustomTag, _>(self.app.db())
            .await
            .oi()?;
        Ok(ListAnswer::items(rows.iter().map(|r| to_item(&self.app, r)).collect()))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        Ok(self.find(guild_id, id).await?.as_ref().map(|r| to_item(&self.app, r)))
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let raw_name = match data.get("name") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(v),
        };
        let name = match tags::validate_new_tag_name(&raw_name) {
            Ok(n) if data.get("name").and_then(Value::as_str) == Some(n.as_str()) => n,
            Ok(_) => return Err(rec_at(RecordErrorCode::Invalid, "That name is not valid.", "name")),
            Err(reason) => return Err(rec_at(RecordErrorCode::Invalid, reason, "name")),
        };
        let actions = list_of(data.get("actions"));
        check_roles(guild_id, &actions, &[], ctx).await?;
        check_channels(&actions, &[], ctx).await?;
        let mut fields: tags::Patch = vec![("name", json!(name)), ("createdBy", json!(actor_id(ctx)))];
        for k in COLUMNS {
            if let Some(v) = data.get(k) {
                fields.push((k, v.clone()));
            }
        }
        fields.extend(action_columns(&actions));
        if let Some(scope) = data.get("cooldownScope") {
            fields.push(("cooldownScope", scope.clone()));
        }
        let none = data.get("cooldownScope").and_then(Value::as_str) == Some("NONE");
        if none {
            fields.push(("cooldownSeconds", json!(0)));
        } else if let Some(s) = data.get("cooldownSeconds") {
            fields.push(("cooldownSeconds", s.clone()));
        }
        self.app.settings().ensure_guild_settings(guild_id).await.oi()?;
        match tags::create_tag_row_with(&self.app, guild_id, fields).await {
            Ok(row) => Ok(Saved::item(to_item(&self.app, &row))),
            Err(err) if is_unique(&err) => {
                Err(rec_at(RecordErrorCode::Conflict, format!("A custom command named \"{name}\" already exists."), "name"))
            }
            Err(err) if tags::as_tag_error(&err).is_some_and(|e| e.code == "LIMIT") => Err(RecordError::new(
                RecordErrorCode::Limit,
                format!("This server already has {} custom commands, the maximum. Delete one first.", limits::CUSTOM_TAGS),
            )
            .into()),
            Err(err) => Err(err.into()),
        }
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let row = self.find_row(guild_id, id).await?;
        let mut patch: Vec<(&'static str, Value)> = Vec::new();
        for k in COLUMNS {
            if let Some(v) = data.get(k) {
                patch.push((k, v.clone()));
            }
        }
        if data.contains_key("actions") {
            let actions = list_of(data.get("actions"));
            let before = effective(&self.app, &row);
            check_roles(guild_id, &actions, &before, ctx).await?;
            check_channels(&actions, &before, ctx).await?;
            patch.extend(action_columns(&actions));
        }
        let scope_changed = data.contains_key("cooldownScope") || data.contains_key("cooldownSeconds");
        let mut new_cooldown: Option<(String, Value)> = None;
        if scope_changed {
            let scope = data.get("cooldownScope").filter(|v| !v.is_null()).map_or_else(|| scope_str(&row), js_string);
            let seconds = if scope == "NONE" {
                json!(0)
            } else {
                data.get("cooldownSeconds").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(row.cooldown_seconds))
            };
            patch.push(("cooldownScope", json!(scope)));
            patch.push(("cooldownSeconds", seconds.clone()));
            new_cooldown = Some((scope, seconds));
        }
        if patch.is_empty() {
            return Ok(Saved::item(to_item(&self.app, &row)));
        }
        // Optimistic, like writeTag: the form was filled from `row`, so a concurrent change is reported, not overwritten.
        let count = UpdateBuilder::new(&CUSTOM_TAG_INFO)
            .set_all(patch)
            .oi()?
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .filter("updatedAt", json!(bot_db::format_js_date(&row.updated_at)))
            .oi()?
            .without_returning()
            .build()
            .oi()?
            .execute(self.app.db())
            .await
            .oi()?;
        if count == 0 {
            if self.find(guild_id, id).await?.is_none() {
                return Err(not_found().into());
            }
            return Err(RecordError::new(RecordErrorCode::Conflict, "Someone else changed this command meanwhile. Reload it and try again.").into());
        }
        if let Some((scope, seconds)) = new_cooldown {
            let changed = scope != scope_str(&row) || seconds.as_f64() != Some(f64::from(row.cooldown_seconds));
            if changed {
                tags::clear_tag_cooldowns(&self.app, guild_id, id).await; // as /tag cooldown does
            }
        }
        let fresh = self.find_row(guild_id, id).await?;
        Ok(Saved::item(to_item(&self.app, &fresh)))
    }

    async fn remove(&self, guild_id: &str, id: &str, _ctx: &StoreCtx) -> R<()> {
        self.find_row(guild_id, id).await?;
        let count = bot_db::delete_where(&CUSTOM_TAG_INFO, [("id", json!(id)), ("guildId", json!(guild_id))])
            .oi()?
            .execute(self.app.db())
            .await
            .oi()?;
        if count == 0 {
            return Err(not_found().into());
        }
        tags::clear_tag_cooldowns(&self.app, guild_id, id).await;
        Ok(())
    }
}
