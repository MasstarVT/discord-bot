//! Port of `src/services/roleTemplateSetup.js`: finds or creates a template's roles (He/Him, Red, CST, ...) and, for a
//! timezone template, links each one to its zone in `GuildSettings.timezoneRoles`, so a Unique reaction-role panel of
//! them sets `/timestamp`'s timezone. Shared by `/reactionrole create template:<key>` and the dashboard's
//! `rr.roleTemplate` task. Everything is checked before anything is created; roles are reused by name, so running it
//! again after a failure is safe.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bot_core::App;
use bot_core::settings::SettingsPatch;
use bot_util::timezones::{TIMEZONE_ROLE_TEMPLATES, TIMEZONE_TEMPLATE_PAST_ZONES, TimezoneRole, normalize_timezone_roles, timezone_role_template};
use serde_json::{Value, json};
use serenity::all::Member;

use crate::reaction_role_service::{
    NewItem, Panel, PanelArg, RoleInfo, RrDiscord, SyncResult, discord_ops, insert_items, invalidate_panel_index, list_unique_panels, max_items,
    sync_panel,
};
use crate::role_templates::get_role_template;

const CREATE_TIMEOUT_MS: u64 = 8_000;
/// Discord's role cap per server (counting @everyone here, to stay on the safe side).
pub const DISCORD_ROLE_CAP: usize = 250;
const TIMEZONE_ROLES_MAX: usize = bot_config::constants::limits::TIMEZONE_ROLES;

/// A refusal with a message for the admin (`RoleTemplateError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleTemplateError(pub String);

impl std::fmt::Display for RoleTemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RoleTemplateError {}

/// `findRoleByName(guild, name)`: the guild's role of that name, ignoring case: not @everyone, not managed by an
/// integration. The highest one when several share the name.
pub fn find_role_by_name<'a>(roles: &'a [RoleInfo], guild_id: &str, name: &str) -> Option<&'a RoleInfo> {
    let lower = name.to_lowercase();
    let mut best: Option<&RoleInfo> = None;
    for role in roles {
        if role.id == guild_id || role.managed || role.name.to_lowercase() != lower {
            continue;
        }
        if best.is_none_or(|b| role.position > b.position) {
            best = Some(role);
        }
    }
    best
}

/// One role of [`SetupResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupRole {
    pub role_id: String,
    pub name: String,
    pub label: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub zone: Option<String>,
    pub created: bool,
}

/// The result of [`setup_role_template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupResult {
    pub roles: Vec<SetupRole>,
    pub created: usize,
    pub reused: usize,
}

/// The `guild.roles.create` body of a template role: no permissions, not mentionable, not hoisted, and its color.
pub fn create_role_body(name: &str, color: u32) -> Value {
    let mut body = json!({ "name": name, "permissions": "0", "mentionable": false, "hoist": false });
    if color != 0 {
        body["colors"] = json!({ "primary_color": color });
    }
    body
}

async fn create_role(ops: Arc<dyn RrDiscord>, guild_id: &str, body: Value, reason: &str) -> Result<RoleInfo, String> {
    let (g, r) = (guild_id.to_string(), reason.to_string());
    match bot_util::async_util::with_timeout(async move { ops.create_role(&g, &body, &r).await }, CREATE_TIMEOUT_MS, "create template role").await {
        Ok(role) => Ok(role),
        Err(e) => Err(e.to_string()),
    }
}

fn links_of(raw: Option<&Value>) -> Vec<TimezoneRole> {
    normalize_timezone_roles(raw.unwrap_or(&Value::Null))
}

fn links_json(links: &[TimezoneRole]) -> Value {
    Value::Array(links.iter().map(|l| json!({ "roleId": l.role_id, "zone": l.zone })).collect())
}

/// `setupRoleTemplate(guild, templateKey, { executor, reason })`: finds or creates the template's roles, and links
/// them to their zones for a timezone template. Fails with a [`RoleTemplateError`] when a check fails (nothing was
/// created then) or Discord refuses a role.
pub async fn setup_role_template(app: &App, guild_id: &str, template_key: &str, executor: Option<&Member>, reason: &str) -> anyhow::Result<SetupResult> {
    let err = |s: String| RoleTemplateError(s);
    let Some(template) = get_role_template(template_key) else { return Err(err("That role template does not exist.".into()).into()) };
    let ops = discord_ops(app);
    if !ops.bot_can_manage_roles(guild_id) {
        return Err(err("I need the **Manage Roles** permission to create the template's roles.".into()).into());
    }

    // 1. Checks first: every role this template reuses must be one the bot can hand out, and the new roles must fit.
    let roles_now = ops.guild_roles(guild_id);
    let plan: Vec<(crate::role_templates::TemplateRole, Option<RoleInfo>)> =
        template.roles.iter().map(|e| (e.clone(), find_role_by_name(&roles_now, guild_id, e.name).cloned())).collect();
    for (entry, role) in &plan {
        let Some(role) = role else { continue };
        let check = ops.check_assignable(guild_id, &role.id, executor);
        if !check.ok {
            return Err(err(format!(
                "The existing **{}** role can't be used: {} Rename or fix it, then try again.",
                entry.name,
                check.reason.unwrap_or_default()
            )).into());
        }
    }
    let missing = plan.iter().filter(|(_, r)| r.is_none()).count();
    if roles_now.len() + missing > DISCORD_ROLE_CAP {
        return Err(err(format!(
            "This needs {missing} new roles, but the server is close to Discord's limit of {DISCORD_ROLE_CAP} roles. Delete some roles first."
        )).into());
    }
    let live: HashSet<String> = roles_now.iter().map(|r| r.id.clone()).collect();
    if template.links_zones {
        let Some(settings) = app.settings().get_settings_safe(guild_id).await else {
            return Err(err("Server settings could not be loaded. Please try again in a moment.".into()).into());
        };
        let linked: HashSet<String> =
            links_of(settings.timezone_roles.as_ref()).into_iter().filter(|l| live.contains(&l.role_id)).map(|l| l.role_id).collect();
        let added = plan.iter().filter(|(_, r)| r.as_ref().is_none_or(|r| !linked.contains(&r.id))).count();
        if linked.len() + added > TIMEZONE_ROLES_MAX {
            return Err(err(format!(
                "This would make {} timezone roles; the most is {TIMEZONE_ROLES_MAX}. Remove some with `/settings timezone-role remove` first.",
                linked.len() + added
            )).into());
        }
    }

    // 2. Create the missing roles (no permissions, not mentionable), in the template's order.
    let mut roles: Vec<SetupRole> = Vec::new();
    for (entry, role) in &plan {
        let target_id = match role {
            Some(r) => r.id.clone(),
            None => match create_role(ops.clone(), guild_id, create_role_body(entry.name, entry.color), reason).await {
                Ok(r) => r.id,
                Err(message) => {
                    bot_util::log_warn!(format!("[role-templates] {guild_id}: creating \"{}\" failed", entry.name), message.clone());
                    let made: Vec<String> = roles.iter().filter(|r| r.created).map(|r| r.name.clone()).collect();
                    let tail = if made.is_empty() {
                        String::new()
                    } else {
                        format!(" Already created: {}; running the template again reuses them.", made.join(", "))
                    };
                    return Err(err(format!("Discord refused to create the **{}** role ({message}).{tail}", entry.name)).into());
                }
            },
        };
        roles.push(SetupRole {
            role_id: target_id,
            name: entry.name.to_string(),
            label: entry.label.to_string(),
            description: entry.description.map(str::to_string),
            emoji: entry.emoji.map(str::to_string),
            zone: entry.zone.map(str::to_string),
            created: role.is_none(),
        });
    }

    // 3. A timezone template links every role to its zone (an existing link of that role is replaced).
    if template.links_zones {
        let ids: HashSet<String> = roles.iter().map(|r| r.role_id.clone()).collect();
        let new_links: Vec<TimezoneRole> =
            roles.iter().map(|r| TimezoneRole { role_id: r.role_id.clone(), zone: r.zone.clone().unwrap_or_default() }).collect();
        let live2 = live.clone();
        app.settings()
            .update_settings_with(guild_id, move |cur| {
                let mut keep: Vec<TimezoneRole> = links_of(cur.timezone_roles.as_ref())
                    .into_iter()
                    .filter(|l| !ids.contains(&l.role_id) && (live2.is_empty() || live2.contains(&l.role_id)))
                    .collect();
                keep.extend(new_links);
                Ok(SettingsPatch::new().set("timezoneRoles", links_json(&keep)))
            })
            .await?;
    }

    let created = roles.iter().filter(|r| r.created).count();
    bot_util::log_info!(format!("[role-templates] {guild_id}: template {template_key}, {created} created, {} reused", roles.len() - created));
    let reused = roles.len() - created;
    Ok(SetupResult { roles, created, reused })
}

// ── Upgrading timezone panels when a template gains roles ───────────────────────

/// The panel side of [`upgrade_timezone_panels_with`] (the JS `panelStore`, for tests).
#[async_trait]
pub trait PanelStore: Send + Sync {
    /// `listUniquePanels(guildId)`.
    async fn list(&self, app: &App, guild_id: &str) -> anyhow::Result<Vec<Panel>>;
    /// `insertItems(group, items, order)`.
    async fn insert_items(&self, app: &App, panel: &Panel, items: &[NewItem], order: &[String]) -> anyhow::Result<()>;
    /// `invalidatePanelIndex(guildId)`.
    async fn invalidate(&self, app: &App, guild_id: &str);
    /// `syncPanel(guild, panelId, { settings })`.
    async fn sync(&self, app: &App, guild_id: &str, panel_id: &str, color: Option<i64>) -> SyncResult;
}

/// The real panel store (PostgreSQL and Discord).
pub struct DbPanelStore;

#[async_trait]
impl PanelStore for DbPanelStore {
    async fn list(&self, app: &App, guild_id: &str) -> anyhow::Result<Vec<Panel>> {
        Ok(list_unique_panels(app.db(), guild_id).await?)
    }
    async fn insert_items(&self, app: &App, panel: &Panel, items: &[NewItem], order: &[String]) -> anyhow::Result<()> {
        insert_items(app.db(), &panel.group.id, &panel.items, items, order).await
    }
    async fn invalidate(&self, app: &App, guild_id: &str) {
        invalidate_panel_index(app, guild_id).await;
    }
    async fn sync(&self, app: &App, guild_id: &str, panel_id: &str, color: Option<i64>) -> SyncResult {
        sync_panel(app, guild_id, PanelArg::Id(panel_id.to_string()), color).await
    }
}

fn past_sets() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    TIMEZONE_TEMPLATE_PAST_ZONES.iter().flat_map(|(key, sets)| sets.iter().map(move |s| (*key, *s)))
}

/// `SMALLEST_PAST_SET`.
pub fn smallest_past_set() -> usize {
    past_sets().map(|(_, s)| s.len()).min().unwrap_or(0)
}

/// `matchPastTemplate(zones)`: the template key whose past zone set the panel's zones are exactly, or `None`.
pub fn match_past_template(zones: &[Option<String>]) -> Option<&'static str> {
    if zones.iter().any(|z| z.as_deref().is_none_or(str::is_empty)) {
        return None;
    }
    let have: HashSet<&str> = zones.iter().filter_map(|z| z.as_deref()).collect();
    if have.len() != zones.len() {
        return None;
    }
    past_sets().find(|(_, set)| set.len() == have.len() && set.iter().all(|z| have.contains(z))).map(|(k, _)| k)
}

/// `upgradeTimezonePanels(guild)`: adds a timezone template's newer roles to every panel made from an older version of
/// it (a Unique dropdown or button panel whose roles are linked to exactly one of the past zone sets). Runs on every
/// start (the reaction-roles `guildReady` hook). Never fails; returns how many panels were upgraded.
pub async fn upgrade_timezone_panels(app: &App, guild_id: &str) -> usize {
    upgrade_timezone_panels_with(app, guild_id, &DbPanelStore).await
}

/// [`upgrade_timezone_panels`] with another panel store (tests).
pub async fn upgrade_timezone_panels_with(app: &App, guild_id: &str, store: &dyn PanelStore) -> usize {
    let res: anyhow::Result<usize> = async {
        let settings = app.settings().get_settings_safe(guild_id).await;
        let ops = discord_ops(app);
        let live: HashSet<String> = ops.guild_roles(guild_id).into_iter().map(|r| r.id).collect();
        let mut links: Vec<TimezoneRole> =
            links_of(settings.as_ref().and_then(|s| s.timezone_roles.as_ref())).into_iter().filter(|l| live.contains(&l.role_id)).collect();
        if links.len() < smallest_past_set() {
            return Ok(0); // most guilds: no query
        }
        let color = settings.as_ref().and_then(|s| s.embed_color).map(i64::from);
        let mut upgraded = 0;
        for panel in store.list(app, guild_id).await? {
            let zone_of: HashMap<String, String> = links.iter().map(|l| (l.role_id.clone(), l.zone.clone())).collect();
            let zones: Vec<Option<String>> = panel.items.iter().map(|it| zone_of.get(&it.role_id).cloned()).collect();
            let Some(key) = match_past_template(&zones) else { continue };
            let added = upgrade_panel(app, guild_id, &panel, key, &mut links, &live, &zone_of, color, store).await?;
            if added > 0 {
                upgraded += 1;
            }
        }
        Ok(upgraded)
    }
    .await;
    match res {
        Ok(n) => n,
        Err(e) => {
            bot_util::log_warn!(format!("[role-templates] {guild_id}: upgrading timezone panels failed"), bot_core::error::log_arg(&e));
            0
        }
    }
}

/// One panel; `links` is updated in place with the links written. Returns the roles added.
#[allow(clippy::too_many_arguments)]
async fn upgrade_panel(
    app: &App,
    guild_id: &str,
    panel: &Panel,
    key: &'static str,
    links: &mut Vec<TimezoneRole>,
    live: &HashSet<String>,
    zone_of: &HashMap<String, String>,
    color: Option<i64>,
    store: &dyn PanelStore,
) -> anyhow::Result<usize> {
    let skip = |why: String| {
        bot_util::log_info!(format!("[role-templates] {guild_id}: timezone panel {} ({key}) not upgraded: {why}", panel.group.id));
        Ok(0)
    };
    let Some(template) = timezone_role_template(key) else { return Ok(0) };
    let have: HashSet<String> = panel.items.iter().filter_map(|it| zone_of.get(&it.role_id).cloned()).collect();
    let ops = discord_ops(app);
    let roles_now = ops.guild_roles(guild_id);
    let plan: Vec<(&bot_util::timezones::TzRoleTemplate, Option<RoleInfo>)> = template
        .roles
        .iter()
        .filter(|e| !have.contains(e.zone))
        .map(|e| (e, find_role_by_name(&roles_now, guild_id, e.name).cloned()))
        .collect();
    if plan.is_empty() {
        return Ok(0);
    }

    // 1. Checks first, as setupRoleTemplate.
    if !ops.bot_can_manage_roles(guild_id) {
        return skip("no Manage Roles".into());
    }
    for (entry, role) in &plan {
        let Some(role) = role else { continue };
        if let Some(linked) = zone_of.get(&role.id)
            && linked != entry.zone
        {
            return skip(format!("the {} role is linked to {linked}", entry.name));
        }
        let check = ops.check_assignable(guild_id, &role.id, None);
        if !check.ok {
            return skip(format!("the {} role can't be used: {}", entry.name, check.reason.unwrap_or_default()));
        }
    }
    if panel.items.len() + plan.len() > max_items(panel.layout()).unwrap_or(0) {
        return skip("too many roles for the panel".into());
    }
    let missing = plan.iter().filter(|(_, r)| r.is_none()).count();
    if roles_now.len() + missing > DISCORD_ROLE_CAP {
        return skip("close to the role cap".into());
    }
    let new_links = plan.iter().filter(|(_, r)| r.as_ref().is_none_or(|r| !zone_of.contains_key(&r.id))).count();
    if links.len() + new_links > TIMEZONE_ROLES_MAX {
        return skip(format!("more than {TIMEZONE_ROLES_MAX} timezone roles"));
    }

    // 2. Create the missing roles; a failure leaves the panel as it was (created roles are reused next time).
    let mut roles: Vec<(&bot_util::timezones::TzRoleTemplate, String)> = Vec::new();
    for (entry, role) in &plan {
        let id = match role {
            Some(r) => r.id.clone(),
            None => match create_role(ops.clone(), guild_id, create_role_body(entry.name, 0), "Timezone template update").await {
                Ok(r) => r.id,
                Err(message) => {
                    bot_util::log_warn!(
                        format!("[role-templates] {guild_id}: creating \"{}\" for panel {} failed", entry.name, panel.group.id),
                        message
                    );
                    return Ok(0);
                }
            },
        };
        roles.push((entry, id));
    }

    // 3. Link them, then add them to the panel in the template's order and re-render it.
    let ids: HashSet<String> = roles.iter().map(|(_, id)| id.clone()).collect();
    let written: Vec<TimezoneRole> = roles.iter().map(|(e, id)| TimezoneRole { role_id: id.clone(), zone: e.zone.to_string() }).collect();
    let (live2, w2) = (live.clone(), written.clone());
    app.settings()
        .update_settings_with(guild_id, move |cur| {
            let mut keep: Vec<TimezoneRole> =
                links_of(cur.timezone_roles.as_ref()).into_iter().filter(|l| !ids.contains(&l.role_id) && live2.contains(&l.role_id)).collect();
            keep.extend(w2);
            Ok(SettingsPatch::new().set("timezoneRoles", links_json(&keep)))
        })
        .await?;
    for l in &written {
        if !zone_of.contains_key(&l.role_id) {
            links.push(l.clone());
        }
    }
    let items: Vec<NewItem> = roles
        .iter()
        .map(|(e, id)| NewItem {
            role_id: id.clone(),
            label: e.label.to_string(),
            emoji: None,
            style: Some(2),
            description: (panel.layout() == "SELECT").then(|| e.description.to_string()),
        })
        .collect();
    let mut role_of_zone: HashMap<String, String> = HashMap::new();
    for it in &panel.items {
        if let Some(z) = zone_of.get(&it.role_id) {
            role_of_zone.insert(z.clone(), it.role_id.clone());
        }
    }
    for (e, id) in &roles {
        role_of_zone.insert(e.zone.to_string(), id.clone());
    }
    let order: Vec<String> = template.roles.iter().filter_map(|e| role_of_zone.get(e.zone).cloned()).collect();
    store.insert_items(app, panel, &items, &order).await?;
    store.invalidate(app, guild_id).await;
    let synced = store.sync(app, guild_id, &panel.group.id, color).await;
    let names: Vec<&str> = roles.iter().map(|(e, _)| e.name).collect();
    let tail = if synced.status == "ok" {
        String::new()
    } else {
        format!(" (message not updated: {})", synced.reason.clone().unwrap_or_else(|| synced.status.to_string()))
    };
    bot_util::log_info!(format!("[role-templates] {guild_id}: timezone panel {} ({key}) gained {}{tail}", panel.group.id, names.join(", ")));
    Ok(roles.len())
}

/// `describeSetup({ created, reused })`: "Created 4 roles and reused 2 that already existed."
pub fn describe_setup(created: usize, reused: usize) -> String {
    let part = |n: usize, verb: &str| format!("{verb} {n} role{}", if n == 1 { "" } else { "s" });
    if reused == 0 {
        return format!("{}.", part(created, "Created"));
    }
    if created == 0 {
        return format!("{} that already existed.", part(reused, "Reused"));
    }
    format!("{} and reused {reused} that already existed.", part(created, "Created"))
}

/// `describeReused({ roles })`: names the reused roles, since a template makes them self-assignable (an existing
/// "Events" role may unlock a private channel); `None` when every role was created.
pub fn describe_reused(roles: &[SetupRole]) -> Option<String> {
    let names: Vec<String> = roles.iter().filter(|r| !r.created).map(|r| format!("**{}**", r.name)).collect();
    if names.is_empty() {
        return None;
    }
    let one = names.len() == 1;
    Some(format!(
        "Existing role{} now on the panel: {}. Members can now give {} to themselves, so check {} unlock anything private.",
        if one { "" } else { "s" },
        names.join(", "),
        if one { "it" } else { "them" },
        if one { "it doesn't" } else { "they don't" }
    ))
}

/// Every timezone template key (for the past-set check).
pub fn timezone_template_keys() -> Vec<&'static str> {
    TIMEZONE_ROLE_TEMPLATES.iter().map(|t| t.key).collect()
}
