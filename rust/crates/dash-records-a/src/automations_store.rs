//! Port of `src/dashboard/collections/automations.store.js`: the `automations` collection, the Automation rows
//! /automation writes, through automationService's Discord-free writes (create_automation, mutate_automation,
//! delete_automation), which also drop the automation cache in Redis (`guild:{g}:automations`). What /automation checks
//! against the server is checked here too: saved embeds exist, reward modules are on, members exist (and are not bots
//! where the command refuses them), a role mention in a message is one the user may ping, and "Clicked this button"
//! points at a real button.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bot_config::tables::limits;
use bot_core::App;
use bot_core::rewards::RewardKind;
use bot_core::settings::is_module_enabled;
use bot_db::{Automation, GuildSettings};
use dash_collections::collections::automations::{
    NEEDS_ACTIONS, SELF_REF_ID, action_rule, action_to_form, action_to_stored, automation_item, condition_to_stored, plan_buttons, problem_key,
};
use dash_collections::collections::embeds::{member_for, ping_role_problem};
use dash_collections::{
    ActionAnswer, ChannelCheck, DashboardDiscord, DiscordApiError, DiscordMethod, DiscordResources, ListAnswer, ListOpts, ReadCtx, RecordError, RecordErrorCode,
    RecordStore, Saved, StoreCtx, StoreError, require_channel_access,
};
use feat_automations::automation_service::{
    self as svc, AutomationInput, AutomationRecord, ErrorCode, ListKind, Mutation, PresetOptions,
};
use feat_embeds::embed_service::{extract_role_mention_ids, get_saved_embed_template};
use serde_json::{Map, Value, json};

use crate::common::internal;

/// The `automations` store.
pub struct AutomationsStore {
    app: App,
}

impl AutomationsStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

fn not_found() -> RecordError {
    RecordError::not_found("That automation no longer exists.")
}

fn stale() -> RecordError {
    RecordError::new(RecordErrorCode::Conflict, "Someone else changed this automation meanwhile. Reload it and try again.")
}

fn as_list(v: Option<&Value>) -> Vec<Value> {
    v.and_then(Value::as_array).cloned().unwrap_or_default()
}

fn js(v: Option<&Value>) -> String {
    dash_schema::js::to_js_string(v)
}

fn type_of(v: &Value) -> Option<&str> {
    v.get("type").and_then(Value::as_str)
}

/// `hydrate(row)` as JSON (the row with normalised arrays).
fn hydrate(row: Automation) -> AutomationRecord {
    AutomationRecord::hydrate(row)
}

/// `automationButtons(actions)`: the automation-button numbers of a stored action list.
fn automation_buttons(actions: &[Value]) -> Vec<f64> {
    actions
        .iter()
        .filter_map(|a| {
            let b = a.get("button").filter(|b| dash_schema::js::truthy(Some(b)))?;
            let url_null = matches!(b.get("url"), None | Some(Value::Null));
            let idx = b.get("index").filter(|i| dash_schema::js::is_integer(i))?;
            url_null.then(|| idx.as_f64()).flatten()
        })
        .collect()
}

/// One automation of the server: its name and its button numbers.
#[derive(Debug, Clone)]
struct Ref {
    name: String,
    buttons: Vec<f64>,
}

type Refs = indexmap::IndexMap<String, Ref>;

/// `loadRefs(guildId, rows)`: every automation of the server, id -> { name, buttons }.
async fn load_refs(app: &App, guild_id: &str, rows: Option<&[AutomationRecord]>) -> Result<Refs, StoreError> {
    let mut out = Refs::new();
    match rows {
        Some(rows) => {
            for r in rows {
                out.insert(r.id.clone(), Ref { name: r.name.clone(), buttons: automation_buttons(&r.actions) });
            }
        }
        None => {
            let list: Vec<(String, String, Value)> =
                sqlx::query_as(r#"SELECT "id", "name", "actions" FROM "Automation" WHERE "guildId" = $1 ORDER BY "createdAt" ASC LIMIT $2"#)
                    .bind(guild_id)
                    .bind(limits::AUTOMATIONS as i64)
                    .fetch_all(app.db())
                    .await
                    .map_err(internal)?;
            for (id, name, actions) in list {
                let items = svc::normalize_list(&actions, ListKind::Action, false).items;
                out.insert(id, Ref { name, buttons: automation_buttons(&items) });
            }
        }
    }
    Ok(out)
}

// ── Problems (automationService.automationProblems, without the gateway client) ──

fn reward_label(kind: &str) -> &'static str {
    if kind == "xp" { "XP" } else { "Coin" }
}

fn reward_module_label(kind: &str) -> &'static str {
    if kind == "xp" { "Levels" } else { "Economy" }
}

fn reward_enabled(settings: Option<&GuildSettings>, kind: &str) -> bool {
    let module = if kind == "xp" { RewardKind::Xp.module() } else { RewardKind::Coins.module() };
    is_module_enabled(settings, Some(module))
}

struct ProblemCtx {
    settings: Option<bot_core::settings::Settings>,
    embeds: HashSet<String>,
    last: HashMap<String, Value>,
}

/// `problemContext(guildId, rows, refs)`: settings, saved embeds that exist, the last runtime problems.
async fn problem_context(app: &App, guild_id: &str, rows: &[AutomationRecord]) -> Result<ProblemCtx, StoreError> {
    let mut names: Vec<Value> = Vec::new();
    for r in rows {
        for a in &r.actions {
            if type_of(a) == Some("SEND_EMBED") {
                let n = a.get("embedName").cloned().unwrap_or(Value::Null);
                if !names.contains(&n) {
                    names.push(n);
                }
            }
        }
    }
    let settings = app.settings().get_settings_safe(guild_id).await;
    let mut embeds = HashSet::new();
    if !names.is_empty() {
        let wanted: Vec<String> = names.iter().filter_map(|n| n.as_str().map(str::to_string)).collect();
        let found: Vec<(String,)> = sqlx::query_as(r#"SELECT "name" FROM "SavedEmbed" WHERE "guildId" = $1 AND "name" = ANY($2)"#)
            .bind(guild_id)
            .bind(&wanted)
            .fetch_all(app.db())
            .await
            .map_err(internal)?;
        embeds = found.into_iter().map(|(n,)| n).collect();
    }
    let mut last = HashMap::new();
    for r in rows {
        if let Some(v) = app.cache().get(&problem_key(&r.id)).await {
            last.insert(r.id.clone(), v);
        }
    }
    Ok(ProblemCtx { settings, embeds, last })
}

/// `storedProblems(row, pc)`: the problems of one row that the database knows about.
fn stored_problems(row: &AutomationRecord, pc: &ProblemCtx, refs: &Refs) -> Vec<String> {
    let mut out = Vec::new();
    for inv in &row.invalid_conditions {
        out.push(format!("Condition {} is invalid and ignored: {}", inv.index, inv.error));
    }
    for inv in &row.invalid_actions {
        out.push(format!("Action {} is invalid and ignored: {}", inv.index, inv.error));
    }
    if row.enabled && row.actions.is_empty() {
        out.push("It is on but has no actions, so it never runs.".into());
    }
    for (k, c) in row.conditions.iter().enumerate() {
        for b in as_list(c.get("buttons")) {
            let aid = b.get("automationId").and_then(Value::as_str).unwrap_or("");
            match refs.get(aid) {
                None => out.push(format!("Condition {}: refers to a button of an automation that no longer exists.", k + 1)),
                Some(r) => {
                    if !b.get("index").and_then(Value::as_f64).is_some_and(|i| r.buttons.contains(&i)) {
                        out.push(format!("Condition {}: button {} no longer exists on \"{}\".", k + 1, js(b.get("index")), r.name));
                    }
                }
            }
        }
    }
    for (k, a) in row.actions.iter().enumerate() {
        if type_of(a) == Some("SEND_EMBED") && !a.get("embedName").and_then(Value::as_str).is_some_and(|n| pc.embeds.contains(n)) {
            out.push(format!("Action {}: the saved embed \"{}\" was not found.", k + 1, js(a.get("embedName"))));
        }
        if let Some(kind) = type_of(a).and_then(action_rule).and_then(|r| r.reward)
            && let Some(s) = &pc.settings
            && !reward_enabled(Some(s), kind)
        {
            out.push(format!("Action {}: {} rewards need the {} module.", k + 1, reward_label(kind), reward_module_label(kind)));
        }
    }
    if let Some(last) = pc.last.get(&row.id)
        && let Some(text) = last.get("text").and_then(Value::as_str)
    {
        let at = match last.get("at").and_then(Value::as_str).map(bot_util::js::date_parse).filter(|t| t.is_finite()).and_then(bot_util::js::to_iso_string) {
            Some(iso) => format!(" ({} UTC)", iso[..16].replace('T', " ")),
            None => String::new(),
        };
        out.push(format!("Last problem{at}: {text}"));
    }
    out
}

/// `discordProblems(row, res)`: deleted channels and roles, and roles the bot can no longer give.
fn discord_problems(row: &AutomationRecord, res: &DiscordResources) -> Vec<String> {
    let mut out = Vec::new();
    let channel_gone = |out: &mut Vec<String>, label: &str, id: &str| {
        if res.channel(id).is_some() {
            return false;
        }
        out.push(format!("{label}: the channel {id} was not found (deleted, or a thread the dashboard cannot see)."));
        true
    };
    let role_gone = |out: &mut Vec<String>, label: &str, id: &str| {
        if res.role(id).is_some() {
            return false;
        }
        out.push(format!("{label}: the role {id} was deleted."));
        true
    };
    for (k, c) in row.conditions.iter().enumerate() {
        let label = format!("Condition {}", k + 1);
        for id in as_list(c.get("channelIds")).iter().chain(as_list(c.get("categoryIds")).iter()) {
            channel_gone(&mut out, &label, &js(Some(id)));
        }
        for id in as_list(c.get("roleIds")) {
            role_gone(&mut out, &label, &js(Some(&id)));
        }
    }
    for (k, a) in row.actions.iter().enumerate() {
        let label = format!("Action {}", k + 1);
        if let Some(ch) = a.get("channelId").filter(|v| dash_schema::js::truthy(Some(v))) {
            channel_gone(&mut out, &label, &js(Some(ch)));
        }
        for id in as_list(a.get("roleIds")) {
            let id = js(Some(&id));
            if role_gone(&mut out, &label, &id) {
                continue;
            }
            let Some(role) = res.role(&id) else { continue };
            // checkAssignableRole without an executor, in the bot's words.
            if role.managed {
                out.push(format!("{label}: the role @{} is managed by an integration.", role.name));
            } else if !res.bot.manage_roles {
                out.push(format!("{label}: the bot needs the Manage Roles permission."));
            } else if role.assignable != Some(true) {
                out.push(format!("{label}: the role @{} is above the bot's highest role.", role.name));
            } else if role.dangerous == Some(true) {
                out.push(format!("{label}: the role @{} has moderator or admin permissions, so the bot will not give it.", role.name));
            }
        }
    }
    out
}

/// `creatorNames(rows, ctx)`: display names of the creators; ids stay ids when Discord cannot answer.
async fn creator_names(rows: &[AutomationRecord], discord: Option<&Arc<dyn DashboardDiscord>>) -> HashMap<String, String> {
    let Some(discord) = crate::common::discord_with(discord, DiscordMethod::GetUsers) else { return HashMap::new() };
    let mut ids: Vec<String> = Vec::new();
    for r in rows {
        if !ids.contains(&r.created_by) {
            ids.push(r.created_by.clone());
        }
    }
    let mut out = HashMap::new();
    if let Ok(users) = discord.get_users(&ids).await {
        for (id, u) in users {
            let Some(u) = u.filter(|u| !u.username.is_empty()) else { continue };
            let name = match u.global_name.as_deref().filter(|g| !g.is_empty() && *g != u.username) {
                Some(g) => format!("{g} (@{})", u.username),
                None => format!("@{}", u.username),
            };
            out.insert(id, name);
        }
    }
    out
}

/// `items(guildId, rows, refs, ctx)`.
async fn items(app: &App, guild_id: &str, rows: &[AutomationRecord], refs: &Refs, discord: Option<&Arc<dyn DashboardDiscord>>) -> Result<Vec<Value>, StoreError> {
    let pc = problem_context(app, guild_id, rows).await?;
    let creators = creator_names(rows, discord).await;
    let name_of = |id: &Value| id.as_str().and_then(|i| refs.get(i)).map(|r| r.name.clone());
    Ok(rows
        .iter()
        .map(|row| automation_item(&row.to_json(), &name_of, &stored_problems(row, &pc, refs), creators.get(&row.created_by).map(String::as_str)))
        .collect())
}

async fn find_row(app: &App, guild_id: &str, id: &str) -> Result<Option<Automation>, StoreError> {
    sqlx::query_as::<_, Automation>(r#"SELECT * FROM "Automation" WHERE "id" = $1 AND "guildId" = $2 LIMIT 1"#)
        .bind(id)
        .bind(guild_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)
}

/// `getItem(guildId, id, ctx, extraProblems)`.
async fn get_item(app: &App, guild_id: &str, id: &str, discord: Option<&Arc<dyn DashboardDiscord>>, extra: Vec<String>) -> Result<Option<Value>, StoreError> {
    let Some(row) = find_row(app, guild_id, id).await? else { return Ok(None) };
    let refs = load_refs(app, guild_id, None).await?;
    let mut item = items(app, guild_id, &[hydrate(row)], &refs, discord).await?.remove(0);
    if !extra.is_empty() {
        let mut problems: Vec<Value> = extra.into_iter().map(Value::String).collect();
        problems.extend(as_list(item.get("problems")));
        item["problems"] = Value::Array(problems);
    }
    Ok(Some(item))
}

// ── Checks against the server (the /automation add-time checks) ──────────────

/// `needDiscord(ctx, what)`: the client, or unavailable when there is none or it lacks `method`.
fn need_discord(ctx: &StoreCtx, method: DiscordMethod) -> Result<&Arc<dyn DashboardDiscord>, StoreError> {
    crate::common::discord_with(ctx.discord.as_ref(), method).ok_or_else(|| RecordError::unavailable("This cannot be checked with Discord from the dashboard right now.").into())
}

fn discord_call(what: &str) -> impl FnOnce(DiscordApiError) -> StoreError + '_ {
    move |_| RecordError::discord(format!("{what} could not be checked with Discord. Try again in a moment.")).into()
}

/// `checkMember(guildId, ctx, userId, path, { people })`: a member of the server (and a person, when `people`).
async fn check_member(guild_id: &str, ctx: &StoreCtx, user_id: &str, path: &str, people: Option<&str>) -> Result<(), StoreError> {
    let discord = need_discord(ctx, DiscordMethod::SearchMembers)?;
    let found = discord.search_members(guild_id, user_id).await.map_err(discord_call("Members"))?;
    let Some(member) = found.iter().find(|m| m.id == user_id) else {
        return Err(RecordError::invalid("That user is not in this server.").with_path(path).into());
    };
    if let Some(p) = people
        && member.bot
    {
        return Err(RecordError::invalid(p).with_path(path).into());
    }
    Ok(())
}

async fn guild_resources(ctx: &StoreCtx, guild_id: &str, what: &str, fresh: bool) -> Result<Arc<DiscordResources>, StoreError> {
    let discord = need_discord(ctx, DiscordMethod::GetGuildResources)?;
    discord.get_guild_resources(guild_id, fresh).await.map_err(discord_call(what))
}

/// `checkRolePings(guildId, ctx, texts, { keep, resources })`: a `<@&id>` typed in a message pings only when the role is
/// mentionable or the user may mention every role. Refuses the first role that fails.
async fn check_role_pings(
    guild_id: &str,
    ctx: &StoreCtx,
    texts: &[(String, Option<String>)],
    keep: &HashSet<String>,
    resources: Option<Arc<DiscordResources>>,
) -> Result<(), StoreError> {
    let pings: Vec<(String, String)> = texts
        .iter()
        .flat_map(|(path, text)| extract_role_mention_ids(text.as_deref()).into_iter().filter(|id| !keep.contains(id)).map(move |id| (path.clone(), id)))
        .collect();
    if pings.is_empty() {
        return Ok(());
    }
    let res = match resources {
        Some(r) => r,
        None => guild_resources(ctx, guild_id, "The roles", false).await?,
    };
    let member = member_for(ctx, Some(&res));
    for (path, id) in pings {
        if let Some(problem) = ping_role_problem(Some(&res), &id, member.as_ref()) {
            return Err(RecordError::invalid(problem).with_path(path).into());
        }
    }
    Ok(())
}

fn text_of(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => Some(js(Some(v))),
    }
}

/// The stored conditions and actions of an automation (`before`), or empty lists on create.
#[derive(Debug, Clone, Default)]
pub struct Before {
    pub conditions: Vec<Value>,
    pub actions: Vec<Value>,
}

impl Before {
    fn of(row: &AutomationRecord) -> Self {
        Self { conditions: row.conditions.clone(), actions: row.actions.clone() }
    }
}

/// `checkAgainstServer(guildId, ctx, { conditions, actions, before })`: the checks /automation action add and condition
/// add make against the server, for what the form adds (entries the stored automation already had are kept).
pub async fn check_against_server(
    app: &App,
    guild_id: &str,
    ctx: &StoreCtx,
    conditions: Option<&[Value]>,
    actions: Option<&[Value]>,
    before: &Before,
) -> Result<(), StoreError> {
    let had = |list: &[Value], f: &dyn Fn(&Value) -> Vec<String>| -> HashSet<String> { list.iter().flat_map(f).collect() };
    let old_embeds = had(&before.actions, &|a| if type_of(a) == Some("SEND_EMBED") { vec![js(a.get("embedName"))] } else { vec![] });
    let old_rewards = had(&before.actions, &|a| {
        if type_of(a).and_then(action_rule).is_some_and(|r| r.reward.is_some()) { vec![format!("{}:{}", js(a.get("type")), js(a.get("amount")))] } else { vec![] }
    });
    let old_thread_users =
        had(&before.actions, &|a| if type_of(a) == Some("ADD_TO_THREAD") { as_list(a.get("userIds")).iter().map(|v| js(Some(v))).collect() } else { vec![] });
    let old_users =
        had(&before.conditions, &|c| if type_of(c) == Some("USER_IS") { as_list(c.get("userIds")).iter().map(|v| js(Some(v))).collect() } else { vec![] });
    let old_pings = had(&before.actions, &|a| {
        if type_of(a).and_then(action_rule).is_some_and(|r| r.button) { extract_role_mention_ids(a.get("content").and_then(Value::as_str)) } else { vec![] }
    });
    let mut ping_texts: Vec<(String, Option<String>)> = Vec::new();
    let mut settings: Option<Option<bot_core::settings::Settings>> = None;
    for (i, a) in actions.unwrap_or(&[]).iter().enumerate() {
        let at = format!("actions.{i}");
        let rule = type_of(a).and_then(action_rule);
        if rule.is_some_and(|r| r.button) {
            ping_texts.push((format!("{at}.content"), text_of(a.get("content"))));
        }
        if type_of(a) == Some("SEND_EMBED") {
            let name = a.get("embedName").and_then(Value::as_str).and_then(svc::normalize_automation_name);
            let name_text = name.clone().unwrap_or_else(|| "null".into());
            if !name.as_ref().is_some_and(|n| old_embeds.contains(n)) {
                let template = match &name {
                    Some(n) => get_saved_embed_template(app, guild_id, n).await,
                    None => None,
                };
                let Some(template) = template.filter(|t| dash_schema::js::truthy(Some(t))) else {
                    return Err(RecordError::invalid(format!("There is no saved embed called \"{name_text}\". Create it under Embed Messages first."))
                        .with_path(format!("{at}.embedName"))
                        .into());
                };
                ping_texts.push((format!("{at}.embedName"), text_of(template.get("content"))));
            }
        }
        if let Some(kind) = rule.and_then(|r| r.reward)
            && !old_rewards.contains(&format!("{}:{}", js(a.get("type")), js(a.get("amount"))))
        {
            if settings.is_none() {
                settings = Some(Some(app.settings().get_settings(guild_id).await.map_err(internal)?));
            }
            let s = settings.as_ref().and_then(|s| s.as_deref());
            if !reward_enabled(s, kind) {
                return Err(RecordError::invalid(format!("{} rewards need the {} module. Turn it on first.", reward_label(kind), reward_module_label(kind)))
                    .with_path(format!("{at}.type"))
                    .into());
            }
        }
        if type_of(a) == Some("ADD_TO_THREAD") {
            for (k, u) in as_list(a.get("users")).iter().enumerate() {
                let uid = js(u.get("userId"));
                if old_thread_users.contains(&uid) {
                    continue;
                }
                check_member(guild_id, ctx, &uid, &format!("{at}.users.{k}.userId"), Some("Bots cannot be added to threads by automations.")).await?;
            }
        }
    }
    for (i, c) in conditions.unwrap_or(&[]).iter().enumerate() {
        if type_of(c) != Some("USER_IS") {
            continue;
        }
        for (k, u) in as_list(c.get("users")).iter().enumerate() {
            let uid = js(u.get("userId"));
            if !old_users.contains(&uid) {
                check_member(guild_id, ctx, &uid, &format!("conditions.{i}.users.{k}.userId"), None).await?;
            }
        }
    }
    check_role_pings(guild_id, ctx, &ping_texts, &old_pings, None).await
}

/// `checkChannelAccess(ctx, actions, before, { path })`: an automation must not post into (or open threads in) a
/// channel its author cannot use. Only an action the stored automation already had, exactly as it is, is kept without
/// a check.
async fn check_channel_access(ctx: &StoreCtx, actions: &[Value], before: &Before, path: Option<&str>) -> Result<(), StoreError> {
    let key = |v: &Value| serde_json::to_string(v).unwrap_or_default();
    let mut kept: HashMap<String, i64> = HashMap::new();
    for a in &before.actions {
        *kept.entry(key(&action_to_stored(&action_to_form(a), None))).or_insert(0) += 1;
    }
    let mut checks = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        let Some(needs) = type_of(a).and_then(svc::executor_channel_needs) else { continue };
        let thread = type_of(a) == Some("CREATE_THREAD") && path.is_none();
        let channel = if thread { a.get("threadChannelId") } else { a.get("channelId") };
        let Some(channel_id) = channel.filter(|c| dash_schema::js::truthy(Some(c))).map(|c| js(Some(c))) else { continue };
        // isKept(a): each stored action keeps one form action.
        let k = key(&action_to_stored(a, None));
        let is_kept = path.is_none() && kept.get(&k).copied().unwrap_or(0) > 0;
        if is_kept {
            if let Some(n) = kept.get_mut(&k) {
                *n -= 1;
            }
            continue;
        }
        let p = match path {
            Some(p) => p.to_string(),
            None => format!("actions.{i}.{}", if thread { "threadChannelId" } else { "channelId" }),
        };
        checks.push(ChannelCheck::new(channel_id).needs(needs).path(p));
    }
    require_channel_access(ctx, &checks, "").await
}

fn old_button_refs(conditions: &[Value]) -> HashSet<String> {
    conditions
        .iter()
        .filter(|c| type_of(c) == Some("BUTTON_IS"))
        .flat_map(|c| as_list(c.get("buttons")).into_iter().map(|b| format!("{}:{}", js(b.get("automationId")), js(b.get("index")))))
        .collect()
}

/// `resolveConditions(conditions, { refs, selfNames, selfId, before })`: the "Clicked this button" references of the
/// form (an automation name, or an id the stored condition already had) in the stored shape (not normalised yet).
fn resolve_conditions(conditions: &[Value], refs: &Refs, self_names: &[&str], self_id: &str, before: &Before) -> Result<Vec<Value>, StoreError> {
    let mut by_name: HashMap<String, String> = HashMap::new();
    for (id, r) in refs {
        by_name.insert(r.name.clone(), id.clone());
    }
    let old_refs = old_button_refs(&before.conditions);
    let mut out = Vec::new();
    for (i, c) in conditions.iter().enumerate() {
        if type_of(c) != Some("BUTTON_IS") {
            out.push(condition_to_stored(c, &|_| Value::Null));
            continue;
        }
        let mut ids: Vec<String> = Vec::new();
        for (k, b) in as_list(c.get("buttons")).iter().enumerate() {
            let path = format!("conditions.{i}.buttons.{k}");
            let raw = match b.get("automation") {
                None | Some(Value::Null) => String::new(),
                Some(v) => js(Some(v)),
            };
            let text = bot_util::js::trim(&raw).to_string();
            let name = svc::normalize_automation_name(&text);
            let index = js(b.get("index"));
            let id = if let Some(n) = name.as_deref().filter(|n| self_names.contains(n)) {
                let _ = n;
                self_id.to_string()
            } else if let Some(id) = name.as_ref().and_then(|n| by_name.get(n)) {
                id.clone()
            } else if refs.contains_key(&text) || old_refs.contains(&format!("{text}:{index}")) {
                text.clone()
            } else {
                return Err(RecordError::invalid(format!("There is no automation called \"{text}\".")).with_path(format!("{path}.automation")).into());
            };
            let has_button = refs.get(&id).is_some_and(|r| b.get("index").and_then(Value::as_f64).is_some_and(|x| r.buttons.contains(&x)));
            if id != self_id && !old_refs.contains(&format!("{id}:{index}")) && !has_button {
                let shown = refs.get(&id).map_or(text.clone(), |r| r.name.clone());
                return Err(RecordError::invalid(format!("\"{shown}\" has no button number {index}.")).with_path(format!("{path}.index")).into());
            }
            ids.push(id);
        }
        let k = Cell::new(0usize);
        out.push(condition_to_stored(c, &|_| {
            let n = k.get();
            k.set(n + 1);
            ids.get(n).map_or(Value::Null, |s| Value::String(s.clone()))
        }));
    }
    Ok(out)
}

/// `checkSelfButtons(conditions, actions, { selfId, before })`: a self reference must name one of this automation's
/// own automation buttons.
fn check_self_buttons(conditions: &[Value], actions: &[Value], self_id: &str, before_conditions: &[Value]) -> Result<(), RecordError> {
    let own = automation_buttons(actions);
    let old_refs = old_button_refs(before_conditions);
    for (i, c) in conditions.iter().enumerate() {
        for (k, b) in as_list(c.get("buttons")).iter().enumerate() {
            let index = js(b.get("index"));
            if b.get("automationId").and_then(Value::as_str) != Some(self_id)
                || b.get("index").and_then(Value::as_f64).is_some_and(|x| own.contains(&x))
                || old_refs.contains(&format!("{self_id}:{index}"))
            {
                continue;
            }
            return Err(RecordError::invalid(format!("This automation has no button number {index}.")).with_path(format!("conditions.{i}.buttons.{k}.index")));
        }
    }
    Ok(())
}

/// `normalizeEach(list, kind)`: automationService's normalisers one element at a time, so a refusal points at its item.
fn normalize_each(list: &[Value], kind: ListKind) -> Result<Vec<Value>, RecordError> {
    list.iter()
        .enumerate()
        .map(|(i, raw)| {
            let r = match kind {
                ListKind::Condition => svc::normalize_condition(raw, false),
                ListKind::Action => svc::normalize_action(raw),
            };
            r.map_err(|e| {
                let (label, key) = match kind {
                    ListKind::Condition => ("Condition", "conditions"),
                    ListKind::Action => ("Action", "actions"),
                };
                RecordError::invalid(format!("{label} {}: {e}", i + 1)).with_path(format!("{key}.{i}"))
            })
        })
        .collect()
}

/// The stored actions of a form action list with the planned button numbers.
fn stored_actions(actions: &[Value], indexes: &[Option<Value>]) -> Vec<Value> {
    actions
        .iter()
        .enumerate()
        .map(|(i, a)| action_to_stored(a, indexes.get(i).cloned().flatten().and_then(|v| v.as_f64()).map(|f| f as i64)))
        .collect()
}

/// `presetShape(guildId, data, ctx)`: the trigger, conditions and actions of a /automation create preset.
async fn preset_shape(app: &App, guild_id: &str, data: &Map<String, Value>, ctx: &StoreCtx) -> Result<svc::BuiltAutomation, StoreError> {
    let preset = data.get("preset").and_then(Value::as_str).unwrap_or("");
    let str_opt = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_string);
    let mut opts = PresetOptions::default();
    if matches!(preset, "welcomer" | "starboard" | "commands-only") {
        let res = guild_resources(ctx, guild_id, "The channel", false).await?;
        let channel_id = str_opt("presetChannelId");
        if let Some(ch) = channel_id.as_deref().and_then(|c| res.channel(c))
            && !["text", "announcement"].contains(&ch.kind.as_str())
        {
            return Err(RecordError::invalid("Pick a text or announcement channel.").with_path("presetChannelId").into());
        }
        if preset == "welcomer" {
            check_role_pings(guild_id, ctx, &[("presetMessage".into(), text_of(data.get("presetMessage")))], &HashSet::new(), Some(res.clone())).await?;
            opts.channel_id = channel_id;
            opts.message = str_opt("presetMessage").map(|m| bot_util::js::trim(&m).to_string()).filter(|m| !m.is_empty());
            opts.role_id = str_opt("presetRoleId");
        } else if preset == "starboard" {
            opts.channel_id = channel_id;
            opts.emoji = str_opt("presetEmoji");
            opts.threshold = data.get("presetThreshold").and_then(Value::as_i64);
        } else {
            // As /automation create: the prefix and moderator roles as they are now.
            let settings = app.settings().get_settings(guild_id).await.map_err(internal)?;
            opts.channel_id = channel_id;
            opts.prefix = feat_tags::tag_service::effective_prefix(Some(settings.custom_prefix.as_str()));
            let mut exempt: Vec<String> = str_opt("presetStaffRoleId").filter(|s| !s.is_empty()).into_iter().collect();
            exempt.extend(settings.mod_role_ids.iter().cloned());
            opts.exempt_role_ids = exempt;
        }
    } else {
        let user_id = str_opt("presetUserId").unwrap_or_default();
        check_member(guild_id, ctx, &user_id, "presetUserId", Some("Pick a person, not a bot.")).await?;
        opts.user_id = Some(user_id);
        opts.channel_id = str_opt("presetChannelId");
    }
    svc::build_preset_automation(preset, &opts)
        .map_err(|e| RecordError::invalid(e.message).with_path(if preset == "starboard" { "presetEmoji" } else { "preset" }).into())
}

/// `writeFailure(err, name)`: a failed automationService write as the error the admin sees.
fn write_failure(err: anyhow::Error, name: &str) -> StoreError {
    if let Some(r) = err.downcast_ref::<RecordError>() {
        return r.clone().into();
    }
    if svc::is_unique_violation(&err) || crate::common::is_unique(&err) {
        return RecordError::new(RecordErrorCode::Conflict, format!("An automation called \"{name}\" already exists.")).with_path("name").into();
    }
    if let Some(e) = svc::as_automation_error(&err) {
        return match e.code {
            ErrorCode::Limit => RecordError::new(
                RecordErrorCode::Limit,
                format!("This server already has {} automations, the maximum. Delete one first.", limits::AUTOMATIONS),
            )
            .into(),
            ErrorCode::Name => RecordError::invalid(e.message.clone()).with_path("name").into(),
            ErrorCode::Buttons => RecordError::invalid(e.message.clone()).with_path("actions").into(),
            _ => RecordError::invalid(e.message.clone()).into(),
        };
    }
    internal(err)
}

/// `cleanName(raw)`.
fn clean_name(raw: Option<&Value>) -> Result<String, StoreError> {
    let text = raw.and_then(Value::as_str);
    match text.and_then(svc::normalize_automation_name) {
        Some(n) if Some(n.as_str()) == text => Ok(n),
        _ => Err(RecordError::invalid("Automation names can use letters, numbers, - and _ (up to 32 characters).").with_path("name").into()),
    }
}

#[async_trait]
impl RecordStore for AutomationsStore {
    async fn list(&self, guild_id: &str, _opts: &ListOpts, ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let rows: Vec<AutomationRecord> =
            sqlx::query_as::<_, Automation>(r#"SELECT * FROM "Automation" WHERE "guildId" = $1 ORDER BY "createdAt" ASC LIMIT $2"#)
                .bind(guild_id)
                .bind(limits::AUTOMATIONS as i64)
                .fetch_all(self.app.db())
                .await
                .map_err(internal)?
                .into_iter()
                .map(hydrate)
                .collect();
        let refs = load_refs(&self.app, guild_id, Some(&rows)).await?;
        Ok(ListAnswer::items(items(&self.app, guild_id, &rows, &refs, ctx.discord.as_ref()).await?))
    }

    async fn get(&self, guild_id: &str, id: &str, ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        get_item(&self.app, guild_id, id, ctx.discord.as_ref(), Vec::new()).await
    }

    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let app = &self.app;
        let name = clean_name(data.get("name"))?;
        let preset = data.get("preset").filter(|p| dash_schema::js::truthy(Some(p))).and_then(Value::as_str).filter(|p| *p != "none");
        let empty = Before::default();
        let shape = if preset.is_some() {
            let shape = preset_shape(app, guild_id, &data, ctx).await?;
            check_channel_access(ctx, &shape.actions, &empty, Some("presetChannelId")).await?;
            shape
        } else {
            let conditions = as_list(data.get("conditions"));
            let actions = as_list(data.get("actions"));
            let refs = load_refs(app, guild_id, None).await?;
            check_against_server(app, guild_id, ctx, Some(&conditions), Some(&actions), &empty).await?;
            check_channel_access(ctx, &actions, &empty, None).await?;
            let plan = plan_buttons(&actions, &[], Some(&json!(0)), true).map_err(|p| StoreError::from(RecordError::invalid(p.message).with_path(p.path)))?;
            let conditions = normalize_each(&resolve_conditions(&conditions, &refs, &[name.as_str()], SELF_REF_ID, &empty)?, ListKind::Condition)?;
            let actions = normalize_each(&stored_actions(&actions, &plan.indexes), ListKind::Action)?;
            check_self_buttons(&conditions, &actions, SELF_REF_ID, &empty.conditions)?;
            svc::BuiltAutomation { trigger: data.get("trigger").and_then(Value::as_str).unwrap_or("").to_string(), conditions, actions }
        };
        let enabled = dash_schema::js::truthy(data.get("enabled"));
        if enabled && shape.actions.is_empty() {
            return Err(RecordError::invalid(NEEDS_ACTIONS).with_path("enabled").into());
        }
        let input = AutomationInput {
            name: Some(name.clone()),
            trigger: shape.trigger,
            conditions: shape.conditions,
            actions: shape.actions,
            enabled,
            cooldown_seconds: data.get("cooldownSeconds").and_then(Value::as_i64).unwrap_or(0),
        };
        let created_by = if ctx.user_id.is_empty() { "0".to_string() } else { ctx.user_id.clone() };
        let row = svc::create_automation(app, guild_id, &input, &created_by, Some(SELF_REF_ID)).await.map_err(|e| write_failure(e, &name))?;
        let item = match get_item(app, guild_id, &row.id, ctx.discord.as_ref(), Vec::new()).await? {
            Some(item) => item,
            None => automation_item(&row.to_json(), &|_| None, &[], None),
        };
        Ok(Saved::item(item))
    }

    async fn update(&self, guild_id: &str, id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let app = &self.app;
        let Some(row) = find_row(app, guild_id, id).await? else { return Err(not_found().into()) };
        let row_updated_ms = row.updated_at.timestamp_millis();
        let row_name = row.name.clone();
        let before_rec = hydrate(row);
        let before = Before::of(&before_rec);
        let name = if data.contains_key("name") { clean_name(data.get("name"))? } else { row_name.clone() };
        let has_conditions = data.contains_key("conditions");
        let has_actions = data.contains_key("actions");
        let form_conditions = as_list(data.get("conditions"));
        let form_actions = as_list(data.get("actions"));
        if has_conditions || has_actions {
            check_against_server(
                app,
                guild_id,
                ctx,
                has_conditions.then_some(form_conditions.as_slice()),
                has_actions.then_some(form_actions.as_slice()),
                &before,
            )
            .await?;
            if has_actions {
                check_channel_access(ctx, &form_actions, &before, None).await?;
            }
        }
        let mut conditions: Option<Vec<Value>> = None;
        if has_conditions {
            let refs = load_refs(app, guild_id, None).await?;
            conditions = Some(normalize_each(&resolve_conditions(&form_conditions, &refs, &[row_name.as_str(), name.as_str()], id, &before)?, ListKind::Condition)?);
        }
        let (id_s, name_c, data_c) = (id.to_string(), name.clone(), data.clone());
        let mutator = move |cur: &AutomationRecord| -> anyhow::Result<Mutation<()>> {
            // Optimistic, like the other dashboard lists: the form was filled from `row`, so a concurrent change is reported.
            if cur.id != id_s || cur.updated_at.timestamp_millis() != row_updated_ms {
                return Err(stale().into());
            }
            let mut out: Vec<(&'static str, Value)> = Vec::new();
            if name_c != cur.name {
                out.push(("name", json!(name_c)));
            }
            if let Some(t) = data_c.get("trigger")
                && !dash_schema::js::strict_eq(t, &json!(cur.trigger))
            {
                out.push(("trigger", t.clone()));
            }
            if let Some(c) = data_c.get("cooldownSeconds")
                && !dash_schema::js::strict_eq(c, &json!(cur.cooldown_seconds))
            {
                out.push(("cooldownSeconds", c.clone()));
            }
            let mut actions = cur.actions.clone();
            if has_actions {
                let existing: Vec<Value> = automation_buttons(&cur.actions).into_iter().map(dash_schema::js::js_num).collect();
                let plan = plan_buttons(&form_actions, &existing, Some(&json!(cur.button_seq)), true)
                    .map_err(|p| anyhow::Error::from(RecordError::invalid(p.message).with_path(p.path)))?;
                actions = normalize_each(&stored_actions(&form_actions, &plan.indexes), ListKind::Action)?;
                out.push(("actions", Value::Array(actions.clone())));
                if plan.button_seq > cur.button_seq as f64 {
                    out.push(("buttonSeq", dash_schema::js::js_num(plan.button_seq)));
                }
            }
            if let Some(c) = &conditions {
                out.push(("conditions", Value::Array(c.clone())));
            }
            check_self_buttons(conditions.as_deref().unwrap_or(&cur.conditions), &actions, &id_s, &cur.conditions)?;
            // On needs an action (/automation enable); emptying the actions turns it off (/automation action remove).
            let enabled = match data_c.get("enabled") {
                Some(v) => dash_schema::js::truthy(Some(v)),
                None => cur.enabled,
            };
            let enabled_value = data_c.get("enabled").cloned().unwrap_or(json!(cur.enabled));
            if enabled && actions.is_empty() {
                if data_c.get("enabled") == Some(&Value::Bool(true)) {
                    return Err(RecordError::invalid(NEEDS_ACTIONS).with_path("enabled").into());
                }
                if cur.enabled {
                    out.push(("enabled", json!(false)));
                }
            } else if !dash_schema::js::strict_eq(&enabled_value, &json!(cur.enabled)) {
                out.push(("enabled", enabled_value));
            }
            Ok(Mutation::new(out, ()))
        };
        let saved = match svc::mutate_automation(app, guild_id, &row_name, mutator).await {
            Ok((saved, ())) => saved,
            Err(err) => {
                if svc::as_automation_error(&err).is_some_and(|e| e.code == ErrorCode::NotFound) {
                    // Renamed or deleted since it was read.
                    if find_row(app, guild_id, id).await?.is_some() {
                        return Err(stale().into());
                    }
                    return Err(not_found().into());
                }
                return Err(write_failure(err, &name));
            }
        };
        let item = match get_item(app, guild_id, id, ctx.discord.as_ref(), Vec::new()).await? {
            Some(item) => item,
            None => automation_item(&saved.to_json(), &|_| None, &[], None),
        };
        Ok(Saved::item(item))
    }

    async fn remove(&self, guild_id: &str, id: &str, _ctx: &StoreCtx) -> Result<(), StoreError> {
        // As /automation delete: buttons it already posted stop doing anything unless another automation listens for them.
        if svc::delete_automation(&self.app, guild_id, id).await.map_err(internal)? == 0 {
            return Err(not_found().into());
        }
        Ok(())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["review"]
    }

    /// /automation info's problem list, with the channel and role checks done against Discord.
    async fn action(&self, guild_id: &str, id: &str, action_id: &str, _data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        if action_id != "review" {
            return Err(RecordError::not_found("Unknown action.").into());
        }
        let app = &self.app;
        let Some(row) = find_row(app, guild_id, id).await? else { return Err(not_found().into()) };
        let res = guild_resources(ctx, guild_id, "Channels and roles", true).await?;
        let extra = discord_problems(&hydrate(row), &res);
        let Some(item) = get_item(app, guild_id, id, ctx.discord.as_ref(), extra).await? else { return Err(not_found().into()) };
        let problems: Vec<String> = as_list(item.get("problems")).iter().map(|p| js(Some(p))).collect();
        let n = problems.len();
        let message = if n > 0 {
            bot_util::js::utf16_prefix(&format!("{n} problem{}: {}", if n == 1 { "" } else { "s" }, problems.join(" ")), 1500).to_string()
        } else {
            "No problems found.".to_string()
        };
        Ok(ActionAnswer { item: Some(item), message: Some(message) })
    }
}
