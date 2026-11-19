//! Port of `src/dashboard/collections/balances.store.js`: the `balances` collection, the EconomyAccount rows of the
//! guild, richest first, in pages. The writes are /economy-admin's and need no gateway, so they call economyService
//! directly: credit (give), removeCoins (take), setBalance (set) and resetEconomy (reset). Like /economy-admin, bots are
//! refused (checked with Discord) and nothing requires the Economy module to be on.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bot_core::App;
use bot_core::settings::Settings;
use bot_db::{EconomyAccount, GuildSettings};
use dash_collections::collections::levels::{
    Currency, PAGE_SIZE, balance_item, is_user_id, load_profiles, profile_name, rank_cursor, read_rank_cursor, search_user_ids,
};
use dash_collections::{
    ActionAnswer, DashboardDiscord, ListAnswer, ListOpts, MemberProfile, ReadCtx, RecordError, RecordStore, Saved, StoreCtx, StoreError,
};
use feat_economy::economy_service::{self as eco, EconomyCode, ResetWhat};
use serde_json::{Map, Value};

use crate::common::{int, internal, row_json, str_at};

/// The `balances` store.
pub struct BalancesStore {
    app: App,
}

impl BalancesStore {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

const ORDER: &str = r#"ORDER BY "balance" DESC, "userId" ASC"#;

/// `rankOf(guildId, balance)`: members with the same balance share a rank (getRank of economyService).
async fn rank_of(app: &App, guild_id: &str, balance: i32) -> Result<i64, StoreError> {
    let (n,): (i64,) = sqlx::query_as(r#"SELECT COUNT(*)::bigint FROM "EconomyAccount" WHERE "guildId" = $1 AND "balance" > $2"#)
        .bind(guild_id)
        .bind(balance)
        .fetch_one(app.db())
        .await
        .map_err(internal)?;
    Ok(n + 1)
}

/// `currencyOf(settings)`.
fn currency_of(settings: Option<&GuildSettings>) -> Currency {
    let cfg = eco::economy_config(settings);
    Currency { name: cfg.currency_name, emoji: cfg.currency_emoji }
}

/// `coins(n, settings)`: "🪙 1,234 coins", without markdown.
fn coins(n: i64, settings: Option<&GuildSettings>) -> String {
    eco::format_coins(n, settings, false)
}

async fn to_items(
    app: &App,
    guild_id: &str,
    rows: &[EconomyAccount],
    discord: Option<&Arc<dyn DashboardDiscord>>,
    settings: Option<&GuildSettings>,
) -> Result<Vec<Value>, StoreError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut balances: Vec<i32> = Vec::new();
    for r in rows {
        if !balances.contains(&r.balance) {
            balances.push(r.balance);
        }
    }
    let ids: Vec<String> = rows.iter().map(|r| r.user_id.clone()).collect();
    let profiles = load_profiles(discord, &ids).await;
    let mut ranks = HashMap::new();
    for b in balances {
        ranks.insert(b, rank_of(app, guild_id, b).await?);
    }
    let currency = currency_of(settings);
    Ok(rows.iter().map(|r| balance_item(&row_json(r), ranks.get(&r.balance).copied(), profile(&profiles, &r.user_id), &currency)).collect())
}

fn profile<'a>(profiles: &'a HashMap<String, Option<MemberProfile>>, id: &str) -> Option<&'a MemberProfile> {
    profiles.get(id).and_then(Option::as_ref)
}

async fn read_item(
    app: &App,
    guild_id: &str,
    user_id: &str,
    discord: Option<&Arc<dyn DashboardDiscord>>,
    settings: Option<&GuildSettings>,
) -> Result<Option<Value>, StoreError> {
    let row = sqlx::query_as::<_, EconomyAccount>(r#"SELECT * FROM "EconomyAccount" WHERE "guildId" = $1 AND "userId" = $2"#)
        .bind(guild_id)
        .bind(user_id)
        .fetch_optional(app.db())
        .await
        .map_err(internal)?;
    let Some(row) = row else { return Ok(None) };
    Ok(to_items(app, guild_id, &[row], discord, settings).await?.into_iter().next())
}

/// `settingsOrNull(guildId)`: the settings, or None when they cannot be read (the default currency is shown).
async fn settings_or_null(app: &App, guild_id: &str) -> Option<Settings> {
    app.settings().get_settings(guild_id).await.ok()
}

/// `getSettings(guildId)` (a database failure fails the write).
async fn settings_of(app: &App, guild_id: &str) -> Result<Settings, StoreError> {
    app.settings().get_settings(guild_id).await.map_err(internal)
}

/// `checkHolder(ctx, userId, path)`: /economy-admin refuses bots; the dashboard asks Discord. Answers the profile.
async fn check_holder(ctx: &StoreCtx, user_id: &str, path: Option<&str>) -> Result<MemberProfile, StoreError> {
    let Some(discord) = crate::common::discord_with(ctx.discord.as_ref(), dash_collections::DiscordMethod::GetUsers) else {
        return Err(RecordError::unavailable("The dashboard cannot check that user with Discord right now. Try again in a moment.").into());
    };
    let profile = match discord.get_users(&[user_id.to_string()]).await {
        Ok(mut map) => map.remove(user_id).flatten(),
        Err(_) => return Err(RecordError::discord("Discord did not answer. Try again in a moment.").into()),
    };
    let with_path = |e: RecordError| match path {
        Some(p) => e.with_path(p),
        None => e,
    };
    let Some(profile) = profile else { return Err(with_path(RecordError::invalid("There is no Discord user with that ID.")).into()) };
    if profile.bot {
        return Err(with_path(RecordError::invalid("Bots can't hold coins or items.")).into());
    }
    Ok(profile)
}

fn user_id_of(id: &str) -> Result<&str, StoreError> {
    if is_user_id(id) { Ok(id) } else { Err(RecordError::not_found("That member has no balance in this server.").into()) }
}

fn log(ctx: &StoreCtx, guild_id: &str, what: &str, user_id: &str) {
    tracing::info!("Dashboard economy {what} for {user_id} in {guild_id} by {}", ctx.user_id);
}

/// `after(guildId, userId, ctx, settings)`: the item after a change.
async fn after(app: &App, guild_id: &str, user_id: &str, ctx: &StoreCtx, settings: Option<&GuildSettings>) -> Result<Value, StoreError> {
    read_item(app, guild_id, user_id, ctx.discord.as_ref(), settings)
        .await?
        .ok_or_else(|| RecordError::not_found("That member has no balance in this server.").into())
}

fn amount_text(v: Option<&Value>) -> String {
    dash_schema::js::to_js_string(v)
}

#[async_trait]
impl RecordStore for BalancesStore {
    async fn list(&self, guild_id: &str, opts: &ListOpts, ctx: &ReadCtx) -> Result<ListAnswer, StoreError> {
        let discord = ctx.discord.as_ref();
        let settings = settings_or_null(&self.app, guild_id).await;
        let s = settings.as_deref();
        let q = bot_util::js::trim(&opts.query);
        if !q.is_empty() {
            let ids = search_user_ids(discord, guild_id, q).await?;
            if ids.is_empty() {
                return Ok(ListAnswer::items(Vec::new()));
            }
            let sql = format!(r#"SELECT * FROM "EconomyAccount" WHERE "guildId" = $1 AND "userId" = ANY($2) {ORDER} LIMIT {PAGE_SIZE}"#);
            let rows =
                sqlx::query_as::<_, EconomyAccount>(sqlx::AssertSqlSafe(sql)).bind(guild_id).bind(&ids).fetch_all(self.app.db()).await.map_err(internal)?;
            return Ok(ListAnswer::items(to_items(&self.app, guild_id, &rows, discord, s).await?));
        }
        let take = PAGE_SIZE as i64 + 1;
        let rows = match read_rank_cursor(&opts.cursor) {
            Some(c) => {
                let sql = format!(r#"SELECT * FROM "EconomyAccount" WHERE "guildId" = $1 AND {} {ORDER} LIMIT $4"#, c.sql_after("balance", 2, 3));
                sqlx::query_as::<_, EconomyAccount>(sqlx::AssertSqlSafe(sql))
                    .bind(guild_id)
                    .bind(c.value as i32)
                    .bind(&c.user_id)
                    .bind(take)
                    .fetch_all(self.app.db())
                    .await
            }
            None => {
                let sql = format!(r#"SELECT * FROM "EconomyAccount" WHERE "guildId" = $1 {ORDER} LIMIT $2"#);
                sqlx::query_as::<_, EconomyAccount>(sqlx::AssertSqlSafe(sql)).bind(guild_id).bind(take).fetch_all(self.app.db()).await
            }
        }
        .map_err(internal)?;
        let page = &rows[..rows.len().min(PAGE_SIZE)];
        let items = to_items(&self.app, guild_id, page, discord, s).await?;
        let next_cursor = (rows.len() > PAGE_SIZE).then(|| page.last().map(|l| rank_cursor(i64::from(l.balance), &l.user_id))).flatten();
        Ok(ListAnswer { items, next_cursor })
    }

    async fn get(&self, guild_id: &str, id: &str, ctx: &ReadCtx) -> Result<Option<Value>, StoreError> {
        if !is_user_id(id) {
            return Ok(None);
        }
        let settings = settings_or_null(&self.app, guild_id).await;
        read_item(&self.app, guild_id, id, ctx.discord.as_ref(), settings.as_deref()).await
    }

    /// /economy-admin coins give for a member who may have no account yet.
    async fn create(&self, guild_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<Saved, StoreError> {
        let user_id = str_at(&data, "userId").unwrap_or_default().to_string();
        check_holder(ctx, &user_id, Some("userId")).await?;
        let settings = settings_of(&self.app, guild_id).await?;
        eco::credit(&self.app, guild_id, &user_id, int(data.get("amount")).unwrap_or(-1), Some(&settings)).await.map_err(internal)?;
        log(ctx, guild_id, &format!("coins give {}", amount_text(data.get("amount"))), &user_id);
        Ok(Saved::item(after(&self.app, guild_id, &user_id, ctx, Some(&settings)).await?))
    }

    async fn update(&self, _guild_id: &str, _id: &str, _data: Map<String, Value>, _ctx: &StoreCtx) -> Result<Saved, StoreError> {
        Err(RecordError::invalid("Use Give coins, Remove coins, Set balance or Reset to change a member.").into())
    }

    async fn remove(&self, _guild_id: &str, _id: &str, _ctx: &StoreCtx) -> Result<(), StoreError> {
        Err(RecordError::invalid("Use \"Reset\" to reset a member's coins or items.").into())
    }

    fn action_ids(&self) -> &[&'static str] {
        &["give", "take", "set", "reset"]
    }

    async fn action(&self, guild_id: &str, id: &str, action_id: &str, data: Map<String, Value>, ctx: &StoreCtx) -> Result<ActionAnswer, StoreError> {
        let app = &self.app;
        match action_id {
            // /economy-admin coins give: { amount }.
            "give" => {
                let user_id = user_id_of(id)?;
                let profile = check_holder(ctx, user_id, None).await?;
                let settings = settings_of(app, guild_id).await?;
                let s = Some(&*settings);
                let amount = int(data.get("amount")).unwrap_or(-1);
                let r = eco::credit(app, guild_id, user_id, amount, s).await.map_err(internal)?;
                log(ctx, guild_id, &format!("coins give {}", amount_text(data.get("amount"))), user_id);
                let message = format!(
                    "Gave {} to {}. New balance: {}{}.",
                    coins(amount, s),
                    profile_name(Some(&profile), Some(user_id)),
                    coins(r.balance, s),
                    if r.capped { " (server maximum reached)" } else { "" }
                );
                Ok(ActionAnswer { item: Some(after(app, guild_id, user_id, ctx, s).await?), message: Some(message) })
            }
            // /economy-admin coins remove: { amount }, never below 0.
            "take" => {
                let user_id = user_id_of(id)?;
                let profile = check_holder(ctx, user_id, None).await?;
                let settings = settings_of(app, guild_id).await?;
                let s = Some(&*settings);
                let amount = int(data.get("amount")).unwrap_or(-1);
                let r = eco::remove_coins(app, guild_id, user_id, amount, s).await.map_err(internal)?;
                log(ctx, guild_id, &format!("coins remove {}", amount_text(data.get("amount"))), user_id);
                let mut message = format!(
                    "Removed {} from {}. New balance: {}.",
                    coins(r.removed, s),
                    profile_name(Some(&profile), Some(user_id)),
                    coins(r.balance, s)
                );
                if r.removed < amount {
                    message.push_str(&format!(" (They only had {}.)", coins(r.removed, s)));
                }
                Ok(ActionAnswer { item: Some(after(app, guild_id, user_id, ctx, s).await?), message: Some(message) })
            }
            // /economy-admin coins set: { balance }, at most the server's maximum balance.
            "set" => {
                let user_id = user_id_of(id)?;
                let profile = check_holder(ctx, user_id, None).await?;
                let settings = settings_of(app, guild_id).await?;
                let s = Some(&*settings);
                let too_high = |max: i64| -> StoreError {
                    RecordError::invalid(format!("The maximum balance on this server is {}.", coins(max, s))).with_path("balance").into()
                };
                let max_balance = eco::economy_config(s).max_balance;
                let balance = int(data.get("balance")).unwrap_or(-1);
                if balance > max_balance {
                    return Err(too_high(max_balance));
                }
                if let Err(err) = eco::set_balance(app, guild_id, user_id, balance, s).await {
                    if let Some(e) = eco::economy_error(&err).filter(|e| e.code == EconomyCode::Invalid) {
                        return Err(too_high(e.data_i64("max").unwrap_or(max_balance)));
                    }
                    return Err(internal(err));
                }
                log(ctx, guild_id, &format!("coins set {}", amount_text(data.get("balance"))), user_id);
                let message = format!("{} now has {}.", profile_name(Some(&profile), Some(user_id)), coins(balance, s));
                Ok(ActionAnswer { item: Some(after(app, guild_id, user_id, ctx, s).await?), message: Some(message) })
            }
            // /economy-admin reset what:<coins|items|all> member:<this member>.
            "reset" => {
                let user_id = user_id_of(id)?;
                check_holder(ctx, user_id, None).await?;
                let settings = settings_of(app, guild_id).await?;
                let s = Some(&*settings);
                let what_text = str_at(&data, "what").unwrap_or("");
                let what = match what_text {
                    "coins" => ResetWhat::Coins,
                    "items" => ResetWhat::Items,
                    _ => ResetWhat::All,
                };
                let r = eco::reset_economy(app, guild_id, Some(user_id), what, s).await.map_err(internal)?;
                log(ctx, guild_id, &format!("reset {what_text}"), user_id);
                let mut done = Vec::new();
                if what_text == "coins" || what_text == "all" {
                    done.push(format!("{} balance{}", r.accounts, if r.accounts == 1 { "" } else { "s" }));
                }
                if what_text == "items" || what_text == "all" {
                    done.push(format!("{} inventory entr{}", r.inventories, if r.inventories == 1 { "y" } else { "ies" }));
                }
                let message = format!("Reset {}.", done.join(" and "));
                Ok(ActionAnswer { item: Some(after(app, guild_id, user_id, ctx, s).await?), message: Some(message) })
            }
            _ => Err(RecordError::not_found("Unknown action.").into()),
        }
    }
}
