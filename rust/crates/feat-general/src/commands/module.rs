//! Port of `src/commands/admin/module.js`: `/module list | enable | disable`. A core command: it is how every module is
//! switched on and off, so it belongs to no module and can never be gated off itself.

use bot_config::{MODULE_NAMES, ModuleFlags, ModuleInfo, get_module};
use bot_core::command::CommandOption as Opt;
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, respond_autocomplete};
use bot_core::prelude::*;
use bot_db::GuildSettings;
use bot_util::text::chunk_lines;
use serenity::all::{EditInteractionResponse, Permissions};
use serenity::builder::CreateEmbed;

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("module", "Turn bot modules on or off for this server.")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::sub_command("list", "Show every module and whether it is on."))
        .option(
            Opt::sub_command("enable", "Turn a module on.")
                .option(Opt::string("module", "The module to turn on").required(true).autocomplete(true)),
        )
        .option(
            Opt::sub_command("disable", "Turn a module off. Its data is kept.")
                .option(Opt::string("module", "The module to turn off").required(true).autocomplete(true)),
        )
}

/// The command (core, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), None, execute)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

fn info_of(key: &str) -> &'static ModuleInfo {
    // Only called with keys from MODULE_NAMES.
    get_module(key).unwrap_or(&bot_config::MODULES[0])
}

/// `resolveModuleKey(input)`: the module key (what autocomplete submits) or, as a convenience, its label.
pub fn resolve_module_key(input: &str) -> Option<&'static str> {
    let text = bot_util::js::lower(bot_util::js::trim(input));
    if let Some(m) = get_module(&text) {
        return Some(m.key.as_str());
    }
    MODULE_NAMES.iter().copied().find(|k| bot_util::js::lower(info_of(k).label) == text)
}

/// `settings[mod.flag] === true`.
pub fn module_is_on(settings: &GuildSettings, key: &str) -> bool {
    settings.module_flag(info_of(key).flag) == Some(true)
}

/// The `/module list` lines: `{emoji} **{label}** — ✅ On · \`{configCommand}\``.
pub fn module_list_lines(settings: &GuildSettings) -> Vec<String> {
    MODULE_NAMES
        .iter()
        .map(|key| {
            let m = info_of(key);
            let state = if module_is_on(settings, key) { "✅ On" } else { "❌ Off" };
            format!("{} **{}** — {state} · `{}`", m.emoji, m.label, m.config_command)
        })
        .collect()
}

/// The `/module list` embed.
pub fn module_list_embed(settings: &GuildSettings) -> CreateEmbed {
    let mut embed = apply_guild_color(
        neutral_embed(Some("🧩 Server Modules"), Some("Turn a module on or off with `/module enable` and `/module disable`."), None),
        Some(settings),
    );
    // 42 lines do not fit one 1024-character field.
    for (i, value) in chunk_lines(&module_list_lines(settings), 1024).into_iter().enumerate() {
        embed = embed.field(if i == 0 { "Modules" } else { "Modules (continued)" }, value, false);
    }
    embed
}

/// The `/module enable|disable` answer.
pub fn toggled_embed(key: &str, enable: bool) -> CreateEmbed {
    let m = info_of(key);
    if enable {
        success_embed("Module Enabled", Some(&format!("**{}** enabled. Configure it with `{}`.", m.label, m.config_command)), None)
    } else {
        success_embed("Module Disabled", Some(&format!("**{}** disabled. Its data is kept.", m.label)), None)
    }
}

async fn edit(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.edit_reply(EditInteractionResponse::new().embeds(vec![embed])).await?;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    let sub = ctx.subcommand().unwrap_or_default();
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/module outside a guild"))?;
    let g = guild_id.to_string();

    if sub == "list" {
        let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else {
            return edit(
                &ctx,
                error_embed("Temporarily Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None),
            )
            .await;
        };
        return edit(&ctx, module_list_embed(&settings)).await;
    }

    let enable = sub == "enable";
    let raw = ctx.get_string("module").ok_or_else(|| anyhow::anyhow!("Required option \"module\" not found."))?;
    let Some(key) = resolve_module_key(&raw) else {
        return edit(&ctx, error_embed("Unknown Module", Some("That is not a module. Use `/module list` to see every module."), None)).await;
    };

    let m = info_of(key);
    if let Err(e) = ctx.app.settings().update_settings(&g, SettingsPatch::new().set(m.flag, enable)).await {
        bot_util::log_error!(format!("/module {sub} {key} failed in guild {g}"), bot_core::error::log_arg(&anyhow::Error::from(e)));
        return edit(&ctx, error_embed("Update Failed", Some("The setting could not be saved. Please try again in a moment."), None)).await;
    }
    // Like /levelconfig disable: open voice sessions are dropped without credit, or the time spent while the module
    // was off would be paid once it is switched back on.
    if key == "leveling" && !enable {
        ctx.app.cache().invalidate(&format!("voice:start:{g}:*")).await;
    }
    // The invite snapshot is only warmed at startup: without this the first join after switching invites on is
    // unattributed.
    if key == "invites" && enable && bot_core::interaction_helpers::with_cached_guild(&ctx.app, guild_id, |_| ()).is_some() {
        tokio::spawn(feat_logging::invite_tracker::refresh_snapshot(&ctx.app, guild_id));
    }
    edit(&ctx, toggled_embed(key, enable)).await
}

/// The autocomplete choices: keys or labels containing the query; with known settings and `enable` / `disable`, the
/// modules the admin can act on come first (Discord shows at most 25 of the 42).
pub fn module_choices(settings: Option<&GuildSettings>, sub: Option<&str>, focused: &str) -> Vec<Choice> {
    let query = bot_util::js::lower(bot_util::js::trim(focused));
    let is_on = |k: &str| settings.is_some_and(|s| module_is_on(s, k));
    let mut keys: Vec<&str> = MODULE_NAMES
        .iter()
        .copied()
        .filter(|k| query.is_empty() || k.contains(query.as_str()) || bot_util::js::lower(info_of(k).label).contains(query.as_str()))
        .collect();
    if settings.is_some() && matches!(sub, Some("enable") | Some("disable")) {
        let enabling = sub == Some("enable");
        let actionable = |k: &&str| if enabling { !is_on(k) } else { is_on(k) };
        let (first, rest): (Vec<&str>, Vec<&str>) = keys.iter().copied().partition(actionable);
        keys = first.into_iter().chain(rest).collect();
    }
    keys.into_iter()
        .take(25)
        .map(|key| {
            let m = info_of(key);
            let state = match settings {
                Some(_) if is_on(key) => " — on",
                Some(_) => " — off",
                None => "",
            };
            Choice::new(format!("{} {} ({key}){state}", m.emoji, m.label), key)
        })
        .collect()
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    // Memo first; a cold load is bounded well inside the 3-second answer window.
    let settings = match ctx.guild_id() {
        Some(g) => ctx.app.settings().peek_or_load(&g.to_string(), 1000).await,
        None => None,
    };
    let sub = ctx.subcommand();
    let choices = module_choices(settings.as_deref(), sub.as_deref(), &ctx.focused_value());
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}
