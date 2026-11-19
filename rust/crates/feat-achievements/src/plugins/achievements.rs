//! Port of `src/plugins/achievements.js`: wires the counting hooks to [`crate::achievement_service`]. Every hook id is
//! exactly `achievements` and is gated by the module through the runner; payout and announcements are started
//! fire-and-forget by the service, so later hooks are never held up. No components, jobs or providers.

use std::sync::Arc;

use bot_core::prelude::*;

use crate::achievement_service::{self as svc, Origin};

fn premium_ms(member: &serenity::all::Member) -> Option<i64> {
    member.premium_since.map(|t| t.unix_timestamp() * 1000 + i64::from(t.timestamp_subsec_millis()))
}

async fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let o = Origin {
        guild_id: ctx.data.guild_id.to_string(),
        settings,
        user_id: ctx.data.message.author.id.to_string(),
        member: ctx.data.member.clone(),
        channel: Some(ctx.data.channel_id),
    };
    svc::on_message_create(&ctx.app, o).await;
    Ok(())
}

async fn on_reaction(ctx: Arc<HookCtx<MessageReactionAdd>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let r = &ctx.data.0;
    let guild_id = r.guild_id.to_string();
    svc::on_reaction_add(&ctx.app, &guild_id, settings, &r.user.id.to_string(), &r.message_id.to_string(), &r.emoji_key, Some(r.channel_id)).await;
    Ok(())
}

async fn on_member_update(ctx: Arc<HookCtx<GuildMemberUpdate>>) -> BotResult {
    let settings = ctx.get_settings().await;
    svc::on_member_update(&ctx.app, &ctx.data.guild_id.to_string(), settings, ctx.data.member.clone(), ctx.data.old_known).await;
    Ok(())
}

async fn on_level_up(ctx: Arc<HookCtx<LevelUp>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let d = &ctx.data;
    svc::on_level_up(&ctx.app, &d.guild_id.to_string(), settings, &d.user_id.to_string(), d.member.clone(), d.new_level, d.channel_id).await;
    Ok(())
}

async fn on_purchase(ctx: Arc<HookCtx<EconomyPurchase>>) -> BotResult {
    let settings = ctx.get_settings().await;
    let d = &ctx.data;
    svc::on_purchase(&ctx.app, &d.guild_id.to_string(), settings, &d.user_id.to_string(), d.member.clone(), d.quantity, d.channel_id).await;
    Ok(())
}

/// The `achievements` plugin.
pub fn plugin() -> Plugin {
    Plugin::new("achievements", Some(ModuleKey::Achievements))
        .hook(
            Hook::<MessageCreate>::new("achievements", on_message)
                .filter(|ctx| svc::message_filter(&ctx.app, &ctx.data.guild_id.to_string(), ctx.data.message.author.bot))
                .timeout_ms(5_000),
        )
        .hook(
            Hook::<MessageReactionAdd>::new("achievements", on_reaction)
                .filter(|ctx| svc::reaction_filter(&ctx.app, &ctx.data.0.guild_id.to_string(), ctx.data.0.user.bot))
                .timeout_ms(5_000),
        )
        .hook(Hook::<GuildMemberUpdate>::new("achievements", on_member_update).filter(|ctx| {
            let d = &ctx.data;
            svc::member_update_filter(
                &ctx.app,
                &d.guild_id.to_string(),
                d.member.user.bot,
                d.old_known,
                d.old_member.as_ref().and_then(premium_ms),
                premium_ms(&d.member),
            )
        }))
        .hook(Hook::<LevelUp>::new("achievements", on_level_up).filter(|ctx| svc::level_up_filter(&ctx.app, &ctx.data.guild_id.to_string())))
        .hook(Hook::<EconomyPurchase>::new("achievements", on_purchase).filter(|ctx| svc::purchase_filter(&ctx.app, &ctx.data.guild_id.to_string())))
}
