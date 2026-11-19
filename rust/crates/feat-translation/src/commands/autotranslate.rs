//! Port of `src/commands/admin/autotranslate.js`: `/autotranslate enable:<bool> language1..5 [channel]` turns
//! auto-translation on or off for a channel. An admin config command (`gated = false`, public replies).

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Choice, cached_guild, check_channel_usable_fetch, respond_autocomplete};
use bot_core::new_targets::{NewOptions, new_channel_option, resolve_new_options};
use bot_core::prelude::*;
use serenity::all::{ChannelId, ChannelType, GuildId, Permissions};
use serenity::builder::{CreateEmbed, EditInteractionResponse};

use crate::translation_service::{
    Provider, get_guild_translate_config, is_supported_target, language_choices, language_name, load_language_support, normalize_target_list,
    provider_label, provider_name, refresh_translate_cache, save_translate_channel, suggest_language, warm_language_support, SaveOutcome,
};

const LANG_OPTIONS: [&str; 5] = ["language1", "language2", "language3", "language4", "language5"];
const CHANNEL_TYPES: [u8; 7] = [
    channel_type::GUILD_TEXT,
    channel_type::GUILD_ANNOUNCEMENT,
    channel_type::GUILD_VOICE,
    channel_type::GUILD_STAGE_VOICE,
    channel_type::PUBLIC_THREAD,
    channel_type::PRIVATE_THREAD,
    channel_type::ANNOUNCEMENT_THREAD,
];
const STALE_CHECKS_PER_RUN: usize = 10;

/// What the bot needs to post translations (replies need Read Message History).
pub const NEEDS: [&str; 4] = ["ViewChannel", "SendMessages", "EmbedLinks", "ReadMessageHistory"];

/// The command's data.
pub fn data() -> SlashCommand {
    let lang = |name: &str, desc: &str| Opt::string(name, desc).max_length(40).autocomplete(true);
    SlashCommand::new("autotranslate", "Set up auto-translation for a channel (up to 5 target languages).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(Opt::boolean("enable", "Turn auto-translation on or off for the channel.").required(true))
        // not required, so turning it off needs no language
        .option(lang("language1", "Target language code or name (e.g. es, ja, zh-tw). Not needed to turn off."))
        .option(lang("language2", "Another target language (optional)."))
        .option(lang("language3", "Another target language (optional)."))
        .option(lang("language4", "Another target language (optional)."))
        .option(lang("language5", "Another target language (optional)."))
        .option(Opt::channel("channel", "Channel to configure (default: this channel).").channel_types(&CHANNEL_TYPES))
        .option(new_channel_option("channel", "channel"))
}

/// The command (`module 'translation'`, `gated = false`, ADMIN).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Translation), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    if provider_name(&ctx.app) == Provider::LibreTranslate {
        warm_language_support(&ctx.app);
    }
    let choices: Vec<Choice> = language_choices(&ctx.app, &ctx.focused_value()).into_iter().map(|c| Choice::new(c.name, c.value)).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

fn kind_code(kind: ChannelType) -> u8 {
    u8::from(kind)
}

/// The "Auto-translate channels (N)" field. Rows of channels Discord reports as deleted (10003) are removed here, the
/// only place such rows can be noticed. A cache miss alone never deletes anything. `None` when it cannot be built.
pub async fn channels_field(app: &App, guild_id: GuildId) -> Option<(String, String)> {
    let g = guild_id.to_string();
    let result: anyhow::Result<(String, String)> = async {
        let mut config = get_guild_translate_config(app, &g).await?;
        if let Some(guild) = cached_guild(app, guild_id) {
            let stale: Vec<String> = config
                .keys()
                .filter(|id| {
                    id.parse::<u64>().ok().filter(|v| *v != 0).is_none_or(|c| bot_core::permissions::find_channel(&guild, ChannelId::new(c)).is_none())
                })
                .take(STALE_CHECKS_PER_RUN)
                .cloned()
                .collect();
            let mut removed = 0;
            for channel_id in stale {
                // `guild.channels.fetch(id)`: caches what it finds; only Discord's 10003 means the channel is gone.
                let gone = match channel_id.parse::<u64>().ok().filter(|v| *v != 0) {
                    Some(c) => matches!(
                        bot_core::interaction_helpers::fetch_channel(app, guild_id, ChannelId::new(c)).await,
                        Err(bot_core::interaction_helpers::FetchChannelError::Rest(e)) if bot_core::discord::error_code(&e) == Some(10003)
                    ),
                    None => false,
                };
                if gone {
                    bot_db::delete_where(&bot_db::TRANSLATE_CHANNEL_INFO, [("guildId", serde_json::json!(g)), ("channelId", serde_json::json!(channel_id))])?
                        .execute(app.db())
                        .await?;
                    removed += 1;
                }
            }
            if removed > 0 {
                refresh_translate_cache(app, &g).await;
                config = get_guild_translate_config(app, &g).await?;
            }
        }
        let lines: Vec<String> = config.iter().map(|(channel_id, langs)| format!("<#{channel_id}> → {}", langs.join(", "))).collect();
        let value = bot_util::text::join_with_limit(&lines, "\n", 1024, bot_util::text::default_more);
        Ok((format!("Auto-translate channels ({})", lines.len()), if value.is_empty() { "None".to_string() } else { value }))
    }
    .await;
    match result {
        Ok(f) => Some(f),
        Err(e) => {
            bot_util::log_warn!(format!("autotranslate: could not list the configured channels: {e}"));
            None
        }
    }
}

async fn with_field(app: &App, guild_id: GuildId, embed: CreateEmbed) -> CreateEmbed {
    match channels_field(app, guild_id).await {
        Some((name, value)) => embed.field(name, value, false),
        None => embed,
    }
}

async fn edit(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await?;
    Ok(())
}

async fn execute(mut ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    if !resolve_new_options(&mut ctx, NewOptions { required: &[], private: &[] }).await {
        return Ok(());
    }
    let guild_id = ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/autotranslate outside a guild"))?;
    // options.getChannel('channel') ?? interaction.channel
    let channel: Option<(ChannelId, u8)> = match ctx.get_channel_id("channel") {
        Some(id) => match ctx.get_channel("channel") {
            Some(c) => Some((c.id, kind_code(c.kind))),
            None => bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, id).map(|c| (c.id, kind_code(c.kind))),
        },
        None => match &ctx.interaction.channel {
            Some(c) => Some((c.id, kind_code(c.kind))),
            None => bot_core::interaction_helpers::cached_channel(&ctx.app, guild_id, ctx.channel_id()).map(|c| (c.id, kind_code(c.kind))),
        },
    };
    let Some((channel_id, _)) = channel.filter(|(_, kind)| CHANNEL_TYPES.contains(kind)) else {
        return edit(&ctx, error_embed("Unsupported Channel", Some("Auto-translation works in text, announcement, voice/stage chat and thread channels."), None)).await;
    };
    let enable = ctx.get_boolean("enable").unwrap_or(false);

    match run(&ctx, guild_id, channel_id, enable).await {
        Ok(embed) => edit(&ctx, embed).await,
        Err(e) => {
            bot_util::log_error!("autotranslate failed", bot_core::error::log_arg(&e));
            let _ = ctx
                .edit_reply(EditInteractionResponse::new().embed(error_embed(
                    "Something Went Wrong",
                    Some("Could not save the auto-translate settings. Please try again."),
                    None,
                )))
                .await;
            Ok(())
        }
    }
}

/// The reply of one run (enable or disable), or the failure the JS catch reports.
async fn run(ctx: &CommandCtx, guild_id: GuildId, channel_id: ChannelId, enable: bool) -> anyhow::Result<CreateEmbed> {
    let app = &ctx.app;
    let g = guild_id.to_string();
    let channel = channel_id.to_string();

    // ── Disable: the language options are ignored entirely, never validated ─────
    if !enable {
        let count = bot_db::delete_where(&bot_db::TRANSLATE_CHANNEL_INFO, [("guildId", serde_json::json!(g)), ("channelId", serde_json::json!(channel))])?
            .execute(app.db())
            .await?;
        refresh_translate_cache(app, &g).await;
        let embed = if count == 0 {
            warn_embed("Not Enabled", Some(&format!("Auto-translation is not turned on in <#{channel}>.")), None)
        } else {
            success_embed("Auto-Translation Disabled", Some(&format!("Auto-translation is now off in <#{channel}>.")), None)
        };
        // The module flag belongs to /module: with no channels left the hook only does a memo lookup.
        return Ok(with_field(app, guild_id, embed).await);
    }

    // ── Enable ───────────────────────────────────────────────────────────────────
    let inputs: Vec<String> = LANG_OPTIONS.iter().filter_map(|name| ctx.get_string(name)).filter(|v| !bot_util::js::trim(v).is_empty()).collect();
    if inputs.is_empty() {
        return Ok(error_embed("No Languages", Some("Pick at least one target language (language1–language5) to turn auto-translation on."), None));
    }

    let list = normalize_target_list(&inputs);
    if !list.invalid.is_empty() {
        let mut lines: Vec<String> = list
            .invalid
            .iter()
            .map(|input| {
                let hint = suggest_language(input).map(|s| format!(" Did you mean `{s}` ({})?", language_name(Some(s)))).unwrap_or_default();
                format!("• `{}` is not a language I know.{hint}", bot_util::text::clamp_text(bot_util::js::trim(input), 40))
            })
            .collect();
        lines.push("Use `/translate languages` to see every code.".to_string());
        return Ok(error_embed("Invalid Language", Some(&bot_util::text::clamp_text(&lines.join("\n"), 4096)), None));
    }

    // Backend support: None = unknown or Google.
    let support = load_language_support(app, false).await;
    let unsupported: Vec<String> = list.valid.iter().filter(|c| is_supported_target(app, c) == Some(false)).cloned().collect();
    let targets: Vec<String> = list.valid.iter().filter(|c| is_supported_target(app, c) != Some(false)).cloned().collect();
    if targets.is_empty() {
        let desc = format!(
            "None of those languages are supported by this bot's translation backend ({}). Use `/translate languages` to see the supported list.",
            provider_label(app)
        );
        return Ok(error_embed("Language Not Supported", Some(&desc), None));
    }

    // Replies need Read Message History. checkChannelUsable's reason already names the channel.
    // The JS checks the option's channel object; an uncached one (an archived thread, say) is fetched first.
    let usable = check_channel_usable_fetch(app, guild_id, Some(channel_id), &NEEDS).await;
    if !usable.ok {
        let reason = usable.reason.clone().unwrap_or_else(|| format!("I need **{}** there.", usable.missing.join(", ")));
        return Ok(error_embed("Missing Permissions", Some(&format!("I can't post translations in <#{channel}>. {reason}")), None));
    }

    app.settings().ensure_guild_settings(&g).await?; // the parent row first
    // The cap check and the insert run under one per-guild lock, shared with the dashboard.
    if let SaveOutcome::Limit { count } = save_translate_channel(app, &g, &channel, &targets, true).await? {
        let desc = format!(
            "This server already has {count} auto-translate channels (max {}). Turn one off first.",
            bot_config::tables::limits::TRANSLATE_CHANNELS
        );
        return Ok(error_embed("Limit Reached", Some(&desc), None));
    }

    let mut module_turned_on = false;
    let settings = app.settings().get_settings(&g).await?;
    if !settings.translation_enabled {
        app.settings().update_settings(&g, SettingsPatch::new().set("translationEnabled", true)).await?;
        module_turned_on = true;
    }
    refresh_translate_cache(app, &g).await;

    let list_text = targets.iter().map(|code| format!("{} (`{code}`)", language_name(Some(code)))).collect::<Vec<_>>().join(", ");
    let desc = bot_util::text::clamp_text(&format!("Messages in <#{channel}> will be translated to: {list_text}."), 4096);
    let mut embed = success_embed("Auto-Translation Enabled", Some(&desc), None);
    if !unsupported.is_empty() {
        let codes: Vec<String> = unsupported.iter().map(|c| format!("`{c}`")).collect();
        let value = format!("{} — not supported by {}.", bot_util::text::join_with_limit(&codes, ", ", 900, bot_util::text::default_more), provider_label(app));
        embed = embed.field("Skipped", value, false);
    }
    if support.is_none() && provider_name(app) == Provider::LibreTranslate {
        embed = embed.field("Note", "Could not check which languages the LibreTranslate server supports; unsupported ones will be skipped.", false);
    }
    if module_turned_on {
        embed = embed.field("Module", "The **Auto-Translate** module was off and is now on.", false);
    }
    Ok(with_field(app, guild_id, embed).await)
}
