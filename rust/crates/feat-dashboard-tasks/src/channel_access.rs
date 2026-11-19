//! Port of `src/services/dashboardTasks/channelAccess.js`: the channel rule of the slash commands, for the web
//! dashboard's record stores (saved embeds, custom commands, automations):
//!   `access.channels`   `{ checks: [{ channelId, needs?: PermissionFlagsBits keys, optional?: boolean }] }`
//! A configuration must not make the bot post in, edit or delete in a channel its author cannot use. Here the admin is a
//! real guild member and Discord's own overwrite resolution decides (`memberChannelProblem`; the guild owner and
//! Administrators are exempt). Every check is run; the answer's `data.problems` lists `{ index, text }` for each one
//! that failed. `optional`: a channel the bot cannot find is skipped, otherwise it is a problem. The bot owner is not
//! checked; any other admin who left the server is refused.

use bot_core::privacy_guards::{MEMBER_CHANNEL_NEEDS, MemberRef, member_channel_problem};
use serde_json::{Value, json};

use crate::index::{DashboardTask, TaskCtx, TaskOutcome};
use crate::shared::{TaskError, task_error};
use crate::util;

/// What a check may ask for: the needs of /embed, /tag and the automation actions (EXECUTOR_CHANNEL_NEEDS).
pub const ACCESS_NEEDS: [&str; 3] = ["ViewChannel", "SendMessages", "CreatePublicThreads"];
pub const ACCESS_CHECKS_MAX: usize = 25;
const INVALID: &str = "Those channel checks were not valid. Reload the page and try again.";

/// One re-validated check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessCheck {
    pub channel_id: String,
    /// `None`: the default needs of `memberChannelProblem`.
    pub needs: Option<Vec<String>>,
    pub optional: bool,
}

/// `readAccessPayload(payload)`: re-validates an `access.channels` payload.
pub fn read_access_payload(payload: &Value) -> Result<Vec<AccessCheck>, TaskError> {
    let invalid = || TaskError::new(INVALID);
    let Some(list) = payload.get("checks").and_then(Value::as_array) else { return Err(invalid()) };
    if list.is_empty() || list.len() > ACCESS_CHECKS_MAX {
        return Err(invalid());
    }
    list.iter()
        .map(|c| {
            let needs = c.get("needs");
            let needs = match needs {
                None => None,
                Some(Value::Array(a)) if !a.is_empty() && a.iter().all(|k| k.as_str().is_some_and(|k| ACCESS_NEEDS.contains(&k))) => {
                    Some(a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
                }
                Some(_) => return Err(invalid()),
            };
            let Some(channel_id) = c.get("channelId").and_then(Value::as_str).filter(|s| util::is_snowflake(s)) else { return Err(invalid()) };
            Ok(AccessCheck { channel_id: channel_id.to_string(), needs, optional: c.get("optional") == Some(&Value::Bool(true)) })
        })
        .collect()
}

/// A refusal as plain text for a toast (no markdown, channel mentions as names).
fn plain(app: &bot_core::App, guild_id: &str, text: &str) -> String {
    util::replace_channel_mentions(&util::strip_bold(text), |id| {
        format!("#{}", util::cached_channel_name(app, guild_id, id).unwrap_or_else(|| "the channel".into()))
    })
}

/// `memberChannelProblem` when the permissions cannot be computed: every need is missing (a thread swaps Send
/// Messages for Send Messages in Threads).
fn all_missing(channel: &serenity::all::GuildChannel, needs: &[&str]) -> String {
    let keys: Vec<&str> = if bot_core::permissions::is_thread(channel) {
        needs.iter().map(|k| if *k == "SendMessages" { "SendMessagesInThreads" } else { k }).collect()
    } else {
        needs.to_vec()
    };
    format!("You need **{}** in <#{}> to do that.", bot_core::permissions::permission_names(&keys).join(", "), channel.id)
}

/// `access.channels`.
pub async fn access_channels(ctx: TaskCtx) -> anyhow::Result<TaskOutcome> {
    let checks = read_access_payload(&ctx.payload)?;
    if ctx.actor.is_bot_owner {
        return Ok(TaskOutcome::new("The bot owner is not checked.", json!({ "problems": [] })));
    }
    let Some(member) = ctx.actor.member.clone() else { return Err(task_error("You need to be a member of this server to do that.")) };
    let app = &ctx.app;
    let mut problems: Vec<Value> = Vec::new();
    for (index, check) in checks.iter().enumerate() {
        let Some(channel) = util::find_channel(app, &ctx.guild_id, &check.channel_id).await else {
            if !check.optional {
                problems.push(json!({ "index": index, "text": "The bot cannot find that channel, so your access to it cannot be checked." }));
            }
            continue;
        };
        let needs: Vec<&str> = match &check.needs {
            Some(n) => n.iter().map(String::as_str).collect(),
            None => MEMBER_CHANNEL_NEEDS.to_vec(),
        };
        let checked = util::gid(&ctx.guild_id)
            .and_then(|g| app.discord().and_then(|d| d.cache.guild(g).map(|guild| member_channel_problem(&guild, Some(&channel), Some(MemberRef::of(&member)), &needs))));
        // The guild cannot be read: permissions cannot be resolved, so every need counts as missing (the JS
        // `permissionsFor` failure path). Fail closed.
        let text = checked.unwrap_or_else(|| Some(all_missing(&channel, &needs)));
        if let Some(text) = text {
            problems.push(json!({ "index": index, "text": plain(app, &ctx.guild_id, &text) }));
        }
    }
    let message = problems.first().and_then(|p| p["text"].as_str()).unwrap_or("You can use those channels.").to_string();
    Ok(TaskOutcome::new(message, json!({ "problems": problems })))
}

/// `tasks` of channelAccess.js.
pub fn tasks() -> Vec<(&'static str, DashboardTask)> {
    vec![("access.channels", DashboardTask::new(access_channels))]
}
