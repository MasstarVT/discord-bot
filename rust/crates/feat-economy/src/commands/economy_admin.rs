//! Port of `src/commands/admin/economy-admin.js`: `/economy-admin` (MEE6 economy admin): coins (give / remove / set),
//! shop items (create / edit / delete / give / remove / list), settings (currency, daily, work, limits, casino
//! channels, show), coin boosts (add / remove / toggle / list) and reset. Option parsing, validation and replies; every
//! data access goes through [`crate::economy_service`] or the settings service.
//!
//! Reply visibility (deliberate): successes and system errors are public; user-input validation errors are private
//! (`reply_fail`, which swaps the public deferral for a private follow-up). Every subcommand defers before any database
//! access: publicly, except `item delete` and `reset`, which defer ephemerally, ask for confirmation in that private
//! message and post their result with a follow-up.

use bot_config::{COIN_MAX, limits};
use bot_core::custom_embed::is_http_url;
use bot_core::embed_builder::guild_embed;
use bot_core::interaction_helpers::{
    Choice, ConfirmOptions, Payload, await_confirm, check_assignable_role, check_assignable_role_cached, with_cached_guild,
};
use bot_core::new_targets::{NewOptions, new_role_option, resolve_new_options};
use bot_core::paginator::{PaginateOptions, paginate};
use bot_core::prelude::*;
use bot_db::{BoostPeriod, ChannelListMode, EconomyBoost, GuildSettings, ShopItem, ShopItemType};
use serde_json::{Map, Value, json};
use serenity::all::{GuildId, Permissions, RoleId, User};
use serenity::builder::CreateEmbed;

use super::{edit_embed, guild_of, opt, respond, settings_of, user_name, within_budget};
use crate::economy_service::{
    BOOST_PERCENT_MAX, EconomyCode, GAME_CHANNELS_MAX, ITEM_DESCRIPTION_MAX, ITEM_IMAGE_MAX, ITEM_MESSAGE_MAX, ITEM_NAME_MAX, MAX_STOCK, NewBoost,
    PITY_AMOUNT, PITY_THRESHOLD, RANGE_MAX, ResetWhat, WORK_COOLDOWN_MAX_MINUTES, boost_period_label, clamp_balances_to_cap, count_items,
    create_boost, create_item, credit, delete_boost, delete_item, economy_config, economy_error, effective_max_owned,
    find_item_by_name_insensitive, fmt_num, format_coins, give_item, is_code, item_label, item_type_label, list_boosts, list_items,
    remove_coins, reply_fail, reset_economy, resolve_boost, resolve_item, search_boosts, search_items, set_balance, set_boost_enabled,
    streak_multiplier_label, take_item, update_item, validate_boost_name, validate_currency_emoji, validate_currency_name, validate_item_name,
};

// ── Builder ───────────────────────────────────────────────────────────────────
// The payload budget is tight (about 3800 of Discord's 4000 characters): keep every description short.

type Opt = CommandOption;

fn member(d: &str) -> Opt {
    Opt::user("member", d).required(true)
}

fn item_opt() -> Opt {
    Opt::string("item", "Shop item").required(true).autocomplete(true).max_length(100)
}

fn boost_name() -> Opt {
    Opt::string("name", "Boost name").required(true).autocomplete(true).max_length(32)
}

fn qty() -> Opt {
    Opt::integer("quantity", "How many copies (default 1)").min_value(1).max_value(limits::ITEM_COPIES_PER_USER as i64)
}

fn type_option(edit: bool) -> Opt {
    Opt::string("type", if edit { "Item type" } else { "Item type (default Static)" })
        .choice("Static (collectible)", "STATIC")
        .choice("Usable (with /use)", "USABLE")
        .choice("Role (gives a role)", "ROLE")
}

fn item_fields(mut s: Opt, edit: bool) -> Opt {
    if edit {
        s = s
            .option(item_opt())
            .option(Opt::string("name", "New name").max_length(ITEM_NAME_MAX as u16))
            .option(Opt::integer("price", "Price in coins").min_value(1).max_value(COIN_MAX));
    } else {
        s = s
            .option(Opt::string("name", "Item name, unique on this server").required(true).max_length(ITEM_NAME_MAX as u16))
            .option(Opt::integer("price", "Price in coins").required(true).min_value(1).max_value(COIN_MAX));
    }
    s = s
        .option(type_option(edit))
        .option(Opt::role("role", "Role a Role item gives"))
        .option(new_role_option("role"))
        .option(Opt::string("description", "Shown in the shop (\"none\" clears)").max_length(ITEM_DESCRIPTION_MAX as u16))
        .option(Opt::string("image", "Image URL, http or https (\"none\" clears)").max_length(ITEM_IMAGE_MAX as u16))
        .option(Opt::boolean("use_on_buy", "Use the item as soon as it is bought"))
        .option(Opt::boolean("destroy_on_use", "Remove one copy each time it is used (default true)"))
        .option(Opt::boolean("can_buy_multiple", "Members may own more than one (default true)"))
        .option(
            Opt::integer("max_per_user", format!("Most copies one member may own (default {})", limits::ITEM_COPIES_PER_USER))
                .min_value(1)
                .max_value(limits::ITEM_COPIES_PER_USER as i64),
        )
        .option(
            Opt::integer("stock", if edit { "Copies left for sale (0 = sold out)" } else { "Copies for sale (default unlimited)" })
                .min_value(if edit { 0 } else { 1 })
                .max_value(MAX_STOCK),
        )
        .option(Opt::string("buy_message", "Shown after buying; {user} {item} {coins} {currency} (\"none\" clears)").max_length(ITEM_MESSAGE_MAX as u16))
        .option(Opt::string("use_message", "Shown after using; same variables (\"none\" clears)").max_length(ITEM_MESSAGE_MAX as u16));
    if edit {
        s = s.option(Opt::boolean("unlimited_stock", "Remove the stock limit")).option(Opt::boolean("enabled", "Show the item in the shop"));
    }
    s
}

pub fn data() -> SlashCommand {
    SlashCommand::new("economy-admin", "Manage the economy: coins, shop items, settings and reset (MEE6 economy admin).")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command_group("coins", "Change a member's coins (MEE6 /give-coins /remove-coins)")
                .option(
                    Opt::sub_command("give", "Add coins to a member")
                        .option(member("Member to give coins to"))
                        .option(Opt::integer("amount", "Coins to add").required(true).min_value(1).max_value(COIN_MAX)),
                )
                .option(
                    Opt::sub_command("remove", "Remove coins from a member (never below 0)")
                        .option(member("Member to take coins from"))
                        .option(Opt::integer("amount", "Coins to remove").required(true).min_value(1).max_value(COIN_MAX)),
                )
                .option(
                    Opt::sub_command("set", "Set a member's balance")
                        .option(member("Member to update"))
                        .option(Opt::integer("amount", "New balance").required(true).min_value(0).max_value(COIN_MAX)),
                ),
        )
        .option(
            Opt::sub_command_group("item", "Shop items (MEE6 /give-item /remove-item /spawn-item)")
                .option(item_fields(Opt::sub_command("create", "Create a shop item"), false))
                .option(item_fields(Opt::sub_command("edit", "Edit a shop item"), true))
                .option(Opt::sub_command("delete", "Delete an item and every copy members own").option(item_opt()))
                .option(
                    Opt::sub_command("give", "Give a member copies of an item (MEE6 /spawn-item)")
                        .option(member("Member to receive the item"))
                        .option(item_opt())
                        .option(qty()),
                )
                .option(
                    Opt::sub_command("remove", "Take copies of an item from a member's inventory")
                        .option(member("Member to take the item from"))
                        .option(item_opt())
                        .option(qty()),
                )
                .option(Opt::sub_command("list", "List every item, including disabled ones")),
        )
        .option(
            Opt::sub_command_group("config", "Economy settings")
                .option(
                    Opt::sub_command("currency", "Set the currency name and emoji")
                        .option(Opt::string("name", "Currency name, e.g. coins").max_length(32))
                        .option(Opt::string("emoji", "Currency emoji").max_length(64)),
                )
                .option(
                    Opt::sub_command("daily", "Set the /daily base reward range (default 50-120)")
                        .option(Opt::integer("min", "Lowest base reward").min_value(1).max_value(RANGE_MAX))
                        .option(Opt::integer("max", "Highest base reward").min_value(1).max_value(RANGE_MAX)),
                )
                .option(
                    Opt::sub_command("work", "Set the /work reward range and cooldown")
                        .option(Opt::integer("min", "Lowest reward").min_value(1).max_value(RANGE_MAX))
                        .option(Opt::integer("max", "Highest reward").min_value(1).max_value(RANGE_MAX))
                        .option(Opt::integer("cooldown", "Minutes between shifts (default 60)").min_value(1).max_value(WORK_COOLDOWN_MAX_MINUTES)),
                )
                .option(
                    Opt::sub_command("limits", "Set the starting balance, maximum balance, maximum bet and payout")
                        .option(Opt::integer("starting_balance", "Balance of a brand-new account").min_value(0).max_value(COIN_MAX))
                        .option(Opt::integer("max_balance", "Most coins one member may hold").min_value(1).max_value(COIN_MAX))
                        .option(Opt::integer("max_bet", "Largest casino bet (0 = no limit)").min_value(0).max_value(COIN_MAX))
                        .option(Opt::integer("max_payout", "Largest net winnings of one casino game (0 = no limit)").min_value(0).max_value(COIN_MAX)),
                )
                .option(
                    Opt::sub_command("channels", "Choose where casino games can be played (empty list = everywhere)")
                        .option(
                            Opt::string("action", "What to do")
                                .required(true)
                                .choice("add", "add")
                                .choice("remove", "remove")
                                .choice("clear", "clear")
                                .choice("list", "list"),
                        )
                        .option(Opt::channel("channel", "Channel to add or remove").channel_types(&[
                            channel_type::GUILD_TEXT,
                            channel_type::GUILD_ANNOUNCEMENT,
                            channel_type::GUILD_VOICE,
                            channel_type::GUILD_FORUM,
                        ]))
                        .option(Opt::string("mode", "allow: only listed channels; deny: all except listed").choice("allow", "allow").choice("deny", "deny")),
                )
                .option(Opt::sub_command("show", "Show the current economy settings")),
        )
        .option(
            Opt::sub_command_group("boost", "Coin boosts for /daily and /work (MEE6 boosts, up to 10).")
                .option(
                    Opt::sub_command("add", "Create a coin boost")
                        .option(Opt::string("name", "Boost name (unique)").required(true).max_length(32))
                        .option(Opt::integer("percent", "Extra coins in percent (1-500)").required(true).min_value(1).max_value(BOOST_PERCENT_MAX))
                        .option(
                            Opt::string("period", "How often a member can use it")
                                .required(true)
                                .choice("One-time", "one-time")
                                .choice("Daily", "daily")
                                .choice("Weekly", "weekly")
                                .choice("Monthly", "monthly"),
                        )
                        .option(Opt::role("role", "Only members with this role (default: everyone)")),
                )
                .option(Opt::sub_command("remove", "Delete a boost").option(boost_name()))
                .option(Opt::sub_command("toggle", "Turn a boost on or off").option(boost_name()))
                .option(Opt::sub_command("list", "List the boosts")),
        )
        .option(
            Opt::sub_command("reset", "Reset coins and/or items for everyone or one member")
                .option(
                    Opt::string("what", "What to reset")
                        .required(true)
                        .choice("Coins", "coins")
                        .choice("Items", "items")
                        .choice("Coins and items", "all"),
                )
                .option(Opt::user("member", "Only this member (default everyone)")),
        )
}

pub fn command() -> CommandDef {
    CommandDef::new("admin", data(), Some(ModuleKey::Economy), execute)
        .gated(false) // works while the module is off, so an admin can set it up first
        .permission_level(PermissionLevel::Admin)
        .autocomplete(autocomplete)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// `STREAK_TABLE`: `Day 1 ×1 · Day 2 ×1.1 · ... · Day 7+ ×4`.
pub fn streak_table() -> String {
    (1..=7).map(|day| format!("Day {day}{} {}", if day == 7 { "+" } else { "" }, streak_multiplier_label(day))).collect::<Vec<_>>().join(" · ")
}

/// A validation error thrown inside the casino-channel mutator (a database failure is not one).
#[derive(Debug)]
struct ChannelListError(String);

impl std::fmt::Display for ChannelListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ChannelListError {}

fn label(text: &str) -> String {
    bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(text), 100)
}

/// `nameOf(user)`: `**display name**`.
fn name_of(user: &User) -> String {
    format!("**{}**", label(&user_name(user)))
}

fn limit_text(n: i64, s: Option<&GuildSettings>) -> String {
    if n > 0 { format_coins(n, s, true) } else { "No limit".into() }
}

/// `field(name, value, inline = true)`: the value clamped to 1024, `—` when empty.
fn field(embed: CreateEmbed, name: &str, value: &str, inline: bool) -> CreateEmbed {
    let v = bot_util::text::clamp_text(value, 1024);
    embed.field(name, if v.is_empty() { "—".to_string() } else { v }, inline)
}

/// `textOption(options, name)`: absent `None`; `none` (any case) or an empty value clears (`Some(None)`).
fn text_option(ctx: &CommandCtx, name: &str) -> Option<Option<String>> {
    let raw = ctx.get_string(name)?;
    let text = bot_util::js::trim(&raw).to_string();
    Some(if text.is_empty() || text.to_lowercase() == "none" { None } else { Some(text) })
}

/// Resolves the `item` option; replies privately and returns `None` when there is no such item.
async fn find_item(ctx: &CommandCtx, guild_id: &str) -> anyhow::Result<Option<ShopItem>> {
    let value = ctx.get_string("item").ok_or_else(|| anyhow::anyhow!("Required option \"item\" not found."))?;
    let item = resolve_item(&ctx.app, guild_id, &value, false).await?;
    if item.is_none() {
        reply_fail(&ctx.responder, "Item Not Found", &format!("No item called **{}** exists.", label(&value))).await;
    }
    Ok(item)
}

/// `itemDetails(item, settings)`: the detail fields of an item, `(name, value, inline)`.
pub fn item_details(item: &ShopItem, s: Option<&GuildSettings>) -> Vec<(String, String, bool)> {
    let mut f = vec![
        ("Price".to_string(), format_coins(i64::from(item.price), s, true), true),
        ("Type".to_string(), item_type_label(item.r#type).to_string(), true),
    ];
    if item.r#type == ShopItemType::Role {
        let role = item.role_id.as_deref().filter(|r| !r.is_empty()).map(|r| format!("<@&{r}>")).unwrap_or_else(|| "—".into());
        f.push(("Role".into(), role, true));
    }
    let stock = match item.stock {
        None => "Unlimited".to_string(),
        Some(0) => "Sold out".to_string(),
        Some(n) => fmt_num(i64::from(n)),
    };
    f.push(("Stock".into(), stock, true));
    f.push(("Limit".into(), format!("{} per member", effective_max_owned(item)), true));
    if item.r#type != ShopItemType::Static {
        f.push(("Use on buy".into(), if item.use_on_buy { "Yes" } else { "No" }.into(), true));
        f.push(("Destroyed on use".into(), if item.destroy_on_use { "Yes" } else { "No" }.into(), true));
    }
    f.push(("In the shop".into(), if item.enabled { "Yes" } else { "No" }.into(), true));
    if let Some(d) = item.description.as_deref().filter(|d| !d.is_empty()) {
        f.push(("Description".into(), d.to_string(), false));
    }
    f
}

/// `channelListText(settings, guild, { max })`: the casino channel list for the current mode (`exists` tells whether
/// a channel is still in the guild).
pub fn channel_list_text(s: Option<&GuildSettings>, exists: impl Fn(&str) -> bool, max: usize) -> String {
    let cfg = economy_config(s);
    let deny = cfg.game_channel_mode == ChannelListMode::Deny;
    let ids = cfg.game_channel_ids;
    if ids.is_empty() {
        return if deny {
            "No channels are blocked: casino games can be played everywhere.".into()
        } else {
            "Casino games can be played in every channel.".into()
        };
    }
    let items: Vec<String> = ids.iter().map(|id| if exists(id) { format!("<#{id}>") } else { format!("~~{id}~~ (deleted)") }).collect();
    let list = bot_util::text::join_with_limit(&items, ", ", max, bot_util::text::default_more);
    let mut text = format!("{}: {list}", if deny { "Casino games are blocked in" } else { "Casino games are allowed only in" });
    if ids.iter().all(|id| !exists(id)) {
        text.push_str("\nAll of these channels are gone, so games are currently allowed everywhere.");
    }
    text
}

/// `guild.channels.cache.has(id)` against the serenity cache (channels and threads).
fn channel_exists(app: &App, guild_id: GuildId) -> impl Fn(&str) -> bool + use<> {
    let guild = app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.clone()));
    move |id: &str| {
        let Some(g) = guild.as_ref() else { return false };
        let Some(cid) = id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::ChannelId::new) else { return false };
        g.channels.contains_key(&cid) || g.threads.iter().any(|t| t.id == cid)
    }
}

fn range_text(min: i64, max: i64) -> String {
    format!("{}–{}", fmt_num(min), fmt_num(max))
}

fn required_user(ctx: &CommandCtx) -> anyhow::Result<User> {
    ctx.get_user("member").ok_or_else(|| anyhow::anyhow!("Required option \"member\" not found."))
}

fn required_int(ctx: &CommandCtx, name: &str) -> anyhow::Result<i64> {
    ctx.get_integer(name).ok_or_else(|| anyhow::anyhow!("Required option \"{name}\" not found."))
}

fn required_string(ctx: &CommandCtx, name: &str) -> anyhow::Result<String> {
    ctx.get_string(name).ok_or_else(|| anyhow::anyhow!("Required option \"{name}\" not found."))
}

// ── coins ─────────────────────────────────────────────────────────────────────

fn log_coins(ctx: &CommandCtx, sub: &str, target: &User, amount: i64) {
    bot_util::log_info!(format!(
        "Economy admin coins {sub} {amount} for {} in {} by {}",
        target.id,
        ctx.guild_id().map(|g| g.to_string()).unwrap_or_default(),
        ctx.user().id
    ));
}

async fn coins_give(ctx: &CommandCtx, g: &str, s: Option<&GuildSettings>) -> BotResult {
    let target = required_user(ctx)?;
    let amount = required_int(ctx, "amount")?;
    let r = credit(&ctx.app, g, &target.id.to_string(), amount, s).await?;
    log_coins(ctx, "give", &target, amount);
    let text = format!(
        "Gave {} to <@{}>.\nNew balance: {}{}",
        format_coins(amount, s, true),
        target.id,
        format_coins(r.balance, s, true),
        if r.capped { " (server maximum reached)" } else { "" }
    );
    edit_embed(ctx, success_embed("Coins Added", Some(&text), None)).await
}

async fn coins_remove(ctx: &CommandCtx, g: &str, s: Option<&GuildSettings>) -> BotResult {
    let target = required_user(ctx)?;
    let amount = required_int(ctx, "amount")?;
    let r = remove_coins(&ctx.app, g, &target.id.to_string(), amount, s).await?;
    log_coins(ctx, "remove", &target, amount);
    let mut text = format!("Removed {} from <@{}>.\nNew balance: {}", format_coins(r.removed, s, true), target.id, format_coins(r.balance, s, true));
    if r.removed < amount {
        text.push_str(&format!("\n(They only had {}.)", format_coins(r.removed, s, true)));
    }
    edit_embed(ctx, success_embed("Coins Removed", Some(&text), None)).await
}

async fn coins_set(ctx: &CommandCtx, g: &str, s: Option<&GuildSettings>) -> BotResult {
    let target = required_user(ctx)?;
    let amount = required_int(ctx, "amount")?;
    let too_high = |max: i64| format!("The maximum balance on this server is {}.", format_coins(max, s, true));
    let max_balance = economy_config(s).max_balance;
    if amount > max_balance {
        reply_fail(&ctx.responder, "Too High", &too_high(max_balance)).await;
        return Ok(());
    }
    if let Err(err) = set_balance(&ctx.app, g, &target.id.to_string(), amount, s).await {
        let Some(e) = economy_error(&err).filter(|e| e.code == EconomyCode::Invalid) else { return Err(err) };
        reply_fail(&ctx.responder, "Too High", &too_high(e.data_i64("max").unwrap_or(max_balance))).await;
        return Ok(());
    }
    log_coins(ctx, "set", &target, amount);
    edit_embed(ctx, success_embed("Balance Set", Some(&format!("<@{}> now has {}.", target.id, format_coins(amount, s, true))), None)).await
}

// ── item ──────────────────────────────────────────────────────────────────────

/// The role picked in `role` (or created by `new_role`): `guild.roles.cache.get(picked.id) ?? picked`, as an id.
fn picked_role(ctx: &CommandCtx) -> Option<RoleId> {
    ctx.get_role_id("role")
}

/// `checkAssignableRole(guild, guild.roles.cache.get(picked.id) ?? picked, { executor })`: the cached role, else the
/// role the interaction resolved (a role the gateway has not delivered yet).
fn role_check(ctx: &CommandCtx, guild_id: GuildId, role: RoleId) -> Result<(), String> {
    let mut chk = check_assignable_role_cached(&ctx.app, guild_id, role, ctx.member(), false);
    if !chk.ok
        && let Some(picked) = ctx.get_role("role").filter(|r| r.id == role)
        && let Some(d) = ctx.app.discord()
    {
        let bot = d.cache.current_user().id;
        let uncached = with_cached_guild(&ctx.app, guild_id, |g| (!g.roles.contains_key(&role)).then(|| check_assignable_role(g, Some(&picked), bot, ctx.member(), false)));
        if let Some(Some(c)) = uncached {
            chk = c;
        }
    }
    if chk.ok { Ok(()) } else { Err(chk.reason.unwrap_or_default()) }
}

async fn item_create(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let g = guild_id.to_string();
    macro_rules! invalid {
        ($reason:expr) => {{
            reply_fail(&ctx.responder, "Invalid Item", &$reason).await;
            return Ok(());
        }};
    }

    // 1-3. Name, uniqueness, server cap.
    let name = match validate_item_name(&required_string(ctx, "name")?) {
        Ok(v) => v,
        Err(reason) => invalid!(reason),
    };
    if find_item_by_name_insensitive(&ctx.app, &g, &name, None).await?.is_some() {
        invalid!(format!("An item named **{}** already exists.", label(&name)));
    }
    if count_items(&ctx.app, &g).await? >= limits::SHOP_ITEMS as i64 {
        invalid!(format!("This server already has {} shop items (the maximum).", limits::SHOP_ITEMS));
    }

    // 4-5. Type and role.
    let kind = ctx.get_string("type").and_then(|t| ShopItemType::parse(&t)).unwrap_or(ShopItemType::Static);
    let role = picked_role(ctx);
    if kind == ShopItemType::Role {
        let Some(r) = role else { invalid!("A Role item needs a role.") };
        if let Err(reason) = role_check(ctx, guild_id, r) {
            invalid!(reason);
        }
    } else if role.is_some() {
        invalid!("Only Role items can have a role. Set type to Role as well.");
    }

    // 6. Image.
    let image_url = text_option(ctx, "image").flatten();
    if let Some(url) = image_url.as_deref()
        && !is_http_url(url)
    {
        invalid!("The image must be an http(s) URL.");
    }

    // 7. Static items cannot be used.
    let use_on_buy = ctx.get_boolean("use_on_buy");
    if use_on_buy == Some(true) && kind == ShopItemType::Static {
        invalid!("Static items can't be used, so they can't be used on buy.");
    }

    // 8. Text fields.
    let mut data = Map::new();
    data.insert("name".into(), json!(name));
    data.insert("price".into(), json!(required_int(ctx, "price")?));
    data.insert("type".into(), json!(kind.as_str()));
    data.insert("roleId".into(), json!(role.map(|r| r.to_string())));
    data.insert("description".into(), json!(text_option(ctx, "description").flatten()));
    data.insert("imageUrl".into(), json!(image_url));
    data.insert("useOnBuy".into(), json!(use_on_buy.unwrap_or(kind == ShopItemType::Role)));
    data.insert("destroyOnUse".into(), json!(ctx.get_boolean("destroy_on_use").unwrap_or(true)));
    data.insert("canBuyMultiple".into(), json!(ctx.get_boolean("can_buy_multiple").unwrap_or(kind != ShopItemType::Role)));
    data.insert("maxPerUser".into(), json!(ctx.get_integer("max_per_user").unwrap_or(limits::ITEM_COPIES_PER_USER as i64)));
    data.insert("stock".into(), json!(ctx.get_integer("stock")));
    data.insert("buyMessage".into(), json!(text_option(ctx, "buy_message").flatten()));
    data.insert("useMessage".into(), json!(text_option(ctx, "use_message").flatten()));
    data.insert("enabled".into(), json!(true));

    let item = match create_item(&ctx.app, &g, data).await {
        Ok(item) => item,
        Err(err) => {
            let Some(e) = economy_error(&err).filter(|e| e.code == EconomyCode::Invalid) else { return Err(err) };
            if e.reason() == Some("limit") {
                invalid!(format!("This server already has {} shop items (the maximum).", limits::SHOP_ITEMS));
            }
            invalid!(format!("An item named **{}** already exists.", label(&name)));
        }
    };

    bot_util::log_info!(format!("Economy admin item create {} in {g} by {}", item.id, ctx.user().id));
    let text = format!("{} is now in the shop for {}.", item_label(&item.name), format_coins(i64::from(item.price), s, true));
    let mut embed = success_embed("Item Created", Some(&text), None);
    for (n, v, inline) in item_details(&item, s) {
        embed = field(embed, &n, &v, inline);
    }
    if let Some(url) = item.image_url.as_deref() {
        embed = embed.thumbnail(url);
    }
    edit_embed(ctx, embed).await
}

/// `ITEM_EDIT_OPTIONS`.
const ITEM_EDIT_OPTIONS: [&str; 15] = [
    "name",
    "price",
    "type",
    "role",
    "description",
    "image",
    "use_on_buy",
    "destroy_on_use",
    "can_buy_multiple",
    "max_per_user",
    "stock",
    "buy_message",
    "use_message",
    "unlimited_stock",
    "enabled",
];

async fn item_edit(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    macro_rules! invalid {
        ($reason:expr) => {{
            reply_fail(&ctx.responder, "Invalid Item", &$reason).await;
            return Ok(());
        }};
    }
    let Some(item) = find_item(ctx, &g).await? else { return Ok(()) };

    // `unlimited_stock: false` asks for nothing, so it does not count as a change.
    let asked = ITEM_EDIT_OPTIONS.iter().any(|n| ctx.has_option(n) && !(*n == "unlimited_stock" && ctx.get_boolean(n) == Some(false)));
    if !asked {
        reply_fail(&ctx.responder, "Nothing To Change", "Pass at least one option to change.").await;
        return Ok(());
    }

    let mut data = Map::new();
    let mut changed: Vec<&str> = Vec::new();
    let mut set = |data: &mut Map<String, Value>, option: &'static str, key: &str, value: Value| {
        data.insert(key.to_string(), value);
        changed.push(option);
    };

    // name
    if let Some(new_name) = ctx.get_string("name") {
        let value = match validate_item_name(&new_name) {
            Ok(v) => v,
            Err(reason) => invalid!(reason),
        };
        if find_item_by_name_insensitive(&ctx.app, &g, &value, Some(&item.id)).await?.is_some() {
            invalid!(format!("An item named **{}** already exists.", label(&value)));
        }
        set(&mut data, "name", "name", json!(value));
    }

    // type and role
    let new_type = ctx.get_string("type").and_then(|t| ShopItemType::parse(&t));
    let effective_type = new_type.unwrap_or(item.r#type);
    let role = picked_role(ctx);
    if role.is_some() && effective_type != ShopItemType::Role {
        invalid!("Only Role items can have a role. Set type to Role as well.");
    }
    if effective_type == ShopItemType::Role {
        if let Some(r) = role {
            if let Err(reason) = role_check(ctx, guild_id, r) {
                invalid!(reason);
            }
            set(&mut data, "role", "roleId", json!(r.to_string()));
        } else if new_type == Some(ShopItemType::Role) && item.role_id.as_deref().is_none_or(str::is_empty) {
            invalid!("A Role item needs a role.");
        }
    } else if item.r#type == ShopItemType::Role {
        data.insert("roleId".into(), Value::Null); // changing away from Role
    }
    if let Some(t) = new_type {
        set(&mut data, "type", "type", json!(t.as_str()));
    }

    // price and the other scalars
    if let Some(price) = ctx.get_integer("price") {
        set(&mut data, "price", "price", json!(price));
    }
    if let Some(max) = ctx.get_integer("max_per_user") {
        set(&mut data, "max_per_user", "maxPerUser", json!(max));
    }
    for (option, key) in [("destroy_on_use", "destroyOnUse"), ("can_buy_multiple", "canBuyMultiple"), ("enabled", "enabled")] {
        if let Some(v) = ctx.get_boolean(option) {
            set(&mut data, option, key, json!(v));
        }
    }

    // use on buy: a Static item cannot be used
    let use_on_buy = ctx.get_boolean("use_on_buy");
    if effective_type == ShopItemType::Static
        && (use_on_buy == Some(true) || (new_type == Some(ShopItemType::Static) && use_on_buy.is_none() && item.use_on_buy))
    {
        let extra = if use_on_buy.is_none() { " Set use_on_buy to false as well." } else { "" };
        invalid!(format!("Static items can't be used, so they can't be used on buy.{extra}"));
    }
    if let Some(v) = use_on_buy {
        set(&mut data, "use_on_buy", "useOnBuy", json!(v));
    }

    // image and texts: "none" clears
    if let Some(image) = text_option(ctx, "image") {
        if let Some(url) = image.as_deref()
            && !is_http_url(url)
        {
            invalid!("The image must be an http(s) URL.");
        }
        set(&mut data, "image", "imageUrl", json!(image));
    }
    for (option, key) in [("description", "description"), ("buy_message", "buyMessage"), ("use_message", "useMessage")] {
        if let Some(v) = text_option(ctx, option) {
            set(&mut data, option, key, json!(v));
        }
    }

    // stock
    let stock = ctx.get_integer("stock");
    let unlimited = ctx.get_boolean("unlimited_stock");
    if stock.is_some() && unlimited == Some(true) {
        invalid!("Use either stock or unlimited_stock, not both.");
    }
    if unlimited == Some(true) {
        set(&mut data, "unlimited_stock", "stock", Value::Null);
    } else if let Some(n) = stock {
        set(&mut data, "stock", "stock", json!(n));
    }

    let new_name = data.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let updated = match update_item(&ctx.app, &g, &item.id, data).await {
        Ok(u) => u,
        Err(err) => {
            if is_code(&err, EconomyCode::ItemGone) {
                reply_fail(&ctx.responder, "Item Not Found", "That item was just deleted.").await;
                return Ok(());
            }
            if is_code(&err, EconomyCode::Invalid) {
                invalid!(format!("An item named **{}** already exists.", label(&new_name)));
            }
            return Err(err);
        }
    };
    let text = format!("{} was updated: {}.", item_label(&updated.name), changed.join(", "));
    edit_embed(ctx, success_embed("Item Updated", Some(&text), None)).await
}

async fn item_delete(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let Some(item) = find_item(ctx, &g).await? else { return Ok(()) };
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Delete item?".into()),
            description: Some(format!("Delete {}? Every copy members own is removed too. This cannot be undone.", item_label(&item.name))),
            confirm_label: "Delete".into(),
            danger: true,
            ephemeral: true,
            ..ConfirmOptions::default()
        },
    )
    .await;
    if !ok {
        return Ok(());
    }
    let copies = delete_item(&ctx.app, &g, &item.id).await?;
    bot_util::log_info!(format!("Economy admin item delete {} ({copies} copies) in {g} by {}", item.id, ctx.user().id));
    let owned = if copies != 0 { format!(" ({} owned cop{} removed)", fmt_num(copies), if copies == 1 { "y" } else { "ies" }) } else { String::new() };
    let text = format!("{} was deleted{owned}.", item_label(&item.name));
    ctx.follow_up(Payload::embed(success_embed("Item Deleted", Some(&text), None)).to_follow_up()).await?;
    Ok(())
}

async fn item_give(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let target = required_user(ctx)?;
    let quantity = ctx.get_integer("quantity").unwrap_or(1);
    let Some(item) = find_item(ctx, &g).await? else { return Ok(()) };
    match give_item(&ctx.app, &g, &target.id.to_string(), &item.id, quantity).await {
        Ok(owned) => {
            bot_util::log_info!(format!("Economy admin item give {quantity}x {} to {} in {g} by {}", item.id, target.id, ctx.user().id));
            let text = format!("Gave {quantity}× {} to <@{}>. They now have {owned}.", item_label(&item.name), target.id);
            edit_embed(ctx, success_embed("Item Given", Some(&text), None)).await
        }
        Err(err) => {
            let Some(e) = economy_error(&err) else { return Err(err) };
            match e.code {
                EconomyCode::MaxOwned => {
                    let text = format!(
                        "<@{}> can hold at most {} copies (they have {}).",
                        target.id,
                        limits::ITEM_COPIES_PER_USER,
                        e.data_i64("owned").unwrap_or(0)
                    );
                    reply_fail(&ctx.responder, "Limit Reached", &text).await;
                }
                EconomyCode::ItemGone => reply_fail(&ctx.responder, "Item Not Found", "That item was just deleted.").await,
                _ => return Err(err),
            }
            Ok(())
        }
    }
}

async fn item_remove(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let target = required_user(ctx)?;
    let quantity = ctx.get_integer("quantity").unwrap_or(1);
    let Some(item) = find_item(ctx, &g).await? else { return Ok(()) };
    match take_item(&ctx.app, &g, &target.id.to_string(), &item.id, quantity).await {
        Ok(r) => {
            bot_util::log_info!(format!("Economy admin item remove {}x {} from {} in {g} by {}", r.removed, item.id, target.id, ctx.user().id));
            let text = format!("Removed {}× {} from <@{}>. They have {} left.", r.removed, item_label(&item.name), target.id, r.remaining);
            edit_embed(ctx, success_embed("Item Removed", Some(&text), None)).await
        }
        Err(err) if is_code(&err, EconomyCode::NotOwned) => {
            reply_fail(&ctx.responder, "Not Owned", &format!("<@{}> doesn't have {}.", target.id, item_label(&item.name))).await;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// One `/economy-admin item list` line.
pub fn item_list_line(i: &ShopItem, s: Option<&GuildSettings>) -> String {
    let stock = i.stock.map(|n| format!(" · stock {}", fmt_num(i64::from(n)))).unwrap_or_default();
    format!(
        "{} · {} · {}{stock}{}",
        item_label(&i.name),
        format_coins(i64::from(i.price), s, false),
        item_type_label(i.r#type),
        if i.enabled { "" } else { " · ⛔ disabled" }
    )
}

async fn item_list(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let items = list_items(&ctx.app, &guild_id.to_string(), false, true).await?;
    if items.is_empty() {
        return edit_embed(ctx, guild_embed(s, Some("Shop Items"), Some("No items yet. Create one with `/economy-admin item create`."), None)).await;
    }
    let lines: Vec<String> = items.iter().map(|i| item_list_line(i, s)).collect();
    let footer = format!("{}/{} items", items.len(), limits::SHOP_ITEMS);
    let pages: Vec<_> = lines.chunks(10).map(|c| guild_embed(s, Some("Shop Items"), Some(&c.join("\n")), Some(&footer))).collect();
    paginate(ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}

// ── config ────────────────────────────────────────────────────────────────────

async fn config_currency(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let name_opt = ctx.get_string("name");
    let emoji_opt = ctx.get_string("emoji");
    if name_opt.is_none() && emoji_opt.is_none() {
        let cfg = economy_config(s);
        let text = format!("Currency: {} **{}**\nPreview: {}", cfg.currency_emoji, cfg.currency_name, format_coins(1234, s, true));
        return edit_embed(ctx, guild_embed(s, Some("Economy Currency"), Some(&text), None)).await;
    }
    let mut patch = SettingsPatch::new();
    let mut note = "";
    if let Some(n) = name_opt {
        match validate_currency_name(&n) {
            Ok(v) => patch.insert("currencyName", v),
            Err(reason) => {
                reply_fail(&ctx.responder, "Invalid Currency", &reason).await;
                return Ok(());
            }
        }
    }
    if let Some(e) = emoji_opt {
        match validate_currency_emoji(&e) {
            Ok((value, custom_id)) => {
                patch.insert("currencyEmoji", value);
                if let Some(id) = custom_id
                    && !guild_has_emoji(&ctx.app, guild_id, &id)
                {
                    note = "\nNote: this emoji is from another server; I can only show it if I am in that server.";
                }
            }
            Err(reason) => {
                reply_fail(&ctx.responder, "Invalid Currency", &reason).await;
                return Ok(());
            }
        }
    }
    let updated = ctx.app.settings().update_settings(&guild_id.to_string(), patch).await?;
    let u = Some(updated.as_ref());
    let cfg = economy_config(u);
    let text = format!("Currency: {} **{}**\nPreview: {}{note}", cfg.currency_emoji, cfg.currency_name, format_coins(1234, u, true));
    edit_embed(ctx, success_embed("Economy Updated", Some(&text), None)).await
}

fn guild_has_emoji(app: &App, guild_id: GuildId, id: &str) -> bool {
    let Some(eid) = id.parse::<u64>().ok().filter(|v| *v != 0).map(serenity::all::EmojiId::new) else { return false };
    app.discord().and_then(|d| d.cache.guild(guild_id).map(|g| g.emojis.contains_key(&eid))).unwrap_or(false)
}

async fn config_daily(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let (min, max) = (ctx.get_integer("min"), ctx.get_integer("max"));
    let cfg = economy_config(s);
    if min.is_none() && max.is_none() {
        let text = format!("Base reward: {}\n{}", range_text(cfg.daily_min, cfg.daily_max), streak_table());
        return edit_embed(ctx, guild_embed(s, Some("Daily Reward"), Some(&text), None)).await;
    }
    let (new_min, new_max) = (min.unwrap_or(cfg.daily_min), max.unwrap_or(cfg.daily_max));
    if new_min > new_max {
        reply_fail(&ctx.responder, "Invalid Range", "The minimum must not be greater than the maximum.").await;
        return Ok(());
    }
    ctx.app.settings().update_settings(&guild_id.to_string(), SettingsPatch::new().set("dailyMin", new_min).set("dailyMax", new_max)).await?;
    let text = format!("The /daily base reward is now {}.\n{}", range_text(new_min, new_max), streak_table());
    edit_embed(ctx, success_embed("Economy Updated", Some(&text), None)).await
}

async fn config_work(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let (min, max, cooldown) = (ctx.get_integer("min"), ctx.get_integer("max"), ctx.get_integer("cooldown"));
    let cfg = economy_config(s);
    if min.is_none() && max.is_none() && cooldown.is_none() {
        let text = format!("Reward: {} every {} min", range_text(cfg.work_min, cfg.work_max), fmt_num(cfg.work_cooldown_minutes));
        return edit_embed(ctx, guild_embed(s, Some("Work Reward"), Some(&text), None)).await;
    }
    let (new_min, new_max) = (min.unwrap_or(cfg.work_min), max.unwrap_or(cfg.work_max));
    if new_min > new_max {
        reply_fail(&ctx.responder, "Invalid Range", "The minimum must not be greater than the maximum.").await;
        return Ok(());
    }
    let mut patch = SettingsPatch::new();
    if min.is_some() || max.is_some() {
        patch.insert("workMin", new_min);
        patch.insert("workMax", new_max);
    }
    if let Some(c) = cooldown {
        patch.insert("workCooldownMinutes", c);
    }
    let updated = ctx.app.settings().update_settings(&guild_id.to_string(), patch).await?;
    let now = economy_config(Some(updated.as_ref()));
    let text = format!("/work now pays {} every {} min.", range_text(now.work_min, now.work_max), fmt_num(now.work_cooldown_minutes));
    edit_embed(ctx, success_embed("Economy Updated", Some(&text), None)).await
}

/// `limitsSummary(settings)`.
pub fn limits_summary(s: Option<&GuildSettings>) -> String {
    let cfg = economy_config(s);
    [
        format!("Starting balance: {}", format_coins(cfg.starting_balance, s, true)),
        format!("Maximum balance: {}", format_coins(cfg.max_balance, s, true)),
        format!("Maximum bet: {}", limit_text(cfg.max_bet, s)),
        format!("Maximum payout: {}", limit_text(cfg.max_payout, s)),
    ]
    .join("\n")
}

async fn config_limits(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let start = ctx.get_integer("starting_balance");
    let max_balance = ctx.get_integer("max_balance");
    let max_bet = ctx.get_integer("max_bet");
    let max_payout = ctx.get_integer("max_payout");
    if start.is_none() && max_balance.is_none() && max_bet.is_none() && max_payout.is_none() {
        return edit_embed(ctx, guild_embed(s, Some("Economy Limits"), Some(&limits_summary(s)), None)).await;
    }
    let cfg = economy_config(s);
    let new_max = max_balance.unwrap_or(cfg.max_balance);
    let new_start = start.unwrap_or(cfg.starting_balance);
    if new_start > new_max {
        reply_fail(&ctx.responder, "Invalid Limits", "The starting balance cannot be above the maximum balance.").await;
        return Ok(());
    }
    let mut patch = SettingsPatch::new();
    for (key, v) in [("economyStartingBalance", start), ("economyMaxBalance", max_balance), ("economyMaxBet", max_bet), ("economyMaxPayout", max_payout)] {
        if let Some(v) = v {
            patch.insert(key, v);
        }
    }
    let g = guild_id.to_string();
    let updated = ctx.app.settings().update_settings(&g, patch).await?;
    let mut text = limits_summary(Some(updated.as_ref()));
    if max_balance.is_some_and(|m| m < cfg.max_balance) {
        let n = clamp_balances_to_cap(&ctx.app, &g, new_max).await?;
        if n > 0 {
            text.push_str(&format!("\n{} balance{} lowered to the new maximum.", fmt_num(n as i64), if n == 1 { " was" } else { "s were" }));
        }
    }
    edit_embed(ctx, success_embed("Economy Updated", Some(&text), None)).await
}

async fn config_channels(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let action = required_string(ctx, "action")?;
    let channel = ctx.get_channel_id("channel").map(|c| c.to_string());
    let mode = match ctx.get_string("mode").as_deref() {
        Some("allow") => Some(ChannelListMode::Allow),
        Some("deny") => Some(ChannelListMode::Deny),
        _ => None,
    };
    if (action == "add" || action == "remove") && channel.is_none() {
        reply_fail(&ctx.responder, "Missing Channel", "Pick a channel.").await;
        return Ok(());
    }
    let exists = channel_exists(&ctx.app, guild_id);
    if action == "list" && mode.is_none() {
        return edit_embed(ctx, guild_embed(s, Some("Casino Channels"), Some(&channel_list_text(s, &exists, 3500)), None)).await;
    }

    let act = action.clone();
    let updated = ctx
        .app
        .settings()
        .update_settings_with(&guild_id.to_string(), move |cur| {
            let mut ids = cur.economy_game_channel_ids.clone();
            let deny = mode.unwrap_or(cur.economy_game_channel_mode) == ChannelListMode::Deny;
            let ch = channel.unwrap_or_default();
            match act.as_str() {
                "add" => {
                    if ids.contains(&ch) {
                        return Err(ChannelListError(format!("That channel is already {}.", if deny { "blocked" } else { "allowed" })).into());
                    }
                    if ids.len() >= GAME_CHANNELS_MAX {
                        return Err(ChannelListError(format!("You can {} at most {GAME_CHANNELS_MAX} channels.", if deny { "block" } else { "allow" })).into());
                    }
                    ids.push(ch);
                }
                "remove" => {
                    let Some(at) = ids.iter().position(|i| *i == ch) else {
                        return Err(ChannelListError("That channel is not in the list.".into()).into());
                    };
                    ids.remove(at);
                }
                "clear" => ids.clear(),
                _ => {}
            }
            let mut patch = SettingsPatch::new().set("economyGameChannelIds", json!(ids));
            if let Some(m) = mode {
                patch.insert("economyGameChannelMode", m.as_str());
            }
            Ok(patch)
        })
        .await;
    let updated = match updated {
        Ok(u) => u,
        Err(err) => {
            let Some(e) = err.downcast_ref::<ChannelListError>() else { return Err(err) };
            reply_fail(&ctx.responder, "Not Changed", &e.0).await;
            return Ok(());
        }
    };
    let mode_line = match mode {
        Some(m) => format!("Mode: **{}**\n", if m == ChannelListMode::Deny { "deny list" } else { "allow list" }),
        None => String::new(),
    };
    let text = format!("{mode_line}{}", channel_list_text(Some(updated.as_ref()), &exists, 3500));
    edit_embed(ctx, success_embed("Casino Channels Updated", Some(&text), None)).await
}

async fn config_show(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let cfg = economy_config(s);
    let count = count_items(&ctx.app, &guild_id.to_string()).await?;
    let deny = cfg.game_channel_mode == ChannelListMode::Deny;
    // An empty list restricts nothing, whichever mode it is in, so the mode name would only suggest a restriction.
    let channel_mode = if cfg.game_channel_ids.is_empty() {
        "No restriction"
    } else if deny {
        "Deny list"
    } else {
        "Allow list"
    };
    let exists = channel_exists(&ctx.app, guild_id);
    let module = if is_module_enabled(s, Some(ModuleKey::Economy)) { "✅ On" } else { "❌ Off · enable with /module enable economy" };
    let fields: Vec<(&str, String, bool)> = vec![
        ("Module", module.into(), true),
        ("Currency", format!("{} {}", cfg.currency_emoji, cfg.currency_name), true),
        ("Starting balance", format_coins(cfg.starting_balance, s, true), true),
        ("Maximum balance", format_coins(cfg.max_balance, s, true), true),
        ("Maximum bet", limit_text(cfg.max_bet, s), true),
        ("Maximum payout", limit_text(cfg.max_payout, s), true),
        ("Daily", format!("{} base · streak up to ×4 · 24 h cooldown, streak kept for 48 h", range_text(cfg.daily_min, cfg.daily_max)), false),
        ("Work", format!("{} every {} min", range_text(cfg.work_min, cfg.work_max), fmt_num(cfg.work_cooldown_minutes)), true),
        ("Pity", format!("{} below {}, every 12 h", fmt_num(PITY_AMOUNT), fmt_num(PITY_THRESHOLD)), true),
        ("Casino channels", format!("**{channel_mode}**\n{}", channel_list_text(s, &exists, 800)), false),
        ("Shop", format!("{count}/{} items", limits::SHOP_ITEMS), true),
    ];
    let embed = fields.into_iter().fold(guild_embed(s, Some("🪙 Economy Settings"), None, None), |e, (n, v, inline)| field(e, n, &v, inline));
    edit_embed(ctx, embed).await
}

// ── boost ─────────────────────────────────────────────────────────────────────

const BOOST_PER_PAGE: usize = 10;

fn period_choice(s: &str) -> Option<BoostPeriod> {
    match s {
        "one-time" => Some(BoostPeriod::OneTime),
        "daily" => Some(BoostPeriod::Daily),
        "weekly" => Some(BoostPeriod::Weekly),
        "monthly" => Some(BoostPeriod::Monthly),
        _ => None,
    }
}

/// `PERIOD_TEXT`.
pub fn period_text(p: BoostPeriod) -> &'static str {
    match p {
        BoostPeriod::OneTime => "once per member, ever",
        BoostPeriod::Daily => "once per member per day",
        BoostPeriod::Weekly => "once per member per week",
        BoostPeriod::Monthly => "once per member per month",
    }
}

fn boost_label(name: &str) -> String {
    format!("**{}**", bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(name), 40))
}

fn boost_target(b: &EconomyBoost) -> String {
    b.role_id.as_deref().filter(|r| !r.is_empty()).map(|r| format!("<@&{r}>")).unwrap_or_else(|| "everyone".into())
}

/// One `/economy-admin boost list` line.
pub fn boost_list_line(b: &crate::economy_service::BoostWithUses) -> String {
    let uses = b.count.uses;
    format!(
        "{} · +{}% · {} · {} · {} · {} use{}",
        boost_label(&b.boost.name),
        b.boost.percent,
        boost_period_label(b.boost.period),
        boost_target(&b.boost),
        if b.boost.enabled { "✅ on" } else { "⛔ off" },
        fmt_num(uses),
        if uses == 1 { "" } else { "s" }
    )
}

async fn boost_add(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let name = match validate_boost_name(&required_string(ctx, "name")?) {
        Ok(v) => v,
        Err(reason) => {
            reply_fail(&ctx.responder, "Invalid Name", &reason).await;
            return Ok(());
        }
    };
    let period = period_choice(&required_string(ctx, "period")?).ok_or_else(|| anyhow::anyhow!("Unknown boost period"))?;
    let role_id = ctx.get_role_id("role").filter(|r| r.get() != guild_id.get()).map(|r| r.to_string()); // @everyone means "everyone"
    let percent = required_int(ctx, "percent")?;
    let g = guild_id.to_string();
    let boost = match create_boost(&ctx.app, &g, NewBoost { name: name.clone(), percent, period, role_id, created_by: ctx.user().id.to_string() }).await {
        Ok(b) => b,
        Err(err) => {
            if let Some(e) = economy_error(&err).filter(|e| e.code == EconomyCode::Invalid) {
                match e.reason() {
                    Some("limit") => {
                        let text = format!("This server already has {} boosts (the maximum).", limits::ECONOMY_BOOSTS);
                        reply_fail(&ctx.responder, "Too Many Boosts", &text).await;
                        return Ok(());
                    }
                    Some("name-taken") => {
                        reply_fail(&ctx.responder, "Name Taken", &format!("A boost named {} already exists.", boost_label(&name))).await;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            return Err(err);
        }
    };
    bot_util::log_info!(format!(
        "Economy boost \"{}\" +{percent}% ({}) created in {g} by {}",
        boost.name,
        period.as_str(),
        ctx.user().id
    ));
    let text = format!(
        "{} gives {} **+{}%** on one /daily or /work payout, {}.",
        boost_label(&boost.name),
        boost_target(&boost),
        boost.percent,
        period_text(boost.period)
    );
    let footer = format!(
        "{}Only the highest unused boost applies to a payout.",
        if s.is_some_and(|s| s.economy_enabled) { "" } else { "The economy module is off right now. " }
    );
    edit_embed(ctx, success_embed("Boost Added", Some(&text), Some(&footer))).await
}

async fn boost_not_found(ctx: &CommandCtx, value: &str) {
    let text = format!("No boost called **{}** exists.", bot_util::text::clamp_text(&bot_core::markdown::escape_markdown(value), 40));
    reply_fail(&ctx.responder, "Boost Not Found", &text).await;
}

async fn boost_remove(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let value = required_string(ctx, "name")?;
    let boost = resolve_boost(&ctx.app, &g, &value).await?;
    let Some(boost) = boost else {
        boost_not_found(ctx, &value).await;
        return Ok(());
    };
    if !delete_boost(&ctx.app, &g, &boost.id).await? {
        boost_not_found(ctx, &value).await;
        return Ok(());
    }
    bot_util::log_info!(format!("Economy boost \"{}\" deleted in {g} by {}", boost.name, ctx.user().id));
    edit_embed(ctx, success_embed("Boost Removed", Some(&format!("{} was deleted.", boost_label(&boost.name))), None)).await
}

async fn boost_toggle(ctx: &CommandCtx, guild_id: GuildId) -> BotResult {
    let g = guild_id.to_string();
    let value = required_string(ctx, "name")?;
    let updated = match resolve_boost(&ctx.app, &g, &value).await? {
        Some(b) => set_boost_enabled(&ctx.app, &g, &b.id, !b.enabled).await?,
        None => None,
    };
    let Some(updated) = updated else {
        boost_not_found(ctx, &value).await;
        return Ok(());
    };
    let title = if updated.enabled { "Boost Enabled" } else { "Boost Disabled" };
    let text = format!("{} is now {}.", boost_label(&updated.name), if updated.enabled { "on" } else { "off" });
    edit_embed(ctx, success_embed(title, Some(&text), None)).await
}

async fn boost_list(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let boosts = list_boosts(&ctx.app, &guild_id.to_string()).await?;
    if boosts.is_empty() {
        return edit_embed(ctx, guild_embed(s, Some("🚀 Coin Boosts"), Some("No boosts yet. Create one with `/economy-admin boost add`."), None)).await;
    }
    let lines: Vec<String> = boosts.iter().map(boost_list_line).collect();
    let footer = format!("{}/{} boosts", boosts.len(), limits::ECONOMY_BOOSTS);
    let pages: Vec<_> = lines.chunks(BOOST_PER_PAGE).map(|c| guild_embed(s, Some("🚀 Coin Boosts"), Some(&c.join("\n")), Some(&footer))).collect();
    paginate(ctx, &pages, PaginateOptions::default()).await?;
    Ok(())
}

// ── reset ─────────────────────────────────────────────────────────────────────

/// The confirmation text of `reset` (without " This cannot be undone.").
pub fn reset_prompt(what: ResetWhat, target: Option<&User>, s: Option<&GuildSettings>) -> String {
    let start = format_coins(economy_config(s).starting_balance, s, true);
    let mut parts = Vec::new();
    if matches!(what, ResetWhat::Coins | ResetWhat::All) {
        parts.push(match target {
            Some(t) => format!("{}'s balance becomes {start} (the starting balance) and their daily streak and cooldowns are cleared.", name_of(t)),
            None => format!(
                "Every member's balance becomes {start} (the starting balance) and daily streaks and cooldowns are cleared. Members who never used the economy here will start at {start} instead of their old global balance."
            ),
        });
    }
    match (what, target) {
        (ResetWhat::Items, Some(t)) => parts.push(format!("Every item in {}'s inventory is removed.", name_of(t))),
        (ResetWhat::Items, None) | (ResetWhat::All, None) => parts.push("Every item in every inventory is removed. Shop items stay.".into()),
        (ResetWhat::All, Some(_)) => parts.push("Every item in their inventory is removed.".into()),
        _ => {}
    }
    parts.join(" ")
}

async fn reset(ctx: &CommandCtx, guild_id: GuildId, s: Option<&GuildSettings>) -> BotResult {
    let g = guild_id.to_string();
    let what_raw = required_string(ctx, "what")?;
    let what = ResetWhat::parse(&what_raw).ok_or_else(|| anyhow::anyhow!("Unknown reset target"))?;
    let target = ctx.get_user("member");
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Reset economy?".into()),
            description: Some(format!("{} This cannot be undone.", reset_prompt(what, target.as_ref(), s))),
            confirm_label: "Reset".into(),
            danger: true,
            ephemeral: true,
            ..ConfirmOptions::default()
        },
    )
    .await;
    if !ok {
        return Ok(());
    }
    let uid = target.as_ref().map(|t| t.id.to_string());
    let r = reset_economy(&ctx.app, &g, uid.as_deref(), what, s).await?;
    let mut done = Vec::new();
    if matches!(what, ResetWhat::Coins | ResetWhat::All) {
        done.push(format!("{} balance{}", fmt_num(r.accounts as i64), if r.accounts == 1 { "" } else { "s" }));
    }
    if matches!(what, ResetWhat::Items | ResetWhat::All) {
        done.push(format!("{} inventory entr{}", fmt_num(r.inventories as i64), if r.inventories == 1 { "y" } else { "ies" }));
    }
    bot_util::log_info!(format!("Economy admin reset {what_raw} ({}) in {g} by {}", uid.as_deref().unwrap_or("everyone"), ctx.user().id));
    let text = format!("Reset {}.", done.join(" and "));
    ctx.follow_up(Payload::embed(success_embed("Economy Reset", Some(&text), None)).to_follow_up()).await?;
    Ok(())
}

// ── Execute ───────────────────────────────────────────────────────────────────

/// `HANDLERS` keys.
pub const HANDLER_KEYS: [&str; 20] = [
    "coins give",
    "coins remove",
    "coins set",
    "item create",
    "item edit",
    "item delete",
    "item give",
    "item remove",
    "item list",
    "config currency",
    "config daily",
    "config work",
    "config limits",
    "config channels",
    "config show",
    "boost add",
    "boost remove",
    "boost toggle",
    "boost list",
    "reset",
];
/// These two confirm privately and post the result with a follow-up; every other subcommand answers publicly.
const PRIVATE_FLOW: [&str; 2] = ["item delete", "reset"];
/// Subcommands with a `member` option that must not be a bot.
const BOT_CHECKED: [&str; 6] = ["coins give", "coins remove", "coins set", "item give", "item remove", "reset"];

async fn execute(mut ctx: CommandCtx) -> BotResult {
    let group = ctx.subcommand_group();
    let sub = ctx.subcommand().unwrap_or_default();
    let key = match &group {
        Some(g) => format!("{g} {sub}"),
        None => sub.clone(),
    };
    if !HANDLER_KEYS.contains(&key.as_str()) {
        ctx.reply_embed(error_embed("Unknown Subcommand", Some("This subcommand no longer exists."), None), true).await?;
        return Ok(());
    }

    // No database needed: a bot can never hold coins or items.
    if BOT_CHECKED.contains(&key.as_str()) && ctx.get_user("member").is_some_and(|u| u.bot) {
        ctx.reply_embed(error_embed("Not Allowed", Some(super::BOTS_CANT), None), true).await?;
        return Ok(());
    }

    // Acknowledge before any settings or database access.
    ctx.defer_reply(PRIVATE_FLOW.contains(&key.as_str())).await?;
    if !resolve_new_options(&mut ctx, NewOptions::default()).await {
        return Ok(());
    }
    let guild_id = guild_of(&ctx)?;
    let settings = settings_of(&ctx, guild_id).await?;
    let s = opt(&settings);
    let g = guild_id.to_string();
    let ctx = &ctx;
    match key.as_str() {
        "coins give" => coins_give(ctx, &g, s).await,
        "coins remove" => coins_remove(ctx, &g, s).await,
        "coins set" => coins_set(ctx, &g, s).await,
        "item create" => item_create(ctx, guild_id, s).await,
        "item edit" => item_edit(ctx, guild_id).await,
        "item delete" => item_delete(ctx, guild_id).await,
        "item give" => item_give(ctx, guild_id).await,
        "item remove" => item_remove(ctx, guild_id).await,
        "item list" => item_list(ctx, guild_id, s).await,
        "config currency" => config_currency(ctx, guild_id, s).await,
        "config daily" => config_daily(ctx, guild_id, s).await,
        "config work" => config_work(ctx, guild_id, s).await,
        "config limits" => config_limits(ctx, guild_id, s).await,
        "config channels" => config_channels(ctx, guild_id, s).await,
        "config show" => config_show(ctx, guild_id, s).await,
        "boost add" => boost_add(ctx, guild_id, s).await,
        "boost remove" => boost_remove(ctx, guild_id).await,
        "boost toggle" => boost_toggle(ctx, guild_id).await,
        "boost list" => boost_list(ctx, guild_id, s).await,
        _ => reset(ctx, guild_id, s).await,
    }
}

// ── Autocomplete ──────────────────────────────────────────────────────────────

/// `boost remove | toggle`: the boost names.
async fn boost_autocomplete(ctx: &AutocompleteCtx) {
    let Some(g) = ctx.guild_id().map(|g| g.to_string()) else {
        respond(ctx, Vec::new()).await;
        return;
    };
    let (app, q) = (ctx.app.clone(), ctx.focused_value());
    match within_budget(ctx, "economy-admin boost autocomplete", async move { search_boosts(&app, &g, &q).await }).await {
        Ok(rows) => {
            let choices = rows
                .into_iter()
                .map(|r| Choice::new(format!("{} (+{}%){}", r.name, r.percent, if r.enabled { "" } else { " (off)" }), r.id))
                .collect();
            respond(ctx, choices).await;
        }
        Err(err) => {
            bot_util::log_debug!(format!("economy-admin boost autocomplete failed: {err}"));
            respond(ctx, Vec::new()).await;
        }
    }
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    if ctx.subcommand_group().as_deref() == Some("boost") {
        boost_autocomplete(&ctx).await;
        return Ok(());
    }
    let focused = ctx.focused();
    let (Some((name, value)), Some(guild_id)) = (focused, ctx.guild_id()) else {
        respond(&ctx, Vec::new()).await;
        return Ok(());
    };
    if name != "item" {
        respond(&ctx, Vec::new()).await;
        return Ok(());
    }
    if bot_core::interaction::ack_budget_ms(ctx.interaction.id) == 0 {
        respond(&ctx, Vec::new()).await;
        return Ok(());
    }
    let query = bot_util::js::utf16_prefix(bot_util::js::trim(&value), 100).to_string();
    let (app, g) = (ctx.app.clone(), guild_id.to_string());
    match within_budget(&ctx, "economy-admin autocomplete", async move { search_items(&app, &g, &query, false, 25).await }).await {
        Ok(rows) => {
            let choices = rows
                .into_iter()
                .map(|r| Choice::new(format!("{}{} — {}", r.name, if r.enabled { "" } else { " (disabled)" }, fmt_num(r.price)), r.id))
                .collect();
            respond(&ctx, choices).await;
        }
        Err(err) => {
            bot_util::log_debug!(format!("economy-admin autocomplete failed: {err}"));
            respond(&ctx, Vec::new()).await;
        }
    }
    Ok(())
}
