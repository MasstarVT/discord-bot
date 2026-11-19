//! Port of `src/commands/admin/automation.js`: `/automation`, Automations as When -> If -> Do rules, from presets or
//! built step by step, with JSON export / import. Thin: data access and logic live in [`crate::automation_service`].
//! An ADMIN config command (`gated = false`): replies are public, except `delete`, whose confirm dialog is private.

use std::collections::HashMap;

use bot_core::command::{CommandOption as Opt, channel_type};
use bot_core::embed_builder::guild_embed;
use bot_core::interaction::InteractionError;
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, await_confirm, cached_guild, resolve_channel, check_assignable_role, check_channel_usable_for, channel_usable_verdict, is_interaction_gone,
    respond_autocomplete, safe_defer,
};
use bot_core::new_targets::{NewOptions, new_channel_option, new_role_option, resolve_new_options};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::permissions::{find_channel, permission_names};
use bot_core::prelude::*;
use bot_config::limits;
use bot_util::text::{chunk_lines, clamp_text, format_number};
use serde_json::{Map, Value, json};
use serenity::all::{ChannelId, ChannelType, Guild, GuildChannel, GuildId, Member, Permissions};
use serenity::builder::{CreateAttachment, CreateEmbed, CreateInteractionResponseFollowup, EditInteractionResponse};

use crate::automation_service::{self as auto, AutomationError, AutomationInput, ErrorCode, ErrorKind, Mutation, ParamsResult, error_texts};

const TRIGGER_CHOICES: [(&str, &str); 11] = [
    ("Message sent", "MESSAGE_SENT"),
    ("Message deleted", "MESSAGE_DELETED"),
    ("Message edited", "MESSAGE_EDITED"),
    ("Button clicked", "BUTTON_CLICKED"),
    ("Reaction added", "REACTION_ADDED"),
    ("Role gained", "ROLE_GAINED"),
    ("Role lost", "ROLE_LOST"),
    ("Joined voice", "VOICE_JOINED"),
    ("Left voice", "VOICE_LEFT"),
    ("Thread created", "THREAD_CREATED"),
    ("Member joined", "MEMBER_JOINED"),
];
const PRESET_CHOICES: [(&str, &str); 4] = [("Welcomer", "welcomer"), ("Starboard", "starboard"), ("Thread Joiner", "thread-joiner"), ("Commands Only", "commands-only")];
const PRESET_CHANNEL_TYPES: [u8; 4] = [channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT, channel_type::GUILD_FORUM, channel_type::GUILD_MEDIA];
const TARGET_CHANNEL_TYPES: [u8; 7] = [
    channel_type::GUILD_TEXT,
    channel_type::GUILD_ANNOUNCEMENT,
    channel_type::GUILD_VOICE,
    channel_type::GUILD_STAGE_VOICE,
    channel_type::PUBLIC_THREAD,
    channel_type::PRIVATE_THREAD,
    channel_type::ANNOUNCEMENT_THREAD,
];

fn name_opt() -> Opt {
    Opt::string("name", "Automation").required(true).autocomplete(true).max_length(32)
}
fn new_name_opt() -> Opt {
    Opt::string("name", "Name (a-z, 0-9, - and _)").required(true).max_length(32)
}
fn cooldown_opt() -> Opt {
    Opt::integer("cooldown", "Cooldown per user in seconds").min_value(0).max_value(604800)
}
fn index_opt(what: &str, max: usize) -> Opt {
    Opt::integer("index", format!("{what} number")).required(true).min_value(1).max_value(max as f64).autocomplete(true)
}

/// The command's data (`new SlashCommandBuilder()...`).
pub fn data() -> SlashCommand {
    let mut preset = Opt::string("preset", "Preset recipe").required(true);
    for (n, v) in PRESET_CHOICES {
        preset = preset.choice(n, v);
    }
    let mut trigger = Opt::string("trigger", "When…").required(true);
    for (n, v) in TRIGGER_CHOICES {
        trigger = trigger.choice(n, v);
    }
    SlashCommand::new("automation", "Automations: When → If → Do rules (MEE6 Automations)")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("create", "Create from a preset")
                .option(preset)
                .option(new_name_opt())
                .option(Opt::channel("channel", "Post/repost here (Thread Joiner, Commands Only: watch here)").channel_types(&PRESET_CHANNEL_TYPES))
                .option(new_channel_option("channel", "channel"))
                .option(Opt::string("message", "Welcomer: message").max_length(2000))
                .option(Opt::role("role", "Welcomer: role to give (Commands Only: staff that may talk)"))
                .option(new_role_option("role"))
                .option(Opt::string("emoji", "Starboard: emoji (default ⭐)").max_length(64))
                .option(Opt::integer("threshold", "Starboard: threshold (default 3)").min_value(1).max_value(1000))
                .option(Opt::user("user", "Thread Joiner: member to add")),
        )
        .option(Opt::sub_command("create-blank", "Create an empty automation").option(new_name_opt()).option(trigger).option(cooldown_opt()))
        .option(
            Opt::sub_command("edit", "Rename or change the cooldown")
                .option(name_opt())
                .option(Opt::string("rename", "New name").max_length(32))
                .option(cooldown_opt()),
        )
        .option(
            Opt::sub_command_group("condition", "If: conditions (all must pass)")
                .option(
                    Opt::sub_command("add", "Add a condition")
                        .option(name_opt())
                        .option(Opt::string("type", "Condition type").required(true).autocomplete(true).max_length(32))
                        .option(Opt::channel("channel", "Channel or category"))
                        .option(Opt::role("role", "Role"))
                        .option(Opt::user("user", "User"))
                        .option(Opt::string("text", "Words (comma-separated), regex, emoji or mentions").max_length(1000))
                        .option(Opt::integer("number", "Minimum reaction count").min_value(1).max_value(1000))
                        .option(Opt::string("button", "Automation button").autocomplete(true).max_length(64)),
                )
                .option(Opt::sub_command("remove", "Remove a condition").option(name_opt()).option(index_opt("Condition", limits::AUTOMATION_CONDITIONS))),
        )
        .option(
            Opt::sub_command_group("action", "Do: actions (run in order)")
                .option(
                    Opt::sub_command("add", "Add an action")
                        .option(name_opt())
                        .option(Opt::string("type", "Action type").required(true).autocomplete(true).max_length(32))
                        .option(Opt::channel("channel", "Target channel (default: where it happened)").channel_types(&TARGET_CHANNEL_TYPES))
                        .option(new_channel_option("channel", "channel"))
                        .option(Opt::string("text", "Message, thread name, or role/user mentions").max_length(2000))
                        .option(Opt::role("role", "Role to give or take"))
                        .option(new_role_option("role"))
                        .option(Opt::user("user", "Member to add to the thread"))
                        .option(Opt::string("emoji", "Emoji").max_length(64))
                        .option(Opt::integer("amount", "Coins or XP").min_value(1).max_value(1000000))
                        .option(Opt::string("embed", "Saved embed").autocomplete(true).max_length(32))
                        .option(Opt::string("button_label", "Button label").max_length(80))
                        .option(Opt::string("button_url", "Button link (empty = automation button)").max_length(512)),
                )
                .option(Opt::sub_command("remove", "Remove an action").option(name_opt()).option(index_opt("Action", limits::AUTOMATION_ACTIONS))),
        )
        .option(Opt::sub_command("list", "List automations"))
        .option(Opt::sub_command("info", "Show an automation").option(name_opt()))
        .option(Opt::sub_command("enable", "Turn an automation on").option(name_opt()))
        .option(Opt::sub_command("disable", "Turn an automation off").option(name_opt()))
        .option(Opt::sub_command("delete", "Delete an automation").option(name_opt()))
        .option(Opt::sub_command("export", "Download as JSON").option(name_opt()))
        .option(
            Opt::sub_command("import", "Create or replace from JSON")
                .option(Opt::attachment("file", "JSON file (max 32 KB)").required(true))
                .option(Opt::string("name", "Name (default: from the file)").max_length(32))
                .option(Opt::boolean("overwrite", "Replace if the name exists")),
        )
}

/// The command (`module 'automations'`, `gated = false`, ADMIN; guild-only).
pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Automations), execute)
        .gated(false)
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Replies ──────────────────────────────────────────────────────────────────

/// The note added while the module is off.
pub const MODULE_OFF_LINE: &str = "\n\n⚠️ The Automations module is off, so nothing runs. Turn it on with `/module enable automations`.";

fn off_line(name: &str) -> String {
    format!("\nThis automation is **off**. Turn it on with `/automation enable name:{name}`.")
}

fn ignored_line(ignored: &[String]) -> String {
    if ignored.is_empty() {
        return String::new();
    }
    let list: Vec<String> = ignored.iter().map(|n| format!("`{n}`")).collect();
    format!("\nIgnored for this type: {}.", list.join(", "))
}

fn err(title: &str, text: &str) -> CreateEmbed {
    error_embed(title, Some(text), None)
}

fn e_name() -> CreateEmbed {
    err("Invalid Name", error_texts::NAME)
}
fn e_not_found(name: &str) -> CreateEmbed {
    err("Automation Not Found", &error_texts::not_found(name))
}
fn e_taken(name: &str, extra: &str) -> CreateEmbed {
    err("Name Taken", &format!("{}{extra}", error_texts::taken(name)))
}
fn e_limit() -> CreateEmbed {
    err("Limit Reached", &error_texts::limit())
}
fn e_nothing() -> CreateEmbed {
    err("Nothing To Change", "Give at least one of the optional values.")
}
fn e_perms(missing: &[String], channel: Option<ChannelId>, reason: Option<&str>) -> CreateEmbed {
    let text = if !missing.is_empty() {
        let where_ = channel.map_or_else(|| "that channel".to_string(), |c| format!("<#{c}>"));
        format!("I need **{}** in {where_} for that.", permission_names(missing).join(", "))
    } else {
        reason.unwrap_or("I cannot use that channel.").to_string()
    };
    err("Missing Permissions", &text)
}
fn e_reward(kind: &str) -> CreateEmbed {
    let xp = kind == "xp";
    err(
        "Not Available",
        &format!(
            "{} rewards need the {} module. Turn it on with `/module enable {}`.",
            if xp { "XP" } else { "Coin" },
            if xp { "Levels" } else { "Economy" },
            if xp { "leveling" } else { "economy" }
        ),
    )
}
fn e_no_access(text: &str) -> CreateEmbed {
    err("No Access", text)
}
fn e_db() -> CreateEmbed {
    err("Something Went Wrong", "I could not do that right now. Please try again in a moment.")
}

/// The state of one `/automation` run (`ctx` in the JS).
struct Run {
    ctx: CommandCtx,
    guild_id: GuildId,
    guild: Guild,
    settings: Option<Settings>,
    module_off: bool,
    /// The name a unique violation reports.
    name: Option<String>,
    taken_hint: String,
    /// `'condition'` or `'action'` (the NOSUCH title).
    remove_kind: &'static str,
}

impl Run {
    fn g(&self) -> String {
        self.guild_id.to_string()
    }

    fn member(&self) -> Option<&Member> {
        self.ctx.member()
    }

    fn settings_row(&self) -> Option<&bot_db::GuildSettings> {
        self.settings.as_deref()
    }

    /// `send(interaction, embed)`: the editReply that never fails for an interaction that is gone.
    async fn send(&self, embed: CreateEmbed) -> BotResult {
        self.send_edit(EditInteractionResponse::new().embed(embed)).await
    }

    async fn send_edit(&self, edit: EditInteractionResponse) -> BotResult {
        match self.ctx.edit_reply(edit).await {
            Ok(_) => Ok(()),
            Err(e) if is_interaction_gone(&e) => {
                bot_util::log_debug!(format!("automations: reply dropped ({})", e.code().map_or("undefined".into(), |c| c.to_string())));
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The shared tail of a success description.
    fn tail(&self, disabled_name: Option<&str>, ignored: &[String]) -> String {
        format!("{}{}{}", disabled_name.map(off_line).unwrap_or_default(), ignored_line(ignored), if self.module_off { MODULE_OFF_LINE } else { "" })
    }

    fn module_off_line(&self) -> &'static str {
        if self.module_off { MODULE_OFF_LINE } else { "" }
    }

    /// `nameFrom(ctx)`: the normalised `name` option (remembered for a unique violation).
    fn name_from(&mut self) -> Option<String> {
        let name = auto::normalize_automation_name(&self.ctx.get_string("name").unwrap_or_default());
        if let Some(n) = &name {
            self.name = Some(n.clone());
        }
        name
    }

    async fn load_row(&self, name: &str) -> anyhow::Result<Option<auto::AutomationRecord>> {
        let row = auto::get_automation_by_name(self.ctx.app.db(), &self.g(), name).await?;
        if row.is_none() {
            self.send(e_not_found(name)).await?;
        }
        Ok(row)
    }

    /// The `guardRoles` check moved into a mutator: the guild and the member, cloned.
    fn guard(&self) -> Guard {
        Guard { guild: self.guild.clone(), member: self.member().cloned() }
    }

    fn summary_of(&self, row: &auto::AutomationRecord) -> String {
        summary_of(row, Some(&self.guild), self.settings_row())
    }
}

/// `summaryOf(row, { guild, settings })`.
pub fn summary_of(row: &auto::AutomationRecord, guild: Option<&Guild>, settings: Option<&bot_db::GuildSettings>) -> String {
    let conds: Vec<String> = row.conditions.iter().map(|c| auto::describe_condition(c, guild)).collect();
    let acts: Vec<String> = row.actions.iter().map(|a| auto::describe_action(a, guild, settings)).collect();
    let conds = if conds.is_empty() { "always".to_string() } else { conds.join(" · ") };
    let acts = if acts.is_empty() { "nothing yet".to_string() } else { acts.join(" · ") };
    let label = auto::trigger_label(&row.trigger).unwrap_or("");
    clamp_text(&format!("**{}** — {label}\n**If:** {conds}\n**Do:** {acts}", row.name), 1500)
}

/// `guardRoles(ctx, cur)`: an automation that gives or takes a role at or above the admin's highest role cannot be
/// changed by that admin (the guild owner is exempt). Runs on the locked row inside every mutation.
#[derive(Clone)]
struct Guard {
    guild: Guild,
    member: Option<Member>,
}

impl Guard {
    fn check(&self, cur: &auto::AutomationRecord) -> Result<(), AutomationError> {
        if !cur.actions.iter().any(|a| a.get("roleIds").and_then(Value::as_array).is_some_and(|r| !r.is_empty())) {
            return Ok(());
        }
        match auto::import_role_hierarchy_problem(&self.guild, self.member.as_ref(), &cur.actions) {
            Some(problem) => Err(AutomationError::with_code(
                format!("{problem}\nThis automation gives or takes a role at or above your highest role, so you cannot change it."),
                ErrorCode::Hierarchy,
            )),
            None => Ok(()),
        }
    }
}

/// The embed for a failed option mapping.
fn embed_for_params(p: &ParamsResult) -> CreateEmbed {
    let error = p.error.as_deref().unwrap_or("");
    match p.error_kind {
        Some(ErrorKind::Perms) => e_perms(&p.missing, p.channel, Some(error)),
        Some(ErrorKind::Role) => err("Role Not Allowed", error),
        Some(ErrorKind::Ping) => err("Role Not Pingable", error),
        Some(ErrorKind::Reward) => e_reward(p.reward_kind.unwrap_or("coins")),
        Some(ErrorKind::Embed) => {
            err("Embed Not Found", &format!("There is no saved embed called **{}**. See `/embed list`.", p.embed_name.as_deref().unwrap_or("")))
        }
        _ => err("Invalid Value", error),
    }
}

/// `embedForError(err, ctx)`: the embed for an error the service threw, or `None` for an unexpected one.
fn embed_for_error(e: &anyhow::Error, run: &Run) -> Option<CreateEmbed> {
    if auto::is_unique_violation(e) {
        return Some(e_taken(run.name.as_deref().unwrap_or("?"), &run.taken_hint));
    }
    let ae = auto::as_automation_error(e)?;
    Some(match ae.code {
        ErrorCode::NotFound => err("Automation Not Found", &ae.message),
        ErrorCode::Limit => e_limit(),
        ErrorCode::Name => e_name(),
        ErrorCode::NoActions => err("No Actions", &ae.message),
        ErrorCode::NoSuch => err(if run.remove_kind == "condition" { "No Such Condition" } else { "No Such Action" }, &ae.message),
        ErrorCode::Hierarchy => err("Role Not Allowed", &ae.message),
        _ => err("Could Not Save", &ae.message),
    })
}

fn channel_kind(ch: &serenity::all::PartialChannel) -> ChannelType {
    ch.kind
}

fn opt_channel(run: &Run, name: &str) -> Option<auto::OptChannel> {
    let id = run.ctx.get_channel_id(name)?;
    let kind = find_channel(&run.guild, id).map(|c| c.kind).or_else(|| run.ctx.get_channel(name).map(|c| channel_kind(&c))).unwrap_or(ChannelType::Text);
    Some(auto::OptChannel { id, kind })
}

fn opt_user(run: &Run, name: &str) -> Option<auto::OptUser> {
    let u = run.ctx.get_user(name)?;
    Some(auto::OptUser { id: u.id, bot: u.bot })
}

// ── Subcommands ──────────────────────────────────────────────────────────────

fn commands_only_note(prefix: Option<&str>, staff_skipped: bool) -> String {
    let first = match prefix {
        None => "Every message there is deleted (prefix commands are off); slash commands still work.".to_string(),
        Some(p) => format!("Messages there that do not start with `{p}` are deleted; slash commands still work."),
    };
    let second = if staff_skipped {
        "Members with the listed roles are skipped."
    } else {
        "⚠️ Nobody is skipped, staff included: pass `role` or set moderator roles with `/modconfig modroles`."
    };
    format!("{first} {second} After changing the prefix or the moderator roles, create it again.")
}

fn is_text_like(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Text | ChannelType::News)
}

fn usable(run: &Run, ch: Option<&GuildChannel>, needs: &[&str]) -> bot_core::interaction_helpers::ChannelUsable {
    match ch {
        Some(c) => check_channel_usable_for(&run.ctx.app, run.guild_id, c, needs),
        None => channel_usable_verdict(None, None, needs),
    }
}

async fn create(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let preset = run.ctx.get_string("preset").unwrap_or_default();
    let channel = opt_channel(run, "channel");
    let role = run.ctx.get_role_id("role");
    let user = run.ctx.get_user("user");
    let message = run.ctx.get_string("message");
    let emoji_raw = run.ctx.get_string("emoji");
    let threshold = run.ctx.get_integer("threshold");
    let given: Vec<String> = ["channel", "message", "role", "emoji", "threshold", "user"].iter().filter(|n| run.ctx.has_option(n)).map(|n| n.to_string()).collect();
    let ignored: Vec<String> = given.into_iter().filter(|n| !auto::preset_options(&preset).contains(&n.as_str())).collect();
    // `guild.channels.cache.get(channel.id) ?? channel`: the option's channel object; an uncached one is fetched.
    let full_channel: Option<GuildChannel> = match channel {
        Some(c) => match find_channel(&run.guild, c.id) {
            Some(ch) => Some(ch.clone()),
            None => resolve_channel(&run.ctx.app, run.guild_id, c.id).await,
        },
        None => None,
    };
    let text_like = channel.is_some_and(|c| is_text_like(c.kind));
    let mut opts = auto::PresetOptions::default();
    let member = run.member().cloned();

    match preset.as_str() {
        "welcomer" => {
            let Some(ch) = channel else { return run.send(err("Channel Needed", "The Welcomer preset needs a channel to post in.")).await };
            if !text_like {
                return run.send(err("Wrong Channel Type", "Pick a text or announcement channel.")).await;
            }
            let check = usable(run, full_channel.as_ref(), &["ViewChannel", "SendMessages"]);
            if !check.ok {
                return run.send(e_perms(&check.missing, Some(ch.id), check.reason.as_deref())).await;
            }
            if let Some(role_id) = role {
                let bot = auto::bot_user_id(&run.ctx.app);
                let rc = check_assignable_role(&run.guild, run.guild.roles.get(&role_id), bot, member.as_ref(), false);
                if !rc.ok {
                    return run.send(err("Role Not Allowed", rc.reason.as_deref().unwrap_or_default())).await;
                }
            }
            if let Some(ping) = auto::role_ping_refusal(&run.guild, member.as_ref(), full_channel.as_ref(), message.as_deref().unwrap_or("")) {
                return run.send(err("Role Not Pingable", &ping)).await;
            }
            opts.channel_id = Some(ch.id.to_string());
            opts.message = message.as_deref().map(bot_util::js::trim).filter(|m| !m.is_empty()).map(str::to_string);
            opts.role_id = role.map(|r| r.to_string());
        }
        "starboard" => {
            let Some(ch) = channel else { return run.send(err("Channel Needed", "The Starboard preset needs a channel to repost to.")).await };
            if !text_like {
                return run.send(err("Wrong Channel Type", "Pick a text or announcement channel.")).await;
            }
            let check = usable(run, full_channel.as_ref(), &["ViewChannel", "SendMessages", "EmbedLinks"]);
            if !check.ok {
                return run.send(e_perms(&check.missing, Some(ch.id), check.reason.as_deref())).await;
            }
            let mut emoji = None;
            if let Some(raw) = emoji_raw.as_deref().filter(|r| !bot_util::js::trim(r).is_empty()) {
                match auto::parse_emoji_input(raw, auto::emoji_resolver_app(&run.ctx.app, Some(&run.guild))) {
                    Ok(e) => emoji = Some(e),
                    Err(e) => return run.send(err("Invalid Emoji", &e)).await,
                }
            }
            opts.channel_id = Some(ch.id.to_string());
            opts.emoji = emoji;
            opts.threshold = threshold;
        }
        "thread-joiner" => {
            let Some(u) = user else { return run.send(err("Member Needed", "The Thread Joiner preset needs a member to add to new threads.")).await };
            if run.ctx.get_member("user").is_none() {
                return run.send(err("Not A Member", "That user is not in this server.")).await;
            }
            if u.bot {
                return run.send(err("Not A Member", "Pick a person, not a bot.")).await;
            }
            opts.user_id = Some(u.id.to_string());
            opts.channel_id = channel.map(|c| c.id.to_string());
        }
        "commands-only" => {
            let Some(ch) = channel else { return run.send(err("Channel Needed", "The Commands Only preset needs the channel to keep clean.")).await };
            if !text_like {
                return run.send(err("Wrong Channel Type", "Pick a text or announcement channel.")).await;
            }
            let check = usable(run, full_channel.as_ref(), &["ViewChannel", "ManageMessages"]);
            if !check.ok {
                return run.send(e_perms(&check.missing, Some(ch.id), check.reason.as_deref())).await;
            }
            // Staff keep talking: the picked role plus the moderator roles (/modconfig modroles), as they are now.
            opts.channel_id = Some(ch.id.to_string());
            opts.prefix = feat_tags::tag_service::effective_prefix(run.settings.as_ref().map(|s| s.custom_prefix.as_str()));
            opts.exempt_role_ids = role.map(|r| r.to_string()).into_iter().chain(run.settings.as_ref().map(|s| s.mod_role_ids.clone()).unwrap_or_default()).collect();
        }
        _ => return run.send(err("Unknown Preset", "Pick a preset from the list.")).await,
    }

    let built = auto::build_preset_automation(&preset, &opts)?;
    if let Some(access) = auto::action_channel_access_problem(&run.guild, member.as_ref(), &built.actions, 1) {
        return run.send(e_no_access(&access)).await;
    }
    let input = AutomationInput {
        name: Some(name),
        trigger: built.trigger.clone(),
        conditions: built.conditions.clone(),
        actions: built.actions.clone(),
        enabled: true,
        cooldown_seconds: 0,
    };
    let row = auto::create_automation(&run.ctx.app, &run.g(), &input, &run.ctx.user().id.to_string(), None).await?;
    let note = if preset == "commands-only" {
        format!("\n\n{}", commands_only_note(opts.prefix.as_deref(), built.conditions.iter().any(|c| c.get("type").and_then(Value::as_str) == Some("HAS_NOT_ANY_ROLE"))))
    } else {
        String::new()
    };
    let text = format!("{}{note}{}", run.summary_of(&row), run.tail(None, &ignored));
    run.send(success_embed("Automation Created", Some(&text), None)).await
}

async fn create_blank(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let trigger = run.ctx.get_string("trigger").unwrap_or_default();
    let cooldown = run.ctx.get_integer("cooldown").unwrap_or(0);
    let input = AutomationInput { name: Some(name.clone()), trigger: trigger.clone(), conditions: Vec::new(), actions: Vec::new(), enabled: false, cooldown_seconds: cooldown };
    auto::create_automation(&run.ctx.app, &run.g(), &input, &run.ctx.user().id.to_string(), None).await?;
    let text = format!(
        "**{name}** ({}) was created **off**.\nAdd conditions with `/automation condition add` and actions with `/automation action add`, then turn it on with `/automation enable`.{}",
        auto::trigger_label(&trigger).unwrap_or(""),
        run.module_off_line()
    );
    run.send(success_embed("Automation Created", Some(&text), None)).await
}

struct EditResult {
    old_name: String,
    old_cooldown: i64,
    changed: Vec<&'static str>,
}

async fn edit(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let rename_raw = run.ctx.get_string("rename");
    let cooldown = run.ctx.get_integer("cooldown");
    if rename_raw.is_none() && cooldown.is_none() {
        return run.send(e_nothing()).await;
    }
    let mut new_name = None;
    if let Some(raw) = &rename_raw {
        let Some(n) = auto::normalize_automation_name(raw) else { return run.send(e_name()).await };
        run.name = Some(n.clone()); // a unique violation on the rename reports the new name
        new_name = Some(n);
    }
    let guard = run.guard();
    let (row, result) = auto::mutate_automation(&run.ctx.app, &run.g(), &name, move |cur| {
        guard.check(cur)?;
        let mut data: Vec<(&'static str, Value)> = Vec::new();
        let mut changed = Vec::new();
        if let Some(n) = new_name.filter(|n| *n != cur.name) {
            data.push(("name", json!(n)));
            changed.push("name");
        }
        if let Some(c) = cooldown.filter(|c| *c != cur.cooldown_seconds) {
            data.push(("cooldownSeconds", json!(c)));
            changed.push("cooldownSeconds");
        }
        Ok(Mutation::new(data, EditResult { old_name: cur.name.clone(), old_cooldown: cur.cooldown_seconds, changed }))
    })
    .await?;
    if result.changed.is_empty() {
        return run.send(warn_embed("No Change", Some(&format!("**{}** already has those values.", row.name)), None)).await;
    }
    let mut lines = Vec::new();
    if result.changed.contains(&"name") {
        lines.push(format!("• **Name:** {} → {}", result.old_name, row.name));
    }
    if result.changed.contains(&"cooldownSeconds") {
        lines.push(format!("• **Cooldown:** {} s → {} s", result.old_cooldown, row.cooldown_seconds));
    }
    let text = format!("{}{}", lines.join("\n"), run.tail((!row.enabled).then_some(row.name.as_str()), &[]));
    run.send(success_embed("Automation Updated", Some(&text), None)).await
}

async fn condition_add(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let Some(row) = run.load_row(&name).await? else { return Ok(()) };
    let type_ = auto::normalize_type_input(&run.ctx.get_string("type").unwrap_or_default());
    let Some(def) = auto::condition_def(&type_) else { return run.send(err("Unknown Type", "Unknown condition type. Pick one from the list.")).await };
    if !def.allows(&row.trigger) {
        let text = format!("\"{}\" doesn't work with the trigger \"{}\".", def.label, auto::trigger_label(&row.trigger).unwrap_or(""));
        return run.send(err("Not Available", &text)).await;
    }
    let opts = auto::ConditionOptions {
        channel: opt_channel(run, "channel"),
        role: run.ctx.get_role_id("role"),
        user: opt_user(run, "user"),
        text: run.ctx.get_string("text"),
        number: run.ctx.get_integer("number"),
        button: run.ctx.get_string("button"),
    };
    let p = auto::condition_params_from_options(&run.ctx.app, def.key, &opts, &run.guild).await;
    if !p.ok {
        return run.send(embed_for_params(&p)).await;
    }
    let guard = run.guard();
    let (key, params) = (def.key, p.params.clone());
    let (saved, result) = auto::mutate_automation(&run.ctx.app, &run.g(), &name, move |cur| {
        guard.check(cur)?;
        let r = auto::add_condition(&cur.conditions, key, &params)?;
        let data = if r.added_new { vec![("conditions", Value::Array(r.conditions.clone()))] } else { Vec::new() };
        Ok(Mutation::new(data, r))
    })
    .await?;
    if result.merged && !result.added_new {
        return run.send(warn_embed("Already There", Some("Those values are already in the condition."), None)).await;
    }
    let cond = result.conditions.get(result.index - 1).cloned().unwrap_or(Value::Null);
    let described = auto::describe_condition(&cond, Some(&run.guild));
    let text = if result.merged {
        format!("Condition {} of **{name}** is now: {described}", result.index)
    } else {
        format!("**{name}** now also checks: {described}")
    };
    let full = format!("{text}{}", run.tail((!saved.enabled).then_some(name.as_str()), &p.ignored));
    run.send(success_embed(if result.merged { "Condition Updated" } else { "Condition Added" }, Some(&full), None)).await
}

async fn condition_remove(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let index = run.ctx.get_integer("index").unwrap_or(0);
    run.remove_kind = "condition";
    let guard = run.guard();
    let n2 = name.clone();
    let (row, removed) = auto::mutate_automation(&run.ctx.app, &run.g(), &name, move |cur| {
        guard.check(cur)?;
        if index < 1 || index as usize > cur.conditions.len() {
            return Err(AutomationError::with_code(format!("**{n2}** has {} condition(s).", cur.conditions.len()), ErrorCode::NoSuch).into());
        }
        let (list, removed) = auto::remove_at(&cur.conditions, index)?;
        Ok(Mutation::new(vec![("conditions", Value::Array(list))], removed))
    })
    .await?;
    let text = format!("Removed: {}{}", auto::describe_condition(&removed, Some(&run.guild)), run.tail((!row.enabled).then_some(name.as_str()), &[]));
    run.send(success_embed("Condition Removed", Some(&text), None)).await
}

fn typed_action(type_: &str, params: &Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(type_));
    if let Some(p) = params.as_object() {
        for (k, v) in p {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

async fn action_add(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let Some(row) = run.load_row(&name).await? else { return Ok(()) };
    let type_ = auto::normalize_type_input(&run.ctx.get_string("type").unwrap_or_default());
    let Some(def) = auto::action_def(&type_) else { return run.send(err("Unknown Type", "Unknown action type. Pick one from the list.")).await };
    if !def.allows(&row.trigger) {
        let text = format!("\"{}\" doesn't work with the trigger \"{}\".", def.label, auto::trigger_label(&row.trigger).unwrap_or(""));
        return run.send(err("Not Available", &text)).await;
    }
    let opts = auto::ActionOptions {
        channel: opt_channel(run, "channel"),
        text: run.ctx.get_string("text"),
        role: run.ctx.get_role_id("role"),
        user: opt_user(run, "user"),
        emoji: run.ctx.get_string("emoji"),
        amount: run.ctx.get_integer("amount"),
        embed: run.ctx.get_string("embed"),
        button_label: run.ctx.get_string("button_label"),
        button_url: run.ctx.get_string("button_url"),
    };
    let fallback = resolve_channel(&run.ctx.app, run.guild_id, run.ctx.channel_id()).await;
    let member = run.member().cloned();
    let p = {
        let actx = auto::ActionContext { guild: &run.guild, member: member.as_ref(), settings: run.settings_row(), fallback_channel: fallback.as_ref() };
        auto::action_params_from_options(&run.ctx.app, def.key, &opts, actx).await
    };
    if !p.ok {
        return run.send(embed_for_params(&p)).await;
    }
    let action = typed_action(def.key, &p.params);
    if let Some(access) = auto::action_channel_access_problem(&run.guild, member.as_ref(), std::slice::from_ref(&action), row.actions.len() + 1) {
        return run.send(e_no_access(&access)).await;
    }
    let guard = run.guard();
    let (saved, result) = auto::mutate_automation(&run.ctx.app, &run.g(), &name, move |cur| {
        guard.check(cur)?;
        let r = auto::append_action(&cur.actions, Some(&json!(cur.button_seq)), &action)?;
        let mut data = vec![("actions", Value::Array(r.actions.clone()))];
        if let Some(seq) = r.button_seq {
            data.push(("buttonSeq", json!(seq)));
        }
        Ok(Mutation::new(data, r))
    })
    .await?;
    let added = result.actions.get(result.index - 1).cloned().unwrap_or(Value::Null);
    let button_note = result
        .button_index
        .map(|b| format!("\nIts button number is **{b}**: use it in a \"Button clicked\" automation with the condition \"Clicked this button\"."))
        .unwrap_or_default();
    let text = format!(
        "Action {} of **{name}**: {}{button_note}{}",
        result.index,
        auto::describe_action(&added, Some(&run.guild), run.settings_row()),
        run.tail((!saved.enabled).then_some(name.as_str()), &p.ignored)
    );
    run.send(success_embed("Action Added", Some(&text), None)).await
}

async fn action_remove(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let index = run.ctx.get_integer("index").unwrap_or(0);
    run.remove_kind = "action";
    let guard = run.guard();
    let n2 = name.clone();
    let (row, (removed, turned_off)) = auto::mutate_automation(&run.ctx.app, &run.g(), &name, move |cur| {
        guard.check(cur)?;
        if index < 1 || index as usize > cur.actions.len() {
            return Err(AutomationError::with_code(format!("**{n2}** has {} action(s).", cur.actions.len()), ErrorCode::NoSuch).into());
        }
        let (list, removed) = auto::remove_at(&cur.actions, index)?;
        let turned_off = list.is_empty() && cur.enabled;
        let mut data = vec![("actions", Value::Array(list))];
        if turned_off {
            data.push(("enabled", json!(false)));
        }
        Ok(Mutation::new(data, (removed, turned_off)))
    })
    .await?;
    let note = if turned_off { "\nIt had no actions left, so it was turned **off**." } else { "" };
    let disabled = if row.enabled || turned_off { None } else { Some(name.as_str()) };
    let text = format!("Removed: {}{note}{}", auto::describe_action(&removed, Some(&run.guild), run.settings_row()), run.tail(disabled, &[]));
    run.send(success_embed("Action Removed", Some(&text), None)).await
}

async fn list(run: &mut Run) -> BotResult {
    let rows = auto::list_automations(run.ctx.app.db(), &run.g()).await?;
    if rows.is_empty() {
        let text = format!("No automations yet. Start with `/automation create` (presets) or `/automation create-blank`.{}", run.module_off_line());
        return run.send(guild_embed(run.settings_row(), Some("⚙️ Automations"), Some(&text), None)).await;
    }
    let lines: Vec<String> = rows
        .iter()
        .map(|r| {
            format!(
                "{} `{}` · {} · {} if · {} do · {} runs",
                if r.enabled { "🟢" } else { "⚫" },
                r.name,
                auto::trigger_label(&r.trigger).unwrap_or(""),
                r.conditions.len(),
                r.actions.len(),
                format_number(r.uses as f64)
            )
        })
        .collect();
    let title = format!("⚙️ Automations ({}/{})", rows.len(), limits::AUTOMATIONS);
    let pages: Vec<CreateEmbed> =
        lines.chunks(15).map(|chunk| guild_embed(run.settings_row(), Some(&title), Some(&format!("{}{}", chunk.join("\n"), run.module_off_line())), None)).collect();
    paginate(&run.ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}

/// `fieldsFor(title, lines)`: the lines in fields of at most 1024 characters.
pub fn fields_for(title: &str, lines: &[String]) -> Vec<(String, String)> {
    let base = strip_count(title);
    chunk_lines(lines, 1024).into_iter().enumerate().map(|(k, value)| (if k == 0 { title.to_string() } else { format!("{base} (cont.)") }, value)).collect()
}

/// `title.replace(/ \(\d+\)$/, '')`.
fn strip_count(title: &str) -> String {
    if let Some(open) = title.rfind(" (")
        && title.ends_with(')')
    {
        let digits = &title[open + 2..title.len() - 1];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return title[..open].to_string();
        }
    }
    title.to_string()
}

async fn info(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let Some(row) = run.load_row(&name).await? else { return Ok(()) };
    let cd = if row.cooldown_seconds != 0 { format!("{} s", row.cooldown_seconds) } else { "none".into() };
    let header = format!(
        "{} · **When:** {} · Cooldown: {cd} · Runs: {}{}",
        if row.enabled { "🟢 On" } else { "⚫ Off" },
        auto::trigger_label(&row.trigger).unwrap_or(""),
        format_number(row.uses as f64),
        run.module_off_line()
    );
    let mut embed = guild_embed(run.settings_row(), Some(&format!("⚙️ {}", row.name)), Some(&header), None);
    let conds: Vec<String> = row.conditions.iter().enumerate().map(|(i, c)| clamp_text(&format!("{}. {}", i + 1, auto::describe_condition(c, Some(&run.guild))), 150)).collect();
    let acts: Vec<String> =
        row.actions.iter().enumerate().map(|(i, a)| clamp_text(&format!("{}. {}", i + 1, auto::describe_action(a, Some(&run.guild), run.settings_row())), 150)).collect();
    let conds = if conds.is_empty() { vec!["Always".to_string()] } else { conds };
    let acts = if acts.is_empty() { vec!["No actions yet — add one with /automation action add.".to_string()] } else { acts };
    for (n, v) in fields_for(&format!("If ({})", row.conditions.len()), &conds) {
        embed = embed.field(n, v, false);
    }
    for (n, v) in fields_for(&format!("Do ({})", row.actions.len()), &acts) {
        embed = embed.field(n, v, false);
    }
    let problems = auto::automation_problems(&run.ctx.app, Some(&run.guild), &row, run.settings_row()).await;
    if !problems.is_empty() {
        let lines: Vec<String> = problems.iter().map(|p| clamp_text(&format!("⚠️ {p}"), 200)).collect();
        embed = embed.field("Problems", clamp_text(&lines.join("\n"), 1024), false);
    }
    run.send(embed).await
}

async fn set_enabled(run: &mut Run, enable: bool) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let guard = run.guard();
    let (row, unchanged) = auto::mutate_automation(&run.ctx.app, &run.g(), &name, move |cur| {
        guard.check(cur)?;
        if enable && cur.actions.is_empty() {
            return Err(AutomationError::with_code(error_texts::NEEDS_ACTIONS, ErrorCode::NoActions).into());
        }
        if cur.enabled == enable {
            return Ok(Mutation::new(Vec::new(), true));
        }
        Ok(Mutation::new(vec![("enabled", json!(enable))], false))
    })
    .await?;
    if unchanged {
        return run.send(warn_embed("No Change", Some(&format!("**{name}** is already {}.", if enable { "on" } else { "off" })), None)).await;
    }
    let mut extra = String::new();
    if enable {
        let problems = auto::automation_problems(&run.ctx.app, Some(&run.guild), &row, run.settings_row()).await;
        if !problems.is_empty() {
            extra = problems.iter().take(5).map(|p| format!("\n⚠️ {}", clamp_text(p, 200))).collect::<String>();
            if problems.len() > 5 {
                extra.push_str(&format!("\n… and {} more (see /automation info)", problems.len() - 5));
            }
        }
    }
    let text = format!("**{name}** is now {}.{extra}{}", if enable { "on" } else { "off" }, run.module_off_line());
    run.send(success_embed(if enable { "Automation Enabled" } else { "Automation Disabled" }, Some(&text), None)).await
}

async fn delete(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let Some(row) = auto::get_automation_by_name(run.ctx.app.db(), &run.g(), &name).await? else { return run.send(e_not_found(&name)).await };
    let opts = ConfirmOptions {
        title: Some("Delete automation?".into()),
        description: Some(format!("Delete **{name}**? Buttons it already posted stop doing anything unless another automation listens for them.")),
        confirm_label: "Delete".into(),
        ..ConfirmOptions::default()
    };
    if !await_confirm(&run.ctx, opts).await {
        return Ok(());
    }
    let count = auto::delete_automation(&run.ctx.app, &run.g(), &row.id).await?;
    let follow = if count == 0 {
        CreateInteractionResponseFollowup::new().embed(e_not_found(&name)).ephemeral(true)
    } else {
        CreateInteractionResponseFollowup::new().embed(success_embed("Automation Deleted", Some(&format!("Deleted **{name}**.")), None))
    };
    run.ctx.follow_up(follow).await?;
    Ok(())
}

/// The export file name (`automation-{name}.json`).
pub fn export_file_name(name: &str) -> String {
    format!("automation-{name}.json")
}

async fn export(run: &mut Run) -> BotResult {
    let Some(name) = run.name_from() else { return run.send(e_name()).await };
    let Some(row) = run.load_row(&name).await? else { return Ok(()) };
    let names: HashMap<String, String> = auto::list_automation_refs(run.ctx.app.db(), &run.g()).await.into_iter().map(|r| (r.id, r.name)).collect();
    let json = auto::to_export_json(&row.to_json(), &names);
    let embed = success_embed("Automation Exported", Some(&format!("Here is **{name}** as JSON. Import it with `/automation import`.")), None);
    run.send_edit(EditInteractionResponse::new().embed(embed).new_attachment(CreateAttachment::bytes(json.into_bytes(), export_file_name(&name)))).await
}

/// `/\.json$/i` on the name or `/json|text\/plain/i` on the content type.
pub fn looks_json(file_name: &str, content_type: Option<&str>) -> bool {
    file_name.to_ascii_lowercase().ends_with(".json")
        || content_type.is_some_and(|c| {
            let c = c.to_ascii_lowercase();
            c.contains("json") || c.contains("text/plain")
        })
}

async fn import(run: &mut Run) -> BotResult {
    let Some(file) = run.ctx.get_attachment("file") else { return Err(anyhow::anyhow!("Required option \"file\" not found.")) };
    if file.size as usize > auto::IMPORT_MAX_BYTES {
        return run.send(err("File Too Large", "The file must be 32 KB or smaller.")).await;
    }
    if !looks_json(&file.filename, file.content_type.as_deref()) {
        return run.send(err("Wrong File", "Attach a .json file from /automation export.")).await;
    }
    let opts = bot_util::safe_fetch::FetchOptions { timeout_ms: 8000, max_bytes: auto::IMPORT_MAX_BYTES as u64, ..Default::default() };
    let text = match bot_util::safe_fetch::safe_fetch_text(&file.url, &opts).await {
        Ok(t) => t,
        Err(_) => return run.send(err("Download Failed", "I could not download that file. Please try again.")).await,
    };
    let fallback = resolve_channel(&run.ctx.app, run.guild_id, run.ctx.channel_id()).await;
    let member = run.member().cloned();
    let user_id = run.ctx.user().id.to_string();
    let req = ImportRequest {
        guild: &run.guild,
        member: member.as_ref(),
        fallback_channel: fallback.as_ref(),
        settings: run.settings.as_deref(),
        module_off: run.module_off,
        user_id: &user_id,
        name: run.ctx.get_string("name"),
        overwrite: run.ctx.get_boolean("overwrite").unwrap_or(false),
    };
    let mut name = None;
    let res = import_from_text(&run.ctx.app, req, &text, &mut name).await;
    if name.is_some() {
        run.name = name;
    }
    let embed = res?;
    run.send(embed).await
}

/// What [`import_from_text`] needs besides the JSON.
pub struct ImportRequest<'a> {
    pub guild: &'a Guild,
    /// The importing admin (`interaction.member`).
    pub member: Option<&'a Member>,
    /// Where the admin runs the command (the role-ping check of replies).
    pub fallback_channel: Option<&'a GuildChannel>,
    pub settings: Option<&'a bot_db::GuildSettings>,
    pub module_off: bool,
    /// The importer (`createdBy` of a new row).
    pub user_id: &'a str,
    /// The `name` option.
    pub name: Option<String>,
    /// The `overwrite` option.
    pub overwrite: bool,
}

/// The part of `importJson` after the download: the checks, then the create or replace. Returns the embed to send;
/// a service error (a taken name on a rename race, the cap) is returned for the caller's error mapping, with the
/// normalised name stored in `name_out` first.
pub async fn import_from_text(app: &App, req: ImportRequest<'_>, text: &str, name_out: &mut Option<String>) -> anyhow::Result<CreateEmbed> {
    let parsed = auto::parse_automation_json(text);
    let Some(data) = parsed.data.clone().filter(|_| parsed.ok) else {
        let lines: Vec<String> = parsed.errors.iter().take(10).map(|e| format!("• {e}")).collect();
        return Ok(err("Invalid JSON", &lines.join("
")));
    };
    let guild = req.guild;
    // Same hierarchy rule as `action add` / the Welcomer preset: an import must not hand out a role above the importer's own.
    if let Some(escalation) = auto::import_role_hierarchy_problem(guild, req.member, &data.actions) {
        return Ok(err("Role Not Allowed", &escalation));
    }
    // The importer must also be allowed to post where the file posts, and to ping the roles it pings.
    if let Some(access) = auto::action_channel_access_problem(guild, req.member, &data.actions, 1) {
        return Ok(e_no_access(&access));
    }
    if let Some(ping) = auto::import_role_ping_problem(guild, req.member, &data.actions, req.fallback_channel) {
        return Ok(err("Role Not Pingable", &ping));
    }
    let raw_name = req.name.clone().or_else(|| data.name.clone()).unwrap_or_default();
    let Some(name) = auto::normalize_automation_name(&raw_name) else { return Ok(e_name()) };
    *name_out = Some(name.clone());
    let g = guild.id.to_string();
    let existing = auto::get_automation_by_name(app.db(), &g, &name).await?;
    if existing.is_some() && !req.overwrite {
        return Ok(e_taken(&name, " Add overwrite:True to replace it."));
    }
    // Replacing is a write to the existing automation: refused when it gives or takes a role the importer outranks.
    if let Some(ex) = &existing
        && ex.actions.iter().any(|a| a.get("roleIds").and_then(Value::as_array).is_some_and(|r| !r.is_empty()))
        && let Some(replaced) = auto::import_role_hierarchy_problem(guild, req.member, &ex.actions)
    {
        let text = format!("{replaced}
The automation you would replace gives or takes a role at or above your highest role.");
        return Ok(err("Role Not Allowed", &text));
    }
    let resolved = auto::resolve_import_references(app, guild, &data, req.settings, existing.as_ref().map(|e| e.id.as_str())).await;
    let enabled = data.enabled && resolved.warnings.is_empty() && !resolved.data.actions.is_empty();
    let input = AutomationInput {
        name: Some(name.clone()),
        trigger: resolved.data.trigger.clone(),
        conditions: resolved.data.conditions.clone(),
        actions: resolved.data.actions.clone(),
        enabled,
        cooldown_seconds: resolved.data.cooldown_seconds,
    };
    let row = match &existing {
        Some(ex) => auto::replace_automation(app, &g, &ex.id, &input).await?,
        None => auto::create_automation(app, &g, &input, req.user_id, resolved.data.exported_id.as_deref()).await?,
    };
    let mut extra = String::new();
    if !resolved.warnings.is_empty() {
        let lines: Vec<String> = resolved.warnings.iter().take(8).map(|w| format!("• {w}")).collect();
        extra.push_str(&format!("

⚠️ It was saved **off** because:
{}", lines.join("
")));
        if resolved.warnings.len() > 8 {
            extra.push_str(&format!("
… and {} more", resolved.warnings.len() - 8));
        }
    }
    if !resolved.info.is_empty() {
        let notes: Vec<&str> = resolved.info.iter().take(5).map(String::as_str).collect();
        extra.push_str(&format!("

ℹ️ {}", notes.join(" ")));
    }
    let text = format!("{}{extra}{}", summary_of(&row, Some(guild), req.settings), if req.module_off { MODULE_OFF_LINE } else { "" });
    Ok(success_embed(if existing.is_some() { "Automation Replaced" } else { "Automation Imported" }, Some(&text), None))
}

const HANDLERS: [&str; 14] =
    ["create", "create-blank", "edit", "condition add", "condition remove", "action add", "action remove", "list", "info", "enable", "disable", "delete", "export", "import"];

async fn dispatch(run: &mut Run, key: &str) -> BotResult {
    match key {
        "create" => create(run).await,
        "create-blank" => create_blank(run).await,
        "edit" => edit(run).await,
        "condition add" => condition_add(run).await,
        "condition remove" => condition_remove(run).await,
        "action add" => action_add(run).await,
        "action remove" => action_remove(run).await,
        "list" => list(run).await,
        "info" => info(run).await,
        "enable" => set_enabled(run, true).await,
        "disable" => set_enabled(run, false).await,
        "delete" => delete(run).await,
        "export" => export(run).await,
        "import" => import(run).await,
        _ => Ok(()),
    }
}

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let key = match group {
        Some(g) => format!("{g} {sub}"),
        None => sub,
    };
    if !HANDLERS.contains(&key.as_str()) {
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    // Every subcommand answers publicly (admin convention) except delete, whose confirm dialog is private.
    if !safe_defer(&ctx.responder, key == "delete").await {
        return Ok(());
    }
    if !resolve_new_options(&mut ctx, NewOptions { required: &[], private: &[] }).await {
        return Ok(());
    }
    let Some(guild) = cached_guild(&ctx.app, guild_id) else {
        let _ = ctx.edit_reply(EditInteractionResponse::new().embed(e_db())).await;
        return Ok(());
    };
    let settings = ctx.app.settings().get_settings_safe(&guild_id.to_string()).await;
    let module_off = !is_module_enabled(settings.as_deref(), Some(ModuleKey::Automations));
    let mut run = Run { ctx, guild_id, guild, settings, module_off, name: None, taken_hint: String::new(), remove_kind: "action" };
    if let Err(e) = dispatch(&mut run, &key).await {
        if let Some(ie) = e.downcast_ref::<InteractionError>()
            && is_interaction_gone(ie)
        {
            bot_util::log_debug!(format!("automations: {key}: interaction gone ({})", ie.code().map_or("undefined".into(), |c| c.to_string())));
            return Ok(());
        }
        if let Some(embed) = embed_for_error(&e, &run) {
            if let Err(e2) = run.send(embed).await {
                bot_util::log_debug!(format!("automations: error reply failed ({e2})"));
            }
            return Ok(());
        }
        bot_util::log_error!(format!("automations: {key} failed"), bot_core::error::log_arg(&e));
        if let Err(e2) = run.send(e_db()).await {
            bot_util::log_debug!(format!("automations: error reply failed ({e2})"));
        }
    }
    Ok(())
}

// ── Autocomplete ─────────────────────────────────────────────────────────────

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let choices = autocomplete_choices(&ctx).await;
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

async fn autocomplete_choices(ctx: &AutocompleteCtx) -> Vec<Choice> {
    let Some(guild_id) = ctx.guild_id() else { return Vec::new() };
    let g = guild_id.to_string();
    let (focused, q) = ctx.focused().unwrap_or_default();
    let group = ctx.subcommand_group();
    let pool = ctx.app.db();
    if focused == "name" {
        let rows = auto::search_automations(pool, &g, &auto::normalize_automation_name(&q).unwrap_or_default()).await;
        return rows
            .into_iter()
            .map(|r| {
                let label = auto::trigger_label(&r.trigger).map_or_else(|| r.trigger.clone(), str::to_string);
                Choice::new(format!("{} · {label}{}", r.name, if r.enabled { "" } else { " (off)" }), r.name)
            })
            .collect();
    }
    if focused == "embed" {
        return auto::search_saved_embed_names(pool, &g, &q).await.into_iter().map(|n| Choice::new(n.clone(), n)).collect();
    }
    if focused == "button" {
        let needle = bot_util::js::trim(&q).to_lowercase();
        return auto::list_automation_buttons(pool, &g)
            .await
            .into_iter()
            .map(|b| (format!("{} · button {} \"{}\"", b.automation_name, b.index, b.label), format!("{}:{}", b.automation_id, b.index)))
            .filter(|(name, _)| needle.is_empty() || name.to_lowercase().contains(&needle))
            .map(|(name, value)| Choice::new(name, value))
            .collect();
    }
    // type and index both depend on the automation named in the same command.
    let name_value = auto::normalize_automation_name(&ctx.get_string("name").unwrap_or_default());
    let row = match name_value {
        Some(n) => auto::get_automation_by_name(pool, &g, &n).await.ok().flatten(),
        None => None,
    };
    let guild = cached_guild(&ctx.app, guild_id);
    if focused == "type" {
        let needle = auto::normalize_type_input(&q);
        let loose = bot_util::js::trim(&q).to_lowercase();
        let matches = |key: &str, label: &str| loose.is_empty() || key.contains(&needle) || label.to_lowercase().contains(&loose);
        if group.as_deref() == Some("condition") {
            return auto::CONDITION_TYPES
                .iter()
                .filter(|d| row.as_ref().is_none_or(|r| d.allows(&r.trigger)) && matches(d.key, d.label))
                .map(|d| Choice::new(d.label, d.key))
                .collect();
        }
        let settings = ctx.app.settings().get_settings_safe(&g).await;
        let has_thread = row.as_ref().is_some_and(|r| r.actions.iter().any(|a| a.get("type").and_then(Value::as_str) == Some("CREATE_THREAD")));
        return auto::ACTION_TYPES
            .iter()
            .filter(|d| {
                if let Some(r) = &row {
                    if !d.allows(&r.trigger) {
                        return false;
                    }
                    if d.thread && !auto::T_THREADY.contains(&r.trigger.as_str()) && !has_thread {
                        return false;
                    }
                }
                matches(d.key, d.label)
            })
            .map(|d| {
                let off = d.reward.is_some_and(|k| {
                    let rk = if k == "xp" { RewardKind::Xp } else { RewardKind::Coins };
                    !ctx.app.rewards().is_reward_available(rk, settings.as_deref())
                });
                Choice::new(if off { format!("{} (module off)", d.label) } else { d.label.to_string() }, d.key)
            })
            .collect();
    }
    if focused == "index" {
        let Some(row) = row else { return Vec::new() };
        let needle = bot_util::js::trim(&q).to_lowercase();
        let settings = if group.as_deref() == Some("action") { ctx.app.settings().get_settings_safe(&g).await } else { None };
        let items: Vec<String> = if group.as_deref() == Some("condition") {
            row.conditions.iter().map(|c| auto::describe_condition(c, guild.as_ref())).collect()
        } else {
            row.actions.iter().map(|a| auto::describe_action(a, guild.as_ref(), settings.as_deref())).collect()
        };
        return items
            .into_iter()
            .enumerate()
            .map(|(i, text)| (format!("{}. {text}", i + 1), i + 1))
            .filter(|(name, _)| needle.is_empty() || name.to_lowercase().contains(&needle))
            .map(|(name, value)| Choice::new(name, json!(value)))
            .collect();
    }
    Vec::new()
}
