//! The start sequence the three casino games share (pkg-economy.md §6.0, written out in each of `blackjack.js`,
//! `roulette.js` and `slots.js`: `loadSettings`, `begin`, `refusal`, `goneOrWarn`) and their test seam (the JS
//! `DEFAULTS` object that `execute(interaction, client, deps)` takes as a third argument).

use std::sync::Arc;

use bot_core::App;
use bot_core::embed_builder::warn_embed;
use bot_core::interaction::{CommandCtx, InteractionError};
use bot_core::interaction_helpers::is_interaction_gone;
use bot_core::settings::Settings;
use bot_db::GuildSettings;
use feat_economy::economy_service::{
    EconomyConfig, GameChannelCheck, Reserved, Settled, economy_config, format_coins, game_channel_check_cached, reply_fail, reply_fail_embed,
};
use futures::FutureExt;
use futures::future::BoxFuture;

use crate::commands::blackjack::{Card, build_deck};
use crate::commands::slots::{Symbol, spin_grid};

/// `randomInt(n)` of `node:crypto`: a uniform integer in `0..n` from the operating system's secure source.
pub fn secure_int(n: u32) -> u32 {
    use rand::RngExt;
    if n <= 1 {
        return 0;
    }
    let mut rng = rand::rand_core::UnwrapErr(rand::rngs::SysRng);
    rng.random_range(0..n)
}

/// The escrow the games use (`tryLockCasino`, `unlockCasino`, `reserveBet`, `settleBet`).
pub trait Escrow: Send + Sync {
    fn try_lock(&self, app: &App, guild_id: &str, user_id: &str) -> bool;
    fn unlock(&self, app: &App, guild_id: &str, user_id: &str);
    fn reserve<'a>(&'a self, app: &'a App, guild_id: &'a str, user_id: &'a str, bet: i64, settings: Option<&'a GuildSettings>) -> BoxFuture<'a, anyhow::Result<Reserved>>;
    fn settle<'a>(
        &'a self,
        app: &'a App,
        guild_id: &'a str,
        user_id: &'a str,
        payout: i64,
        settings: Option<&'a GuildSettings>,
        stake: i64,
    ) -> BoxFuture<'a, anyhow::Result<Settled>>;
}

/// The real escrow: feat-economy's casino functions.
pub struct EconomyEscrow;

impl Escrow for EconomyEscrow {
    fn try_lock(&self, app: &App, guild_id: &str, user_id: &str) -> bool {
        feat_economy::economy_service::try_lock_casino(app, guild_id, user_id)
    }

    fn unlock(&self, app: &App, guild_id: &str, user_id: &str) {
        feat_economy::economy_service::unlock_casino(app, guild_id, user_id)
    }

    fn reserve<'a>(&'a self, app: &'a App, guild_id: &'a str, user_id: &'a str, bet: i64, settings: Option<&'a GuildSettings>) -> BoxFuture<'a, anyhow::Result<Reserved>> {
        feat_economy::economy_service::reserve_bet(app, guild_id, user_id, bet, settings).boxed()
    }

    fn settle<'a>(
        &'a self,
        app: &'a App,
        guild_id: &'a str,
        user_id: &'a str,
        payout: i64,
        settings: Option<&'a GuildSettings>,
        stake: i64,
    ) -> BoxFuture<'a, anyhow::Result<Settled>> {
        feat_economy::economy_service::settle_bet(app, guild_id, user_id, payout, settings, stake).boxed()
    }
}

/// The test seam of the casino commands (the JS `DEFAULTS`): the economy service, the shuffle, the wheel, the reels
/// and the pause. The router only ever uses [`CasinoDeps::default`].
#[derive(Clone)]
pub struct CasinoDeps {
    pub escrow: Arc<dyn Escrow>,
    /// `loadSettings(interaction)`; `None`: the router's settings, else the memo, else a lookup bounded by the time
    /// left to acknowledge.
    #[allow(clippy::type_complexity)]
    pub load_settings: Option<Arc<dyn Fn() -> Option<Settings> + Send + Sync>>,
    pub new_deck: Arc<dyn Fn() -> Vec<Card> + Send + Sync>,
    pub spin_number: Arc<dyn Fn() -> anyhow::Result<u32> + Send + Sync>,
    pub spin_grid: Arc<dyn Fn() -> anyhow::Result<Vec<Symbol>> + Send + Sync>,
    pub sleep: Arc<dyn Fn(u64) -> BoxFuture<'static, ()> + Send + Sync>,
}

impl Default for CasinoDeps {
    fn default() -> Self {
        Self {
            escrow: Arc::new(EconomyEscrow),
            load_settings: None,
            new_deck: Arc::new(|| build_deck(&mut secure_int)),
            spin_number: Arc::new(|| Ok(crate::commands::roulette::spin_number(&mut secure_int))),
            spin_grid: Arc::new(|| Ok(spin_grid(&mut secure_int))),
            sleep: Arc::new(|ms| bot_util::async_util::sleep(ms).boxed()),
        }
    }
}

/// `goneOrWarn(what)` of the game `game` (`Blackjack`, `Roulette`, `Slots`).
pub fn gone_or_warn(game: &str, what: &str, err: &InteractionError) {
    if is_interaction_gone(err) {
        let code = err.code().map(|c| c.to_string()).unwrap_or_else(|| "undefined".into());
        bot_util::log_debug!(format!("{game}: {what}: interaction gone ({code})"));
    } else {
        bot_util::log_warn!(format!("{game}: {what} failed"), bot_util::logger::LogArg::Error { message: err.to_string(), stack: None });
    }
}

/// An error as a log argument.
pub fn err_arg(err: &anyhow::Error) -> bot_util::logger::LogArg {
    bot_core::error::log_arg(err)
}

/// The settings of the game: `loadSettings(interaction)`, memo first, then a lookup bounded by the time left to
/// acknowledge (00-overview rule 21).
pub async fn load_settings(ctx: &CommandCtx, deps: &CasinoDeps) -> Option<Settings> {
    if let Some(f) = &deps.load_settings {
        return f();
    }
    if let Some(s) = &ctx.settings {
        return Some(s.clone());
    }
    let g = ctx.guild_id()?.to_string();
    ctx.app.settings().peek_or_load(&g, ctx.ack_budget_ms()).await
}

/// `begin(interaction, bet, deps)`: settings, channel rule and max bet, all without a database access and before the
/// acknowledgement. Replies privately and returns `None` when the game is refused.
pub async fn begin(ctx: &CommandCtx, bet: i64, deps: &CasinoDeps) -> Option<(Settings, EconomyConfig)> {
    let Some(settings) = load_settings(ctx, deps).await else {
        reply_fail(&ctx.responder, "Temporarily Unavailable", "Server settings could not be loaded. Please try again in a moment.").await;
        return None;
    };
    let cfg = economy_config(Some(&settings));
    let guild_id = ctx.guild_id()?;
    let chk = game_channel_check_cached(&ctx.app, Some(&settings), guild_id, ctx.interaction.channel.as_ref(), ctx.channel_id());
    match chk {
        GameChannelCheck::Ok => {}
        GameChannelCheck::Denied { .. } => {
            reply_fail(&ctx.responder, "Wrong Channel", "Casino games are not allowed in this channel.").await;
            return None;
        }
        GameChannelCheck::NotAllowed { allowed_ids } => {
            let mentions: Vec<String> = allowed_ids.iter().map(|id| format!("<#{id}>")).collect();
            let list = bot_util::text::join_with_limit(&mentions, ", ", 1500, bot_util::text::default_more);
            reply_fail(&ctx.responder, "Wrong Channel", &format!("Casino games can only be played in {list}.")).await;
            return None;
        }
    }
    if cfg.max_bet > 0 && bet > cfg.max_bet {
        reply_fail(&ctx.responder, "Bet Too High", &format!("The maximum bet on this server is {}.", format_coins(cfg.max_bet, Some(&settings), true))).await;
        return None;
    }
    Some((settings, cfg))
}

/// `refusal(res, settings, cfg)`: the title and text for a refused reserve.
pub fn refusal(res: &Reserved, settings: &GuildSettings, cfg: &EconomyConfig) -> (&'static str, String) {
    match res {
        Reserved::Insufficient { balance } => {
            ("Insufficient Funds", format!("You only have {}. Try /daily, /work or /pity.", format_coins(*balance, Some(settings), true)))
        }
        Reserved::MaxBet { max_bet } => {
            let _ = cfg;
            ("Bet Too High", format!("The maximum bet on this server is {}.", format_coins(*max_bet, Some(settings), true)))
        }
        _ => ("Invalid Bet", "That is not a valid bet.".to_string()),
    }
}

/// The JS `finally { unlockCasino(...) }`: releases the member's casino lock when the game's `execute` ends, whatever
/// way it ends, a panic included (the router survives a panic, and without this the member would stay "in a game"
/// until a restart). [`LockGuard::hand_off`] disarms it once the blackjack collector owns the lock.
pub struct LockGuard {
    escrow: Arc<dyn Escrow>,
    app: App,
    guild_id: String,
    user_id: String,
    armed: bool,
}

impl LockGuard {
    /// Guards a lock that `try_lock` just took.
    pub fn new(deps: &CasinoDeps, app: &App, guild_id: &str, user_id: &str) -> Self {
        Self { escrow: deps.escrow.clone(), app: app.clone(), guild_id: guild_id.to_string(), user_id: user_id.to_string(), armed: true }
    }

    /// The lock now belongs to someone else (the blackjack collector's settlement or forfeit unlocks it).
    pub fn hand_off(&mut self) {
        self.armed = false;
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if self.armed {
            self.escrow.unlock(&self.app, &self.guild_id, &self.user_id);
        }
    }
}

/// The "Game In Progress" refusal.
pub async fn refuse_in_progress(ctx: &CommandCtx) {
    reply_fail_embed(&ctx.responder, warn_embed("Game In Progress", Some("You already have a casino game running. Finish it first."), None)).await;
}

/// What a settlement did (`{ failed, balance, capped, payoutCapped }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settlement {
    pub failed: bool,
    pub balance: Option<i64>,
    pub capped: bool,
    pub payout_capped: bool,
}

impl Settlement {
    pub fn from(r: anyhow::Result<Settled>) -> Result<Self, anyhow::Error> {
        r.map(|s| Self { failed: false, balance: Some(s.balance), capped: s.capped, payout_capped: s.payout_capped })
    }

    pub fn failed() -> Self {
        Self { failed: true, ..Default::default() }
    }
}

/// The Balance field: `before → after coins`, or the failure notice.
pub fn balance_text(settlement: &Settlement, before: i64, cfg: &EconomyConfig) -> String {
    if settlement.failed {
        return "⚠️ Payout failed — an admin has been notified in the bot logs.".into();
    }
    format!(
        "{} → {} {}{}",
        feat_economy::economy_service::fmt_num(before),
        feat_economy::economy_service::fmt_num(settlement.balance.unwrap_or(0)),
        cfg.currency_name,
        if settlement.capped { " (server maximum)" } else { "" }
    )
}

/// `{ guildId, userId, ... }` for the log lines.
pub fn ctx_json(guild_id: &str, user_id: &str, extra: &[(&str, i64)]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("guildId".into(), guild_id.into());
    m.insert("userId".into(), user_id.into());
    for (k, v) in extra {
        m.insert((*k).into(), (*v).into());
    }
    serde_json::Value::Object(m)
}
