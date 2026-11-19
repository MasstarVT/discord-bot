//! Port of `src/dashboard/collections/ticketPanels.store.js`: the same TicketPanel / TicketPanelOption rows as
//! /ticket-panel (feat-tickets). Panels and settings are written through the service's Discord-free functions
//! (create_panel, update_panel_fields) and, for the whole option list at once, in one transaction under the same
//! advisory lock the service's option edits use. What needs the gateway client is a task run in the bot:
//!   ticket.check    {categoryIds, transcriptChannelId}  the bot's permissions in new categories / transcript channel
//!   ticket.publish  {panelId, channelId}                post the panel message, or move / update it
//!   ticket.refresh  {panelId, ...}                      update the posted message and back-fill manager roles
//!   ticket.delete   {panelId}                           delete the panel and its message

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_db::{InsertBuilder, SelectBuilder, TICKET_PANEL_INFO, TICKET_PANEL_OPTION_INFO, TicketPanel, TicketPanelOption, UpdateBuilder};
use dash_collections::collections::embeds::{member_for, ping_role_problem};
use dash_collections::collections::ticket_panels::{
    OptionsDiff, default_intro_template, default_panel_template, diff_options, is_default_template, is_empty_template, normalize_options,
    panel_item, to_json,
};
use dash_collections::errors::TaskStatus;
use dash_collections::{
    ActionAnswer, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode, RecordStore, Saved, StoreCtx, StoreError,
};
use feat_tickets::ticket_service as ts;
use serde_json::{Map, Value, json};
use std::sync::Arc;

use crate::common::{
    OrInternal, R, custom_emoji_id, load_resources, lock, log_info, now_ms, rec_at, recheck, row_json, row_map, run_after_save, run_task,
    truthy,
};

/// Raw statements (test/db.test.js runs each one): (name, SQL, sample bind).
pub const RAW_SQL: [(&str, &str, &str); 1] = [("dashboard.ticketPanels.lockOptions", crate::common::LOCK_SQL, "ticket-option:0")];

/// `ACTIVE`: the ticket states that count as open on a panel.
const ACTIVE: [&str; 3] = ["OPEN", "CLAIMED", "CLOSED"];

fn not_found() -> RecordError {
    RecordError::not_found("That ticket panel no longer exists.")
}

fn require_tasks(ctx: &StoreCtx) -> R<()> {
    if ctx.can_run_tasks() { Ok(()) } else { Err(RecordError::unavailable("Panels cannot be published from the dashboard right now.").into()) }
}

fn list_of(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn emoji_ids(options: &[Value]) -> Vec<String> {
    options.iter().filter_map(|o| o.get("emoji").and_then(Value::as_str).and_then(custom_emoji_id).map(str::to_string)).collect()
}

/// The server's roles and emojis (read again, fresh, when a custom emoji of the form is missing from the cached list).
async fn resources_for(ctx: &StoreCtx, guild_id: &str, options: &[Value]) -> R<Arc<DiscordResources>> {
    load_resources(
        ctx,
        guild_id,
        "The roles and emojis of this server cannot be checked from the dashboard right now.",
        "The server could not be loaded from Discord. Try again in a moment.",
        &emoji_ids(options),
    )
    .await
}

/// A Unicode option emoji must be one the bot can put on a button (parseOptionEmoji: exactly one emoji).
fn assert_usable_emojis(options: &[Value]) -> R<()> {
    for (i, o) in options.iter().enumerate() {
        let Some(emoji) = o.get("emoji").filter(|v| truthy(Some(v))) else { continue };
        let text = match emoji {
            Value::String(s) => s.clone(),
            other => dash_schema::js::to_js_string(Some(other)),
        };
        if custom_emoji_id(&text).is_some() {
            continue;
        }
        if let Err(e) = ts::parse_option_emoji(&text, &|_| false, &[]) {
            return Err(rec_at(RecordErrorCode::Invalid, e, &format!("options.{i}.emoji")));
        }
    }
    Ok(())
}

/// The stored form of a message: empty or the built-in default reads as "not set" (null); a panel message has no link
/// buttons.
fn stored_template(template: Option<&Value>, defaults: &[Value], strip_buttons: bool) -> Value {
    let t = template.cloned().unwrap_or(Value::Null);
    if is_empty_template(&t) || is_default_template(&t, defaults) {
        return Value::Null;
    }
    if strip_buttons {
        let mut n = bot_core::custom_embed::normalize_template(&t);
        n["buttons"] = json!([]);
        to_json(&n)
    } else {
        to_json(&t)
    }
}

fn button_templates() -> [Value; 2] {
    [default_panel_template(Some("BUTTONS")), default_panel_template(Some("SELECT"))]
}

/// `sameJson(a, b)`.
fn same_json(a: &Value, b: &Value) -> bool {
    serde_json::to_string(a).unwrap_or_default() == serde_json::to_string(b).unwrap_or_default()
}

/// Manager roles that still exist, once each (like updateManagers, which prunes deleted roles).
fn live_managers(ids: &[String], resources: &DiscordResources) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if out.contains(id) {
            continue;
        }
        if resources.roles.iter().any(|r| !r.everyone && &r.id == id) {
            out.push(id.clone());
        }
    }
    out
}

/// Configuration time: a {managers} ping must be something the admin could send themselves (the /ticket-panel rule).
fn assert_managers_pingable(ctx: &StoreCtx, resources: Option<&DiscordResources>, manager_ids: &[String], intros: &[Value]) -> R<()> {
    if manager_ids.is_empty() || !intros.iter().any(|t| truthy(Some(t)) && ts::template_uses_token(t, "managers")) {
        return Ok(());
    }
    let member = member_for(ctx, resources);
    for id in manager_ids {
        if let Some(problem) = ping_role_problem(resources, id, member.as_ref()) {
            return Err(rec_at(
                RecordErrorCode::Invalid,
                format!("A ticket intro of this panel pings {{managers}}, and one manager role cannot be pinged by you. {problem}"),
                "managerRoleIds",
            ));
        }
    }
    Ok(())
}

/// Asks the bot whether it may use the categories and the transcript channel (the checks /ticket-panel makes before
/// saving).
async fn precheck(ctx: &StoreCtx, category_ids: &[Value], transcript_channel_id: Option<&Value>) -> R<()> {
    let mut ids: Vec<Value> = Vec::new();
    for c in category_ids.iter().filter(|c| truthy(Some(c))) {
        if !ids.contains(c) {
            ids.push(c.clone());
        }
    }
    let transcript = transcript_channel_id.filter(|t| truthy(Some(t)));
    if ids.is_empty() && transcript.is_none() {
        return Ok(());
    }
    require_tasks(ctx)?;
    let out = run_task(ctx, "ticket.check", json!({ "categoryIds": ids, "transcriptChannelId": transcript.cloned().unwrap_or(Value::Null) })).await?;
    // Nothing is saved on a guess: a check that is still running when the wait ends is as good as none.
    if out.status == TaskStatus::Pending {
        return Err(RecordError::unavailable("The bot is slow to answer right now, so nothing was saved. Try again in a moment.").into());
    }
    Ok(())
}

/// An option change the posted message shows (anything but the category and the intro message).
fn shows_on_panel(plan: &OptionsDiff) -> bool {
    !plan.create.is_empty()
        || !plan.remove.is_empty()
        || plan.update.iter().any(|u| u["data"].as_object().is_some_and(|d| d.keys().any(|k| k != "categoryId" && k != "introTemplate")))
}

/// The custom emoji ids the stored options hold.
fn custom_ids(options: &[TicketPanelOption]) -> HashSet<String> {
    options.iter().filter_map(|o| o.emoji.as_deref().and_then(custom_emoji_id).map(str::to_string)).collect()
}

fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => dash_schema::js::to_js_string(Some(other)),
    }
}

fn strings(v: Option<&Value>) -> Vec<String> {
    list_of(v).iter().map(js_string).collect()
}

/// A loaded panel with its options (`include: { options: { orderBy: OPTION_ORDER } }`).
struct Loaded {
    panel: TicketPanel,
    options: Vec<TicketPanelOption>,
}

impl Loaded {
    fn json(&self) -> Value {
        let mut m = row_map(&self.panel);
        m.insert("options".into(), Value::Array(self.options.iter().map(row_json).collect()));
        Value::Object(m)
    }
}

/// The `ticketPanels` store.
pub struct TicketPanelsStore {
    app: App,
}

impl TicketPanelsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    async fn options_of(&self, panel_id: &str) -> R<Vec<TicketPanelOption>> {
        SelectBuilder::new(&TICKET_PANEL_OPTION_INFO)
            .filter("panelId", json!(panel_id))
            .oi()?
            .order_by("position", true)
            .oi()?
            .order_by("id", true)
            .oi()?
            .build()
            .oi()?
            .fetch_all(self.app.db())
            .await
            .oi()
    }

    async fn load_row(&self, guild_id: &str, id: &str) -> R<Option<Loaded>> {
        let panel: Option<TicketPanel> = SelectBuilder::new(&TICKET_PANEL_INFO)
            .filter("id", json!(id))
            .oi()?
            .filter("guildId", json!(guild_id))
            .oi()?
            .limit(1)
            .build()
            .oi()?
            .fetch_optional(self.app.db())
            .await
            .oi()?;
        let Some(panel) = panel else { return Ok(None) };
        let options = self.options_of(&panel.id).await?;
        Ok(Some(Loaded { panel, options }))
    }

    async fn get_item(&self, guild_id: &str, id: &str) -> R<Option<Value>> {
        let Some(row) = self.load_row(guild_id, id).await? else { return Ok(None) };
        let open: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*)::bigint FROM "Ticket" WHERE "guildId" = $1 AND "panelId" = $2 AND "status"::text = ANY($3)"#,
        )
        .bind(guild_id)
        .bind(id)
        .bind(ACTIVE.map(str::to_string).to_vec())
        .fetch_one(self.app.db())
        .await
        .oi()?;
        Ok(Some(panel_item(&row.json(), open)))
    }

    /// Writes the whole option list in one transaction (the service's option lock). Returns the plan that was applied.
    async fn replace_options(&self, guild_id: &str, panel_id: &str, next: Vec<Value>) -> R<OptionsDiff> {
        if next.is_empty() {
            return Err(rec_at(RecordErrorCode::Invalid, "A panel needs at least one option.", "options"));
        }
        if next.len() > limits::TICKET_PANEL_OPTIONS {
            return Err(rec_at(RecordErrorCode::Limit, format!("A panel can have at most {} options.", limits::TICKET_PANEL_OPTIONS), "options"));
        }
        let (gid, pid) = (guild_id.to_string(), panel_id.to_string());
        let result = bot_db::transaction(self.app.db(), bot_db::TxOptions::new(2_000, 10_000), move |tx| {
            Box::pin(async move {
                lock(&mut *tx, &format!("ticket-option:{pid}")).await?;
                let exists: Option<(String,)> = sqlx::query_as(r#"SELECT "id" FROM "TicketPanel" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
                    .bind(&pid)
                    .bind(&gid)
                    .fetch_optional(&mut *tx)
                    .await?;
                if exists.is_none() {
                    return Err(anyhow::Error::from(not_found()));
                }
                let stored: Vec<TicketPanelOption> = SelectBuilder::new(&TICKET_PANEL_OPTION_INFO)
                    .filter("panelId", json!(pid))?
                    .order_by("position", true)?
                    .order_by("id", true)?
                    .build()?
                    .fetch_all(&mut *tx)
                    .await?;
                let stored: Vec<Value> = stored.iter().map(row_json).collect();
                let plan = diff_options(&stored, &next);
                // Tickets of a removed option keep working with optionId null (the foreign key sets it).
                if !plan.remove.is_empty() {
                    let ids: Vec<String> = plan.remove.iter().map(|r| js_string(&r["id"])).collect();
                    sqlx::query(r#"DELETE FROM "TicketPanelOption" WHERE "id" = ANY($1) AND "panelId" = $2"#)
                        .bind(&ids)
                        .bind(&pid)
                        .execute(&mut *tx)
                        .await?;
                }
                for u in &plan.update {
                    let data = u.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
                    UpdateBuilder::new(&TICKET_PANEL_OPTION_INFO)
                        .set_all(data.iter().map(|(k, v)| (k.as_str(), v.clone())))?
                        .filter("id", u.get("id").cloned().unwrap_or(Value::Null))?
                        .without_returning()
                        .build()?
                        .execute(&mut *tx)
                        .await?;
                }
                for c in &plan.create {
                    let mut fields: Vec<(&str, Value)> = c.as_object().map(|m| m.iter().map(|(k, v)| (k.as_str(), v.clone())).collect()).unwrap_or_default();
                    if !fields.iter().any(|(k, _)| *k == "introTemplate") {
                        fields.push(("introTemplate", Value::Null));
                    }
                    fields.push(("panelId", json!(pid)));
                    InsertBuilder::new(&TICKET_PANEL_OPTION_INFO).values(fields)?.without_returning().build()?.execute(&mut *tx).await?;
                }
                Ok::<_, anyhow::Error>(plan)
            })
        })
        .await;
        ts::invalidate_panel(&self.app, panel_id);
        result.map_err(|err| match err.downcast_ref::<RecordError>() {
            Some(e) => e.clone().into(),
            None => StoreError::Internal(err),
        })
    }
}

#[async_trait]
impl RecordStore for TicketPanelsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, _ctx: &ReadCtx) -> R<ListAnswer> {
        let panels: Vec<TicketPanel> = SelectBuilder::new(&TICKET_PANEL_INFO)
            .filter("guildId", json!(guild_id))
            .oi()?
            .order_by("name", true)
            .oi()?
            .limit(limits::TICKET_PANELS as i64)
            .build()
            .oi()?
            .fetch_all(self.app.db())
            .await
            .oi()?;
        let ids: Vec<String> = panels.iter().map(|p| p.id.clone()).collect();
        let options: Vec<TicketPanelOption> =
            sqlx::query_as(r#"SELECT * FROM "TicketPanelOption" WHERE "panelId" = ANY($1) ORDER BY "position" ASC, "id" ASC"#)
                .bind(&ids)
                .fetch_all(self.app.db())
                .await
                .oi()?;
        let counts: Vec<(Option<String>, i64)> = sqlx::query_as(
            r#"SELECT "panelId", COUNT(*)::bigint FROM "Ticket" WHERE "guildId" = $1 AND "status"::text = ANY($2) GROUP BY "panelId""#,
        )
        .bind(guild_id)
        .bind(ACTIVE.map(str::to_string).to_vec())
        .fetch_all(self.app.db())
        .await
        .oi()?;
        let by_panel: HashMap<String, i64> = counts.into_iter().filter_map(|(p, n)| p.map(|p| (p, n))).collect();
        let items = panels
            .into_iter()
            .map(|panel| {
                let mine: Vec<TicketPanelOption> = options.iter().filter(|o| o.panel_id == panel.id).cloned().collect();
                let open = by_panel.get(&panel.id).copied().unwrap_or(0);
                panel_item(&Loaded { panel, options: mine }.json(), open)
            })
            .collect();
        Ok(ListAnswer::items(items))
    }

    async fn get(&self, guild_id: &str, id: &str, _ctx: &ReadCtx) -> R<Option<Value>> {
        self.get_item(guild_id, id).await
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        let channel_id = data.get("channelId").filter(|v| truthy(Some(v))).cloned();
        if channel_id.is_some() {
            require_tasks(ctx)?;
        }
        let format = ts::validate_name_format(&data.get("channelNameFormat").map(js_string).unwrap_or_default())
            .map_err(|e| rec_at(RecordErrorCode::Invalid, e, "channelNameFormat"))?;
        let raw_options = list_of(data.get("options"));
        let resources = resources_for(ctx, guild_id, &raw_options).await?;
        let options = normalize_options(&raw_options, Some(&resources), &HashSet::new())?;
        assert_usable_emojis(&options)?;
        let manager_role_ids = live_managers(&strings(data.get("managerRoleIds")), &resources);
        let template = stored_template(data.get("template"), &button_templates(), true);
        let intro_template = stored_template(data.get("introTemplate"), &[default_intro_template()], false);
        let mut intros = vec![intro_template.clone()];
        intros.extend(options.iter().map(|o| o.get("introTemplate").cloned().unwrap_or(Value::Null)));
        assert_managers_pingable(ctx, Some(&resources), &manager_role_ids, &intros)?;
        let mut categories: Vec<Value> =
            ["openCategoryId", "claimedCategoryId", "closedCategoryId"].iter().map(|k| data.get(*k).cloned().unwrap_or(Value::Null)).collect();
        categories.extend(options.iter().map(|o| o.get("categoryId").cloned().unwrap_or(Value::Null)));
        precheck(ctx, &categories, data.get("transcriptChannelId")).await?;

        let name = data.get("name").map(js_string).unwrap_or_default();
        let mut input = ts::db::NewPanel::new(guild_id, &name);
        if let Some(s) = data.get("style").and_then(|v| serde_json::from_value(v.clone()).ok()) {
            input.style = s;
        }
        if let Some(a) = data.get("askReason").and_then(Value::as_bool) {
            input.ask_reason = a;
        }
        if let Some(m) = data.get("maxOpenPerUser").and_then(Value::as_i64) {
            input.max_open_per_user = m as i32;
        }
        input.channel_name_format = format;
        let made = match ts::create_panel(&self.app, input).await.oi()? {
            Ok(p) => p,
            Err(fail) if fail.code == "PANEL_LIMIT" => {
                return Err(RecordError::new(
                    RecordErrorCode::Limit,
                    format!("This server already has {} ticket panels, the maximum. Delete one first.", limits::TICKET_PANELS),
                )
                .into());
            }
            Err(_) => return Err(rec_at(RecordErrorCode::Conflict, format!("A panel called \"{name}\" already exists."), "name")),
        };
        let id = made.panel.id.clone();
        let fields: Vec<(&str, Value)> = vec![
            ("managerRoleIds", json!(manager_role_ids)),
            ("openCategoryId", data.get("openCategoryId").cloned().unwrap_or(Value::Null)),
            ("claimedCategoryId", data.get("claimedCategoryId").cloned().unwrap_or(Value::Null)),
            ("closedCategoryId", data.get("closedCategoryId").cloned().unwrap_or(Value::Null)),
            ("transcriptChannelId", data.get("transcriptChannelId").cloned().unwrap_or(Value::Null)),
            ("dmTranscript", json!(data.get("dmTranscript") == Some(&Value::Bool(true)))),
            ("template", template),
            ("introTemplate", intro_template),
        ];
        let saved: R<()> = async {
            match ts::update_panel_fields(&self.app, &id, guild_id, &fields).await.oi()? {
                Ok(()) => {}
                Err(_) => return Err(not_found().into()),
            }
            self.replace_options(guild_id, &id, options).await?;
            Ok(())
        }
        .await;
        if let Err(err) = saved {
            // Nothing is half-made: the panel goes again, then the error.
            if let Ok(q) = bot_db::delete_where(&TICKET_PANEL_INFO, [("id", json!(id)), ("guildId", json!(guild_id))]) {
                let _ = q.execute(self.app.db()).await;
            }
            ts::invalidate_panel(&self.app, &id);
            return Err(err);
        }
        log_info(format!("Dashboard ticket panel {id} created in {guild_id} by {}", ctx.user_id));

        let mut published = None;
        if let Some(channel) = channel_id {
            published = Some(
                run_after_save(
                    ctx,
                    "ticket.publish",
                    json!({ "panelId": id, "channelId": channel }),
                    "The panel was saved, but it was not published:",
                    "Fix that, close this window, then use \"Publish\" on the panel in the list.",
                )
                .await?,
            );
        }
        // The bot's note (where it published, e.g. a missing manager role warning) replaces the plain "Created" toast.
        Ok(Saved::with_message(self.get_item(guild_id, &id).await?.unwrap_or(Value::Null), published.map(|p| p.message)))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<Saved> {
        require_tasks(ctx)?;
        let row = self.load_row(guild_id, id).await?.ok_or_else(not_found)?;
        let row_value = row.json();
        // The cross-field rules again, against the row as it is now (the request was validated against an earlier read).
        recheck("ticketPanels", &data, &panel_item(&row_value, 0), now_ms(&self.app))?;
        let has = |k: &str| data.contains_key(k);
        let row_field = |k: &str| row_value.get(k).cloned().unwrap_or(Value::Null);

        let mut patch: Vec<(&'static str, Value)> = Vec::new();
        let in_patch = |p: &Vec<(&'static str, Value)>, k: &str| p.iter().any(|(x, _)| *x == k);
        let patched = |p: &Vec<(&'static str, Value)>, k: &str| p.iter().find(|(x, _)| *x == k).map(|(_, v)| v.clone());
        for key in [
            "name", "style", "askReason", "dmTranscript", "maxOpenPerUser", "openCategoryId", "claimedCategoryId", "closedCategoryId",
            "transcriptChannelId",
        ] {
            if has(key) {
                let want = data.get(key).cloned().unwrap_or(Value::Null);
                if !dash_schema::js::strict_eq(&want, &row_field(key)) {
                    patch.push((key, want));
                }
            }
        }
        if has("channelNameFormat") {
            let format = ts::validate_name_format(&data.get("channelNameFormat").map(js_string).unwrap_or_default())
                .map_err(|e| rec_at(RecordErrorCode::Invalid, e, "channelNameFormat"))?;
            if format != row.panel.channel_name_format {
                patch.push(("channelNameFormat", json!(format)));
            }
        }
        if has("template") {
            let t = stored_template(data.get("template"), &button_templates(), true);
            let current = row.panel.template.as_ref().filter(|v| truthy(Some(v))).map_or(Value::Null, to_json);
            if !same_json(&t, &current) {
                patch.push(("template", t));
            }
        }
        let mut intro_template = None;
        if has("introTemplate") {
            let t = stored_template(data.get("introTemplate"), &[default_intro_template()], false);
            let current = row.panel.intro_template.as_ref().filter(|v| truthy(Some(v))).map_or(Value::Null, to_json);
            if !same_json(&t, &current) {
                patch.push(("introTemplate", t.clone()));
            }
            intro_template = Some(t);
        }

        // Roles, emojis and the ping rule need the server's roles and emojis.
        let need_resources = has("managerRoleIds") || has("options") || in_patch(&patch, "introTemplate");
        let resources = if need_resources {
            let opts = data.get("options").filter(|v| !v.is_null()).map(|v| list_of(Some(v))).unwrap_or_default();
            Some(resources_for(ctx, guild_id, &opts).await?)
        } else {
            None
        };
        let mut managers_added: Vec<String> = Vec::new();
        let mut managers_removed: Vec<String> = Vec::new();
        let mut managers = row.panel.manager_role_ids.clone();
        if has("managerRoleIds")
            && let Some(res) = &resources
        {
            managers = live_managers(&strings(data.get("managerRoleIds")), res);
            let live = live_managers(&row.panel.manager_role_ids, res);
            managers_added = managers.iter().filter(|r| !row.panel.manager_role_ids.contains(r)).cloned().collect();
            managers_removed = live.iter().filter(|r| !managers.contains(r)).cloned().collect();
            if managers.len() != row.panel.manager_role_ids.len() || !managers_added.is_empty() {
                patch.push(("managerRoleIds", json!(managers)));
            }
        }
        let mut options: Option<Vec<Value>> = None;
        if has("options") {
            let raw = list_of(data.get("options"));
            let normalized = normalize_options(&raw, resources.as_deref(), &custom_ids(&row.options))?;
            assert_usable_emojis(&normalized)?;
            options = Some(normalized);
        }

        // {managers} in an intro: the roles it pings must be pingable by the admin (when the intros or the roles change).
        let mut intros_now = vec![intro_template.clone().unwrap_or_else(|| row.panel.intro_template.clone().unwrap_or(Value::Null))];
        match &options {
            Some(o) => intros_now.extend(o.iter().map(|x| x.get("introTemplate").cloned().unwrap_or(Value::Null))),
            None => intros_now.extend(row.options.iter().map(|x| x.intro_template.clone().unwrap_or(Value::Null))),
        }
        let intro_in_patch = in_patch(&patch, "introTemplate");
        if intro_in_patch || options.is_some() || !managers_added.is_empty() {
            let ids = if intro_in_patch || options.is_some() { &managers } else { &managers_added };
            assert_managers_pingable(ctx, resources.as_deref(), ids, &intros_now)?;
        }

        // The bot must be able to use any category or transcript channel that is new.
        let mut had: HashSet<String> = HashSet::new();
        for c in [&row.panel.open_category_id, &row.panel.claimed_category_id, &row.panel.closed_category_id].into_iter().flatten() {
            if !c.is_empty() {
                had.insert(c.clone());
            }
        }
        for o in &row.options {
            if let Some(c) = o.category_id.as_ref().filter(|c| !c.is_empty()) {
                had.insert(c.clone());
            }
        }
        let mut wanted: Vec<Value> = ["openCategoryId", "claimedCategoryId", "closedCategoryId"]
            .iter()
            .map(|k| patched(&patch, k).unwrap_or(Value::Null))
            .collect();
        if let Some(o) = &options {
            wanted.extend(o.iter().map(|x| x.get("categoryId").cloned().unwrap_or(Value::Null)));
        }
        let wanted: Vec<Value> = wanted.into_iter().filter(|c| truthy(Some(c)) && !had.contains(&js_string(c))).collect();
        let transcript = patched(&patch, "transcriptChannelId")
            .filter(|t| truthy(Some(t)) && t.as_str() != row.panel.transcript_channel_id.as_deref())
            .unwrap_or(Value::Null);
        precheck(ctx, &wanted, Some(&transcript)).await?;

        let publish_to = data
            .get("channelId")
            .filter(|c| truthy(Some(c)) && c.as_str() != row.panel.channel_id.as_deref())
            .cloned();

        if !patch.is_empty() {
            match ts::update_panel_fields(&self.app, id, guild_id, &patch).await.oi()? {
                Ok(()) => {}
                Err(fail) if fail.code == "NAME_TAKEN" => {
                    let name = patched(&patch, "name").map(|v| js_string(&v)).unwrap_or_else(|| "undefined".into());
                    return Err(rec_at(RecordErrorCode::Conflict, format!("A panel called \"{name}\" already exists."), "name"));
                }
                Err(_) => return Err(not_found().into()),
            }
        }
        let mut plan = None;
        if let Some(o) = options {
            plan = Some(self.replace_options(guild_id, id, o).await?);
        }
        log_info(format!("Dashboard ticket panel {id} updated in {guild_id} by {}", ctx.user_id));

        // The posted message follows the style, the panel message and the options; a move re-posts it anyway.
        let published = row.panel.channel_id.as_deref().is_some_and(|c| !c.is_empty()) && row.panel.message_id.as_deref().is_some_and(|m| !m.is_empty());
        let refresh_message = published
            && publish_to.is_none()
            && (in_patch(&patch, "style") || in_patch(&patch, "template") || plan.as_ref().is_some_and(shows_on_panel));
        let managers_changed = !managers_added.is_empty() || !managers_removed.is_empty();
        let mut notes: Vec<String> = Vec::new();
        if let Some(channel) = publish_to {
            let out = run_after_save(
                ctx,
                "ticket.publish",
                json!({ "panelId": id, "channelId": channel }),
                "Your changes were saved, but the panel was not published in the new channel:",
                "Use \"Publish\" to try again.",
            )
            .await?;
            notes.push(out.message);
        }
        if refresh_message || managers_changed {
            let out = run_after_save(
                ctx,
                "ticket.refresh",
                json!({ "panelId": id, "message": refresh_message, "managersAdded": managers_added, "managersRemoved": managers_removed }),
                "Your changes were saved, but the published panel was not fully updated:",
                "Use \"Update posted message\" to try again.",
            )
            .await?;
            notes.push(out.message);
        }
        let note: Vec<String> = notes.into_iter().filter(|n| !n.is_empty()).collect();
        Ok(Saved::with_message(self.get_item(guild_id, id).await?.unwrap_or(Value::Null), Some(note.join(" "))))
    }

    async fn remove(&self, guild_id: &str, id: &str, ctx: &StoreCtx) -> R<()> {
        let row = self.load_row(guild_id, id).await?.ok_or_else(not_found)?;
        let published = row.panel.channel_id.as_deref().is_some_and(|c| !c.is_empty()) && row.panel.message_id.as_deref().is_some_and(|m| !m.is_empty());
        if published {
            require_tasks(ctx)?;
            run_task(ctx, "ticket.delete", json!({ "panelId": id })).await?;
        } else {
            // Nothing is published: there is no message to delete, so the bot is not needed. Open tickets keep their channels.
            bot_db::delete_where(&TICKET_PANEL_INFO, [("id", json!(id)), ("guildId", json!(guild_id))])
                .oi()?
                .execute(self.app.db())
                .await
                .oi()?;
            ts::invalidate_panel(&self.app, id);
        }
        log_info(format!("Dashboard ticket panel {id} deleted in {guild_id} by {}", ctx.user_id));
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["publish", "refresh"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> R<ActionAnswer> {
        let (kind, payload) = match action_id {
            // Posts the panel message in a channel (or moves / updates it there): `{ channelId }`.
            "publish" => {
                let mut payload = json!({ "panelId": id });
                if let Some(c) = data.get("channelId") {
                    payload["channelId"] = c.clone();
                }
                ("ticket.publish", payload)
            }
            // Applies the saved panel to the published message again.
            "refresh" => ("ticket.refresh", json!({ "panelId": id })),
            _ => return Err(RecordError::not_found("Unknown action.").into()),
        };
        require_tasks(ctx)?;
        if self.load_row(guild_id, id).await?.is_none() {
            return Err(not_found().into());
        }
        let out = run_task(ctx, kind, payload).await?;
        Ok(ActionAnswer { item: self.get_item(guild_id, id).await?, message: Some(out.message) })
    }
}
