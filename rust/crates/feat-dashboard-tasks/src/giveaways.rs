//! Port of `src/services/dashboardTasks/giveaways.js`: the `dashboard.task` kinds of the Giveaways module, done with
//! /giveaway's own service functions (feat_giveaways_polls::giveaway_service), so the message with the Participate
//! button, the end job, the draw, the announcement, the prizes and the cap behave exactly as with the command.
//!   `giveaway.start`   /giveaway start    payload: the form's values (prize, channelId, winnerCount, duration | endsAt, ...)
//!   `giveaway.end`     /giveaway end      payload: `{ giveawayId }`
//!   `giveaway.reroll`  /giveaway reroll   payload: `{ giveawayId, winners }`
//!   `giveaway.cancel`  /giveaway cancel   payload: `{ giveawayId }`
//!   `giveaway.restart` /giveaway restart  payload: `{ giveawayId, channelId?, duration? | endsAt? }`
//!   `giveaway.odds`    /giveaway odds     payload: `{ giveawayId, roleWeights }` (the complete new list)
//! Every task re-checks what the router and the command check: the module is on, the dashboard user is still in the
//! server and is at least a moderator, then the command's own rules for each option.

use bot_config::{ModuleKey, PermissionLevel, limits};
use bot_core::App;
use bot_core::rewards::RewardKind;
use bot_core::settings::{Settings, is_module_enabled};
use bot_db::query::SelectBuilder;
use bot_db::{GIVEAWAY_INFO, Giveaway, GiveawayStatus};
use bot_util::text::clamp_text;
use chrono::{DateTime, Utc};
use feat_giveaways_polls::giveaway_service::{self as svc, EndSource, Ended, GiveawayError, OddsOp, Rerolled, RoleWeight, StartGiveaway};
use feat_giveaways_polls::guild_view;
use serde_json::{Value, json};
use serenity::all::{ChannelType, GuildChannel, Permissions};

use crate::index::{Actor, DashboardTask, TaskCtx, TaskOutcome};
use crate::shared::{TaskError, task_error};
use crate::util::{self, is_id};

const PRIZE_MAX: usize = 256;
const MAX_GATE_ROLES: usize = 10;
const GENERIC_ERROR: &str = "Discord or the database did not respond. Please try again in a moment.";
const INVALID: &str = "Those giveaway details were not valid. Reload the page and try again.";

fn invalid() -> TaskError {
    TaskError::new(INVALID)
}

/// `/^c[a-z0-9]{20,31}$/`.
pub fn is_cuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.first() == Some(&b'c') && (21..=32).contains(&b.len()) && b[1..].iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn id_list(value: Option<&Value>, max: usize) -> Result<Vec<String>, TaskError> {
    if util::is_nullish(value) {
        return Ok(Vec::new());
    }
    let Some(arr) = value.and_then(Value::as_array) else { return Err(invalid()) };
    if arr.len() > max || !arr.iter().all(|v| is_id(Some(v))) {
        return Err(invalid());
    }
    let mut out: Vec<String> = Vec::new();
    for v in arr.iter().filter_map(Value::as_str) {
        if !out.iter().any(|x| x == v) {
            out.push(v.to_string());
        }
    }
    Ok(out)
}

fn optional_int(value: Option<&Value>, min: f64, max: f64) -> Result<Option<i64>, TaskError> {
    if util::is_nullish(value) {
        return Ok(None);
    }
    match util::js_integer(value) {
        Some(n) if n >= min && n <= max => Ok(Some(n as i64)),
        _ => Err(invalid()),
    }
}

/// `optionalText(value, max)`: the length is checked before the trim.
fn optional_text(value: Option<&Value>, max: usize) -> Result<Option<String>, TaskError> {
    if util::is_nullish(value) {
        return Ok(None);
    }
    let Some(s) = value.and_then(Value::as_str) else { return Err(invalid()) };
    if util::len16(s) > max {
        return Err(invalid());
    }
    let t = bot_util::js::trim(s);
    Ok(if t.is_empty() { None } else { Some(t.to_string()) })
}

fn giveaway_id(payload: &Value) -> Result<String, TaskError> {
    match payload.get("giveawayId").and_then(Value::as_str) {
        Some(id) if is_cuid(id) => Ok(id.to_string()),
        _ => Err(TaskError::new("That giveaway no longer exists.")),
    }
}

/// `nameList(guild, ids)`: names of up to five members for a toast: "alice, bob and 3 more".
pub fn name_list(app: &App, guild_id: &str, ids: &[String]) -> String {
    name_list_with(ids, &|id| util::cached_member(app, guild_id, id).map(|m| util::member_display_name(&m)))
}

/// [`name_list`] with the cached display names given by `display`.
pub fn name_list_with(ids: &[String], display: &dyn Fn(&str) -> Option<String>) -> String {
    let names: Vec<String> = ids.iter().take(5).map(|id| display(id).unwrap_or_else(|| format!("user {id}"))).collect();
    let more = ids.len() - names.len();
    if more > 0 { format!("{} and {more} more", names.join(", ")) } else { names.join(", ") }
}

/// `plain(text, channel)`: a bot error text the Discord commands format with markdown, as plain text for a toast.
pub fn plain(text: &str, channel_name: Option<&str>) -> String {
    let replacement = channel_name.map(|n| format!("#{n}")).unwrap_or_else(|| "the channel".into());
    util::replace_channel_mentions(&util::strip_bold(text), |_| replacement.clone())
}

fn is_text_type(channel: &GuildChannel) -> bool {
    matches!(channel.kind, ChannelType::Text | ChannelType::News)
}

/// `validWeight(w)`: a role id and 2..10 entries.
fn valid_weight(w: &Value) -> bool {
    is_id(w.get("roleId")) && util::js_integer(w.get("weight")).is_some_and(|n| n >= svc::MIN_BONUS_WEIGHT as f64 && n <= svc::MAX_WEIGHT as f64)
}

fn weight_of(w: &Value) -> RoleWeight {
    RoleWeight { role_id: w["roleId"].as_str().unwrap_or_default().to_string(), weight: w["weight"].as_f64().unwrap_or(0.0) as i64 }
}

/// The router's gate and the command's level: the module is on, the user is in the server and is a moderator.
async fn access(app: &App, guild_id: &str, actor: &Actor) -> anyhow::Result<Settings> {
    if actor.member.is_none() && !actor.is_bot_owner {
        return Err(task_error("You are no longer a member of this server."));
    }
    let Some(settings) = app.settings().get_settings_safe(guild_id).await else {
        return Err(task_error("The server settings could not be loaded. Try again in a moment."));
    };
    if !is_module_enabled(Some(&settings), Some(ModuleKey::Giveaways)) {
        let m = ModuleKey::Giveaways.info();
        return Err(task_error(format!("The {} {} module is turned off on this server. Turn it on first.", m.emoji, m.label)));
    }
    let level = if actor.is_bot_owner {
        PermissionLevel::Owner
    } else {
        actor.member.as_ref().map(|m| util::member_level(app, guild_id, m, Some(&settings))).unwrap_or(PermissionLevel::Member)
    };
    if level < PermissionLevel::Moderator {
        return Err(task_error("You need to be a moderator of this server to manage giveaways."));
    }
    Ok(settings)
}

async fn load_giveaway(app: &App, guild_id: &str, id: &str) -> anyhow::Result<Giveaway> {
    let g: Option<Giveaway> =
        SelectBuilder::new(&GIVEAWAY_INFO).filter("id", json!(id))?.filter("guildId", json!(guild_id))?.limit(1).build()?.fetch_optional(app.db()).await?;
    g.ok_or_else(|| task_error("That giveaway no longer exists."))
}

/// The channel the giveaway is posted in, with the command's checks for the bot and for the dashboard user.
async fn post_channel(app: &App, guild_id: &str, actor: &Actor, channel_id: &str, missing: &str) -> anyhow::Result<GuildChannel> {
    let channel = match svc::fetch_channel(app, guild_id, channel_id).await {
        Ok(c) => c,
        // 50001 / 50013: the channel exists but the bot may not see it. Anything else is a real error.
        Err(e) if matches!(e.code, Some(50001) | Some(50013)) => {
            return Err(task_error("I can't see that channel. Give me View Channel there, or pick another one."));
        }
        Err(e) => return Err(e.into()),
    };
    let Some(channel) = channel else { return Err(task_error(missing)) };
    if !is_text_type(&channel) {
        return Err(task_error("Giveaways can be posted in text or announcement channels only."));
    }
    let g = channel.guild_id;
    if let Some(member) = &actor.member
        && !guild_view::member_has_in(app, g, &channel, member, Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
    {
        return Err(task_error(format!("You can't send messages in #{}.", channel.name)));
    }
    let usable = guild_view::channel_usable(app, g, &channel, &svc::POST_NEEDS);
    if !usable.ok {
        return Err(task_error(if !usable.missing.is_empty() {
            format!("I need {} in #{} to run a giveaway there.", bot_core::permissions::permission_names(&usable.missing).join(", "), channel.name)
        } else {
            let reason = plain(usable.reason.as_deref().unwrap_or(""), Some(&channel.name));
            format!("I can't use #{}: {}.", channel.name, reason.strip_suffix('.').unwrap_or(&reason))
        }));
    }
    Ok(channel)
}

/// `endTime({ duration, endsAt }, settings, at)`: a length from now or an exact moment, checked like /giveaway start.
pub fn end_time(duration: Option<&str>, ends_at: Option<&str>, timezone: &str, at: DateTime<Utc>) -> Result<DateTime<Utc>, TaskError> {
    let duration = duration.filter(|d| !d.is_empty());
    let ends_at = ends_at.filter(|e| !e.is_empty());
    if duration.is_some() && ends_at.is_some() {
        return Err(TaskError::new("Use either the length or the end time, not both."));
    }
    let date: Option<DateTime<Utc>> = if let Some(d) = duration {
        let Some(ms) = bot_util::parse_duration::parse_duration(d) else { return Err(TaskError::new("Use a length like 30m, 2h, 3d or 1w.")) };
        DateTime::from_timestamp_millis(at.timestamp_millis().saturating_add(ms))
    } else if let Some(e) = ends_at {
        let t = bot_util::js::date_parse(e);
        if t.is_finite() { DateTime::from_timestamp_millis(t as i64) } else { None }
    } else {
        return Err(TaskError::new("Set how long the giveaway runs, or when it ends."));
    };
    match svc::validate_ends_at(date, at, timezone) {
        Ok(()) => date.ok_or_else(|| TaskError::new("Use a length like 30m, 2h, 3d or 1w.")),
        Err((_, message)) => Err(TaskError::new(message)),
    }
}

/// `pickRole(guild, id, what)`: a role that must exist and must not be @everyone; its name.
async fn pick_role(app: &App, guild_id: &str, id: &str, what: &str) -> anyhow::Result<String> {
    if id == guild_id {
        return Err(task_error(format!("Choose a specific role for {what}, not @everyone.")));
    }
    util::fetch_role_name(app, guild_id, id).await.ok_or_else(|| task_error(format!("A role chosen for {what} no longer exists. Pick another one.")))
}

/// `failStartOrRestart(res, channel)`: why `startGiveaway` / `restartGiveaway` failed.
pub fn start_failure_text(res: &GiveawayError, channel_name: &str) -> String {
    match res {
        GiveawayError::Limit => format!("This server already has {} running or draft giveaways. End or cancel one first.", limits::ACTIVE_GIVEAWAYS),
        GiveawayError::SendFailed(_) => format!("I couldn't post the giveaway in #{channel_name}. Check my permissions there."),
        _ => GENERIC_ERROR.to_string(),
    }
}

/// Why `giveaway.restart` failed.
pub fn restart_failure_text(res: &GiveawayError, channel_name: &str) -> String {
    match res {
        GiveawayError::NotDraft | GiveawayError::Conflict => "That giveaway changed meanwhile. Reload the list and try again.".into(),
        other => start_failure_text(other, channel_name),
    }
}

/// The answer of `giveaway.end` for an `endGiveaway` result: the message, or the refusal.
pub fn end_text(prize: &str, res: &Result<Ended, GiveawayError>, names: &dyn Fn(&[String]) -> String) -> Result<String, String> {
    let prize = clamp_text(prize, 80);
    match res {
        Ok(r) => {
            let mut message = if r.winner_ids.is_empty() {
                format!("Ended \"{prize}\" with no valid entrants.")
            } else {
                format!("Ended \"{prize}\". {}: {}.", if r.winner_ids.len() == 1 { "Winner" } else { "Winners" }, names(&r.winner_ids))
            };
            if let Some(w) = r.pay_warnings.first() {
                message.push_str(&format!(" {}", pay_note(w)));
            }
            if !r.announced {
                message.push_str(" I could not post the announcement in the giveaway channel.");
            }
            Ok(message)
        }
        Err(e) => Err(match e {
            GiveawayError::NotActive | GiveawayError::NotFound => "That giveaway was already ended.".to_string(),
            GiveawayError::Drafted => {
                "The giveaway message or its channel was deleted, so the giveaway was moved to drafts. Restart it to post it again.".to_string()
            }
            GiveawayError::NoAccess { missing, reason } => {
                if !missing.is_empty() {
                    format!("I need {} in the giveaway channel to end this giveaway.", bot_core::permissions::permission_names(missing).join(", "))
                } else {
                    plain(reason.as_deref().unwrap_or("I can't use the giveaway channel to end this giveaway."), None)
                }
            }
            GiveawayError::ResolveFailed => {
                "I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.".to_string()
            }
            _ => GENERIC_ERROR.to_string(),
        }),
    }
}

/// The answer of `giveaway.reroll` for a `rerollGiveaway` result.
pub fn reroll_text(prize: &str, res: &Result<Rerolled, GiveawayError>, names: &dyn Fn(&[String]) -> String) -> Result<String, String> {
    match res {
        Ok(r) => {
            let n = r.new_winner_ids.len();
            let mut message = format!("New winner{} of \"{}\": {}.", if n == 1 { "" } else { "s" }, clamp_text(prize, 80), names(&r.new_winner_ids));
            if let Some(w) = r.pay_warnings.first() {
                message.push_str(&format!(" {}", pay_note(w)));
            }
            if !r.announced {
                message.push_str(" I could not post the announcement in the giveaway channel.");
            }
            Ok(message)
        }
        Err(e) => Err(match e {
            GiveawayError::NoEntrants => "There are no other eligible entrants to draw from.",
            GiveawayError::NotEnded | GiveawayError::NotFound => "Only ended giveaways can be rerolled.",
            GiveawayError::Conflict => "That giveaway changed while I was drawing. Please try again.",
            GiveawayError::ResolveFailed => "I couldn't check every entrant right now (Discord did not answer in time). Please try again in a minute.",
            _ => GENERIC_ERROR,
        }
        .to_string()),
    }
}

/// `payWarnings[0]` as a note: plain text, the leading warning emoji replaced.
fn pay_note(warning: &str) -> String {
    let p = plain(warning, None);
    match p.strip_prefix("⚠️") {
        Some(rest) => format!("Note: {}", rest.trim_start_matches(bot_util::js::is_js_space)),
        None => p,
    }
}

fn now(app: &App) -> DateTime<Utc> {
    app.clock().now()
}

fn read_ends_at(payload: &Value) -> Result<Option<String>, TaskError> {
    match payload.get("endsAt") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if util::len16(s) <= 40 => Ok(Some(s.clone())),
        _ => Err(invalid()),
    }
}

/// `giveaway.start`.
pub async fn start(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let actor = &ctx.actor;
    let payload = &ctx.payload;
    let settings = access(app, guild_id, actor).await?;

    // The payload is the dashboard's validated form; it is read again as untrusted JSON.
    let Some(prize) = payload.get("prize").and_then(Value::as_str) else { return Err(invalid().into()) };
    let prize = bot_util::js::trim(prize).to_string();
    if prize.is_empty() {
        return Err(task_error("The prize cannot be empty."));
    }
    if util::len16(&prize) > PRIZE_MAX {
        return Err(task_error(format!("The prize can be at most {PRIZE_MAX} characters.")));
    }
    let description = optional_text(payload.get("description"), 1000)?;
    let Some(channel_id) = payload.get("channelId").and_then(Value::as_str).filter(|s| util::is_snowflake(s)) else { return Err(invalid().into()) };
    let winner_count = match payload.get("winnerCount") {
        None => Some(1.0),
        v => util::js_integer(v),
    };
    let winner_count = match winner_count {
        Some(n) if n >= 1.0 && n <= limits::GIVEAWAY_WINNERS as f64 => n as i64,
        _ => return Err(task_error(format!("Pick between 1 and {} winners.", limits::GIVEAWAY_WINNERS))),
    };
    let duration = optional_text(payload.get("duration"), 40)?;
    let ends_at_raw = read_ends_at(payload)?;
    let required_ids = id_list(payload.get("requiredRoleIds"), MAX_GATE_ROLES)?;
    let denied_ids = id_list(payload.get("deniedRoleIds"), MAX_GATE_ROLES)?;
    let raw_weights: Vec<Value> = match payload.get("roleWeights") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) if a.len() <= svc::MAX_ROLE_WEIGHTS => a.clone(),
        _ => return Err(invalid().into()),
    };
    let mut weight_ids: Vec<String> = Vec::new();
    for w in &raw_weights {
        if !valid_weight(w) {
            return Err(invalid().into());
        }
        weight_ids.push(w["roleId"].as_str().unwrap_or_default().to_string());
    }
    let mut seen = weight_ids.clone();
    seen.sort();
    seen.dedup();
    if seen.len() != weight_ids.len() {
        return Err(task_error("Each role can have bonus odds only once."));
    }
    let xp_prize = optional_int(payload.get("xpPrize"), 1.0, svc::MAX_PRIZE_AMOUNT as f64)?;
    let coin_prize = optional_int(payload.get("coinPrize"), 1.0, svc::MAX_PRIZE_AMOUNT as f64)?;
    let ping_role_id = match payload.get("pingRoleId") {
        None | Some(Value::Null) => None,
        v if is_id(v) => v.and_then(Value::as_str).map(String::from),
        _ => return Err(invalid().into()),
    };
    let embed_color = optional_int(payload.get("embedColor"), 0.0, f64::from(0xffffff))?;

    // 1-3. Channel, dashboard user's access, bot permissions.
    let channel = post_channel(app, guild_id, actor, channel_id, "That channel no longer exists. Pick another one.").await?;

    // 4. Duration.
    let ends_at = end_time(duration.as_deref(), ends_at_raw.as_deref(), &settings.timezone, now(app))?;

    // 6. Required and blocked roles (managed roles are fine: nothing is assigned).
    for id in &required_ids {
        pick_role(app, guild_id, id, "the required roles").await?;
    }
    for id in &denied_ids {
        pick_role(app, guild_id, id, "the blocked roles").await?;
    }
    if required_ids.iter().any(|id| denied_ids.contains(id)) {
        return Err(task_error("The same role cannot be both required and blocked."));
    }

    // 7. Bonus odds.
    for id in &weight_ids {
        pick_role(app, guild_id, id, "the bonus odds").await?;
    }
    let role_weights: Vec<RoleWeight> = raw_weights.iter().map(weight_of).collect();

    // 8. XP and coin prizes.
    for (kind, amount) in [(RewardKind::Xp, xp_prize), (RewardKind::Coins, coin_prize)] {
        if amount.is_none() {
            continue;
        }
        if let Some(refusal) = svc::reward_unavailable_message(app, kind, Some(&settings)) {
            return Err(task_error(plain(&refusal, None)));
        }
    }

    // 9. Ping role: the bot sends the ping, and the dashboard user must be allowed to ping it too.
    if let Some(ping) = &ping_role_id {
        let role_name = pick_role(app, guild_id, ping, "the role to ping").await?;
        let g = channel.guild_id;
        let ids = vec![ping.clone()];
        if !guild_view::pingable_by_bot(app, g, &channel, &ids).refused.is_empty() {
            return Err(task_error(format!(
                "I can't ping @{role_name} in #{}. Make the role mentionable, or give me Mention @everyone, @here and All Roles there.",
                channel.name
            )));
        }
        if let Some(member) = &actor.member
            && !guild_view::pingable(app, g, &channel, Some((member.user.id, member.roles.clone())), &ids).refused.is_empty()
        {
            return Err(task_error(format!("You don't have permission to ping @{role_name} in #{}.", channel.name)));
        }
    }

    // 12-13. Start (posts the message and schedules `giveaway.end`).
    let opts = StartGiveaway {
        guild_id: guild_id.to_string(),
        channel: channel.clone(),
        host_id: actor.id.clone(),
        prize: prize.clone(),
        description,
        winner_count,
        ends_at,
        required_role_ids: required_ids,
        denied_role_ids: denied_ids,
        role_weights,
        xp_prize,
        coin_prize,
        ping_role_id,
        embed_color: embed_color.map(|c| c as u32),
        settings: Some(settings),
    };
    match svc::start_giveaway(app, opts).await {
        Ok(started) => Ok(TaskOutcome::new(
            format!("Started the giveaway for \"{}\" in #{}. It ends {}.", clamp_text(&prize, 80), channel.name, util::utc(ends_at)),
            json!({ "giveawayId": started.giveaway.id }),
        )),
        Err(e) => Err(task_error(start_failure_text(&e, &channel.name))),
    }
}

/// `giveaway.end`.
pub async fn end(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    access(app, guild_id, &ctx.actor).await?;
    let g = load_giveaway(app, guild_id, &giveaway_id(&ctx.payload)?).await?;
    if g.status != GiveawayStatus::Active {
        return Err(task_error(if g.status == GiveawayStatus::Draft {
            "That giveaway is a draft (its message was deleted). Restart it first.".to_string()
        } else {
            format!("That giveaway is {}.", svc::status_text(g.status))
        }));
    }
    let res = svc::end_giveaway(app, guild_id, &g.id, EndSource::Command, 1).await?;
    let message = end_text(&g.prize, &res, &|ids| name_list(app, guild_id, ids)).map_err(task_error)?;
    Ok(TaskOutcome::new(message, json!({ "giveawayId": g.id })))
}

/// `giveaway.reroll`.
pub async fn reroll(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    access(app, guild_id, &ctx.actor).await?;
    let g = load_giveaway(app, guild_id, &giveaway_id(payload)?).await?;
    if g.status != GiveawayStatus::Ended {
        return Err(task_error(if g.status == GiveawayStatus::Active {
            "That giveaway is still running. End it first."
        } else {
            "Only ended giveaways can be rerolled."
        }));
    }
    let count = match payload.get("winners") {
        None => Some(1.0),
        v => util::js_integer(v),
    };
    let count = match count {
        Some(n) if n >= 1.0 && n <= limits::GIVEAWAY_WINNERS as f64 => n as i64,
        _ => return Err(task_error(format!("Pick between 1 and {} winners.", limits::GIVEAWAY_WINNERS))),
    };
    let res = svc::reroll_giveaway(app, guild_id, &g.id, count).await?;
    let message = reroll_text(&g.prize, &res, &|ids| name_list(app, guild_id, ids)).map_err(task_error)?;
    Ok(TaskOutcome::new(message, json!({ "giveawayId": g.id })))
}

/// `giveaway.cancel`.
pub async fn cancel(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    access(app, guild_id, &ctx.actor).await?;
    let g = load_giveaway(app, guild_id, &giveaway_id(&ctx.payload)?).await?;
    if !matches!(g.status, GiveawayStatus::Active | GiveawayStatus::Draft) {
        return Err(task_error(format!("That giveaway is {}.", svc::status_text(g.status))));
    }
    match svc::cancel_giveaway(app, guild_id, &g.id).await? {
        Ok(()) => Ok(TaskOutcome::new(format!("Cancelled the giveaway for \"{}\".", clamp_text(&g.prize, 80)), json!({ "giveawayId": g.id }))),
        Err(_) => Err(task_error("That giveaway already ended or was cancelled.")),
    }
}

/// `giveaway.restart`.
pub async fn restart(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    let settings = access(app, guild_id, &ctx.actor).await?;
    let g = load_giveaway(app, guild_id, &giveaway_id(payload)?).await?;
    if g.status != GiveawayStatus::Draft {
        return Err(task_error("Only draft giveaways can be restarted. A giveaway becomes a draft when its message is deleted."));
    }
    let picked = match payload.get("channelId") {
        None | Some(Value::Null) => None,
        v if is_id(v) => v.and_then(Value::as_str).map(String::from),
        _ => return Err(invalid().into()),
    };
    let duration = optional_text(payload.get("duration"), 40)?;
    let ends_at_raw = read_ends_at(payload)?;

    let missing = if picked.is_some() { "That channel no longer exists. Pick another one." } else { "The original channel no longer exists. Pick another one." };
    let channel = post_channel(app, guild_id, &ctx.actor, picked.as_deref().unwrap_or(&g.channel_id), missing).await?;

    let at = now(app);
    let ends_at = if duration.is_some() || ends_at_raw.as_deref().is_some_and(|e| !e.is_empty()) {
        end_time(duration.as_deref(), ends_at_raw.as_deref(), &settings.timezone, at)?
    } else if (g.ends_at - at).num_milliseconds() >= svc::MIN_DURATION_MS {
        g.ends_at
    } else {
        return Err(task_error("This giveaway's end time has passed. Give it a new length or end time."));
    };

    match svc::restart_giveaway(app, guild_id, &g.id, channel.clone(), ends_at, Some(settings)).await {
        Ok(_) => Ok(TaskOutcome::new(
            format!(
                "\"{}\" is live again in #{} and ends {}. Existing entries were kept.",
                clamp_text(&g.prize, 80),
                channel.name,
                util::utc(ends_at)
            ),
            json!({ "giveawayId": g.id }),
        )),
        Err(e) => Err(task_error(restart_failure_text(&e, &channel.name))),
    }
}

fn odds_failure(res: &GiveawayError) -> anyhow::Error {
    match res {
        GiveawayError::Limit => task_error(format!("A giveaway can have at most {} bonus-odds roles.", svc::MAX_ROLE_WEIGHTS)),
        GiveawayError::NotOpen => task_error("Odds can only be changed while a giveaway is running or in drafts."),
        _ => task_error("That giveaway no longer exists."),
    }
}

/// The plan of `giveaway.odds`: `(removals, additions)` to bring `current` to `wanted`.
pub fn odds_plan(current: &[RoleWeight], wanted: &[RoleWeight]) -> (Vec<RoleWeight>, Vec<RoleWeight>) {
    let removals = current.iter().filter(|w| !wanted.iter().any(|x| x.role_id == w.role_id)).cloned().collect();
    let additions = wanted.iter().filter(|w| current.iter().find(|c| c.role_id == w.role_id).map(|c| c.weight) != Some(w.weight)).cloned().collect();
    (removals, additions)
}

/// `giveaway.odds`: brings the bonus odds to the given list with the same calls /giveaway odds add and remove make.
pub async fn odds(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let app = &ctx.app;
    let guild_id = ctx.guild_id.as_str();
    let payload = &ctx.payload;
    let settings = access(app, guild_id, &ctx.actor).await?;
    let g = load_giveaway(app, guild_id, &giveaway_id(payload)?).await?;
    if !matches!(g.status, GiveawayStatus::Active | GiveawayStatus::Draft) {
        return Err(task_error("Odds can only be changed while a giveaway is running or in drafts."));
    }
    let Some(wanted) = payload.get("roleWeights").and_then(Value::as_array).filter(|a| a.len() <= svc::MAX_ROLE_WEIGHTS) else {
        return Err(invalid().into());
    };
    if !wanted.iter().all(valid_weight) {
        return Err(invalid().into());
    }
    let wanted: Vec<RoleWeight> = wanted.iter().map(weight_of).collect();
    let mut ids: Vec<&str> = wanted.iter().map(|w| w.role_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != wanted.len() {
        return Err(task_error("Each role can have bonus odds only once."));
    }

    let current = svc::normalize_role_weights(g.role_weights.as_ref().unwrap_or(&Value::Null));
    let (removals, additions) = odds_plan(&current, &wanted);
    for w in &additions {
        pick_role(app, guild_id, &w.role_id, "the bonus odds").await?;
    }
    if removals.is_empty() && additions.is_empty() {
        return Ok(TaskOutcome::new("The bonus odds are already like that.", json!({ "giveawayId": g.id })));
    }

    // Removals first, so a full list never hits the cap in the middle.
    for w in &removals {
        if let Err(e) = svc::update_odds(app, guild_id, &g.id, OddsOp::Remove, &w.role_id, Some(settings.clone())).await?
            && e != GiveawayError::NotSet
        {
            return Err(odds_failure(&e));
        }
    }
    for w in &additions {
        if let Err(e) = svc::update_odds(app, guild_id, &g.id, OddsOp::Add { weight: w.weight }, &w.role_id, Some(settings.clone())).await? {
            return Err(odds_failure(&e));
        }
    }
    Ok(TaskOutcome::new(format!("Updated the bonus odds of \"{}\".", clamp_text(&g.prize, 80)), json!({ "giveawayId": g.id })))
}

/// `createGiveawayTasks()`.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![
        ("giveaway.start", DashboardTask::new(start)),
        ("giveaway.end", DashboardTask::new(end)),
        ("giveaway.reroll", DashboardTask::new(reroll)),
        ("giveaway.cancel", DashboardTask::new(cancel)),
        ("giveaway.restart", DashboardTask::new(restart)),
        ("giveaway.odds", DashboardTask::new(odds)),
    ]
}
