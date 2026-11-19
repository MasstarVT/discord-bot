//! Port of `src/commands/community/application.js`: `/application`. Members apply with `apply`; admins build the
//! forms. Thin: the rules, the apply and review flows and the database access live in
//! [`crate::application_service`]. Mixed command: the base level is MEMBER (so `apply` is open to everyone) and every
//! other subcommand is ADMIN through subcommand permissions, so it carries no `default_member_permissions`.
//! `gated = false` so admins can set it up first; `apply` refuses while the module is off.

use bot_config::limits;
use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::interaction_helpers::{Choice, ConfirmOptions, Payload, await_confirm, cached_guild, check_assignable_role_cached, respond_autocomplete, safe_defer, safe_reply};
use bot_core::prelude::*;
use bot_db::ApplicationForm;
use bot_util::js::{trim, utf16_prefix};
use bot_util::text::{chunk_lines, clamp_text};
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, CommandDataOptionValue, RoleId};
use serenity::builder::CreateEmbed;

use crate::application_service::{
    Question, QuestionStyle, Refusal, RoleList, add_question, application as A, begin_application, change_role, create_form, delete_form,
    delete_message_quietly, find_form, find_forms_for_autocomplete, is_clear_word, list_forms, parse_questions, pending_count, post_panel,
    refresh_panel, refusal_embed, remove_question, update_form,
};

const TEXT_CHANNELS: [u8; 2] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT];

fn form_option() -> Opt {
    Opt::string("form", "The application form.").required(true).autocomplete(true).max_length(100)
}

fn review_option() -> Opt {
    Opt::channel("review_channel", "Where answers are posted for review.").channel_types(&TEXT_CHANNELS)
}

/// The command's data (`new SlashCommandBuilder()...`). The payload budget is 3000 characters.
pub fn data() -> SlashCommand {
    SlashCommand::new("application", "Apply for something here, or build application forms.")
        .option(Opt::sub_command("apply", "Fill in an application form.").option(form_option()))
        .option(
            Opt::sub_command_group("form", "Build application forms.")
                .option(
                    Opt::sub_command("create", "Create a form.")
                        .option(Opt::string("name", "The form name.").required(true).max_length(A::NAME_MAX as u16))
                        .option(Opt::string("description", "Shown on the panel.").max_length(A::DESCRIPTION_MAX as u16))
                        .option(review_option()),
                )
                .option(
                    Opt::sub_command("edit", "Change a form. Text options take \"none\" to clear.")
                        .option(form_option())
                        .option(Opt::string("name", "A new name.").max_length(A::NAME_MAX as u16))
                        .option(Opt::string("description", "Shown on the panel.").max_length(A::DESCRIPTION_MAX as u16))
                        .option(review_option())
                        .option(Opt::integer("cooldown_hours", "Hours before a denied member may apply again.").min_value(0).max_value(A::COOLDOWN_MAX_HOURS))
                        .option(Opt::boolean("enabled", "Whether members can apply."))
                        .option(Opt::string("accept_message", "DM on accept. Use {form} {user} {server}.").max_length(A::MESSAGE_MAX as u16))
                        .option(Opt::string("deny_message", "DM on deny. Use {form} {user} {server}.").max_length(A::MESSAGE_MAX as u16)),
                )
                .option(Opt::sub_command("delete", "Delete a form and its submissions.").option(form_option()))
                .option(Opt::sub_command("list", "List the forms."))
                .option(
                    Opt::sub_command("role", "Roles given on accept, or needed to apply.")
                        .option(form_option())
                        .option(Opt::string("list", "Which list.").required(true).choice("Given on accept", "accept").choice("Needed to apply", "required"))
                        .option(Opt::role("role", "The role.").required(true))
                        .option(Opt::boolean("remove", "Remove it instead of adding it.")),
                ),
        )
        .option(
            Opt::sub_command_group("question", "Edit the questions of a form.")
                .option(
                    Opt::sub_command("add", "Add a question.")
                        .option(form_option())
                        .option(Opt::string("label", "The question.").required(true).max_length(A::LABEL_MAX as u16))
                        .option(Opt::string("style", "Answer length (default short).").choice("Short answer", "SHORT").choice("Paragraph", "PARAGRAPH"))
                        .option(Opt::boolean("required", "Must be answered (default yes)."))
                        .option(Opt::string("placeholder", "Hint inside the box.").max_length(A::PLACEHOLDER_MAX as u16)),
                )
                .option(
                    Opt::sub_command("remove", "Remove a question.")
                        .option(form_option())
                        .option(Opt::string("question", "The question to remove.").required(true).autocomplete(true).max_length(100)),
                ),
        )
        .option(
            Opt::sub_command("panel", "Post a message with an Apply button.")
                .option(form_option())
                .option(Opt::channel("channel", "Where to post it (default: here).").channel_types(&TEXT_CHANNELS)),
        )
}

/// The command (`module 'applications'`, `gated = false`, MEMBER with ADMIN subcommands).
pub fn command() -> CommandDef {
    let mut def = CommandDef::new("community", data(), Some(ModuleKey::Applications), execute).gated(false).permission_level(PermissionLevel::Member);
    for path in ["form create", "form edit", "form delete", "form list", "form role", "question add", "question remove", "panel"] {
        def = def.subcommand_permission(path, PermissionLevel::Admin);
    }
    def.autocomplete(autocomplete)
}

// ── Messages ──────────────────────────────────────────────────────────────────

fn settings_unavailable() -> CreateEmbed {
    error_embed("Settings Unavailable", Some("Server settings could not be loaded. Please try again in a moment."), None)
}

fn could_not_save() -> CreateEmbed {
    error_embed("Could Not Save", Some("The change could not be saved. Please try again in a moment."), None)
}

fn cannot_use(reason: &str) -> CreateEmbed {
    error_embed("Cannot Use That Channel", Some(&format!("{reason} Pick another channel, or fix my permissions there.")), None)
}

/// `MSG[code]()` for the refusals a form write returns.
fn refusal_msg(r: &Refusal) -> CreateEmbed {
    match r {
        Refusal::NameTaken => error_embed("Name Taken", Some("Another form already has that name."), None),
        Refusal::FormLimit => error_embed("Form Limit Reached", Some(&format!("A server can have up to **{}** forms. Delete one first.", limits::APPLICATION_FORMS)), None),
        other => refusal_embed(other),
    }
}

/// The note appended to admin replies while the module is off.
pub const OFF_NOTE: &str = "\n\n⚠️ The **Applications** module is off, so members cannot apply yet. Turn it on with `/module enable applications`.";

/// `finish(interaction, embed)`: the final answer (overwrites the deferred reply); never pings.
async fn finish(ctx: &CommandCtx, embed: CreateEmbed) -> BotResult {
    safe_reply(&ctx.responder, Payload::embed(embed).no_mentions()).await;
    Ok(())
}

/// `offNote(guildId)`: the "module is off" note, or `""` (also when the settings cannot be loaded).
async fn off_note(app: &App, guild_id: &str) -> &'static str {
    match app.settings().get_settings_safe(guild_id).await {
        Some(s) if !is_module_enabled(Some(&s), Some(ModuleKey::Applications)) => OFF_NOTE,
        _ => "",
    }
}

/// `questionLines(questions)`: the numbered question list shown after a question changes.
pub fn question_lines(questions: &[Question]) -> String {
    if questions.is_empty() {
        return "*No questions yet. Add one with `/application question add`.*".into();
    }
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            format!(
                "**{}.** {}{}{}",
                i + 1,
                clamp_text(&q.label, 60),
                if q.required { "" } else { " *(optional)*" },
                if q.style == QuestionStyle::Paragraph { " ¶" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn guild_id_of(ctx: &CommandCtx) -> anyhow::Result<serenity::all::GuildId> {
    ctx.guild_id().ok_or_else(|| anyhow::anyhow!("/application outside a guild"))
}

async fn resolve_form(ctx: &CommandCtx) -> anyhow::Result<Option<ApplicationForm>> {
    let reference = ctx.get_string("form").ok_or_else(|| anyhow::anyhow!("Required option \"form\" not found."))?;
    let form = find_form(ctx.app.db(), &guild_id_of(ctx)?.to_string(), &reference).await?;
    if form.is_none() {
        finish(ctx, refusal_embed(&Refusal::FormNotFound)).await?;
    }
    Ok(form)
}

/// `usableChannel(interaction, option)`: `Ok(None)` when the option was not given, `Err(())` after answering that
/// the bot cannot post there.
async fn usable_channel(ctx: &CommandCtx, option: &str) -> anyhow::Result<Result<Option<ChannelId>, ()>> {
    let Some(picked) = ctx.get_channel_id(option) else { return Ok(Ok(None)) };
    let g = guild_id_of(ctx)?;
    // discord.js adds the resolved option channel to the cache when it builds the interaction: fetched (and cached).
    let usable = bot_core::interaction_helpers::check_channel_usable_fetch(&ctx.app, g, Some(picked), &bot_core::interaction_helpers::CHANNEL_POST_NEEDS).await;
    if !usable.ok {
        finish(ctx, cannot_use(usable.reason.as_deref().unwrap_or(""))).await?;
        return Ok(Err(()));
    }
    Ok(Ok(Some(picked)))
}

// ── /application form create | edit | delete | list | role ────────────────────

async fn run_form_create(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_id_of(&ctx)?.to_string();
    let name = trim(&ctx.get_string("name").unwrap_or_default()).to_string();
    if name.is_empty() {
        return finish(&ctx, error_embed("Name Needed", Some("Give the form a name."), None)).await;
    }
    let Ok(channel) = usable_channel(&ctx, "review_channel").await? else { return Ok(()) };
    let description = ctx.get_string("description").map(|d| trim(&d).to_string()).filter(|d| !d.is_empty());
    let result = create_form(&ctx.app, &g, &name, description.as_deref(), channel.map(|c| c.to_string()).as_deref()).await;
    match result {
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: create failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
        Ok(Err(r)) => return finish(&ctx, refusal_msg(&r)).await,
        Ok(Ok(_)) => {}
    }
    bot_util::log_info!(format!("[applications] {g}: form \"{name}\" created by {}", ctx.user().id));
    let mut todo = vec!["`/application question add` for each question"];
    if channel.is_none() {
        todo.push("`/application form edit review_channel:` to say where answers go");
    }
    todo.push("`/application panel` to post the Apply button");
    let todo: Vec<String> = todo.iter().map(|t| format!("• {t}")).collect();
    let text = format!("**{name}** is ready to set up:\n{}{}", todo.join("\n"), off_note(&ctx.app, &g).await);
    finish(&ctx, success_embed("Form Created", Some(&text), None)).await
}

async fn run_form_edit(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(form) = resolve_form(&ctx).await? else { return Ok(()) };
    let gid = guild_id_of(&ctx)?;
    let g = gid.to_string();
    let mut data = Map::new();
    let mut changed: Vec<&str> = Vec::new();

    if let Some(raw) = ctx.get_string("name") {
        let name = trim(&raw).to_string();
        if name.is_empty() {
            return finish(&ctx, error_embed("Name Needed", Some("A form needs a name."), None)).await;
        }
        data.insert("name".into(), json!(name));
        changed.push("name");
    }
    for (key, column, label) in [("description", "description", "description"), ("accept_message", "acceptMessage", "accept message"), ("deny_message", "denyMessage", "deny message")] {
        if let Some(raw) = ctx.get_string(key) {
            let t = trim(&raw);
            data.insert(column.into(), if is_clear_word(&raw) || t.is_empty() { Value::Null } else { json!(t) });
            changed.push(label);
        }
    }
    let Ok(channel) = usable_channel(&ctx, "review_channel").await? else { return Ok(()) };
    if let Some(c) = channel {
        data.insert("reviewChannelId".into(), json!(c.to_string()));
        changed.push("review channel");
    }
    if let Some(c) = ctx.get_integer("cooldown_hours") {
        data.insert("cooldownHours".into(), json!(c));
        changed.push("cooldown");
    }
    if let Some(e) = ctx.get_boolean("enabled") {
        data.insert("enabled".into(), json!(e));
        changed.push(if e { "opened" } else { "closed" });
    }
    if changed.is_empty() {
        return finish(&ctx, error_embed("Nothing To Change", Some("Pick at least one option to change."), None)).await;
    }
    let updated = match update_form(&ctx.app, &g, &form.id, data).await {
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: edit failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
        Ok(Err(r)) => return finish(&ctx, refusal_msg(&r)).await,
        Ok(Ok(f)) => f,
    };
    refresh_panel(&ctx.app, gid, &updated).await;
    finish(&ctx, success_embed("Form Updated", Some(&format!("**{}**: changed {}.", clamp_text(&updated.name, 80), changed.join(", "))), None)).await
}

async fn run_form_delete(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let Some(form) = resolve_form(&ctx).await? else { return Ok(()) };
    let gid = guild_id_of(&ctx)?;
    let g = gid.to_string();
    let pending = pending_count(ctx.app.db(), &form.id).await.unwrap_or(0);
    let waiting = if pending > 0 { format!(", including **{pending}** waiting for review") } else { String::new() };
    let ok = await_confirm(
        &ctx,
        ConfirmOptions {
            title: Some(format!("Delete \"{}\"?", clamp_text(&form.name, 80))),
            description: Some(format!("This deletes the form and all its submissions{waiting}. This cannot be undone.")),
            confirm_label: "Delete".into(),
            timeout_ms: 30_000.0,
            ..Default::default()
        },
    )
    .await;
    if !ok {
        return Ok(());
    }
    let deleted = match delete_form(ctx.app.db(), &g, &form.id).await {
        Ok(d) => d,
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: delete failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    if let Some(d) = deleted.as_ref()
        && let Some(m) = d.panel_message_id.as_deref()
    {
        delete_message_quietly(&ctx.app, gid, d.panel_channel_id.as_deref(), m).await;
    }
    bot_util::log_info!(format!("[applications] {g}: form \"{}\" deleted by {}", form.name, ctx.user().id));
    let embed = success_embed("Form Deleted", Some(&format!("**{}** and its submissions were deleted.", clamp_text(&form.name, 80))), None);
    let _ = ctx.edit_reply(Payload::embed(embed).components(Vec::new()).to_edit()).await;
    Ok(())
}

/// One line of `/application form list`.
pub fn form_list_line(guild_id: &str, f: &ApplicationForm, pending: i64) -> String {
    let count = parse_questions(&f.questions).len();
    let mut bits = vec![
        if f.enabled { "✅ open".to_string() } else { "⏸️ closed".to_string() },
        if count > 0 { format!("{count} question{}", if count == 1 { "" } else { "s" }) } else { "⚠️ no questions".to_string() },
        match f.review_channel_id.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => format!("review <#{c}>"),
            None => "⚠️ no review channel".to_string(),
        },
        format!("{pending} pending"),
    ];
    let mention = |ids: &[String]| ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" ");
    if !f.required_role_ids.is_empty() {
        bits.push(format!("needs {}", mention(&f.required_role_ids)));
    }
    if !f.accept_role_ids.is_empty() {
        bits.push(format!("gives {}", mention(&f.accept_role_ids)));
    }
    if let Some(m) = f.panel_message_id.as_deref().filter(|m| !m.is_empty()) {
        bits.push(format!("[panel](https://discord.com/channels/{guild_id}/{}/{m})", f.panel_channel_id.as_deref().unwrap_or("null")));
    }
    format!("**{}** · {}", clamp_text(&f.name, 60), bits.join(" · "))
}

async fn run_form_list(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let g = guild_id_of(&ctx)?.to_string();
    let Some(settings) = ctx.app.settings().get_settings_safe(&g).await else { return finish(&ctx, settings_unavailable()).await };
    let forms = match list_forms(ctx.app.db(), &g).await {
        Ok(f) => f,
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: list failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
    };
    let off = if is_module_enabled(Some(&settings), Some(ModuleKey::Applications)) { "" } else { OFF_NOTE };
    if forms.is_empty() {
        let text = format!("No forms yet. Create one with `/application form create`.{off}");
        return finish(&ctx, guild_embed(Some(&settings), Some("📝 Application forms"), Some(&text), None)).await;
    }
    let lines: Vec<String> = forms.iter().map(|f| form_list_line(&g, &f.form, f.pending)).collect();
    let title = format!("📝 Application forms ({}/{})", forms.len(), limits::APPLICATION_FORMS);
    let off_text = trim(off);
    let mut embed = guild_embed(Some(&settings), Some(&title), (!off_text.is_empty()).then_some(off_text), None);
    for (i, chunk) in chunk_lines(&lines, 1024).into_iter().enumerate() {
        embed = embed.field(if i == 0 { "Forms" } else { "\u{200b}" }, chunk, false);
    }
    finish(&ctx, embed).await
}

async fn run_form_role(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(form) = resolve_form(&ctx).await? else { return Ok(()) };
    let gid = guild_id_of(&ctx)?;
    let g = gid.to_string();
    let which = ctx.get_string("list").unwrap_or_default();
    let list = if which == "accept" { RoleList::Accept } else { RoleList::Required };
    let remove = ctx.get_boolean("remove").unwrap_or(false);
    let picked: RoleId = ctx.get_role_id("role").ok_or_else(|| anyhow::anyhow!("Required option \"role\" not found."))?;
    // discord.js adds the resolved option role to `guild.roles.cache` when it builds the interaction.
    crate::welcome_service::adopt_resolved_role(&ctx.app, gid, ctx.get_role("role").as_ref());
    let exists = cached_guild(&ctx.app, gid).is_some_and(|gd| gd.roles.contains_key(&picked));
    if !remove {
        if !exists || picked.to_string() == g {
            return finish(&ctx, error_embed("Cannot Use That Role", Some("@everyone cannot be used."), None)).await;
        }
        if list == RoleList::Accept {
            let check = check_assignable_role_cached(&ctx.app, gid, picked, ctx.member(), false);
            if !check.ok {
                let text = format!("{} Accepted members could not receive it.", check.reason.unwrap_or_default());
                return finish(&ctx, error_embed("Cannot Give That Role", Some(&text), None)).await;
            }
        }
    }
    let (updated, changed) = match change_role(&ctx.app, &g, &form.id, list, &picked.to_string(), remove).await {
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: role change failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
        Ok(Err(Refusal::RoleLimit)) => {
            return finish(&ctx, error_embed("Role Limit Reached", Some(&format!("A list holds up to **{}** roles.", limits::APPLICATION_ROLES)), None)).await;
        }
        Ok(Err(r)) => return finish(&ctx, refusal_msg(&r)).await,
        Ok(Ok(v)) => v,
    };
    let what = if list == RoleList::Accept { "given on accept" } else { "needed to apply" };
    if !changed {
        let text = format!("<@&{picked}> {} the {what} list.", if remove { "is not in" } else { "is already in" });
        return finish(&ctx, warn_embed("No Change", Some(&text), None)).await;
    }
    let ids = if list == RoleList::Accept { &updated.accept_role_ids } else { &updated.required_role_ids };
    let now = if ids.is_empty() { "*none*".to_string() } else { ids.iter().map(|id| format!("<@&{id}>")).collect::<Vec<_>>().join(" ") };
    let text = format!(
        "<@&{picked}> {} the roles {what} for **{}**.\nNow: {now}",
        if remove { "removed from" } else { "added to" },
        clamp_text(&form.name, 80)
    );
    finish(&ctx, success_embed("Form Updated", Some(&text), None)).await
}

// ── /application question add | remove ────────────────────────────────────────

async fn run_question_add(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(form) = resolve_form(&ctx).await? else { return Ok(()) };
    let gid = guild_id_of(&ctx)?;
    let g = gid.to_string();
    let label = utf16_prefix(trim(&ctx.get_string("label").unwrap_or_default()), A::LABEL_MAX).to_string();
    if label.is_empty() {
        return finish(&ctx, error_embed("Question Needed", Some("Write the question."), None)).await;
    }
    let placeholder = ctx.get_string("placeholder").map(|p| utf16_prefix(trim(&p), A::PLACEHOLDER_MAX).to_string()).filter(|p| !p.is_empty());
    let question = Question {
        label,
        style: if ctx.get_string("style").as_deref() == Some("PARAGRAPH") { QuestionStyle::Paragraph } else { QuestionStyle::Short },
        required: ctx.get_boolean("required").unwrap_or(true),
        placeholder,
    };
    let (updated, count) = match add_question(&ctx.app, &g, &form.id, question).await {
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: question add failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
        Ok(Err(Refusal::QuestionLimit)) => {
            let text = format!("A form holds up to **{}** questions.", limits::APPLICATION_QUESTIONS);
            return finish(&ctx, error_embed("Question Limit Reached", Some(&text), None)).await;
        }
        Ok(Err(Refusal::QuestionDup)) => return finish(&ctx, error_embed("Duplicate Question", Some("The form already has a question with that text."), None)).await,
        Ok(Err(r)) => return finish(&ctx, refusal_msg(&r)).await,
        Ok(Ok(v)) => v,
    };
    refresh_panel(&ctx.app, gid, &updated).await;
    let text = format!(
        "**{}** ({count}/{}):\n{}",
        clamp_text(&form.name, 80),
        limits::APPLICATION_QUESTIONS,
        question_lines(&parse_questions(&updated.questions))
    );
    finish(&ctx, success_embed("Question Added", Some(&text), None)).await
}

/// JavaScript `Number.parseInt(s, 10)` (`None` for NaN).
pub fn js_parse_int(s: &str) -> Option<i64> {
    let t = trim(s);
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().unwrap_or(i64::MAX);
    Some(if neg { -n } else { n })
}

async fn run_question_remove(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let Some(form) = resolve_form(&ctx).await? else { return Ok(()) };
    let gid = guild_id_of(&ctx)?;
    let g = gid.to_string();
    let number = js_parse_int(&ctx.get_string("question").unwrap_or_default());
    let (updated, removed) = match remove_question(&ctx.app, &g, &form.id, number).await {
        Err(e) => {
            bot_util::log_error!(format!("[applications] {g}: question remove failed"), bot_core::error::log_arg(&e));
            return finish(&ctx, could_not_save()).await;
        }
        Ok(Err(Refusal::QuestionNotFound)) => return finish(&ctx, error_embed("Question Not Found", Some("Pick a question from the list."), None)).await,
        Ok(Err(r)) => return finish(&ctx, refusal_msg(&r)).await,
        Ok(Ok(v)) => v,
    };
    refresh_panel(&ctx.app, gid, &updated).await;
    let text = format!(
        "Removed \"{}\" from **{}**:\n{}",
        clamp_text(&removed.label, 80),
        clamp_text(&form.name, 80),
        question_lines(&parse_questions(&updated.questions))
    );
    finish(&ctx, success_embed("Question Removed", Some(&text), None)).await
}

// ── /application panel ────────────────────────────────────────────────────────

async fn run_panel(ctx: CommandCtx) -> BotResult {
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    let Some(form) = resolve_form(&ctx).await? else { return Ok(()) };
    let gid = guild_id_of(&ctx)?;
    let g = gid.to_string();
    if form.review_channel_id.as_deref().is_none_or(str::is_empty) || parse_questions(&form.questions).is_empty() {
        return finish(&ctx, error_embed("Not Ready", Some("Set a review channel (`/application form edit`) and add at least one question first."), None)).await;
    }
    // As in the JS (`(await usableChannel(...)) ?? interaction.channel`), a refused channel option falls back to the
    // current channel after its refusal was answered.
    let channel = usable_channel(&ctx, "channel").await?.ok().flatten().unwrap_or(ctx.channel_id());
    if let Err(reason) = post_panel(&ctx.app, gid, &form, channel).await {
        return finish(&ctx, cannot_use(&reason)).await;
    }
    bot_util::log_info!(format!("[applications] {g}: panel for \"{}\" posted in {channel} by {}", form.name, ctx.user().id));
    let text = format!("The Apply button for **{}** is in <#{channel}>.{}", clamp_text(&form.name, 80), off_note(&ctx.app, &g).await);
    finish(&ctx, success_embed("Panel Posted", Some(&text), None)).await
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/// A string option of an autocomplete interaction (`options.getString(name)`), focused or not.
fn autocomplete_string(ctx: &AutocompleteCtx, name: &str) -> Option<String> {
    bot_core::interaction::leaf_options(&ctx.interaction.data.options).iter().find(|o| o.name == name).and_then(|o| match &o.value {
        CommandDataOptionValue::String(s) => Some(s.clone()),
        CommandDataOptionValue::Autocomplete { value, .. } => Some(value.clone()),
        _ => None,
    })
}

/// The numbered question choices (`{ name: "2. Label", value: "2" }`) matching what was typed.
pub fn question_choices(questions: &[Question], typed: &str) -> Vec<Choice> {
    let q = typed.to_lowercase();
    questions
        .iter()
        .enumerate()
        .map(|(i, question)| (format!("{}. {}", i + 1, question.label), (i + 1).to_string()))
        .filter(|(name, _)| q.is_empty() || name.to_lowercase().contains(&q))
        .map(|(name, value)| Choice::new(name, value))
        .collect()
}

async fn autocomplete_inner(ctx: &AutocompleteCtx) -> anyhow::Result<Vec<Choice>> {
    let Some(gid) = ctx.guild_id() else { return Ok(Vec::new()) };
    let g = gid.to_string();
    let (focused, typed) = ctx.focused().unwrap_or_default();
    if focused == "question" {
        let form = find_form(ctx.app.db(), &g, &autocomplete_string(ctx, "form").unwrap_or_default()).await?;
        let questions = form.map(|f| parse_questions(&f.questions)).unwrap_or_default();
        return Ok(question_choices(&questions, &typed));
    }
    if focused != "form" {
        return Ok(Vec::new());
    }
    let apply = ctx.subcommand().as_deref() == Some("apply");
    if apply {
        let budget = bot_core::interaction::ack_budget_ms(ctx.interaction.id);
        let settings = crate::welcome_service::settings_for_ack(&ctx.app, &g, budget).await;
        if !settings.is_some_and(|s| is_module_enabled(Some(&s), Some(ModuleKey::Applications))) {
            return Ok(Vec::new());
        }
    }
    let forms = find_forms_for_autocomplete(ctx.app.db(), &g, &typed, apply).await?;
    Ok(forms.into_iter().map(|f| Choice::new(format!("{}{}", if f.enabled { "" } else { "⏸️ " }, f.name), f.id)).collect())
}

/// `form` lists the forms (members only get open ones, and only while the module is on); `question` lists the
/// numbered questions of the form already picked. The router gates this exactly like execute.
async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let choices = match autocomplete_inner(&ctx).await {
        Ok(c) => c,
        Err(e) => {
            bot_util::log_debug!(format!("[applications] autocomplete failed: {e}"));
            Vec::new()
        }
    };
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    match (group.as_deref(), sub.as_str()) {
        (None, "apply") => {
            let reference = ctx.get_string("form").unwrap_or_default();
            begin_application(&ctx, &reference).await
        }
        (None, "panel") => run_panel(ctx).await,
        (Some("form"), "create") => run_form_create(ctx).await,
        (Some("form"), "edit") => run_form_edit(ctx).await,
        (Some("form"), "delete") => run_form_delete(ctx).await,
        (Some("form"), "list") => run_form_list(ctx).await,
        (Some("form"), "role") => run_form_role(ctx).await,
        (Some("question"), "add") => run_question_add(ctx).await,
        (Some("question"), "remove") => run_question_remove(ctx).await,
        _ => Ok(()),
    }
}
