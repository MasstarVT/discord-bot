//! Port of `src/commands/games/slots.js`: /slots, a 3x3 slot machine on the per-server economy (pkg-economy.md §5.12
//! and §6.3).
//!
//! Money: the bet is ESCROWED before the reels spin and settled once with the gross return (R007, R136); the casino
//! lock is released at the end whatever happens (R077). The result shows the gross payout, the signed net and "balance
//! before -> after" from the reserve (R240); every winning spin returns at least one coin of profit.

use bot_config::colors;
use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand};
use bot_core::embed_builder::neutral_embed;
use bot_core::interaction::CommandCtx;
use bot_core::prelude::{ModuleKey, PermissionLevel};
use feat_economy::economy_service::{clamp_payout, format_coins, format_net, reply_fail};
use serenity::all::EditInteractionResponse;

use crate::casino::{CasinoDeps, LockGuard, Settlement, balance_text, begin, ctx_json, err_arg, gone_or_warn, refusal, refuse_in_progress};

/// One reel symbol. Rarer symbols have lower weight and higher payout multipliers (per winning line).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Symbol {
    pub emoji: &'static str,
    pub weight: u32,
    pub multiplier: f64,
}

/// The paytable (RTP 8800 / 9261 = 95.02 %; the derivation is in `slots.js`).
pub const SYMBOLS: [Symbol; 6] = [
    Symbol { emoji: "💎", weight: 1, multiplier: 15.0 },
    Symbol { emoji: "7️⃣", weight: 2, multiplier: 11.0 },
    Symbol { emoji: "🍇", weight: 3, multiplier: 7.0 },
    Symbol { emoji: "🍒", weight: 4, multiplier: 5.0 },
    Symbol { emoji: "🍋", weight: 5, multiplier: 4.0 },
    Symbol { emoji: "🍎", weight: 6, multiplier: 3.0 },
];

/// `TOTAL_WEIGHT` (21).
pub const TOTAL_WEIGHT: u32 = 21;

/// Winning lines: 3 rows + 2 diagonals (indices into the flat 9-cell grid).
pub const LINES: [[usize; 3]; 5] = [[0, 1, 2], [3, 4, 5], [6, 7, 8], [0, 4, 8], [2, 4, 6]];

/// `spinSymbol(rng)`: one weighted symbol; `r = rng(TOTAL_WEIGHT)` is walked through the weights.
pub fn spin_symbol(rng: &mut dyn FnMut(u32) -> u32) -> Symbol {
    let mut r = rng(TOTAL_WEIGHT);
    for sym in SYMBOLS {
        if r < sym.weight {
            return sym;
        }
        r -= sym.weight;
    }
    SYMBOLS[SYMBOLS.len() - 1]
}

/// `spinGrid(rng)`: the nine cells, row by row.
pub fn spin_grid(rng: &mut dyn FnMut(u32) -> u32) -> Vec<Symbol> {
    (0..9).map(|_| spin_symbol(rng)).collect()
}

/// A line of three equal symbols: `{ a, b, c, symbol }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Win {
    pub a: usize,
    pub b: usize,
    pub c: usize,
    pub symbol: Symbol,
}

/// `checkWins(grid)`.
pub fn check_wins(grid: &[Symbol]) -> Vec<Win> {
    LINES
        .iter()
        .filter(|[a, b, c]| grid[*a].emoji == grid[*b].emoji && grid[*a].emoji == grid[*c].emoji)
        .map(|&[a, b, c]| Win { a, b, c, symbol: grid[a] })
        .collect()
}

/// `slotsPayout(grid, bet)`: `{ payout, multiplier, wins }`.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotsPayout {
    pub payout: i64,
    pub multiplier: f64,
    pub wins: Vec<Win>,
}

/// Gross payout (stake included): the multipliers of every winning line add up, the product is floored, and a winning
/// spin always returns at least `bet + 1` (R240). 0 when nothing lines up.
pub fn slots_payout(grid: &[Symbol], bet: i64) -> SlotsPayout {
    let wins = check_wins(grid);
    let multiplier: f64 = wins.iter().map(|w| w.symbol.multiplier).sum();
    let payout = if wins.is_empty() { 0 } else { ((bet as f64 * multiplier).floor() as i64).max(bet + 1) };
    SlotsPayout { payout, multiplier, wins }
}

/// `renderGrid(grid, wins)`.
pub fn render_grid(grid: &[Symbol], wins: &[Win]) -> String {
    let mut rows = Vec::new();
    for r in 0..3 {
        let line = [r * 3, r * 3 + 1, r * 3 + 2];
        let cells: Vec<&str> = line.iter().map(|i| grid[*i].emoji).collect();
        let is_win = wins.iter().any(|w| w.a == line[0] && w.b == line[1] && w.c == line[2]);
        rows.push(format!("{}{}", cells.join(" "), if is_win { "  ◀ WIN" } else { "" }));
    }
    let mut diagonals = Vec::new();
    if wins.iter().any(|w| w.a == 0 && w.b == 4 && w.c == 8) {
        diagonals.push("↘ Diagonal: WIN");
    }
    if wins.iter().any(|w| w.a == 2 && w.b == 4 && w.c == 6) {
        diagonals.push("↙ Diagonal: WIN");
    }
    let mut out = rows.join("\n");
    if !diagonals.is_empty() {
        out.push('\n');
        out.push_str(&diagonals.join("\n"));
    }
    out
}

/// A multiplier as JS prints it (`3`, `1.5`).
fn js_num(n: f64) -> String {
    bot_util::js::number_to_string(n)
}

/// The data: `/slots bet`.
pub fn data() -> SlashCommand {
    SlashCommand::new("slots", "Spin the 3×3 slot machine for coins.").option(
        Opt::integer("bet", "Coins to bet").required(true).min_value(1).max_value(bot_config::COIN_MAX as i64),
    )
}

/// The command with the real economy and reels.
pub fn command() -> CommandDef {
    command_with(CasinoDeps::default())
}

/// The command with injected dependencies (the JS `execute(interaction, client, deps)` test seam).
pub fn command_with(deps: CasinoDeps) -> CommandDef {
    CommandDef::new("games", data(), Some(ModuleKey::Economy), move |ctx| {
        let deps = deps.clone();
        async move { execute(ctx, deps).await }
    })
    .permission_level(PermissionLevel::Member)
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
    // `finally { unlockCasino() }`, a panic included.
    let guard = LockGuard::new(&deps, &ctx.app, &guild_id, &user_id);

    let mut reserved = false;
    let mut settled = false;
    // A panic is a thrown error here, as in the JS `catch`: after the reserve it refunds the stake once.
    let result: anyhow::Result<()> = bot_core::error::guard(async {
        ctx.defer_reply(false).await?;

        let res = match deps.escrow.reserve(&ctx.app, &guild_id, &user_id, bet, Some(&settings)).await {
            Ok(r) => r,
            Err(e) => {
                bot_util::log_error!("Slots: reserving the bet failed", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e));
                reply_fail(&ctx.responder, "Economy Unavailable", "Could not place your bet. Nothing was taken — please try again in a moment.").await;
                return Ok(());
            }
        };
        let before = match res {
            feat_economy::economy_service::Reserved::Ok { before, .. } => before,
            other => {
                let (title, text) = refusal(&other, &settings, &cfg);
                reply_fail(&ctx.responder, title, &text).await;
                return Ok(());
            }
        };
        reserved = true;

        let grid = (deps.spin_grid)()?;
        let SlotsPayout { payout, multiplier, wins } = slots_payout(&grid, bet);

        settled = true;
        let settlement = match Settlement::from(deps.escrow.settle(&ctx.app, &guild_id, &user_id, payout, Some(&settings), bet).await) {
            Ok(s) => s,
            Err(e) => {
                bot_util::log_error!("Slots: settlement failed", ctx_json(&guild_id, &user_id, &[("payout", payout), ("stake", bet)]), err_arg(&e));
                Settlement::failed()
            }
        };

        let clamp = clamp_payout(payout, bet, cfg.max_payout);
        let shown = clamp.payout;
        let mut result_text = if shown > 0 {
            format!(
                "🎉 {} winning line{} ({}× payout) — {}!",
                wins.len(),
                if wins.len() == 1 { "" } else { "s" },
                js_num(multiplier),
                format_net(shown - bet, Some(&settings))
            )
        } else {
            "❌ No match — better luck next time!".to_string()
        };
        if settlement.payout_capped || clamp.capped {
            result_text.push_str(&format!("\nWinnings capped at {} by the server's max payout.", format_coins(cfg.max_payout, Some(&settings), true)));
        }
        let balance = balance_text(&settlement, before, &cfg);

        let color = if shown > bet {
            colors::SUCCESS
        } else if shown > 0 {
            colors::NEUTRAL
        } else {
            colors::ERROR
        };
        let embed = neutral_embed(Some("🎰 Slot Machine"), Some(&format!("```\n{}\n```\n{result_text}", render_grid(&grid, &wins))), None)
            .color(color)
            .field("Bet", format_coins(bet, Some(&settings), true), true)
            .field("Payout", format_coins(shown, Some(&settings), true), true)
            .field("Net", format_net(shown - bet, Some(&settings)), true)
            .field("Balance", balance, true);
        if let Err(e) = ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await {
            gone_or_warn("Slots", "result", &e);
        }
        Ok(())
    })
    .await;

    let outcome = match result {
        Err(e) if reserved && !settled => {
            // Unexpected failure after the bet was escrowed and before the spin was settled: the stake goes back, once.
            bot_util::log_error!("Slots failed after the bet was reserved", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e));
            if let Err(e2) = deps.escrow.settle(&ctx.app, &guild_id, &user_id, bet, Some(&settings), bet).await {
                bot_util::log_error!("Slots: refund failed", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e2));
            }
            reply_fail(&ctx.responder, "Slots Unavailable", "Something went wrong; your bet has been refunded.").await;
            Ok(())
        }
        other => other,
    };
    drop(guard);
    outcome
}
