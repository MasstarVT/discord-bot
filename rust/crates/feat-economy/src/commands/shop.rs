//! Port of `src/commands/economy/shop.js`: `/shop view | buy | info` (MEE6 /shop /buy). Purchases are ONE database
//! transaction (stock, price, limit, inventory), so two buyers can never take the last copy and nobody can spend coins
//! twice. A role that cannot be granted after the commit refunds the purchase. Emits the internal hook
//! `economyPurchase` after a successful purchase.

use bot_core::custom_embed::is_http_url;
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{Choice, Payload, check_assignable_role_cached};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_core::sync::KeyedMutex;
use bot_db::{GuildSettings, ShopItem, ShopItemType};
use serenity::all::{Member, RoleId, UserId};

use super::{cached_role, edit_embed, guild_of, opt, render_message, respond, settings_of, within_budget};
use crate::economy_service::{
    DEFAULT_BUY_MESSAGE, DEFAULT_USE_MESSAGE, EconomyCode, Purchase, Refund, economy_config, economy_error, effective_max_owned, fmt_num,
    format_coins, get_owned_quantity, goes_to_inventory, item_label, item_type_label, item_vars, list_items, peek_account, purchase_item,
    refund_purchase, reply_fail, resolve_item, search_items,
};

const PER_PAGE: usize = 6;

/// Use-on-buy role purchases, serialised per member (`roleBuys = createKeyedMutex()`).
#[derive(Default)]
pub struct RoleBuys(pub KeyedMutex);

pub fn data() -> SlashCommand {
    SlashCommand::new("shop", "Browse and buy items from this server's shop (MEE6 /shop /buy).")
        .option(
            CommandOption::sub_command("view", "List the items for sale")
                .option(CommandOption::integer("page", "Page to open").min_value(1).max_value(50)),
        )
        .option(
            CommandOption::sub_command("buy", "Buy an item (MEE6 /buy)")
                .option(CommandOption::string("item", "Item to buy").required(true).autocomplete(true).max_length(100))
                .option(CommandOption::integer("quantity", "How many to buy (default 1)").min_value(1).max_value(200)),
        )
        .option(
            CommandOption::sub_command("info", "Show details about an item")
                .option(CommandOption::string("item", "Item to show").required(true).autocomplete(true).max_length(100)),
        )
}

pub fn command() -> CommandDef {
    CommandDef::new("economy", data(), Some(ModuleKey::Economy), execute)
        .permission_level(PermissionLevel::Member)
        .autocomplete(autocomplete)
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let focused = ctx.focused_value();
    let Some(guild_id) = ctx.guild_id().map(|g| g.to_string()) else {
        respond(&ctx, Vec::new()).await;
        return Ok(());
    };
    let app = ctx.app.clone();
    let looked_up = within_budget(&ctx, "shop autocomplete", async move {
        let (rows, settings) = tokio::join!(search_items(&app, &guild_id, &focused, true, 25), app.settings().get_settings_safe(&guild_id));
        Ok((rows?, settings))
    })
    .await;
    match looked_up {
        Ok((rows, settings)) => {
            let currency = economy_config(settings.as_deref()).currency_name;
            let choices = rows.into_iter().map(|r| Choice::new(format!("{} — {} {currency}", r.name, fmt_num(r.price)), r.id)).collect();
            respond(&ctx, choices).await;
        }
        Err(err) => {
            bot_util::log_debug!(format!("shop autocomplete failed: {err}"));
            respond(&ctx, Vec::new()).await;
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn not_found(ctx: &CommandCtx, value: &str) -> BotResult {
    let text = format!("There is no item called **{}** in the shop.", bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(value), 100));
    reply_fail(&ctx.responder, "Item Not Found", &text).await;
    Ok(())
}

/// `stockText(item)`.
pub fn stock_text(item: &ShopItem) -> String {
    match item.stock {
        None => "Unlimited".into(),
        Some(0) => "Sold out".into(),
        Some(n) => fmt_num(i64::from(n)),
    }
}

/// `limitText(item)`.
pub fn limit_text(item: &ShopItem) -> String {
    if item.can_buy_multiple { format!("{} per member", effective_max_owned(item)) } else { "1 per member".into() }
}

/// The field of one item on a `/shop view` page: `(name, value)`.
pub fn view_field(item: &ShopItem, settings: Option<&GuildSettings>) -> (String, String) {
    let cfg = economy_config(settings);
    let mut meta = vec![item_type_label(item.r#type).to_string()];
    if item.r#type == ShopItemType::Role
        && let Some(r) = item.role_id.as_deref().filter(|r| !r.is_empty())
    {
        meta.push(format!("gives <@&{r}>"));
    }
    if let Some(stock) = item.stock {
        meta.push(if stock == 0 { "**Sold out**".into() } else { format!("{} left", fmt_num(i64::from(stock))) });
    }
    if !item.can_buy_multiple {
        meta.push("one per member".into());
    }
    let clamp = bot_util::text::clamp_text;
    let name = format!("{} — {} {}", clamp(&item.name, 200), cfg.currency_emoji, fmt_num(i64::from(item.price)));
    let value = clamp(&format!("{}\n{}", clamp(item.description.as_deref().unwrap_or("*No description*"), 300), meta.join(" · ")), 1024);
    (name, value)
}

// ── /shop view ────────────────────────────────────────────────────────────────

async fn view(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let s = opt(settings);
    let g = guild_of(ctx)?.to_string();
    let items = list_items(&ctx.app, &g, true, false).await?;
    if items.is_empty() {
        let embed = guild_embed(s, Some("🛒 Shop"), Some("The shop is empty. Admins can add items with `/economy-admin item create`."), None);
        return edit_embed(ctx, embed).await;
    }
    let acc = peek_account(&ctx.app, &g, &ctx.user().id.to_string(), s).await?; // viewing never creates a row
    let description = format!("Buy with `/shop buy`. You have {}.", format_coins(acc.balance, s, true));
    let pages: Vec<_> = items
        .chunks(PER_PAGE)
        .map(|chunk| {
            chunk.iter().fold(guild_embed(s, Some("🛒 Shop"), Some(&description), None), |embed, item| {
                let (name, value) = view_field(item, s);
                embed.field(name, value, false)
            })
        })
        .collect();
    let start = (ctx.get_integer("page").unwrap_or(1) - 1).min(pages.len() as i64 - 1);
    paginate(ctx, &pages, PaginateOptions { start_page: start as f64, ..PaginateOptions::default() }).await?;
    Ok(())
}

// ── /shop info ────────────────────────────────────────────────────────────────

async fn info(ctx: &CommandCtx, settings: &Settings) -> BotResult {
    let s = opt(settings);
    let g = guild_of(ctx)?.to_string();
    let value = ctx.get_string("item").ok_or_else(|| anyhow::anyhow!("Required option \"item\" not found."))?;
    let Some(item) = resolve_item(&ctx.app, &g, &value, true).await? else { return not_found(ctx, &value).await };
    let owned = get_owned_quantity(&ctx.app, &g, &ctx.user().id.to_string(), &item.id).await?;
    let mut embed = guild_embed(s, Some(&bot_util::text::clamp_text(&item.name, 240)), item.description.as_deref(), None);
    if let Some(url) = item.image_url.as_deref().filter(|u| is_http_url(u)) {
        embed = embed.thumbnail(url);
    }
    embed = embed.field("Price", format_coins(i64::from(item.price), s, true), true).field("Type", item_type_label(item.r#type), true);
    if item.r#type == ShopItemType::Role {
        let role = item.role_id.as_deref().filter(|r| !r.is_empty()).map(|r| format!("<@&{r}>")).unwrap_or_else(|| "—".into());
        embed = embed.field("Role", role, true);
    }
    embed = embed.field("Stock", stock_text(&item), true).field("Limit", limit_text(&item), true);
    if item.r#type != ShopItemType::Static {
        embed = embed
            .field("Use on buy", if item.use_on_buy { "Yes" } else { "No" }, true)
            .field("Destroyed on use", if item.destroy_on_use { "Yes" } else { "No" }, true);
    }
    embed = embed.field("You own", fmt_num(owned), true);
    edit_embed(ctx, embed).await
}

// ── /shop buy ─────────────────────────────────────────────────────────────────

/// `checkRoleItem(interaction, item, member)`: true when a role item is fine to hand out (otherwise a private error
/// was shown).
async fn check_role_item(ctx: &CommandCtx, item: &ShopItem, member_roles: &[RoleId]) -> anyhow::Result<bool> {
    let guild_id = guild_of(ctx)?;
    let Some(role) = item.role_id.as_deref().and_then(|r| cached_role(&ctx.app, guild_id, r)) else {
        reply_fail(&ctx.responder, "Item Broken", "This item's role no longer exists. Ask an admin to fix the item.").await;
        return Ok(false);
    };
    let chk = check_assignable_role_cached(&ctx.app, guild_id, role.id, None, false);
    if !chk.ok {
        reply_fail(&ctx.responder, "Item Broken", &format!("I can't give this item's role: {}", chk.reason.unwrap_or_default())).await;
        return Ok(false);
    }
    if member_roles.contains(&role.id) {
        reply_fail(&ctx.responder, "Already Owned", &format!("You already have the <@&{}> role.", role.id)).await;
        return Ok(false);
    }
    Ok(true)
}

async fn limit_reached(ctx: &CommandCtx, item: &ShopItem, max: i64, owned: i64) {
    let have = if owned != 0 { format!(" (you have {owned})") } else { String::new() };
    reply_fail(&ctx.responder, "Limit Reached", &format!("You can own at most {max} {}{have}.", item_label(&item.name))).await;
}

async fn buy(ctx: &CommandCtx, settings: Settings) -> BotResult {
    let guild_id = guild_of(ctx)?;
    let g = guild_id.to_string();
    let quantity = ctx.get_integer("quantity").unwrap_or(1);
    let value = ctx.get_string("item").ok_or_else(|| anyhow::anyhow!("Required option \"item\" not found."))?;
    let Some(item) = resolve_item(&ctx.app, &g, &value, true).await? else { return not_found(ctx, &value).await };
    let payload_roles: Vec<RoleId> = ctx.member().map(|m| m.roles.clone()).unwrap_or_default();
    if item.r#type != ShopItemType::Role || !item.use_on_buy {
        return buy_item(ctx, &settings, item, quantity, payload_roles).await;
    }

    // Two buys sent together each carry a member payload without the role, so both passed the "already owned" check
    // and both were charged for one role. Role buys are serialised per member and checked against a fresh fetch.
    let user_id = ctx.user().id;
    let c = ctx.clone();
    ctx.app
        .ext::<RoleBuys>()
        .0
        .run(&format!("{g}:{user_id}"), move || async move {
            let roles = fresh_member(&c.app, guild_id, user_id).await.map(|m| m.roles).unwrap_or(payload_roles);
            buy_item(&c, &settings, item, quantity, roles).await
        })
        .await
}

/// `interaction.guild.members.fetch({ user, force: true })`; `None` when it fails (the payload member is kept).
async fn fresh_member(app: &App, guild_id: serenity::all::GuildId, user_id: UserId) -> Option<Member> {
    let http = app.http()?;
    http.get_member(guild_id, user_id).await.ok()
}

async fn buy_item(ctx: &CommandCtx, settings: &Settings, item: ShopItem, quantity: i64, member_roles: Vec<RoleId>) -> BotResult {
    let s = opt(settings);
    let guild_id = guild_of(ctx)?;
    let (g, u) = (guild_id.to_string(), ctx.user().id.to_string());
    // Pre-checks are for a friendly message only; the transaction re-checks everything atomically.
    if !item.can_buy_multiple && quantity > 1 {
        reply_fail(&ctx.responder, "One Per Member", &format!("You can only own one {}.", item_label(&item.name))).await;
        return Ok(());
    }
    let role_on_buy = item.r#type == ShopItemType::Role && item.use_on_buy;
    if role_on_buy && quantity > 1 {
        reply_fail(&ctx.responder, "One At A Time", "Role items are bought one at a time.").await;
        return Ok(());
    }
    if let Some(stock) = item.stock
        && i64::from(stock) < quantity
    {
        let text = if stock == 0 { format!("{} is sold out.", item_label(&item.name)) } else { format!("Only {stock} left.") };
        reply_fail(&ctx.responder, "Sold Out", &text).await;
        return Ok(());
    }
    if role_on_buy && !check_role_item(ctx, &item, &member_roles).await? {
        return Ok(());
    }
    if goes_to_inventory(&item) {
        let owned = get_owned_quantity(&ctx.app, &g, &u, &item.id).await?;
        let max = effective_max_owned(&item);
        if owned + quantity > max {
            limit_reached(ctx, &item, max, owned).await;
            return Ok(());
        }
    }

    let res: Purchase = match purchase_item(&ctx.app, &g, &u, &item.id, quantity, s).await {
        Ok(r) => r,
        Err(err) => {
            let Some(e) = economy_error(&err) else { return Err(err) };
            match e.code {
                EconomyCode::OutOfStock | EconomyCode::ItemGone => {
                    reply_fail(&ctx.responder, "Not Available", &format!("{} is sold out or no longer for sale.", item_label(&item.name))).await;
                }
                EconomyCode::Insufficient => {
                    let acc = peek_account(&ctx.app, &g, &u, s).await?;
                    let text = format!(
                        "{} costs {} but you only have {}.",
                        item_label(&item.name),
                        format_coins(e.data_i64("total").unwrap_or(0), s, true),
                        format_coins(acc.balance, s, true)
                    );
                    reply_fail(&ctx.responder, "Insufficient Funds", &text).await;
                }
                EconomyCode::MaxOwned => {
                    let max = e.data_i64("max").unwrap_or(0);
                    let owned = get_owned_quantity(&ctx.app, &g, &u, &item.id).await?;
                    limit_reached(ctx, &item, max, owned).await;
                }
                _ => return Err(err),
            }
            return Ok(());
        }
    };

    // From here on every decision uses the row that the transaction locked, not the pre-read item.
    let bought = &res.item;
    let use_on_buy = bought.use_on_buy && bought.r#type != ShopItemType::Static;
    if bought.r#type == ShopItemType::Role && bought.use_on_buy {
        let granted = grant_role(&ctx.app, guild_id, ctx.user().id, bought.role_id.as_deref(), &format!("Bought shop item: {}", bought.name)).await;
        if let Err(msg) = granted {
            bot_util::log_warn!(format!(
                "Shop: could not grant role {} for item {} in {g}: {msg}",
                bought.role_id.as_deref().unwrap_or("null"),
                bought.id
            ));
            let refund = Refund {
                guild_id: &g,
                user_id: &u,
                item: bought,
                quantity,
                total: res.total,
                to_inventory: res.to_inventory,
                settings: s,
            };
            if refund_purchase(&ctx.app, refund).await.is_err() {
                reply_fail(&ctx.responder, "Purchase Failed", "I couldn't give you the role and the automatic refund failed. Please contact an admin.").await;
                return Ok(());
            }
            reply_fail(
                &ctx.responder,
                "Purchase Refunded",
                "I couldn't give you the role, so your coins were refunded. Ask an admin to check my role permissions.",
            )
            .await;
            return Ok(());
        }
    }

    let price = i64::from(bought.price);
    let buy_vars = item_vars(&bought.name, price, quantity, res.total, s);
    let mut description = render_message(ctx, ctx.member(), bought.buy_message.as_deref().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_BUY_MESSAGE), buy_vars, 3500);
    if use_on_buy {
        let use_vars = item_vars(&bought.name, price, 1, price, s);
        let used = render_message(ctx, ctx.member(), bought.use_message.as_deref().filter(|m| !m.is_empty()).unwrap_or(DEFAULT_USE_MESSAGE), use_vars, 500);
        description.push_str(&format!("\n\n{used}"));
    }
    let mut embed = success_embed("Purchase Complete", Some(&description), None).field("Balance", format_coins(res.balance, s, true), true);
    if let Some(owned) = res.owned {
        embed = embed.field("You own", fmt_num(owned), true);
    }
    if let Some(url) = bought.image_url.as_deref().filter(|u| is_http_url(u)) {
        embed = embed.thumbnail(url);
    }
    // The purchase is already committed: a failed reply must not stop the hook below.
    if let Err(err) = ctx.edit_reply(Payload::embed(embed).to_edit()).await {
        bot_util::log_warn!(format!("Shop: purchase reply failed: {err}"));
    }

    ctx.app
        .run_hooks(EconomyPurchase {
            guild_id,
            member: ctx.member().cloned(),
            user_id: ctx.user().id,
            item: res.item.clone(),
            quantity,
            total_price: res.total,
            channel_id: Some(ctx.channel_id()),
        })
        .await;
    Ok(())
}

/// `member.roles.add(roleId, reason)`; `Err(message)` when the role could not be granted (offline included).
pub(crate) async fn grant_role(app: &App, guild_id: serenity::all::GuildId, user_id: UserId, role_id: Option<&str>, reason: &str) -> Result<(), String> {
    let role = role_id.and_then(|r| r.parse::<u64>().ok()).filter(|v| *v != 0).map(RoleId::new).ok_or_else(|| "Unknown Role".to_string())?;
    let http = app.http().ok_or_else(|| "no Discord connection".to_string())?;
    http.add_member_role(guild_id, user_id, role, Some(reason)).await.map_err(|e| bot_core::discord::error_message(&e))
}

// ── Entry point ───────────────────────────────────────────────────────────────

async fn execute(ctx: CommandCtx) -> BotResult {
    ctx.defer_reply(false).await?;
    let guild_id = guild_of(&ctx)?;
    let settings = settings_of(&ctx, guild_id).await?;
    match ctx.subcommand().as_deref() {
        Some("view") => view(&ctx, &settings).await,
        Some("info") => info(&ctx, &settings).await,
        Some("buy") => buy(&ctx, settings).await,
        _ => Ok(()),
    }
}
