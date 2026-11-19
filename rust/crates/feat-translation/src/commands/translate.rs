//! Port of `src/commands/utility/translate.js`: `/translate text | detect | languages`. A core command (never
//! gated) that also works in DMs.

use bot_core::command::CommandOption as Opt;
use bot_core::embed_builder::apply_guild_color;
use bot_core::interaction_helpers::{Choice, Payload, is_interaction_gone, respond_autocomplete, safe_reply};
use bot_core::prelude::*;
use bot_util::jsre::LazyRe;
use bot_util::lazy_re;
use serenity::builder::{CreateAllowedMentions, CreateEmbed, CreateEmbedFooter, CreateInteractionResponseFollowup, CreateInteractionResponseMessage, EditInteractionResponse};

use crate::translate_stats_service::{StatVia, record_translation};
use crate::translation_service::{
    EMBED_MAX_PARTS, EMBED_PART_MAX, ErrorCode, Provider, TRANSLATE_MAX_CHUNKS, TranslationError, clean_for_translation,
    describe_translation_error, detect_language, get_languages, has_translatable_text, is_language, is_supported_target, language_choices,
    language_name, load_language_support, provider_label, provider_name, resolve_language_input, split_for_embeds, suggest_language,
    translate_text, translation_paused, warm_language_support,
};

/// The command's data.
pub fn data() -> SlashCommand {
    SlashCommand::new("translate", "Translate text, detect its language or list the supported languages.")
        .option(
            Opt::sub_command("text", "Translate text into another language.")
                .option(Opt::string("text", "The text to translate").required(true).max_length(1500))
                .option(
                    Opt::string("target_language", "Target language code or name (e.g. es, ja, zh-tw). See /translate languages.")
                        .required(true)
                        .max_length(40)
                        .autocomplete(true),
                ),
        )
        .option(
            Opt::sub_command("detect", "Detect the language of a piece of text.")
                .option(Opt::string("text", "The text to analyse").required(true).max_length(1500)),
        )
        .option(Opt::sub_command("languages", "List all supported translation language codes."))
}

/// The command (core, MEMBER, allowed in DMs).
pub fn command() -> CommandDef {
    CommandDef::new("utility", data(), None, execute)
        .permission_level(PermissionLevel::Member)
        .allow_in_dms()
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

/// Autocomplete for `target_language`.
pub async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    if provider_name(&ctx.app) == Provider::LibreTranslate {
        warm_language_support(&ctx.app);
    }
    let choices: Vec<Choice> = language_choices(&ctx.app, &ctx.focused_value()).into_iter().map(|c| Choice::new(c.name, c.value)).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

fn ephemeral_embed(embed: CreateEmbed) -> CreateInteractionResponseMessage {
    CreateInteractionResponseMessage::new().embed(embed).ephemeral(true)
}

/// `replyFailure(interaction, embed)`: replaces the public "thinking..." placeholder with a failure only the caller
/// sees.
pub async fn reply_failure(ctx: &CommandCtx, embed: CreateEmbed) {
    let mut removed = false;
    let attempt = async {
        ctx.delete_reply().await?;
        removed = true;
        ctx.follow_up(CreateInteractionResponseFollowup::new().embed(embed.clone()).ephemeral(true)).await?;
        Ok::<(), bot_core::interaction::InteractionError>(())
    }
    .await;
    let Err(err) = attempt else { return };
    if is_interaction_gone(&err) {
        bot_util::log_debug!(format!("/translate: private failure not sent, interaction gone ({})", err.code().map(|c| c.to_string()).unwrap_or_default()));
        return;
    }
    bot_util::log_warn!(format!("/translate: could not send the failure privately: {err}"));
    if removed {
        safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true)).await;
        return;
    }
    if let Err(edit_err) = ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await
        && !is_interaction_gone(&edit_err)
    {
        bot_util::log_warn!(format!("/translate: could not edit the reply either: {edit_err}"));
    }
}

fn log_failure(where_: &str, err: &TranslationError, log: &str) {
    match log {
        "warn" => bot_util::log_warn!(format!(
            "/translate {where_}: {} {} {}",
            err.code.as_str(),
            err.status.map(|s| s.to_string()).unwrap_or_default(),
            err.message
        )),
        "error" => bot_util::log_error!(format!("/translate {where_} failed"), err.message.clone()),
        _ => {}
    }
}

static WS_RUN: LazyRe = lazy_re!(r"\s+", "");

async fn settings_of(ctx: &CommandCtx) -> Option<Settings> {
    match ctx.guild_id() {
        Some(g) => ctx.app.settings().get_settings_safe(&g.to_string()).await,
        None => None,
    }
}

async fn execute_text(ctx: CommandCtx) -> BotResult {
    let text = bot_util::js::trim(&ctx.get_string("text").unwrap_or_default()).to_string();
    let input = ctx.get_string("target_language").unwrap_or_default();

    let Some(target) = resolve_language_input(&input) else {
        let hint = suggest_language(&input).map(|s| format!(" Did you mean `{s}` ({})?", language_name(Some(s)))).unwrap_or_default();
        let desc = format!(
            "`{}` is not a language I know.{hint} Use `/translate languages` to see every code.",
            bot_util::text::clamp_text(&input, 40)
        );
        ctx.reply(ephemeral_embed(error_embed("Invalid Language", Some(&desc), None))).await?;
        return Ok(());
    };
    if !has_translatable_text(&clean_for_translation(&text)) {
        ctx.reply(ephemeral_embed(error_embed("Nothing to Translate", Some("That text has no words to translate."), None))).await?;
        return Ok(());
    }
    if translation_paused(&ctx.app) {
        ctx.reply(ephemeral_embed(describe_translation_error(&ctx.app, Some(&TranslationError::of(ErrorCode::Paused))).embed)).await?;
        return Ok(());
    }

    ctx.defer_reply(false).await?;

    load_language_support(&ctx.app, false).await; // never fails
    if is_supported_target(&ctx.app, target) == Some(false) {
        let desc = format!(
            "{} (`{target}`) isn't supported by this bot's translation backend ({}). Use `/translate languages` to see what is available.",
            language_name(Some(target)),
            provider_label(&ctx.app)
        );
        reply_failure(&ctx, error_embed("Language Not Supported", Some(&desc), None)).await;
        return Ok(());
    }

    // The network call is alone in the match, so a failure while building the reply is never reported as "could
    // not reach the service".
    let result = match translate_text(&ctx.app, &text, target, "auto", TRANSLATE_MAX_CHUNKS).await {
        Ok(r) => r,
        Err(err) => {
            let d = describe_translation_error(&ctx.app, Some(&err));
            log_failure("text", &err, d.log);
            reply_failure(&ctx, d.embed).await;
            return Ok(());
        }
    };

    let parts = split_for_embeds(&result.text, EMBED_PART_MAX, EMBED_MAX_PARTS);
    if parts.is_empty() {
        reply_failure(&ctx, error_embed("Translation Failed", Some("The translation service returned an empty result."), None)).await;
        return Ok(());
    }

    let settings = settings_of(&ctx).await;
    let title = if parts.len() > 1 { format!("Translation (1/{})", parts.len()) } else { "Translation".to_string() };
    let mut first = apply_guild_color(info_embed(&title, Some(&parts[0]), None), settings.as_deref())
        .field("Source", result.detected.as_deref().map(|d| language_name(Some(d))).unwrap_or_else(|| "Unknown".into()), true)
        .field("Target", language_name(Some(target)), true);
    if result.detected.as_deref() == Some(target) {
        first = first.field("Note", format!("The text already looks like {}.", language_name(Some(target))), false);
    }
    let original = bot_util::text::clamp_text(&WS_RUN.replace_all(&text, " "), 100);
    first = first.footer(CreateEmbedFooter::new(format!("Original: {original}")));

    match ctx.edit_reply(EditInteractionResponse::new().embed(first)).await {
        Ok(_) => {
            if let Some(g) = ctx.guild_id() {
                let (app, g, c) = (ctx.app.clone(), g.to_string(), ctx.channel_id().to_string());
                let detected = result.detected.clone();
                let targets: Vec<String> = if detected.as_deref() == Some(target) { Vec::new() } else { vec![target.to_string()] };
                tokio::spawn(async move {
                    record_translation(&app, &g, &c, StatVia::Command, detected.as_deref(), &targets).await;
                });
            }
        }
        Err(err) => {
            if is_interaction_gone(&err) {
                bot_util::log_debug!(format!("/translate text: interaction gone ({})", err.code().map(|c| c.to_string()).unwrap_or_default()));
                return Ok(());
            }
            return Err(err.into());
        }
    }

    for (i, part) in parts.iter().enumerate().skip(1) {
        let embed = apply_guild_color(info_embed(&format!("Translation ({}/{})", i + 1, parts.len()), Some(part), None), settings.as_deref());
        if let Err(err) = ctx.follow_up(CreateInteractionResponseFollowup::new().embed(embed).allowed_mentions(CreateAllowedMentions::new())).await {
            if is_interaction_gone(&err) {
                bot_util::log_debug!(format!("/translate text: follow-up not sent, interaction gone ({})", err.code().map(|c| c.to_string()).unwrap_or_default()));
            } else {
                bot_util::log_warn!(format!("/translate text: follow-up failed: {err}"));
            }
        }
    }
    Ok(())
}

async fn execute_detect(ctx: CommandCtx) -> BotResult {
    let text = ctx.get_string("text").unwrap_or_default();
    if !has_translatable_text(&clean_for_translation(&text)) {
        ctx.reply(ephemeral_embed(error_embed("Nothing to Detect", Some("That text has no words I can analyse."), None))).await?;
        return Ok(());
    }
    if translation_paused(&ctx.app) {
        ctx.reply(ephemeral_embed(describe_translation_error(&ctx.app, Some(&TranslationError::of(ErrorCode::Paused))).embed)).await?;
        return Ok(());
    }

    ctx.defer_reply(false).await?;

    let found = match detect_language(&ctx.app, &text).await {
        Ok(f) => f,
        Err(err) => {
            let d = describe_translation_error(&ctx.app, Some(&err));
            log_failure("detect", &err, d.log);
            reply_failure(&ctx, d.embed).await;
            return Ok(());
        }
    };

    let Some(code) = found.code.clone() else {
        let embed = warn_embed(
            "Unknown Language",
            Some("I could not tell which language that is. Try a longer sentence; it may also be a language the translator does not have (`/translate languages`)."),
            None,
        );
        ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await?;
        return Ok(());
    };

    let settings = settings_of(&ctx).await;
    let mut embed = apply_guild_color(info_embed("Language Detected", Some(&format!("**{}** (`{code}`)", found.name)), None), settings.as_deref());
    if !is_language(&code) {
        embed = embed.footer(CreateEmbedFooter::new("Detected only: this language is not available as a translation target."));
    }
    ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await?;
    Ok(())
}

/// The `/translate languages` embed (without the guild colour): every language the backend offers, by name.
pub fn languages_embed(app: &App, support_loaded: bool) -> CreateEmbed {
    let mut entries = get_languages(app);
    entries.sort_by(|a, b| bot_util::locale_compare(a.1, b.1));
    let lines: Vec<String> = entries.iter().map(|(code, name)| format!("`{code}` — {name}")).collect();
    let footer = format!("{} languages · backend: {}", entries.len(), provider_label(app));
    let mut embed = info_embed("Supported Languages", Some(&bot_util::text::clamp_text(&lines.join("\n"), 4096)), Some(&footer)).field(
        "Also accepted",
        "Aliases: `iw`→`he`, `nb`→`no`, `zh-cn`→`zh`, `zh-hant`→`zh-tw`, `fil`→`tl`. Names work too, e.g. `Japanese`.",
        false,
    );
    if provider_name(app) == Provider::LibreTranslate && !support_loaded {
        embed = embed.field("Note", "Could not load the LibreTranslate server's language list; some of these may be unavailable.", false);
    }
    embed
}

async fn execute_languages(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(true).await?; // the LibreTranslate list may need a network call
    let support = load_language_support(&ctx.app, false).await; // never fails; None for Google or when unknown
    let settings = settings_of(&ctx).await;
    let embed = apply_guild_color(languages_embed(&ctx.app, support.is_some()), settings.as_deref());
    ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await?;
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    match ctx.subcommand().as_deref() {
        Some("detect") => execute_detect(ctx).await,
        Some("languages") => execute_languages(ctx).await,
        _ => execute_text(ctx).await,
    }
}
