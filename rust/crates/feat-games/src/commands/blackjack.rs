//! Port of `src/commands/games/blackjack.js`: /blackjack against the dealer on the per-server economy (pkg-economy.md
//! §5.10 and §6.1).
//!
//! Money: the bet is ESCROWED (debited atomically) before a card is dealt and settled exactly once with its gross
//! payout (R007, R136). A hand that is abandoned or times out keeps the stake taken (R018-R020). Double settlement is
//! impossible because `settled` is claimed and the collector is stopped (`done`) before any await (R016, R017, R079).
//! The casino lock is released at the end or, once the collector owns the hand, after the settlement / forfeit
//! (R077). The dealer peeks for a natural before the player acts (R078). Every collector click is acknowledged with
//! `deferUpdate()` as its FIRST await, before any database access, and the hand message is edited through the
//! command's `editReply`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_config::colors;
use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand};
use bot_core::embed_builder::{apply_guild_color, neutral_embed};
use bot_core::interaction::{CommandCtx, ComponentCtx, Responder};
use bot_core::prelude::{ModuleKey, PermissionLevel};
use bot_core::settings::Settings;
use bot_core::App;
use feat_economy::economy_service::{EconomyConfig, Reserved, clamp_payout, format_coins, format_net, reply_fail};
use serde::Serialize;
use serenity::all::{ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateInteractionResponseMessage, EditInteractionResponse};

use crate::casino::{CasinoDeps, LockGuard, Settlement, balance_text, begin, ctx_json, err_arg, gone_or_warn, refusal, refuse_in_progress, secure_int};
use crate::collector_driver::{Stopper, Timers, drive, id_filter};

pub const SUITS: [&str; 4] = ["♠️", "♥️", "♦️", "♣️"];
pub const RANKS: [&str; 13] = ["A", "2", "3", "4", "5", "6", "7", "8", "9", "10", "J", "Q", "K"];
/// The collector ends after this much silence (an abandoned hand is forfeited) ...
pub const IDLE_MS: u64 = 60_000;
/// ... or after this long in total (interaction tokens live 15 minutes).
pub const MAX_MS: u64 = 600_000;

/// A card: `{ suit, rank, value }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Card {
    pub suit: String,
    pub rank: String,
    pub value: i64,
}

impl Card {
    pub fn new(rank: &str, suit: &str) -> Self {
        Self { suit: suit.to_string(), rank: rank.to_string(), value: rank_value(rank) }
    }
}

/// `rankValue(rank)`.
pub fn rank_value(rank: &str) -> i64 {
    match rank {
        "A" => 11,
        "J" | "Q" | "K" => 10,
        n => n.parse().unwrap_or(0),
    }
}

/// `buildDeck(rng)`: 52 cards, Fisher-Yates shuffled with `rng(n)` in `0..n`.
pub fn build_deck(rng: &mut dyn FnMut(u32) -> u32) -> Vec<Card> {
    let mut deck: Vec<Card> = SUITS.iter().flat_map(|suit| RANKS.iter().map(move |rank| Card::new(rank, suit))).collect();
    let mut i = deck.len();
    while i > 1 {
        i -= 1;
        let j = (rng(i as u32 + 1) as usize).min(i);
        deck.swap(i, j);
    }
    deck
}

/// Hard total (every ace as 1) and whether one ace can still count as 11 without busting.
fn totals(hand: &[Card]) -> (i64, bool) {
    let mut hard = 0;
    let mut aces = 0;
    for card in hand {
        if card.rank == "A" {
            hard += 1;
            aces += 1;
        } else {
            hard += rank_value(&card.rank);
        }
    }
    (hard, aces > 0 && hard + 10 <= 21)
}

/// `handValue(hand)`: aces count 11 and drop to 1 one at a time while the hand would bust.
pub fn hand_value(hand: &[Card]) -> i64 {
    let (hard, soft) = totals(hand);
    if soft { hard + 10 } else { hard }
}

/// `isSoft(hand)`: an ace is still counted as 11.
pub fn is_soft(hand: &[Card]) -> bool {
    totals(hand).1
}

/// `isNatural(hand)`: exactly two cards totalling 21.
pub fn is_natural(hand: &[Card]) -> bool {
    hand.len() == 2 && hand_value(hand) == 21
}

/// `shouldDealerHit(hand)`: the dealer draws below 17 and on a soft 17 (H17).
pub fn should_dealer_hit(hand: &[Card]) -> bool {
    let value = hand_value(hand);
    value < 17 || (value == 17 && is_soft(hand))
}

/// An outcome and its gross payout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub result: &'static str,
    pub payout: i64,
}

/// `naturalOutcome(player, dealer, bet)`: the dealer's peek settles the hand before the player acts when either side
/// has a natural (R078).
pub fn natural_outcome(player: &[Card], dealer: &[Card], bet: i64) -> Option<Outcome> {
    let p = is_natural(player);
    let d = is_natural(dealer);
    if p && d {
        return Some(Outcome { result: "push-naturals", payout: bet });
    }
    if p {
        return Some(Outcome { result: "blackjack", payout: bet + (bet * 3) / 2 }); // 3:2
    }
    if d {
        return Some(Outcome { result: "dealer-blackjack", payout: 0 });
    }
    None
}

/// `finalOutcome(player, dealer, stake)`: the outcome once the dealer has played. `stake` is the total escrowed (2x
/// the bet after a double-down).
pub fn final_outcome(player: &[Card], dealer: &[Card], stake: i64) -> Outcome {
    let p = hand_value(player);
    let d = hand_value(dealer);
    if p > 21 {
        return Outcome { result: "bust", payout: 0 };
    }
    if d > 21 {
        return Outcome { result: "dealer-bust", payout: stake * 2 };
    }
    if p > d {
        return Outcome { result: "win", payout: stake * 2 };
    }
    if p < d {
        return Outcome { result: "lose", payout: 0 };
    }
    Outcome { result: "push", payout: stake }
}

/// `RESULTS[result]`: the result text and the embed colour.
pub fn result_text(result: &str) -> (&'static str, u32) {
    match result {
        "blackjack" => ("🃏 Blackjack! Paid 3:2.", colors::SUCCESS),
        "push-naturals" => ("🤝 Both have blackjack — push.", colors::NEUTRAL),
        "dealer-blackjack" => ("🂡 Dealer has blackjack.", colors::ERROR),
        "bust" => ("💥 You busted.", colors::ERROR),
        "dealer-bust" => ("🎉 Dealer busted — you win!", colors::SUCCESS),
        "win" => ("🎉 You win!", colors::SUCCESS),
        "lose" => ("😔 Dealer wins.", colors::ERROR),
        _ => ("🤝 Push — your bet is returned.", colors::NEUTRAL),
    }
}

// ── Views ────────────────────────────────────────────────────────────────────

const TITLE: &str = "♠️ Blackjack";

/// `renderHand(hand, hideSecond)`.
pub fn render_hand(hand: &[Card], hide_second: bool) -> String {
    hand.iter()
        .enumerate()
        .map(|(i, c)| if hide_second && i == 1 { "🂠".to_string() } else { format!("{}{}", c.suit, c.rank) })
        .collect::<Vec<_>>()
        .join("  ")
}

fn hand_fields(embed: CreateEmbed, player: &[Card], dealer: &[Card], revealed: bool) -> CreateEmbed {
    let dealer_value = if revealed { hand_value(dealer).to_string() } else { "?".into() };
    embed
        .field(format!("Dealer ({dealer_value})"), render_hand(dealer, !revealed), false)
        .field(format!("You ({})", hand_value(player)), render_hand(player, false), false)
}

/// `buildButtons(id, { canDouble, disabled })`: Hit, Stand and Double; Double is enabled only for a fresh two-card
/// hand within the max bet.
pub fn build_buttons(id: &str, can_double: bool, disabled: bool) -> CreateActionRow {
    let cid = |action: &str| bot_config::custom_id!("bj", id, action).unwrap_or_default();
    CreateActionRow::Buttons(vec![
        CreateButton::new(cid("hit")).label("👊 Hit").style(ButtonStyle::Primary).disabled(disabled),
        CreateButton::new(cid("stand")).label("✋ Stand").style(ButtonStyle::Secondary).disabled(disabled),
        CreateButton::new(cid("double")).label("💰 Double").style(ButtonStyle::Success).disabled(disabled || !can_double),
    ])
}

/// The embed of a hand in play.
pub fn progress_embed(settings: Option<&bot_db::GuildSettings>, player: &[Card], dealer: &[Card], stake: i64, can_double: bool) -> CreateEmbed {
    let e = apply_guild_color(neutral_embed(Some(TITLE), Some(if can_double { "Hit, stand or double down." } else { "Hit or stand." }), None), settings);
    hand_fields(e, player, dealer, false).field("Bet", format_coins(stake, settings, true), true)
}

/// What the final embed shows.
pub struct Final<'a> {
    pub player: &'a [Card],
    pub dealer: &'a [Card],
    pub result: &'a str,
    pub payout: i64,
    pub stake: i64,
    pub doubled: bool,
    pub before: i64,
    pub settlement: Settlement,
}

/// The embed of a settled hand: Bet / Payout / Net / Balance (R240).
pub fn final_embed(settings: Option<&bot_db::GuildSettings>, cfg: &EconomyConfig, f: &Final<'_>) -> CreateEmbed {
    let (text, color) = result_text(f.result);
    let mut description = format!("{text}{}", if f.doubled { " (doubled down)" } else { "" });
    let clamp = clamp_payout(f.payout, f.stake, cfg.max_payout);
    if f.settlement.payout_capped || clamp.capped {
        description.push_str(&format!("\nWinnings capped at {} by the server's max payout.", format_coins(cfg.max_payout, settings, true)));
    }
    let shown = clamp.payout;
    let e = neutral_embed(Some(TITLE), Some(&description), None).color(color);
    hand_fields(e, f.player, f.dealer, true)
        .field("Bet", format_coins(f.stake, settings, true), true)
        .field("Payout", format_coins(shown, settings, true), true)
        .field("Net", format_net(shown - f.stake, settings), true)
        .field("Balance", balance_text(&f.settlement, f.before, cfg), true)
}

/// The embed of an abandoned hand.
pub fn forfeit_embed(settings: Option<&bot_db::GuildSettings>, player: &[Card], dealer: &[Card], stake: i64) -> CreateEmbed {
    let e = neutral_embed(
        Some(TITLE),
        Some(&format!("⏰ You walked away from the table. Your bet of {} is forfeited.", format_coins(stake, settings, true))),
        None,
    )
    .color(colors::WARNING);
    hand_fields(e, player, dealer, true).field("Net", format_net(-stake, settings), true)
}

// ── Command ──────────────────────────────────────────────────────────────────

/// The data: `/blackjack bet`.
pub fn data() -> SlashCommand {
    SlashCommand::new("blackjack", "Play blackjack against the dealer for coins.")
        .option(Opt::integer("bet", "Coins to bet").required(true).min_value(1).max_value(bot_config::COIN_MAX as i64))
}

/// The command with the real economy and shuffle.
pub fn command() -> CommandDef {
    command_with(CasinoDeps::default())
}

/// The command with injected dependencies.
pub fn command_with(deps: CasinoDeps) -> CommandDef {
    CommandDef::new("games", data(), Some(ModuleKey::Economy), move |ctx| {
        let deps = deps.clone();
        async move { execute(ctx, deps).await }
    })
    .permission_level(PermissionLevel::Member)
}

/// The mutable hand.
struct Hand {
    deck: Vec<Card>,
    player: Vec<Card>,
    dealer: Vec<Card>,
    stake: i64,
    settled: bool,
    busy: bool,
}

impl Hand {
    fn draw(&mut self) -> Card {
        match self.deck.pop() {
            Some(c) => c,
            None => build_deck(&mut secure_int).pop().unwrap_or_else(|| Card::new("A", SUITS[0])),
        }
    }
}

/// Everything a running hand needs.
struct Table {
    app: App,
    deps: CasinoDeps,
    responder: Responder,
    interaction_id: String,
    guild_id: String,
    user_id: String,
    settings: Settings,
    cfg: EconomyConfig,
    bet: i64,
    before: i64,
    stopper: Stopper,
    hand: Mutex<Hand>,
}

impl Table {
    fn lock(&self) -> std::sync::MutexGuard<'_, Hand> {
        self.hand.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn can_double(&self, h: &Hand) -> bool {
        !h.settled && h.player.len() == 2 && h.stake == self.bet && !(self.cfg.max_bet > 0 && self.bet * 2 > self.cfg.max_bet)
    }

    /// Credits the gross payout once; never fails (a failed credit is shown in the Balance field).
    async fn settle(&self, payout: i64, total: i64) -> Settlement {
        match Settlement::from(self.deps.escrow.settle(&self.app, &self.guild_id, &self.user_id, payout, Some(&self.settings), total).await) {
            Ok(s) => s,
            Err(e) => {
                bot_util::log_error!(
                    "Blackjack: settlement failed",
                    ctx_json(&self.guild_id, &self.user_id, &[("payout", payout), ("stake", total)]),
                    err_arg(&e)
                );
                Settlement::failed()
            }
        }
    }

    /// Edits the hand message; a click acknowledged with deferUpdate can edit it too if the original token fails.
    async fn show(&self, edit: EditInteractionResponse, click: Option<&ComponentCtx>) {
        if let Err(e) = self.responder.edit_reply(edit.clone()).await {
            if let Some(c) = click
                && c.edit_reply(edit).await.is_ok()
            {
                return;
            }
            gone_or_warn("Blackjack", "edit", &e);
        }
    }

    /// Settles the hand once: `settled` and the collector stop come before any await (R016, R017, R079).
    async fn finish(&self, click: Option<&ComponentCtx>) {
        let (player, dealer, outcome, stake) = {
            let mut h = self.lock();
            if h.settled {
                return;
            }
            h.settled = true;
            self.stopper.stop("done");
            if hand_value(&h.player) <= 21 {
                while should_dealer_hit(&h.dealer) {
                    let c = h.draw();
                    h.dealer.push(c);
                }
            }
            let outcome = final_outcome(&h.player, &h.dealer, h.stake);
            (h.player.clone(), h.dealer.clone(), outcome, h.stake)
        };
        let settlement = self.settle(outcome.payout, stake).await;
        let embed = final_embed(
            Some(&self.settings),
            &self.cfg,
            &Final { player: &player, dealer: &dealer, result: outcome.result, payout: outcome.payout, stake, doubled: stake != self.bet, before: self.before, settlement },
        );
        self.show(EditInteractionResponse::new().embed(embed).components(Vec::new()), click).await;
        self.deps.escrow.unlock(&self.app, &self.guild_id, &self.user_id);
    }

    async fn act(&self, click: &ComponentCtx, action: &str) {
        if self.lock().settled {
            return;
        }
        match action {
            "hit" => {
                let view = {
                    let mut h = self.lock();
                    let c = h.draw();
                    h.player.push(c);
                    if hand_value(&h.player) >= 21 {
                        None
                    } else {
                        let can = self.can_double(&h);
                        Some((progress_embed(Some(&self.settings), &h.player, &h.dealer, h.stake, can), can))
                    }
                };
                match view {
                    // bust, or 21: stand automatically
                    None => self.finish(Some(click)).await,
                    Some((embed, can)) => {
                        let edit = EditInteractionResponse::new().embed(embed).components(vec![build_buttons(&self.interaction_id, can, false)]);
                        self.show(edit, Some(click)).await;
                    }
                }
            }
            "stand" => self.finish(Some(click)).await,
            "double" => {
                if !{
                    let h = self.lock();
                    self.can_double(&h)
                } {
                    return; // already acknowledged; the button was disabled
                }
                let r = match self.deps.escrow.reserve(&self.app, &self.guild_id, &self.user_id, self.bet, Some(&self.settings)).await {
                    Ok(r) => r,
                    Err(e) => {
                        bot_util::log_error!(
                            "Blackjack: reserving the double-down failed",
                            ctx_json(&self.guild_id, &self.user_id, &[("bet", self.bet)]),
                            err_arg(&e)
                        );
                        reply_fail(&click.responder, "Cannot Double Down", "Something went wrong; nothing was taken. Hit or stand instead.").await;
                        return;
                    }
                };
                match r {
                    Reserved::Ok { .. } => {}
                    Reserved::Insufficient { .. } => {
                        reply_fail(
                            &click.responder,
                            "Cannot Double Down",
                            &format!("You need another {} to double down.", format_coins(self.bet, Some(&self.settings), true)),
                        )
                        .await;
                        return;
                    }
                    _ => {
                        reply_fail(&click.responder, "Cannot Double Down", "The second bet was refused. Hit or stand instead.").await;
                        return;
                    }
                }
                let ended = {
                    let mut h = self.lock();
                    if h.settled {
                        true
                    } else {
                        h.stake = self.bet * 2;
                        let c = h.draw();
                        h.player.push(c);
                        false
                    }
                };
                if ended {
                    // The collector ended during the await (the hand was forfeited): give the extra stake back.
                    let refund = self.settle(self.bet, self.bet).await;
                    if refund.failed {
                        bot_util::log_error!("Blackjack: refund of a late double-down failed", ctx_json(&self.guild_id, &self.user_id, &[("bet", self.bet)]));
                    }
                    return;
                }
                self.finish(Some(click)).await;
            }
            _ => {}
        }
    }

    async fn on_click(self: Arc<Self>, click: ComponentCtx) {
        if click.user().id.to_string() != self.user_id {
            if let Err(e) = click.reply(CreateInteractionResponseMessage::new().content("❌ This isn't your game!").ephemeral(true)).await {
                gone_or_warn("Blackjack", "reply", &e);
            }
            return;
        }
        // Concurrent clicks are dropped (never queued): the decision is synchronous, the acknowledgement is the FIRST
        // await.
        let drop = {
            let mut h = self.lock();
            let d = h.settled || h.busy;
            if !d {
                h.busy = true;
            }
            d
        };
        if let Err(e) = click.defer_update().await {
            gone_or_warn("Blackjack", "acknowledge", &e);
        }
        if drop {
            return;
        }
        let action = click.parsed().1.get(1).cloned().unwrap_or_default();
        self.act(&click, &action).await;
        self.lock().busy = false;
    }

    /// The collector's `end`: `done` means finish() is settling and unlocks when the payout is in. Anything else
    /// abandons the hand (idle, total time): the escrowed stake stays taken (R018-R020).
    async fn on_end(self: Arc<Self>, reason: String) {
        if reason == "done" {
            return;
        }
        self.deps.escrow.unlock(&self.app, &self.guild_id, &self.user_id);
        let view = {
            let mut h = self.lock();
            if h.settled {
                return;
            }
            h.settled = true;
            forfeit_embed(Some(&self.settings), &h.player, &h.dealer, h.stake)
        };
        if let Err(e) = self.responder.edit_reply(EditInteractionResponse::new().embed(view).components(Vec::new())).await {
            gone_or_warn("Blackjack", "forfeit message", &e);
        }
    }
}

/// `execute(interaction, client, deps)`.
pub async fn execute(ctx: CommandCtx, deps: CasinoDeps) -> anyhow::Result<()> {
    let bet = ctx.get_integer("bet").ok_or_else(|| anyhow::anyhow!("Required option \"bet\" not found."))?;
    let Some((settings, cfg)) = begin(&ctx, bet, &deps).await else { return Ok(()) };
    let Some(guild_id) = ctx.guild_id().map(|g| g.to_string()) else { return Ok(()) };
    let user_id = ctx.user().id.to_string();

    // The lock is taken synchronously, with nothing awaited between begin() and here that could interleave.
    if !deps.escrow.try_lock(&ctx.app, &guild_id, &user_id) {
        refuse_in_progress(&ctx).await;
        return Ok(());
    }
    // `finally { if (!handedOff) unlockCasino() }`, a panic included.
    let mut guard = LockGuard::new(&deps, &ctx.app, &guild_id, &user_id);

    // From here the lock is released at the end unless the collector owns it (its settlement / forfeit unlocks).
    let mut handed_off = false;
    let mut reserved = false;
    let mut settled = false;
    // A panic is a thrown error here, as in the JS `catch`: after the reserve it refunds the stake once.
    let result: anyhow::Result<()> = bot_core::error::guard(async {
        ctx.defer_reply(false).await?;

        let res = match deps.escrow.reserve(&ctx.app, &guild_id, &user_id, bet, Some(&settings)).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("Blackjack: reserving the bet failed", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e));
                reply_fail(&ctx.responder, "Economy Unavailable", "Could not place your bet. Nothing was taken — please try again in a moment.").await;
                return Ok(());
            }
        };
        let before = match res {
            Reserved::Ok { before, .. } => before,
            other => {
                let (title, text) = refusal(&other, &settings, &cfg);
                reply_fail(&ctx.responder, title, &text).await;
                return Ok(());
            }
        };
        reserved = true;

        let mut hand = Hand { deck: (deps.new_deck)(), player: Vec::new(), dealer: Vec::new(), stake: bet, settled: false, busy: false };
        let p1 = hand.draw();
        let p2 = hand.draw();
        let d1 = hand.draw();
        let d2 = hand.draw();
        hand.player = vec![p1, p2];
        hand.dealer = vec![d1, d2];

        let table = Arc::new(Table {
            app: ctx.app.clone(),
            deps: deps.clone(),
            responder: ctx.responder.clone(),
            interaction_id: ctx.interaction.id.to_string(),
            guild_id: guild_id.clone(),
            user_id: user_id.clone(),
            settings: settings.clone(),
            cfg: cfg.clone(),
            bet,
            before,
            stopper: Stopper::new(),
            hand: Mutex::new(hand),
        });

        // Peek (R078): a natural settles the hand before the player acts.
        let natural = {
            let h = table.lock();
            natural_outcome(&h.player, &h.dealer, bet).map(|n| (n, h.player.clone(), h.dealer.clone()))
        };
        if let Some((natural, player, dealer)) = natural {
            settled = true;
            table.lock().settled = true;
            let settlement = table.settle(natural.payout, bet).await;
            let embed = final_embed(
                Some(&settings),
                &cfg,
                &Final { player: &player, dealer: &dealer, result: natural.result, payout: natural.payout, stake: bet, doubled: false, before, settlement },
            );
            table.show(EditInteractionResponse::new().embed(embed).components(Vec::new()), None).await;
            return Ok(());
        }

        let (embed, can) = {
            let h = table.lock();
            let can = table.can_double(&h);
            (progress_embed(Some(&settings), &h.player, &h.dealer, h.stake, can), can)
        };
        let edit = EditInteractionResponse::new().embed(embed).components(vec![build_buttons(&table.interaction_id, can, false)]);
        if let Err(e) = ctx.edit_reply(edit).await {
            // The hand never reached the player: the stake goes back, once.
            bot_util::log_error!(
                "Blackjack: could not start the hand after the bet was reserved — refunding",
                ctx_json(&guild_id, &user_id, &[("bet", bet)]),
                bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
            );
            settled = true;
            table.lock().settled = true;
            let settlement = table.settle(bet, bet).await;
            if settlement.failed {
                bot_util::log_error!("Blackjack: refund failed after a start-up error", ctx_json(&guild_id, &user_id, &[("bet", bet)]));
            }
            reply_fail(
                &ctx.responder,
                "Blackjack Unavailable",
                if settlement.failed {
                    "The hand could not be started and your bet could not be refunded automatically. An admin has been notified in the bot logs."
                } else {
                    "The hand could not be started. Your bet has been refunded."
                },
            )
            .await;
            return Ok(());
        }

        let t1 = table.clone();
        let t2 = table.clone();
        drive(
            &ctx.app,
            id_filter("bj", table.interaction_id.clone()),
            Timers { idle: Some(Duration::from_millis(IDLE_MS)), time: Duration::from_millis(MAX_MS) },
            table.stopper.clone(),
            move |click| t1.clone().on_click(click),
            move |reason, _| t2.on_end(reason),
        );
        handed_off = true;
        Ok(())
    })
    .await;

    let outcome = match result {
        Err(e) if reserved && !settled && !handed_off => {
            // Unexpected failure after the bet was escrowed and before the hand could be played.
            bot_util::log_error!("Blackjack failed after the bet was reserved", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e));
            if let Err(e2) = deps.escrow.settle(&ctx.app, &guild_id, &user_id, bet, Some(&settings), bet).await {
                bot_util::log_error!("Blackjack: refund failed", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e2));
            }
            reply_fail(&ctx.responder, "Blackjack Unavailable", "Something went wrong; your bet has been refunded.").await;
            Ok(())
        }
        other => other,
    };
    if handed_off {
        guard.hand_off();
    }
    drop(guard);
    outcome
}
