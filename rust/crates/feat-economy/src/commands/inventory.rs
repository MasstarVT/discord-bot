//! Port of `src/commands/economy/inventory.js`: `/inventory [user]` (MEE6 /items), the items a member owns on THIS
//! server.

use bot_core::embed_builder::guild_embed;
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;

use super::{BOTS_CANT, edit_embed, guild_of, opt, private_error, settings_of, target_label};
use crate::economy_service::{InventoryEntry, fmt_num, get_inventory, item_label, item_type_label};

const PER_PAGE: usize = 10;

pub fn data() -> SlashCommand {
    SlashCommand::new("inventory", "Show your or a member's items (MEE6 /items).").option(CommandOption::user("user", "Member to check (default: you)"))
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute).permission_level(PermissionLevel::Member)
}

/// One line per item type: `**name** ×N · 📦 Collectible` (`· *retired*` for a disabled item).
pub fn line(row: &InventoryEntry) -> String {
    let kind = row.item.r#type.map(item_type_label).unwrap_or_default();
    let retired = if row.item.enabled.unwrap_or(true) { "" } else { " · *retired*" };
    format!("{} ×{} · {kind}{retired}", item_label(&row.item.name), fmt_num(i64::from(row.row.quantity)))
}

async fn execute(ctx: CommandCtx) -> BotResult {
    let target = ctx.get_user("user").unwrap_or_else(|| ctx.user().clone());
    if target.bot {
        return private_error(&ctx, "Not Allowed", BOTS_CANT).await;
    }
    ctx.defer_reply(false).await?;

    let guild_id = guild_of(&ctx)?;
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let is_self = target.id == ctx.user().id;
    let name = target_label(&ctx, "user", &target, is_self);
    let title = format!("🎒 {name}'s inventory");
    let rows = get_inventory(&ctx.app, &guild_id.to_string(), &target.id.to_string()).await?;

    if rows.is_empty() {
        let text = if is_self { "You have no items. Browse the shop with `/shop view`.".to_string() } else { format!("{name} has no items.") };
        return edit_embed(&ctx, guild_embed(s, Some(&title), Some(&text), None)).await;
    }

    let lines: Vec<String> = rows.iter().map(line).collect();
    let footer = format!("{} item type{}", rows.len(), if rows.len() == 1 { "" } else { "s" });
    let pages: Vec<_> = lines.chunks(PER_PAGE).map(|c| guild_embed(s, Some(&title), Some(&c.join("\n")), Some(&footer))).collect();
    paginate(&ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}
