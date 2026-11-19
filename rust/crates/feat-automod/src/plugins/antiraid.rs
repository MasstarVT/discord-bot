//! Port of `src/plugins/antiraid.js`: counts joins, starts a lockdown when a raid is detected, kicks or bans what
//! joins during it and the young accounts, and ends it again (End button, `antiraid.unlock` job, `/antiraid`). State
//! and effects: [`crate::anti_raid_service`].
//!
//! Known limit (as in the JS): the welcome and captcha hooks still run for a member this plugin removes (nothing may
//! call `ctx.stop()` but automod and custom commands). `ctx.state.antiraid` (`{ action }`) tells later hooks that
//! the member was removed.

use std::sync::Arc;

use bot_core::interaction_helpers::{Payload, is_interaction_gone, safe_reply};
use bot_core::prelude::*;
use serde_json::json;
use serenity::all::{GuildId, Member};
use serenity::builder::EditInteractionResponse;
use tokio::task::JoinHandle;

use crate::anti_raid_service::{
    EndAnnounce, Ended, Started, StartInput, Trigger, YoungReport, add_to_tally, announce_end, announce_start, count_join, detection_of, end_lockdown,
    harsher, is_locked, remove_member, report_young, start_lockdown, young_account,
};

/// The `ctx.state` key telling later `guildMemberAdd` hooks what anti-raid did (`{ action }`).
pub const STATE_KEY: &str = "antiraid";

/// When the member joined, never later than now.
fn join_time(member: &Member, now: i64) -> i64 {
    member.joined_at.map(|t| t.timestamp_millis().min(now)).unwrap_or(now)
}

/// Starts the lockdown of a detected raid and posts its alert. Never fails.
pub async fn start_and_announce(app: &App, guild_id: GuildId, settings: Option<Settings>, trigger: Trigger) -> Started {
    let res = start_lockdown(app, guild_id, StartInput { guild_id: Some(guild_id), settings: settings.clone(), trigger: Some(trigger), ..Default::default() }).await;
    if let Started::Ok { started: true, state, effects, .. } = &res {
        announce_start(app, guild_id, settings.as_deref(), state, effects.as_ref(), Some(trigger)).await;
    }
    res
}

/// What [`handle_member_add`] did.
#[derive(Debug, Default)]
pub struct JoinOutcome {
    pub raid: bool,
    /// The background lockdown start (tests await it).
    pub start: Option<JoinHandle<Started>>,
    /// `KICK | BAN | ALERT`.
    pub action: Option<String>,
    pub result: Option<crate::anti_raid_service::Removed>,
}

/// One member joined. Counts the join; a raid starts a lockdown in the background. While a lockdown is on every
/// joiner gets `antiRaidAction`, and an account younger than `antiRaidMinAccountAgeDays` gets `antiRaidYoungAction`;
/// the harsher of the two is applied once. Bots are never touched. Never fails.
pub async fn handle_member_add(app: &App, state: Option<&bot_core::hooks::HookState>, guild_id: GuildId, member: &Member, settings: Option<Settings>) -> Option<JoinOutcome> {
    if member.user.bot {
        return None;
    }
    let s = settings?;
    let g = guild_id.to_string();
    let now = app.clock().now_ms();
    let mut out = JoinOutcome::default();

    let mut locked = is_locked(app, guild_id, now).await;
    let mut trigger = None;
    if !locked {
        // The memo is checked again, the join counted and the lockdown marked under one lock: a burst of joins
        // cannot start two lockdowns.
        let d = detection_of(Some(&s));
        let (now_locked, tripped) = count_join(app, &g, join_time(member, now), now, d);
        locked = now_locked;
        if let Some(win) = tripped {
            out.raid = true;
            let t = Trigger { joins: win.count, seconds: d.seconds };
            trigger = Some(t);
            let (a, st) = (app.clone(), s.clone());
            out.start = Some(tokio::spawn(async move { start_and_announce(&a, guild_id, Some(st), t).await }));
        }
    }

    let raid_action = locked.then(|| s.anti_raid_action.as_str());
    let created = member.user.id.created_at().timestamp_millis() as f64;
    let young = young_account(Some(created), Some(&s), now);
    let Some(action) = harsher(raid_action, young.map(|_| s.anti_raid_young_action.as_str())).map(str::to_string) else { return Some(out) };
    out.action = Some(action.clone());

    let mut reasons = Vec::new();
    let mut details = Vec::new();
    if locked && raid_action == Some(action.as_str()) {
        reasons.push("raid lockdown".to_string());
        details.push(match trigger {
            Some(t) => format!("Raid: {} joins in {} seconds", t.joins, t.seconds),
            None => "Joined during a raid lockdown".into(),
        });
    }
    if let Some(y) = young
        && (reasons.is_empty() || s.anti_raid_young_action.as_str() == action)
    {
        let days = if y.age_days == 1 { "" } else { "s" };
        reasons.push(format!("account is {} day{days} old (minimum {})", y.age_days, y.min_days));
        details.push(format!("Account age: {} day{days} (minimum {})", y.age_days, y.min_days));
    }

    if action != "ALERT" {
        let res = remove_member(app, guild_id, member, &action, &format!("Anti-Raid: {}", reasons.join("; ")), Some(&details.join("\n"))).await;
        if res.ok {
            if let Some(st) = state {
                st.insert(STATE_KEY, json!({ "action": action }));
            }
            if locked {
                add_to_tally(app, &g, |t| if action == "BAN" { t.banned += 1 } else { t.kicked += 1 });
            }
        } else if !res.skipped && locked {
            add_to_tally(app, &g, |t| t.failed += 1);
        }
        out.result = Some(res);
    } else if locked {
        add_to_tally(app, &g, |t| t.flagged += 1);
    }

    if let Some(y) = young {
        let done = if out.result.as_ref().is_some_and(|r| r.ok) { action.clone() } else { "ALERT".into() };
        let report = YoungReport {
            user_id: member.user.id.to_string(),
            user_tag: bot_core::adapters::user_tag(&member.user),
            age_days: y.age_days,
            min_days: y.min_days,
            action: done,
        };
        report_young(app, guild_id, Some(&s), report).await;
    }
    Some(out)
}

async fn on_member_add(ctx: Arc<HookCtx<GuildMemberAdd>>) -> BotResult {
    let settings = ctx.get_settings().await;
    handle_member_add(&ctx.app, Some(&ctx.state), ctx.data.guild_id, &ctx.data.member, settings).await;
    Ok(())
}

/// The alert channel was deleted: forget it (alerts fall back to the moderation log channel).
pub async fn on_channel_delete(app: &App, guild_id: GuildId, channel_id: &str, settings: Option<Settings>) -> BotResult {
    let Some(s) = settings else { return Ok(()) };
    if s.anti_raid_alert_channel_id.as_deref() != Some(channel_id) {
        return Ok(());
    }
    app.settings().update_settings(&guild_id.to_string(), SettingsPatch::new().set("antiRaidAlertChannelId", serde_json::Value::Null)).await?;
    Ok(())
}

/// Sync hook filter: false when the memoised settings name another channel.
pub fn may_be_alert_channel(app: &App, guild_id: &str, channel_id: &str) -> bool {
    match app.settings().peek_settings(guild_id) {
        None => true,
        Some(s) => s.anti_raid_alert_channel_id.as_deref().is_some_and(|a| !a.is_empty() && a == channel_id),
    }
}

/// Scheduler handler for `antiraid.unlock`. Idempotent; a lockdown that is not due yet is left alone. Not
/// module-gated: a lockdown always gets lifted. Fails only to be retried (database failure, or @everyone could not
/// be restored right now); a missing permission is permanent.
pub async fn handle_unlock_job(app: &App, guild_id: GuildId) -> BotResult<JobOutcome> {
    let settings = app.settings().get_settings_safe(&guild_id.to_string()).await;
    let res: Ended = end_lockdown(app, guild_id, true).await?;
    // A restore that can never succeed on its own must be said out loud, also on a retry.
    if res.was_active || (res.pending_restore && res.permanent) {
        announce_end(app, guild_id, settings.as_deref(), &res, EndAnnounce { how: "expired", ended_by_id: None, except_channel_id: None }).await;
    }
    if res.pending_restore {
        let message = format!("antiraid.unlock: {}", res.effects.errors.join(" "));
        if res.permanent {
            return Err(PermanentJobError::new(message).into());
        }
        anyhow::bail!(message);
    }
    Ok(JobOutcome::Done)
}

async fn button_says(ctx: &ComponentCtx, embed: serenity::builder::CreateEmbed) {
    safe_reply(&ctx.responder, Payload::embed(embed).ephemeral(true)).await;
}

/// The persistent `antiraid:end` button on the alert. Not module-gated (a lockdown can be ended while the module is
/// off); MODERATOR or higher is checked here. Never fails.
pub async fn handle_end_button(ctx: ComponentCtx) -> BotResult {
    let (_, params) = ctx.parsed();
    if params.first().map(String::as_str) != Some("end") {
        safe_reply(&ctx.responder, Payload::text("This button is no longer active.").ephemeral(true)).await;
        return Ok(());
    }
    let Some(guild_id) = ctx.guild_id().filter(|g| bot_core::interaction_helpers::cached_guild(&ctx.app, *g).is_some()) else {
        button_says(&ctx, error_embed("Try Again", Some("I can't check your roles right now. Please try again in a moment."), None)).await;
        return Ok(());
    };
    let g = guild_id.to_string();
    let budget = ctx.ack_budget_ms().min(1000);
    let settings = ctx.app.settings().peek_or_load(&g, budget).await;
    let level = resolve_permission_level(ctx.member_access().as_ref(), settings.as_deref(), ctx.app.config().bot_owner_id.as_deref());
    if level < PermissionLevel::Moderator {
        button_says(&ctx, error_embed("Moderators Only", Some("Only moderators can end a raid lockdown."), None)).await;
        return Ok(());
    }

    if let Err(e) = ctx.defer_update().await {
        // The JS rethrew into its own catch, which only logs: never an error reply.
        if !is_interaction_gone(&e) {
            bot_util::log_error!("[antiraid] end button failed".to_string(), e.to_string());
        }
        return Ok(());
    }
    let res = match end_lockdown(&ctx.app, guild_id, false).await {
        Ok(r) => r,
        Err(e) => {
            bot_util::log_error!(format!("[antiraid] {g}: ending the lockdown failed"), bot_core::error::log_arg(&e));
            button_says(&ctx, error_embed("Could Not End Lockdown", Some("The lockdown could not be ended. Please try again in a moment."), None)).await;
            return Ok(());
        }
    };
    if let Err(e) = ctx.edit_reply(EditInteractionResponse::new().components(vec![])).await {
        bot_util::log_debug!(format!("[antiraid] end button: {e}"));
    }
    if !res.was_active && !res.restored && !res.pending_restore {
        button_says(&ctx, info_embed("No Lockdown", Some("There is no lockdown to end."), None)).await;
        return Ok(());
    }
    let user = ctx.user().id.to_string();
    bot_util::log_info!(format!("[antiraid] {g}: lockdown ended with the button by {user}"));
    let embed = if res.pending_restore {
        warn_embed("Lockdown Ended", Some(&res.effects.errors.join("\n")), None)
    } else {
        success_embed("Lockdown Ended", Some("The lockdown is over."), None)
    };
    button_says(&ctx, embed).await;
    announce_end(&ctx.app, guild_id, settings.as_deref(), &res, EndAnnounce { how: "manual", ended_by_id: Some(&user), except_channel_id: None }).await;
    Ok(())
}

/// The plugin (`src/plugins/antiraid.js`).
pub fn plugin() -> Plugin {
    Plugin::new("antiraid", Some(ModuleKey::Antiraid))
        // HOOK_ORDER.guildMemberAdd: after moderation, before logging / captcha / welcome.
        .hook(Hook::<GuildMemberAdd>::new("antiraid", on_member_add).filter(|ctx| !ctx.data.member.user.bot).timeout_ms(15_000))
        .hook(
            Hook::<ChannelDelete>::new("antiraid", |ctx: Arc<HookCtx<ChannelDelete>>| async move {
                let settings = ctx.get_settings().await;
                on_channel_delete(&ctx.app, ctx.data.guild_id, &ctx.data.channel.id.to_string(), settings).await
            })
            .always()
            .filter(|ctx| may_be_alert_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel.id.to_string())),
        )
        // module: null: ending a lockdown works while the module is off (the handler checks MODERATOR itself).
        .button("antiraid", handle_end_button)
        .component_module(ModuleRef::Ungated)
        .job(
            JobType::AntiraidUnlock,
            job_handler(|job: Job, ctx: JobCtx| async move {
                let Some(g) = job.guild_id.parse::<u64>().ok().filter(|v| *v != 0).map(GuildId::new) else { return Ok(JobOutcome::Done) };
                handle_unlock_job(&ctx.app, g).await
            }),
        )
}
