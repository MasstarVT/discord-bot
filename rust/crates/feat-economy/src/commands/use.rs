//! Port of `src/commands/economy/use.js`: `/use <item>` (MEE6 /use), uses an item from your inventory. A role item
//! gives its role (hierarchy checked first, so nothing is consumed when the role cannot be granted); a failed grant
//! after the item was consumed gives the item back.

use bot_core::custom_embed::is_http_url;
use bot_core::interaction_helpers::{Choice, check_assignable_role_cached};
use bot_core::prelude::*;
use bot_db::ShopItemType;

use super::shop::grant_role;
use super::{cached_role, edit_embed, guild_of, opt, render_message, respond, settings_of, within_budget};
use crate::economy_service::{
    DEFAULT_USE_MESSAGE, EconomyCode, consume_item, fmt_num, get_owned_quantity, is_code, item_label, item_vars, reply_fail, resolve_item,
    restore_item, search_inventory,
};

pub fn data() -> SlashCommand {
    SlashCommand::new("use", "Use an item from your inventory (MEE6 /use).")
        .option(CommandOption::string("item", "Item to use").required(true).autocomplete(true).max_length(100))
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute)
        .permission_level(PermissionLevel::Member)
        .autocomplete(autocomplete)
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let focused = ctx.focused_value();
    let Some(guild_id) = ctx.guild_id().map(|g| g.to_string()) else {
        respond(&ctx, Vec::new()).await;
        return Ok(());
    };
    let user_id = ctx.interaction.user.id.to_string();
    let app = ctx.app.clone();
    let looked_up = within_budget(&ctx, "use autocomplete", async move { search_inventory(&app, &guild_id, &user_id, &focused, true).await }).await;
    match looked_up {
        Ok(rows) => {
            let choices = rows.into_iter().map(|r| Choice::new(format!("{} ×{}", r.item.name, r.row.quantity), r.item.id)).collect();
            respond(&ctx, choices).await;
        }
        Err(err) => {
            bot_util::log_debug!(format!("use autocomplete failed: {err}"));
            respond(&ctx, Vec::new()).await;
        }
    }
    Ok(())
}

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    let guild_id = guild_of(&ctx)?;
    let (g, u) = (guild_id.to_string(), ctx.user().id.to_string());
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let value = ctx.get_string("item").ok_or_else(|| anyhow::anyhow!("Required option \"item\" not found."))?;

    // A retired (disabled) item can still be used.
    let Some(item) = resolve_item(&ctx.app, &g, &value, false).await? else {
        let text = format!("There is no item called **{}** here.", bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(&value), 100));
        reply_fail(&ctx.responder, "Item Not Found", &text).await;
        return Ok(());
    };
    let owned = get_owned_quantity(&ctx.app, &g, &u, &item.id).await?;
    if owned == 0 {
        reply_fail(&ctx.responder, "Not Owned", &format!("You don't have {}.", item_label(&item.name))).await;
        return Ok(());
    }
    if item.r#type == ShopItemType::Static {
        reply_fail(&ctx.responder, "Not Usable", &format!("{} is a collectible and can't be used.", item_label(&item.name))).await;
        return Ok(());
    }

    // Role items: validate before anything is consumed.
    let mut role = None;
    if item.r#type == ShopItemType::Role {
        let Some(r) = item.role_id.as_deref().and_then(|r| cached_role(&ctx.app, guild_id, r)) else {
            reply_fail(&ctx.responder, "Item Broken", "This item's role no longer exists. Ask an admin to fix the item.").await;
            return Ok(());
        };
        let chk = check_assignable_role_cached(&ctx.app, guild_id, r.id, None, false);
        if !chk.ok {
            reply_fail(&ctx.responder, "Item Broken", &format!("I can't give this item's role: {}", chk.reason.unwrap_or_default())).await;
            return Ok(());
        }
        if ctx.member().is_some_and(|m| m.roles.contains(&r.id)) {
            reply_fail(&ctx.responder, "Already Owned", &format!("You already have the <@&{}> role.", r.id)).await;
            return Ok(());
        }
        role = Some(r);
    }

    let mut remaining = owned;
    if item.destroy_on_use {
        match consume_item(&ctx.app, &g, &u, &item.id).await {
            Ok(n) => remaining = n,
            Err(err) if is_code(&err, EconomyCode::NotOwned) => {
                reply_fail(&ctx.responder, "Not Owned", &format!("You don't have {}.", item_label(&item.name))).await;
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    }

    if let Some(r) = &role {
        let role_id = r.id.to_string();
        if let Err(msg) = grant_role(&ctx.app, guild_id, ctx.user().id, Some(&role_id), &format!("Used item: {}", item.name)).await {
            bot_util::log_warn!(format!("Use: could not grant role {role_id} for item {} in {g}: {msg}", item.id));
            if item.destroy_on_use {
                restore_item(&ctx.app, &g, &u, &item.id, 1).await;
            }
            reply_fail(&ctx.responder, "Could Not Use", "I couldn't give you the role, so your item was returned.").await;
            return Ok(());
        }
    }

    let template = item.use_message.as_deref().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_USE_MESSAGE);
    let description = render_message(&ctx, ctx.member(), template, item_vars(&item.name, i64::from(item.price), 1, i64::from(item.price), s), 4000);
    let mut embed = success_embed("Item Used", Some(&description), None).field("Left", fmt_num(remaining), true);
    if let Some(url) = item.image_url.as_deref().filter(|u| is_http_url(u)) {
        embed = embed.thumbnail(url);
    }
    edit_embed(&ctx, embed).await
}
