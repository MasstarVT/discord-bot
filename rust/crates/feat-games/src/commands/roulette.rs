//! Port of `src/commands/games/roulette.js`: /roulette, European roulette on the per-server economy (pkg-economy.md
//! §5.11 and §6.2).
//!
//! Money: the bet is ESCROWED before the wheel spins and settled once with the gross return (R007, R136); the casino
//! lock is released at the end (R077). Spaces are parsed strictly (R082) and "green" is the 0 pocket paying 36x like
//! the number 0 (R081). Everything is shown as gross payout, signed net and "balance before -> after" taken from the
//! reserve (R240).

use bot_config::colors;
use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand};
use bot_core::embed_builder::neutral_embed;
use bot_core::interaction::{AutocompleteCtx, CommandCtx};
use bot_core::interaction_helpers::{Choice, respond_autocomplete};
use bot_core::markdown::escape_markdown;
use bot_core::prelude::{ModuleKey, PermissionLevel};
use bot_util::text::clamp_text;
use feat_economy::economy_service::{Reserved, clamp_payout, format_coins, format_net, reply_fail};
use serenity::all::EditInteractionResponse;

use crate::casino::{CasinoDeps, LockGuard, Settlement, balance_text, begin, ctx_json, err_arg, gone_or_warn, refusal, refuse_in_progress};

/// Standard European roulette red numbers (0 is green, the rest split red/black).
pub const RED_NUMBERS: [u32; 18] = [1, 3, 5, 7, 9, 12, 14, 16, 18, 19, 21, 23, 25, 27, 30, 32, 34, 36];

/// What a bet covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpaceKind {
    Colour,
    Number,
    Parity,
    Range,
    Dozen,
}

/// A parsed space: `{ token, kind, label, multiplier }`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Space {
    pub token: String,
    pub kind: SpaceKind,
    pub label: String,
    pub multiplier: i64,
}

/// `ROULETTE_SPACES`: the named spaces by token. `0` is the green pocket: "green" and "0" are the same bet (R081).
pub fn named_space(token: &str) -> Option<(SpaceKind, &'static str, i64)> {
    Some(match token {
        "red" => (SpaceKind::Colour, "Red", 2),
        "black" => (SpaceKind::Colour, "Black", 2),
        "0" => (SpaceKind::Number, "Green (0)", 36),
        "even" => (SpaceKind::Parity, "Even", 2),
        "odd" => (SpaceKind::Parity, "Odd", 2),
        "low" => (SpaceKind::Range, "Low (1–18)", 2),
        "high" => (SpaceKind::Range, "High (19–36)", 2),
        "1-12" => (SpaceKind::Dozen, "1st 12", 3),
        "13-24" => (SpaceKind::Dozen, "2nd 12", 3),
        "25-36" => (SpaceKind::Dozen, "3rd 12", 3),
        _ => return None,
    })
}

const STRAIGHT_UP: i64 = 36;

/// `wheelColor(n)`: `{ label, emoji }`.
pub fn wheel_color(n: u32) -> (&'static str, &'static str) {
    if n == 0 {
        ("Green", "🟢")
    } else if RED_NUMBERS.contains(&n) {
        ("Red", "🔴")
    } else {
        ("Black", "⚫")
    }
}

/// `/^(?:[1-9]|[12]\d|3[0-6])$/`: 1..36, never "00" or "07".
fn is_number_space(s: &str) -> bool {
    let b = s.as_bytes();
    match b.len() {
        1 => (b'1'..=b'9').contains(&b[0]),
        2 => (b[0] == b'1' || b[0] == b'2') && b[1].is_ascii_digit() || b[0] == b'3' && (b'0'..=b'6').contains(&b[1]),
        _ => false,
    }
}

/// JS `/\s/` (ECMAScript WhiteSpace and LineTerminator).
fn is_js_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `String(raw).toLowerCase().replace(/[–—]/g, '-').replace(/\s+/g, '')`.
fn normalise(raw: &str) -> String {
    raw.to_lowercase().chars().map(|c| if c == '–' || c == '—' { '-' } else { c }).filter(|c| !is_js_space(*c)).collect()
}

/// `parseSpace(raw)`: the whole input, lower-cased with all whitespace removed (and en/em dashes read as "-"), must
/// be one of: red, black, green, 0, even, odd, low, 1-18, high, 19-36, 1-12, 13-24, 25-36 or a number 1-36.
pub fn parse_space(raw: &str) -> Option<Space> {
    let s = normalise(raw);
    let token = match s.as_str() {
        "green" | "0" => Some("0".to_string()),
        "1-18" => Some("low".to_string()),
        "19-36" => Some("high".to_string()),
        other if named_space(other).is_some() => Some(other.to_string()),
        _ => None,
    };
    if let Some(token) = token {
        let (kind, label, multiplier) = named_space(&token)?;
        return Some(Space { token, kind, label: label.to_string(), multiplier });
    }
    if is_number_space(&s) {
        return Some(Space { label: format!("Number {s}"), token: s, kind: SpaceKind::Number, multiplier: STRAIGHT_UP });
    }
    None
}

fn covers(space: &Space, n: u32) -> bool {
    match space.kind {
        SpaceKind::Number => space.token.parse::<u32>().ok() == Some(n),
        SpaceKind::Colour => n != 0 && wheel_color(n).0.to_lowercase() == space.token,
        SpaceKind::Parity => n != 0 && n.is_multiple_of(2) == (space.token == "even"),
        SpaceKind::Range => {
            if space.token == "low" {
                (1..=18).contains(&n)
            } else {
                (19..=36).contains(&n)
            }
        }
        SpaceKind::Dozen => {
            let mut parts = space.token.split('-').map(|p| p.parse::<u32>().unwrap_or(0));
            let (from, to) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
            n >= from && n <= to
        }
    }
}

/// `spaceReturn(space, winNumber, bet)`: gross payout (stake included) for a bet on `space` when the ball lands on
/// `winNumber`; 0 for a loss.
pub fn space_return(space: Option<&Space>, win_number: u32, bet: i64) -> i64 {
    match space {
        Some(s) if covers(s, win_number) => bet * s.multiplier,
        _ => 0,
    }
}

/// `spinNumber(rng)`: a uniformly random pocket, 0..36.
pub fn spin_number(rng: &mut dyn FnMut(u32) -> u32) -> u32 {
    rng(37)
}

pub const SPIN_FRAMES: [&str; 3] = [
    "🎰  Spinning...       🔴 ⚫ 🟢 🔴 ⚫ 🔴 ⚫ 🟢 ⚫ 🔴",
    "🎰  Slowing down...   ⚫ 🟢 🔴 ⚫ 🔴 🟢 ⚫ 🔴 🟢",
    "🎰  Almost there...   🔴 🟢 ⚫ 🔴 🟢 ⚫ 🔴",
];

const INVALID_SPACE_HELP: &str = "Use red, black, green, even, odd, low, high, 1-12, 13-24, 25-36, or a number from 0 to 36.";

/// The data: `/roulette bet space`.
pub fn data() -> SlashCommand {
    SlashCommand::new("roulette", "Bet coins on the European roulette wheel (MEE6 /roulette).")
        .option(Opt::integer("bet", "Coins to bet").required(true).min_value(1).max_value(bot_config::COIN_MAX as i64))
        .option(
            Opt::string("space", "red, black, green, even, odd, low, high, 1-12, 13-24, 25-36, or 0-36")
                .required(true)
                .autocomplete(true)
                .max_length(10),
        )
}

/// The autocomplete candidates, named spaces first.
pub fn candidates() -> Vec<String> {
    let mut c: Vec<String> = ["red", "black", "green", "even", "odd", "low", "high", "1-12", "13-24", "25-36"].iter().map(|s| s.to_string()).collect();
    c.extend((0..37).map(|n| n.to_string()));
    c
}

/// The `space` choices for what was typed: `(name, value)`.
pub fn autocomplete_choices(focused: &str) -> Vec<(String, String)> {
    let q: String = focused.to_lowercase().chars().filter(|c| !is_js_space(*c)).collect();
    candidates()
        .into_iter()
        .filter(|t| t.starts_with(&q))
        .take(25)
        .filter_map(|token| {
            let space = parse_space(&token)?;
            // "0" is also the green pocket; keep its two entries apart in the list.
            let label = if token == "0" { "Number 0".to_string() } else { space.label.clone() };
            Some((format!("{label} ({}× return)", space.multiplier), token))
        })
        .collect()
}

/// `autocomplete(interaction)`: no database access.
pub async fn autocomplete(ctx: AutocompleteCtx) -> anyhow::Result<()> {
    let focused = ctx.focused_value();
    let choices: Vec<Choice> = autocomplete_choices(&focused).into_iter().map(|(n, v)| Choice::new(n, v)).collect();
    respond_autocomplete(&ctx.responder, &choices).await;
    Ok(())
}

/// The command with the real economy and wheel.
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
    .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

/// `execute(interaction, client, deps)`.
pub async fn execute(ctx: CommandCtx, deps: CasinoDeps) -> anyhow::Result<()> {
    let bet = ctx.get_integer("bet").ok_or_else(|| anyhow::anyhow!("Required option \"bet\" not found."))?;
    let raw_space = ctx.get_string("space").ok_or_else(|| anyhow::anyhow!("Required option \"space\" not found."))?;
    let Some(space) = parse_space(&raw_space) else {
        reply_fail(
            &ctx.responder,
            "Invalid Space",
            &format!("**{}** is not a valid bet. {INVALID_SPACE_HELP}", clamp_text(&escape_markdown(&raw_space), 20)),
        )
        .await;
        return Ok(());
    };

    let Some((settings, cfg)) = begin(&ctx, bet, &deps).await else { return Ok(()) };
    let Some(guild_id) = ctx.guild_id().map(|g| g.to_string()) else { return Ok(()) };
    let user_id = ctx.user().id.to_string();

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
                bot_util::log_error!("Roulette: reserving the bet failed", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e));
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

        // The bet is escrowed: whatever happens to the animation, the spin is settled below.
        for frame in SPIN_FRAMES {
            if let Err(e) = ctx.edit_reply(EditInteractionResponse::new().embed(neutral_embed(Some(frame), None, None))).await {
                gone_or_warn("Roulette", "animation frame", &e);
            }
            (deps.sleep)(1000).await;
        }

        let win_number = (deps.spin_number)()?;
        let (label, emoji) = wheel_color(win_number);
        let payout = space_return(Some(&space), win_number, bet);

        settled = true;
        let settlement = match Settlement::from(deps.escrow.settle(&ctx.app, &guild_id, &user_id, payout, Some(&settings), bet).await) {
            Ok(s) => s,
            Err(e) => {
                bot_util::log_error!("Roulette: settlement failed", ctx_json(&guild_id, &user_id, &[("payout", payout), ("stake", bet)]), err_arg(&e));
                Settlement::failed()
            }
        };

        let clamp = clamp_payout(payout, bet, cfg.max_payout);
        let shown = clamp.payout;
        let mut description =
            if shown > 0 { format!("🎉 You win {}!", format_net(shown - bet, Some(&settings))) } else { "😔 You lose. Better luck next time.".to_string() };
        if settlement.payout_capped || clamp.capped {
            description.push_str(&format!("\nWinnings capped at {} by the server's max payout.", format_coins(cfg.max_payout, Some(&settings), true)));
        }
        let balance = balance_text(&settlement, before, &cfg);

        let embed = neutral_embed(Some(&format!("🎰 The ball lands on {win_number} {emoji} {label}")), Some(&description), None)
            .color(if shown > 0 { colors::SUCCESS } else { colors::ERROR })
            .field("Your bet", format!("{} — {}", space.label, format_coins(bet, Some(&settings), true)), true)
            .field("Bet", format_coins(bet, Some(&settings), true), true)
            .field("Payout", format_coins(shown, Some(&settings), true), true)
            .field("Net", format_net(shown - bet, Some(&settings)), true)
            .field("Balance", balance, true);
        if let Err(e) = ctx.edit_reply(EditInteractionResponse::new().embed(embed)).await {
            gone_or_warn("Roulette", "result", &e);
        }
        Ok(())
    })
    .await;

    let outcome = match result {
        Err(e) if reserved && !settled => {
            bot_util::log_error!("Roulette failed after the bet was reserved", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e));
            if let Err(e2) = deps.escrow.settle(&ctx.app, &guild_id, &user_id, bet, Some(&settings), bet).await {
                bot_util::log_error!("Roulette: refund failed", ctx_json(&guild_id, &user_id, &[("bet", bet)]), err_arg(&e2));
            }
            reply_fail(&ctx.responder, "Roulette Unavailable", "Something went wrong; your bet has been refunded.").await;
            Ok(())
        }
        other => other,
    };
    drop(guard);
    outcome
}
