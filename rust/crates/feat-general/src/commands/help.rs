//! Port of `src/commands/utility/help.js`: `/help` lists the commands the caller may use here (from the app's command
//! registry, grouped by module and category), `/help command:<name>` shows one command. A core command.

use bot_config::limits::CUSTOM_TAGS;
use bot_config::{MODULE_NAMES, get_module};
use bot_core::command::{CommandDef, CommandOption as Opt, command_name_compare, option_type};
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, respond_autocomplete, with_cached_guild};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::{CUSTOM_TAG_INFO, CustomTag, GuildSettings, SelectBuilder};
use bot_util::jsre::LazyRe;
use bot_util::js;
use bot_util::text::{chunk_lines, clamp_text};
use serde_json::{Value, json};
use serenity::all::{EditInteractionResponse, GuildId};
use serenity::builder::CreateEmbed;
use std::sync::Arc;

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("help", "Show the commands you can use here.")
        .option(Opt::string("command", "Show details for one command").autocomplete(true))
}

/// The command (core, MEMBER).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), None, execute)
        .permission_level(PermissionLevel::Member)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

/// Labels of the core (module-less) commands, by folder name. Anything else is "Other".
pub const CATEGORY_LABELS: [(&str, &str); 8] = [
    ("utility", "🔧 Utility"),
    ("admin", "⚙️ Admin"),
    ("moderation", "🔨 Moderation"),
    ("community", "👥 Community"),
    ("economy", "🪙 Economy"),
    ("games", "🎮 Games"),
    ("leveling", "⭐ Levels"),
    ("search", "🔎 Search"),
];
pub const OTHER_LABEL: &str = "📦 Other";
pub const SETUP_LABEL: &str = "⚙️ Setup (modules currently off)";
pub const CUSTOM_LABEL: &str = "🏷️ Custom Commands";

/// `LEVEL_NAMES`.
pub fn level_name(level: PermissionLevel) -> &'static str {
    match level {
        PermissionLevel::Member => "Member",
        PermissionLevel::Moderator => "Moderator",
        PermissionLevel::Admin => "Admin",
        PermissionLevel::Owner => "Owner",
    }
}

// Discord caps an embed at 25 fields and 6000 characters in total; stay clear of both.
const MAX_FIELDS: usize = 25;
const MAX_CHARS: usize = 5500;
const FIELD_MAX: usize = 1024;

// ── Visibility ────────────────────────────────────────────────────────────────

/// `minRequiredLevel(mod)`: the lowest level that can run any part of the command.
pub fn min_required_level(def: &CommandDef) -> PermissionLevel {
    def.subcommand_permissions.iter().map(|(_, l)| *l).fold(def.permission_level, std::cmp::min)
}

/// Where a command goes in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Listed normally.
    Show,
    /// A config command of a module that is off, shown to admins only.
    Setup,
    Hide,
}

/// `visibility(mod, userLevel, settings)`. With no settings the module state is unknown, so nothing is hidden for it.
pub fn visibility(def: &CommandDef, level: PermissionLevel, settings: Option<&GuildSettings>) -> Visibility {
    if level < min_required_level(def) {
        return Visibility::Hide;
    }
    let Some(key) = def.module else { return Visibility::Show };
    let on = settings.is_none_or(|s| is_module_enabled(Some(s), Some(key)));
    if on {
        return Visibility::Show;
    }
    if !def.gated {
        return if level >= PermissionLevel::Admin { Visibility::Setup } else { Visibility::Hide };
    }
    Visibility::Hide
}

// ── The command list ──────────────────────────────────────────────────────────

/// One field group (`{ label, lines }`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Group {
    pub label: String,
    pub lines: Vec<String>,
}

fn sorted_lines(mut entries: Vec<(String, String)>) -> Vec<String> {
    entries.sort_by(|a, b| command_name_compare(&a.0, &b.0));
    entries.into_iter().map(|(_, l)| l).collect()
}

/// `buildGroups(client, userLevel, settings)`: the commands this user may see, as module groups, core groups, then
/// the setup group; and how many there are.
pub fn build_groups(commands: &[Arc<CommandDef>], level: PermissionLevel, settings: Option<&GuildSettings>) -> (Vec<Group>, usize) {
    let mut modules: Vec<(ModuleKey, Vec<(String, String)>)> = Vec::new();
    let mut categories: Vec<(&'static str, Vec<(String, String)>)> = Vec::new();
    let mut setup: Vec<(String, String)> = Vec::new();
    let mut count = 0;
    for def in commands {
        let verdict = visibility(def, level, settings);
        if verdict == Visibility::Hide {
            continue;
        }
        count += 1;
        let name = def.name().to_string();
        let entry = (name.clone(), format!("`/{name}` — {}", clamp_text(&def.data.description, 100)));
        if verdict == Visibility::Setup {
            setup.push(entry);
        } else if let Some(key) = def.module {
            match modules.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push(entry),
                None => modules.push((key, vec![entry])),
            }
        } else {
            let category = if def.category.is_empty() { "general" } else { def.category };
            match categories.iter_mut().find(|(c, _)| *c == category) {
                Some((_, list)) => list.push(entry),
                None => categories.push((category, vec![entry])),
            }
        }
    }

    let mut groups = Vec::new();
    let mut add = |label: String, entries: Vec<(String, String)>| {
        if !entries.is_empty() {
            groups.push(Group { label, lines: sorted_lines(entries) });
        }
    };
    for key in MODULE_NAMES {
        let Some(m) = get_module(key) else { continue };
        if let Some((_, list)) = modules.iter().find(|(k, _)| *k == m.key) {
            add(format!("{} {}", m.emoji, m.label), list.clone());
        }
    }
    for (category, label) in CATEGORY_LABELS {
        if let Some((_, list)) = categories.iter().find(|(c, _)| *c == category) {
            add(label.to_string(), list.clone());
        }
    }
    let other: Vec<(String, String)> = categories
        .iter()
        .filter(|(c, _)| !CATEGORY_LABELS.iter().any(|(k, _)| k == c))
        .flat_map(|(_, e)| e.clone())
        .collect();
    add(OTHER_LABEL.to_string(), other);
    add(SETUP_LABEL.to_string(), setup);
    (groups, count)
}

static WHITESPACE: LazyRe = bot_util::lazy_re!(r"\s", "u");

/// `customPrefixOf(p)`: the prefix custom commands answer to, by the custom-commands module's rule (`effectivePrefix`
/// in tagService): '' means prefix invocation is off (`None`), a valid prefix (1-5 characters, no whitespace) is
/// itself, anything else (a hand-edited row, a non-string) is '!'. Duplicated here because core must not import a
/// feature package.
pub fn custom_prefix_of(p: &Value) -> Option<String> {
    match p {
        Value::String(s) if s.is_empty() => None,
        Value::String(s) if s.chars().count() <= 5 && !WHITESPACE.test(s) => Some(s.clone()),
        _ => Some("!".into()),
    }
}

/// A custom command as `/help` reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HelpTag {
    pub name: String,
    pub description: Option<String>,
    pub allow_role_ids: Vec<String>,
    pub deny_role_ids: Vec<String>,
}

impl From<CustomTag> for HelpTag {
    fn from(t: CustomTag) -> Self {
        Self { name: t.name, description: t.description, allow_role_ids: t.allow_role_ids, deny_role_ids: t.deny_role_ids }
    }
}

/// `customCommandLine(tag, prefix)`: `!name` while prefix commands are on, `/tag use name:<x>` when the prefix is off,
/// because a bare name would not run anything then.
pub fn custom_command_line(name: &str, description: Option<&str>, prefix: Option<&str>) -> String {
    let usage = match prefix {
        None => format!("/tag use name:{name}"),
        Some(p) => format!("{p}{name}"),
    };
    format!("`{usage}` — {}", clamp_text(description.unwrap_or("custom command"), 100))
}

/// `tagRolesAllow(tag, member)`: whether a member's roles (`member.roles.cache` keys; `None` without a member) may run
/// a custom command. The role half of `checkTagAccess` in tagService.
pub fn tag_roles_allow(allow: &[String], deny: &[String], member_roles: Option<&[String]>) -> bool {
    let has = |id: &String| member_roles.is_some_and(|r| r.contains(id));
    if deny.iter().any(has) {
        return false;
    }
    allow.is_empty() || allow.iter().any(has)
}

/// The group of custom commands, from tags already read (`viewer`: a non-admin member's roles hides the commands
/// their roles cannot run, as /tag list does).
pub fn custom_group_from(tags: &[HelpTag], settings: &GuildSettings, viewer: Option<&[String]>) -> Option<Group> {
    let prefix = custom_prefix_of(&json!(settings.custom_prefix));
    let lines: Vec<String> = tags
        .iter()
        .filter(|t| viewer.is_none() || tag_roles_allow(&t.allow_role_ids, &t.deny_role_ids, viewer))
        .take(25)
        .map(|t| custom_command_line(&t.name, t.description.as_deref(), prefix.as_deref()))
        .collect();
    (!lines.is_empty()).then(|| Group { label: CUSTOM_LABEL.into(), lines })
}

/// `customCommandGroup(guildId, settings, viewer)`: a read-only look at the guild's enabled custom commands. `None`
/// when the module is off, there are none, or the read fails (logged at debug level).
pub async fn custom_command_group(db: &bot_db::PgPool, guild_id: &str, settings: Option<&GuildSettings>, viewer: Option<&[String]>) -> Option<Group> {
    let settings = settings?;
    if !is_module_enabled(Some(settings), Some(ModuleKey::Customcommands)) {
        return None;
    }
    let take = if viewer.is_some() { CUSTOM_TAGS as i64 } else { 25 };
    let query = async {
        let rows: Vec<CustomTag> = SelectBuilder::new(&CUSTOM_TAG_INFO)
            .filter("guildId", json!(guild_id))?
            .filter("enabled", json!(true))?
            .order_by("uses", false)?
            .order_by("name", true)?
            .limit(take)
            .build()?
            .fetch_all(db)
            .await?;
        Ok::<_, bot_db::DbError>(rows)
    };
    match tokio::time::timeout(std::time::Duration::from_millis(3000), query).await {
        Ok(Ok(rows)) => {
            let tags: Vec<HelpTag> = rows.into_iter().map(HelpTag::from).collect();
            custom_group_from(&tags, settings, viewer)
        }
        Ok(Err(e)) => {
            bot_util::log_debug!(format!("/help: could not read custom commands: {e}"));
            None
        }
        Err(_) => {
            bot_util::log_debug!("/help: could not read custom commands: help custom commands timed out after 3000ms");
            None
        }
    }
}

/// `buildPages(groups, settings, userLevel, count)`: the groups split into embeds that respect the field and character
/// limits.
pub fn build_pages(groups: &[Group], settings: Option<&GuildSettings>, level: PermissionLevel, count: usize) -> Vec<CreateEmbed> {
    let title = "📖 Commands";
    let description = format!("Your access level: **{}** · Details: `/help command:<name>`", level_name(level));
    let footer = format!("{count} command{} available to you", if count == 1 { "" } else { "s" });
    let overhead = js::utf16_len(title) + js::utf16_len(&description) + js::utf16_len(&footer) + 40; // room for the page indicator

    let mut fields: Vec<(String, String)> = Vec::new();
    for g in groups {
        for (i, value) in chunk_lines(&g.lines, FIELD_MAX).into_iter().enumerate() {
            fields.push((if i == 0 { g.label.clone() } else { format!("{} (cont.)", g.label) }, value));
        }
    }

    let mut pages: Vec<Vec<(String, String)>> = Vec::new();
    let mut current: Vec<(String, String)> = Vec::new();
    let mut size = overhead;
    for field in fields {
        let cost = js::utf16_len(&field.0) + js::utf16_len(&field.1);
        if !current.is_empty() && (current.len() >= MAX_FIELDS || size + cost > MAX_CHARS) {
            pages.push(std::mem::take(&mut current));
            size = overhead;
        }
        current.push(field);
        size += cost;
    }
    if !current.is_empty() {
        pages.push(current);
    }

    pages
        .into_iter()
        .map(|page| {
            let mut e = neutral_embed(Some(title), Some(&description), Some(&footer));
            for (name, value) in page {
                e = e.field(name, value, false);
            }
            apply_guild_color(e, settings)
        })
        .collect()
}

// ── /help command:<name> ──────────────────────────────────────────────────────

fn level_badge(level: PermissionLevel, base: PermissionLevel) -> String {
    if level > base { format!(" *({}+)*", level_name(level)) } else { String::new() }
}

fn sub_permission(def: &CommandDef, key: &str) -> Option<PermissionLevel> {
    def.subcommand_permissions.iter().find(|(k, _)| k == key).map(|(_, l)| *l)
}

/// `subcommandLines(mod, json)`.
pub fn subcommand_lines(def: &CommandDef) -> Vec<String> {
    let base = def.permission_level;
    let mut lines = Vec::new();
    for opt in &def.data.options {
        if opt.kind == option_type::SUB_COMMAND {
            let level = sub_permission(def, &opt.name).unwrap_or(base);
            lines.push(format!("`{}` — {}{}", opt.name, opt.description, level_badge(level, PermissionLevel::Member)));
        } else if opt.kind == option_type::SUB_COMMAND_GROUP {
            for sub in &opt.options {
                let level = sub_permission(def, &format!("{} {}", opt.name, sub.name)).or_else(|| sub_permission(def, &sub.name)).unwrap_or(base);
                lines.push(format!("`{} {}` — {}{}", opt.name, sub.name, sub.description, level_badge(level, PermissionLevel::Member)));
            }
        }
    }
    lines
}

/// `usageOf(name, json)`: `/name <required> [optional]` (top-level options only).
pub fn usage_of(def: &CommandDef) -> String {
    let mut out = format!("/{}", def.name());
    for o in def.data.options.iter().filter(|o| o.kind != option_type::SUB_COMMAND && o.kind != option_type::SUB_COMMAND_GROUP) {
        if o.required {
            out.push_str(&format!(" <{}>", o.name));
        } else {
            out.push_str(&format!(" [{}]", o.name));
        }
    }
    out
}

/// The name `/help command:` looks up: trimmed, without a leading `/`, lowercased.
pub fn wanted_name(wanted: &str) -> String {
    let t = js::trim(wanted);
    js::lower(t.strip_prefix('/').unwrap_or(t))
}

/// `showCommand(...)`'s answer: the command's embed, or `None` for "No such command." (unknown or hidden).
pub fn command_embed(def: &CommandDef, level: PermissionLevel, settings: Option<&GuildSettings>) -> Option<CreateEmbed> {
    if visibility(def, level, settings) == Visibility::Hide {
        return None;
    }
    let module_text = match def.module {
        Some(key) => {
            let m = key.info();
            let state = match settings {
                Some(s) if is_module_enabled(Some(s), Some(key)) => " — on",
                Some(_) => " — off",
                None => "",
            };
            format!("{} {}{state}", m.emoji, m.label)
        }
        None => "Core (always available)".into(),
    };
    let mut embed = apply_guild_color(neutral_embed(Some(&format!("/{}", def.name())), Some(&clamp_text(&def.data.description, 4000)), None), settings)
        .field("Usage", format!("`{}`", clamp_text(&usage_of(def), 200)), false)
        .field("Module", module_text, true)
        .field("Required level", level_name(def.permission_level), true);
    for (i, value) in chunk_lines(&subcommand_lines(def), FIELD_MAX).into_iter().take(10).enumerate() {
        embed = embed.field(if i == 0 { "Subcommands" } else { "Subcommands (cont.)" }, value, false);
    }
    Some(embed)
}

/// The autocomplete choices: visible commands whose name contains the query, sorted, at most 25 (`/name` → `name`).
pub fn help_choices(commands: &[Arc<CommandDef>], level: PermissionLevel, settings: Option<&GuildSettings>, focused: &str) -> Vec<Choice> {
    let query = wanted_name(focused);
    let mut names: Vec<&str> = commands
        .iter()
        .filter(|d| (query.is_empty() || d.name().contains(query.as_str())) && visibility(d, level, settings) != Visibility::Hide)
        .map(|d| d.name())
        .collect();
    names.sort_by(|a, b| command_name_compare(a, b));
    names.into_iter().take(25).map(|n| Choice::new(format!("/{n}"), n)).collect()
}

// ── Entry points ──────────────────────────────────────────────────────────────

/// The member's roles as `member.roles.cache` keys (`@everyone` included), for the custom-command role filter.
fn viewer_roles(ctx: &CommandCtx, guild_id: GuildId) -> Vec<String> {
    let Some(member) = ctx.member() else { return Vec::new() };
    let cached = with_cached_guild(&ctx.app, guild_id, |g| bot_core::interaction_helpers::member_role_ids(g, member));
    match cached {
        Some(ids) => ids.into_iter().map(|r| r.to_string()).collect(),
        None => std::iter::once(guild_id.to_string()).chain(member.roles.iter().map(|r| r.to_string())).collect(),
    }
}

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(true).await?;
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/help outside a guild"))?;
    let g = guild_id.to_string();
    let settings = ctx.app.settings().get_settings_safe(&g).await;
    let level = resolve_permission_level(ctx.member_access().as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref());

    let commands = ctx.app.commands().all();
    if let Some(wanted) = ctx.get_string("command").filter(|w| !w.is_empty()) {
        let name = wanted_name(&wanted);
        let embed = ctx.app.commands().get(&name).and_then(|def| command_embed(&def, level, settings.as_deref()));
        let edit = match embed {
            Some(e) => EditInteractionResponse::new().embeds(vec![e]),
            None => EditInteractionResponse::new().content("No such command."),
        };
        ctx.edit_reply(edit).await?;
        return Ok(());
    }

    let (mut groups, count) = build_groups(&commands, level, settings.as_deref());
    let viewer = (level < PermissionLevel::Admin).then(|| viewer_roles(&ctx, guild_id));
    let custom = custom_command_group(ctx.app.db(), &g, settings.as_deref(), viewer.as_deref()).await;
    let custom_count = custom.as_ref().map_or(0, |c| c.lines.len());
    if let Some(c) = custom {
        // Custom commands sit before the setup group, which always comes last.
        let at = groups.iter().position(|x| x.label == SETUP_LABEL).unwrap_or(groups.len());
        groups.insert(at, c);
    }
    if groups.is_empty() {
        ctx.edit_reply(EditInteractionResponse::new().content("No commands are available to you.")).await?;
        return Ok(());
    }
    let pages = build_pages(&groups, settings.as_deref(), level, count + custom_count);
    paginate(&ctx, &pages, PaginateOptions { ephemeral: true, ..PaginateOptions::default() }).await?;
    Ok(())
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    // Memo first; a cold load is bounded well inside the 3-second answer window.
    let settings = match ctx.guild_id() {
        Some(g) => ctx.app.settings().peek_or_load(&g.to_string(), 1000).await,
        None => None,
    };
    let level = resolve_permission_level(ctx.member_access().as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref());
    let choices = help_choices(&ctx.app.commands().all(), level, settings.as_deref(), &ctx.focused_value());
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

/// The embeds as Discord JSON without the timestamp (for comparisons).
pub fn embed_json(e: &CreateEmbed) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("timestamp");
    }
    v
}
