//! Port of `src/commands/games/games.js`: /games (pkg-games). `list` browses the games; `rps`, `numguess` and
//! `tictactoe` are the games that need no setup. Scrabble has its own /scrabble; the casino games (/blackjack,
//! /roulette, /slots) are separate commands and show up in `list`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand, command_name_compare, option_type};
use bot_core::embed_builder::{apply_guild_color, error_embed, guild_embed, info_embed, success_embed, warn_embed};
use bot_core::hooks::Hook;
use bot_core::hooks::events::MessageCreate;
use bot_core::interaction::{CommandCtx, ComponentCtx, Responder};
use bot_core::interaction_helpers::{PermTarget, cached_channel, is_interaction_gone, missing_bot_permissions, reply_error};
use bot_core::markdown::escape_markdown;
use bot_core::prelude::{ModuleKey, PermissionLevel};
use bot_core::settings::{Settings, is_module_enabled_by_name};
use bot_core::App;
use bot_db::GuildSettings;
use bot_util::js::utf16_len;
use bot_util::text::{chunk_lines, clamp_text};
use serenity::all::{
    ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    EditInteractionResponse, Member, User,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::casino::secure_int;
use crate::collector_driver::{Stopper, Timers, drive, id_filter};

/// `GAME_EMOJI`: keys are the command (or `command subcommand`) paths the list shows.
pub fn game_emoji(name: &str) -> Option<&'static str> {
    Some(match name {
        "blackjack" => "🃏",
        "games numguess" => "🔢",
        "roulette" => "🎡",
        "games rps" => "🪨",
        "slots" => "🎰",
        "games tictactoe" => "⭕",
        "scrabble" => "🔤",
        "scrabble start" => "🔤",
        "scrabble word" => "📝",
        "scrabble pass" => "⏭️",
        "scrabble exchange" => "🔄",
        "scrabble resign" => "🏳️",
        "scrabble help" => "❓",
        _ => return None,
    })
}

fn is_scrabble(name: &str) -> bool {
    name == "scrabble" || name.starts_with("scrabble ")
}

pub const MAX_GUESSES: u32 = 7;

/// The data: `/games list|rps|numguess|tictactoe`.
pub fn data() -> SlashCommand {
    SlashCommand::new("games", "Browse the games, or play Rock Paper Scissors, a number guess or Tic-Tac-Toe.")
        .option(Opt::sub_command("list", "Browse all games available to play."))
        .option(Opt::sub_command("rps", "Play Rock, Paper, Scissors against the bot!"))
        .option(Opt::sub_command("numguess", format!("Guess the bot's number (1–100) in {MAX_GUESSES} tries!")))
        .option(
            Opt::sub_command("tictactoe", "Challenge another player to Tic-Tac-Toe!")
                .option(Opt::user("target", "The player you want to challenge").required(true)),
        )
}

/// The command.
pub fn command() -> CommandDef {
    CommandDef::new("games", data(), Some(ModuleKey::Games), |ctx| async move { execute(ctx).await }).permission_level(PermissionLevel::Member)
}

/// `/games <subcommand>`.
pub async fn execute(ctx: CommandCtx) -> anyhow::Result<()> {
    match ctx.subcommand().as_deref() {
        Some("rps") => execute_rps(ctx).await,
        Some("numguess") => execute_numguess(ctx).await,
        Some("tictactoe") => execute_tictactoe(ctx).await,
        _ => execute_list(ctx).await,
    }
}

/// Memo first, then a lookup bounded by the time left to acknowledge (00-overview rule 21).
async fn load_settings(ctx: &CommandCtx) -> Option<Settings> {
    if let Some(s) = &ctx.settings {
        return Some(s.clone());
    }
    let g = ctx.guild_id()?.to_string();
    ctx.app.settings().peek_or_load(&g, ctx.ack_budget_ms()).await
}

fn err_arg(e: &impl std::fmt::Display) -> bot_util::logger::LogArg {
    bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
}

/// The per-app sets of players with a game running (`rpsActive`, `numguessActive`, `tttActive`) and the numguess
/// message listeners.
#[derive(Default)]
pub struct GamesState {
    rps: Mutex<HashSet<String>>,
    numguess: Mutex<HashSet<String>>,
    ttt: Mutex<HashSet<String>>,
    /// (channelId, userId) → the running guessing game's inbox.
    guess_inboxes: Mutex<HashMap<(String, String), mpsc::UnboundedSender<String>>>,
}

fn guard<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Adds `id` to a lock set; `false` when it was already there.
fn take_lock(set: &Mutex<HashSet<String>>, id: &str) -> bool {
    guard(set).insert(id.to_string())
}

fn games_state(app: &App) -> Arc<GamesState> {
    app.ext::<GamesState>()
}

// ── /games list ──────────────────────────────────────────────────────────────

const MAX_FIELDS: usize = 25;
/// Stay clear of Discord's 6,000-character embed total.
const EMBED_BUDGET: usize = 5500;

/// One listed game: `{ name, description, module }`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GameEntry {
    pub name: String,
    pub description: String,
    pub module: String,
}

/// `collectGameCommands(client)`: every command in the `games` category (or declaring the `games` module). A command
/// with subcommands is listed once per subcommand (`games rps`, `scrabble word`), except /games list itself.
pub fn collect_game_commands<'a>(commands: impl IntoIterator<Item = &'a CommandDef>) -> Vec<GameEntry> {
    let mut entries = Vec::new();
    for def in commands {
        let name = def.name();
        let module = def.module.map(|m| m.as_str());
        if def.category != "games" && module != Some("games") {
            continue;
        }
        let module = module.unwrap_or("games").to_string();
        let subs: Vec<_> =
            def.data.options.iter().filter(|o| o.kind == option_type::SUB_COMMAND && !(name == "games" && o.name == "list")).collect();
        if !subs.is_empty() {
            for s in subs {
                entries.push(GameEntry { name: format!("{name} {}", s.name), description: s.description.clone(), module: module.clone() });
            }
        } else if name != "games" {
            entries.push(GameEntry { name: name.to_string(), description: def.data.description.clone(), module });
        }
    }
    entries
}

const SCRABBLE_ORDER: [&str; 6] = ["scrabble start", "scrabble word", "scrabble pass", "scrabble exchange", "scrabble resign", "scrabble help"];

fn scrabble_rank(name: &str) -> i64 {
    SCRABBLE_ORDER.iter().position(|n| *n == name).map(|p| p as i64).unwrap_or(-1)
}

fn by_name(a: &GameEntry, b: &GameEntry) -> std::cmp::Ordering {
    command_name_compare(&a.name, &b.name)
}

/// `buildGamesEmbed(entries, settings)`.
pub fn build_games_embed(entries: &[GameEntry], settings: Option<&GuildSettings>) -> CreateEmbed {
    if entries.is_empty() {
        return guild_embed(settings, Some("🎮 Games"), Some("No games are available on this server right now."), None);
    }
    let embed = guild_embed(settings, Some("🎮 Games"), Some("Here are all the games you can play!"), None);

    // Groups, in display order; empty groups are omitted.
    let mut groups: Vec<(String, Vec<GameEntry>)> = Vec::new();
    let mut games: Vec<GameEntry> = entries.iter().filter(|e| e.module == "games" && !is_scrabble(&e.name)).cloned().collect();
    games.sort_by(by_name);
    groups.push(("🕹️ Games".into(), games));
    let mut scrabble: Vec<GameEntry> = entries.iter().filter(|e| is_scrabble(&e.name)).cloned().collect();
    scrabble.sort_by_key(|e| scrabble_rank(&e.name));
    groups.push(("🔤 Scrabble".into(), scrabble));
    let mut casino: Vec<GameEntry> = entries.iter().filter(|e| e.module == "economy").cloned().collect();
    casino.sort_by(by_name);
    let has_casino = !casino.is_empty();
    groups.push(("🎰 Casino".into(), casino));
    let mut others: Vec<String> = Vec::new();
    for e in entries {
        if e.module != "games" && e.module != "economy" && !others.contains(&e.module) {
            others.push(e.module.clone());
        }
    }
    for m in others {
        let label = match bot_config::get_module(&m) {
            Some(info) => format!("{} {}", info.emoji, info.label),
            None => format!("🎮 {m}"),
        };
        let mut items: Vec<GameEntry> = entries.iter().filter(|e| e.module == m).cloned().collect();
        items.sort_by(by_name);
        groups.push((label, items));
    }

    // Lay the groups out as fields. A very long listing shortens the descriptions (100, then 50, then 20 characters)
    // so every group still fits; whatever still does not fit is dropped.
    let mut layout: Vec<(String, String)> = Vec::new();
    for desc_limit in [100usize, 50, 20] {
        layout.clear();
        let mut size = 0usize;
        let mut truncated = false;
        'outer: for (label, items) in &groups {
            if items.is_empty() {
                continue;
            }
            let lines: Vec<String> = items
                .iter()
                .map(|e| format!("{} `/{}` — {}", game_emoji(&e.name).unwrap_or("🎮"), e.name, clamp_text(&e.description, desc_limit)))
                .collect();
            let chunks = chunk_lines(&lines, 1024);
            for (i, chunk) in chunks.iter().enumerate() {
                let name = if i == 0 { label.clone() } else { format!("{label} (cont.)") };
                if layout.len() >= MAX_FIELDS || size + utf16_len(&name) + utf16_len(chunk) > EMBED_BUDGET {
                    truncated = true;
                    break 'outer;
                }
                size += utf16_len(&name) + utf16_len(chunk);
                layout.push((name, chunk.clone()));
            }
        }
        if !truncated {
            break;
        }
    }
    let mut embed = embed;
    for (name, value) in layout {
        embed = embed.field(name, value, false);
    }
    if has_casino {
        embed = embed.footer(CreateEmbedFooter::new("Casino games use this server's coins — see /balance."));
    }
    embed
}

async fn execute_list(ctx: CommandCtx) -> anyhow::Result<()> {
    let settings = load_settings(&ctx).await;
    // Hides the casino games when Economy is off (and any other module that is off).
    let all = ctx.app.commands().all();
    let entries: Vec<GameEntry> = collect_game_commands(all.iter().map(|c| c.as_ref()))
        .into_iter()
        .filter(|e| settings.is_none() || is_module_enabled_by_name(settings.as_deref(), Some(&e.module)))
        .collect();
    ctx.reply(CreateInteractionResponseMessage::new().embed(build_games_embed(&entries, settings.as_deref()))).await?;
    Ok(())
}

// ── /games rps ───────────────────────────────────────────────────────────────

pub const RPS_CHOICES: [&str; 3] = ["rock", "paper", "scissors"];

/// `RPS_EMOJI[choice]`.
pub fn rps_emoji(choice: &str) -> &'static str {
    match choice {
        "rock" => "🪨",
        "paper" => "📄",
        _ => "✂️",
    }
}

/// `RPS_BEATS[x]`: what x defeats.
pub fn rps_beats(choice: &str) -> &'static str {
    match choice {
        "rock" => "scissors",
        "paper" => "rock",
        _ => "paper",
    }
}

/// `decideRps(player, bot)`: the outcome for the player (`win`, `lose`, `draw`).
pub fn decide_rps(player: &str, bot: &str) -> &'static str {
    if player == bot {
        return "draw";
    }
    if rps_beats(player) == bot { "win" } else { "lose" }
}

/// `buildRpsRow(id, disabled)`.
pub fn build_rps_row(id: &str, disabled: bool) -> CreateActionRow {
    let b = |choice: &str, label: &str| {
        CreateButton::new(bot_config::custom_id!("rps", id, choice).unwrap_or_default()).label(label).style(ButtonStyle::Primary).disabled(disabled)
    };
    CreateActionRow::Buttons(vec![b("rock", "🪨 Rock"), b("paper", "📄 Paper"), b("scissors", "✂️ Scissors")])
}

const RPS_TITLE: &str = "✂️ Rock, Paper, Scissors";

/// How long the RPS buttons stay live.
pub const RPS_TIME_MS: u64 = 30_000;

async fn execute_rps(ctx: CommandCtx) -> anyhow::Result<()> {
    let user_id = ctx.user().id.to_string();
    let st = games_state(&ctx.app);
    let busy = guard(&st.rps).contains(&user_id);
    if busy {
        ctx.reply_text("⚠️ You already have an active RPS game!", true).await?;
        return Ok(());
    }
    let settings = load_settings(&ctx).await;
    if !take_lock(&st.rps, &user_id) {
        ctx.reply_text("⚠️ You already have an active RPS game!", true).await?;
        return Ok(());
    }

    // R089: the collector's end releases the lock; release it here if anything fails before it exists.
    let id = ctx.interaction.id.to_string();
    let reply = CreateInteractionResponseMessage::new()
        .embed(guild_embed(settings.as_deref(), Some(RPS_TITLE), Some("Choose your move! You have **30 seconds**."), None))
        .components(vec![build_rps_row(&id, false)]);
    if let Err(e) = ctx.reply(reply).await {
        guard(&st.rps).remove(&user_id);
        return Err(e.into());
    }

    // The filter checks only this game's prefix; the user is checked inside the handler.
    let played = Arc::new(AtomicBool::new(false));
    let stopper = Stopper::new();
    let (st2, user2, id2, settings2, responder) = (st.clone(), user_id.clone(), id.clone(), settings.clone(), ctx.responder.clone());
    let (user1, id1, settings1, stop1) = (user_id.clone(), id.clone(), settings.clone(), stopper.clone());
    drive(
        &ctx.app,
        id_filter("rps", id.clone()),
        Timers { idle: None, time: Duration::from_millis(RPS_TIME_MS) },
        stopper,
        move |i| {
            let (played, user, id, settings, stopper) = (played.clone(), user1.clone(), id1.clone(), settings1.clone(), stop1.clone());
            async move {
                let r: Result<(), bot_core::interaction::InteractionError> = async {
                    if i.user().id.to_string() != user {
                        i.reply(CreateInteractionResponseMessage::new().content("❌ This isn't your game!").ephemeral(true)).await?;
                        return Ok(());
                    }
                    // R237: a second press before the first update lands is acknowledged and ignored.
                    // The handlers of two clicks run concurrently: `played` is tested and set in one step.
                    let choice = i.parsed().1.get(1).cloned().unwrap_or_default();
                    if !RPS_CHOICES.contains(&choice.as_str()) || played.swap(true, Ordering::SeqCst) {
                        let _ = i.defer_update().await;
                        return Ok(());
                    }
                    stopper.stop("done"); // synchronously, before any await

                    let bot = RPS_CHOICES[secure_int(RPS_CHOICES.len() as u32) as usize];
                    let description = format!("You: {} **{choice}**\nBot: {} **{bot}**", rps_emoji(&choice), rps_emoji(bot));
                    let embed = match decide_rps(&choice, bot) {
                        "win" => success_embed("You win!", Some(&description), None),
                        "lose" => error_embed("You lose!", Some(&description), None),
                        _ => guild_embed(settings.as_deref(), Some("🤝 It's a draw!"), Some(&description), None),
                    };
                    i.update(CreateInteractionResponseMessage::new().embed(embed).components(vec![build_rps_row(&id, true)])).await
                }
                .await;
                if let Err(e) = r {
                    if is_interaction_gone(&e) {
                        bot_util::log_debug!("rps: interaction gone", err_arg(&e));
                    } else {
                        bot_util::log_error!("RPS collect failed", err_arg(&e));
                    }
                    if !i.is_acknowledged() {
                        let _ = i.defer_update().await;
                    }
                }
            }
        },
        move |reason, _| async move {
            guard(&st2.rps).remove(&user2);
            if reason != "time" {
                return;
            }
            let edit = EditInteractionResponse::new()
                .embed(guild_embed(settings2.as_deref(), Some(RPS_TITLE), Some("⏰ Time's up! No move was made."), None))
                .components(vec![build_rps_row(&id2, true)]);
            let _ = responder.edit_reply(edit).await;
        },
    );
    Ok(())
}

// ── /games numguess ──────────────────────────────────────────────────────────

/// Disclosed in the intro; reset by every guess (R234).
pub const GUESS_IDLE_MS: u64 = 60_000;
/// Hard cap, below the 15-minute interaction token lifetime.
pub const NUMGUESS_MAX_MS: u64 = 10 * 60_000;
const NUMGUESS_WARN_EVERY_MS: i64 = 5_000;

/// `evaluateGuess(secret, guess)`: `out-of-range`, `correct`, `higher` (the secret is higher than the guess) or
/// `lower`.
pub fn evaluate_guess(secret: i64, guess: f64) -> &'static str {
    if !guess.is_finite() || guess.fract() != 0.0 || !(1.0..=100.0).contains(&guess) {
        return "out-of-range";
    }
    let g = guess as i64;
    if g == secret {
        "correct"
    } else if secret > g {
        "higher"
    } else {
        "lower"
    }
}

/// The collector filter: `/^\s*\d{1,6}\s*$/`.
pub fn is_guess_message(content: &str) -> bool {
    // JS `\s` (no U+0085, which `char::is_whitespace` would accept).
    let t = bot_util::js::trim(content);
    !t.is_empty() && t.len() <= 6 && t.bytes().all(|b| b.is_ascii_digit())
}

/// The state machine of one guessing game.
#[derive(Debug, Clone)]
pub struct Numguess {
    pub secret: i64,
    pub guess_count: u32,
    pub finished: bool,
    pub last_warn_at: i64,
}

/// What a guess does: the embed to send (if any) and whether the game is over (`won` / `exhausted`).
pub enum GuessStep {
    Silent,
    Say(CreateEmbed),
    End(&'static str, CreateEmbed),
}

impl Numguess {
    pub fn new(secret: i64) -> Self {
        Self { secret, guess_count: 0, finished: false, last_warn_at: 0 }
    }

    /// One collected message at `now_ms`.
    pub fn step(&mut self, content: &str, now_ms: i64, settings: Option<&GuildSettings>) -> GuessStep {
        if self.finished {
            return GuessStep::Silent; // R235
        }
        let trimmed = bot_util::js::trim(content);
        let guess = trimmed.parse::<i64>().map(|n| n as f64).unwrap_or(f64::NAN);
        let verdict = evaluate_guess(self.secret, guess);

        if verdict == "out-of-range" {
            // Doesn't count against the limit; answered at most once per 5 s so spam can't flood the webhook.
            if now_ms - self.last_warn_at < NUMGUESS_WARN_EVERY_MS {
                return GuessStep::Silent;
            }
            self.last_warn_at = now_ms;
            return GuessStep::Say(warn_embed("Out of Range", Some("Please guess a number between **1** and **100**."), None));
        }

        self.guess_count += 1;
        let remaining = MAX_GUESSES - self.guess_count.min(MAX_GUESSES);
        if verdict == "correct" {
            self.finished = true;
            return GuessStep::End(
                "won",
                success_embed(
                    "Correct!",
                    Some(&format!("The number was **{}**! You got it in **{}/{MAX_GUESSES}** guesses. 🎉", self.secret, self.guess_count)),
                    None,
                ),
            );
        }
        if self.guess_count >= MAX_GUESSES {
            self.finished = true;
            return GuessStep::End(
                "exhausted",
                error_embed("Out of Guesses!", Some(&format!("The number was **{}**. Better luck next time!", self.secret)), None),
            );
        }
        let title = if verdict == "higher" { "📈 Higher!" } else { "📉 Lower!" };
        GuessStep::Say(apply_guild_color(
            info_embed(
                title,
                Some(&format!(
                    "**{}** is wrong. {remaining} guess{} remaining · 60 s to guess.",
                    guess as i64,
                    if remaining != 1 { "es" } else { "" }
                )),
                None,
            ),
            settings,
        ))
    }

    /// The message for an end reason other than `won` / `exhausted` (`None`: no message).
    pub fn end_message(&self, reason: &str, collected: usize) -> Option<CreateEmbed> {
        match reason {
            "idle" => {
                // No collected message at all usually means the bot can't see messages here.
                let hint = if collected == 0 { "\nIf you did type numbers, I may not be able to read messages in this channel." } else { "" };
                Some(error_embed("Time's Up!", Some(&format!("You didn't guess for 60 seconds. The number was **{}**.{hint}", self.secret)), None))
            }
            "time" => Some(error_embed("Time's Up!", Some(&format!("The game reached its 10-minute limit. The number was **{}**.", self.secret)), None)),
            _ => None,
        }
    }
}

/// Feeds a user's numeric message to their running guessing game in that channel (the JS message collector). `true`
/// when a game took it.
pub fn offer_guess(app: &App, channel_id: &str, user_id: &str, content: &str) -> bool {
    if !is_guess_message(content) {
        return false;
    }
    let st = games_state(app);
    let inboxes = guard(&st.guess_inboxes);
    inboxes.get(&(channel_id.to_string(), user_id.to_string())).is_some_and(|tx| tx.send(content.to_string()).is_ok())
}

/// Whether the user has a guessing game in the channel (the listener's cheap filter).
pub fn has_guess_game(app: &App, channel_id: &str, user_id: &str) -> bool {
    guard(&games_state(app).guess_inboxes).contains_key(&(channel_id.to_string(), user_id.to_string()))
}

/// The id of the listener that feeds guesses to the games.
pub const NUMGUESS_LISTENER_ID: &str = "games-fixes:numguess";

/// Registers the `messageCreate` listener that stands in for the JS `channel.createMessageCollector` (bot-core has
/// no message collector; see the crate report). It is ungated, like the collector, and filters synchronously on a
/// running game of that user in that channel.
pub fn register_numguess_listener(app: &App) -> anyhow::Result<()> {
    let hook = Hook::<MessageCreate>::new(NUMGUESS_LISTENER_ID, |ctx| async move {
        offer_guess(&ctx.app, &ctx.data.channel_id.to_string(), &ctx.data.message.author.id.to_string(), &ctx.data.message.content);
        Ok(())
    })
    .ungated()
    .always()
    .filter(|ctx| {
        is_guess_message(&ctx.data.message.content)
            && has_guess_game(&ctx.app, &ctx.data.channel_id.to_string(), &ctx.data.message.author.id.to_string())
    });
    app.hooks().register_hook(hook, "games-fixes", Some(ModuleKey::Games)).map_err(anyhow::Error::msg)
}

async fn say(responder: &Responder, embed: CreateEmbed) {
    if let Err(e) = responder.follow_up(CreateInteractionResponseFollowup::new().embed(embed)).await {
        if is_interaction_gone(&e) {
            bot_util::log_debug!("numguess: interaction gone", err_arg(&e));
        } else {
            bot_util::log_warn!("numguess follow-up failed", err_arg(&e));
        }
    }
}

async fn execute_numguess(ctx: CommandCtx) -> anyhow::Result<()> {
    let user_id = ctx.user().id.to_string();
    let Some(guild_id) = ctx.guild_id() else { return Ok(()) };
    let channel_id = ctx.channel_id();

    let channel = cached_channel(&ctx.app, guild_id, channel_id);
    if !channel.as_ref().is_some_and(bot_core::permissions::is_text_based) {
        reply_error(&ctx.responder, "Unavailable Here", Some("I can't read messages in this channel, so I can't play here."), true).await;
        return Ok(());
    }
    // R080: the only channel permission the game needs. All bot output goes through the interaction webhook
    // (followUp), so Send Messages and Read Message History are not required.
    if !missing_bot_permissions(&ctx.app, PermTarget::Channel(guild_id, channel_id), &["ViewChannel"]).is_empty() {
        reply_error(
            &ctx.responder,
            "Missing Permissions",
            Some("I need **View Channel** here to read your guesses. Try another channel or ask an admin."),
            true,
        )
        .await;
        return Ok(());
    }
    let st = games_state(&ctx.app);
    let busy = guard(&st.numguess).contains(&user_id);
    if busy {
        ctx.reply_text("⚠️ You already have an active guessing game!", true).await?;
        return Ok(());
    }

    let secret = i64::from(secure_int(100)) + 1;
    let settings = load_settings(&ctx).await;
    if !take_lock(&st.numguess, &user_id) {
        ctx.reply_text("⚠️ You already have an active guessing game!", true).await?;
        return Ok(());
    }

    let intro = apply_guild_color(
        info_embed(
            "Number Guessing Game",
            Some(&format!(
                "I'm thinking of a number between **1** and **100**.\nYou have **{MAX_GUESSES}** guesses — type them in this channel.\nThe game ends if you don't guess for **60 seconds** (10 minutes max)."
            )),
            None,
        ),
        settings.as_deref(),
    );
    if let Err(e) = ctx.reply(CreateInteractionResponseMessage::new().embed(intro)).await {
        guard(&st.numguess).remove(&user_id);
        return Err(e.into());
    }

    // Only numeric messages of the player; typed messages are the only input.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let key = (channel_id.to_string(), user_id.clone());
    guard(&st.guess_inboxes).insert(key.clone(), tx);

    let app = ctx.app.clone();
    let responder = ctx.responder.clone();
    tokio::spawn(async move {
        let mut game = Numguess::new(secret);
        let start = Instant::now();
        let total = start + Duration::from_millis(NUMGUESS_MAX_MS);
        let mut idle = start + Duration::from_millis(GUESS_IDLE_MS);
        let mut collected = 0usize;
        let reason: &str = loop {
            let left = idle.min(total).saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, rx.recv()).await {
                Ok(Some(content)) => {
                    collected += 1;
                    idle = Instant::now() + Duration::from_millis(GUESS_IDLE_MS);
                    match game.step(&content, app.clock().now_ms(), settings.as_deref()) {
                        GuessStep::Silent => {}
                        GuessStep::Say(embed) => say(&responder, embed).await,
                        GuessStep::End(why, embed) => {
                            // Stopped before the reply (R080, R235).
                            guard(&games_state(&app).guess_inboxes).remove(&key);
                            say(&responder, embed).await;
                            break why;
                        }
                    }
                }
                Ok(None) => break "user",
                Err(_) => break if Instant::now() >= total { "time" } else { "idle" },
            }
        };
        let st = games_state(&app);
        guard(&st.guess_inboxes).remove(&key);
        guard(&st.numguess).remove(&key.1);
        // Anything other than idle and time (won, exhausted): no message.
        if let Some(embed) = game.end_message(reason, collected) {
            say(&responder, embed).await;
        }
    });
    Ok(())
}

// ── /games tictactoe ─────────────────────────────────────────────────────────

pub const WIN_CONDITIONS: [[usize; 3]; 8] = [
    [0, 1, 2],
    [3, 4, 5],
    [6, 7, 8], // rows
    [0, 3, 6],
    [1, 4, 7],
    [2, 5, 8], // columns
    [0, 4, 8],
    [2, 4, 6], // diagonals
];

/// R022/R091: the game ends after this long WITHOUT a click, with a hard cap that keeps challenge (30 s) + game
/// inside the 15-minute interaction token.
pub const TTT_MOVE_IDLE_MS: u64 = 60_000;
pub const TTT_MAX_MS: u64 = 10 * 60_000;
pub const TTT_CHALLENGE_MS: u64 = 30_000;

/// `buildBoard(board, id, disabled)`: the 3×3 button grid. Filled cells are disabled; every cell is disabled when
/// `disabled`.
pub fn build_board<S: AsRef<str>>(board: &[S], id: &str, disabled: bool) -> Vec<CreateActionRow> {
    (0..3)
        .map(|r| {
            let buttons = (0..3)
                .map(|c| {
                    let idx = r * 3 + c;
                    let mark = board.get(idx).map(|m| m.as_ref()).unwrap_or("");
                    CreateButton::new(bot_config::custom_id!("ttt", id, idx).unwrap_or_default())
                        .label(if mark.is_empty() { "⬜" } else { mark })
                        .style(match mark {
                            "X" => ButtonStyle::Danger,
                            "O" => ButtonStyle::Primary,
                            _ => ButtonStyle::Secondary,
                        })
                        .disabled(disabled || !mark.is_empty())
                })
                .collect();
            CreateActionRow::Buttons(buttons)
        })
        .collect()
}

/// `checkWinner(board)`: `X`, `O`, `draw` or `None`.
pub fn check_winner<S: AsRef<str>>(board: &[S]) -> Option<String> {
    let at = |i: usize| board.get(i).map(|m| m.as_ref()).unwrap_or("");
    for [a, b, c] in WIN_CONDITIONS {
        if !at(a).is_empty() && at(a) == at(b) && at(a) == at(c) {
            return Some(at(a).to_string());
        }
    }
    if (0..9).all(|i| !at(i).is_empty()) { Some("draw".into()) } else { None }
}

/// `shownName(member, user)`.
pub fn shown_name(member: Option<&Member>, user: &User) -> String {
    let raw = member.map(|m| m.display_name().to_string()).unwrap_or_else(|| user.name.clone());
    let name = escape_markdown(&clamp_text(&raw, 32));
    if name.is_empty() { "Player".into() } else { name }
}

struct TttGame {
    settled: bool,
    game_started: bool,
    finished: bool,
    board: Vec<String>,
    current: String,
}

struct Ttt {
    app: App,
    id: String,
    settings: Option<Settings>,
    challenger: String,
    target: String,
    challenger_name: String,
    target_name: String,
    responder: Responder,
    released: AtomicBool,
    state: Mutex<TttGame>,
}

impl Ttt {
    fn lock(&self) -> std::sync::MutexGuard<'_, TttGame> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let st = games_state(&self.app);
        let mut set = guard(&st.ttt);
        set.remove(&self.challenger);
        set.remove(&self.target);
    }

    fn name_of(&self, id: &str) -> &str {
        if id == self.challenger { &self.challenger_name } else { &self.target_name }
    }

    fn mark_of(&self, id: &str) -> &'static str {
        if id == self.challenger { "X" } else { "O" }
    }

    fn turn_embed(&self, current: &str) -> CreateEmbed {
        guild_embed(
            self.settings.as_deref(),
            Some("⭕ Tic-Tac-Toe"),
            Some(&format!(
                "**{}** ❌ vs **{}** 🔵\n\nIt's **{}**'s turn ({}) · 60 s per move",
                self.challenger_name,
                self.target_name,
                self.name_of(current),
                self.mark_of(current)
            )),
            None,
        )
    }

    async fn on_challenge_click(self: Arc<Self>, i: ComponentCtx, stopper: Stopper) {
        let r: Result<(), bot_core::interaction::InteractionError> = async {
            let action = i.parsed().1.get(1).cloned().unwrap_or_default();
            if self.lock().settled {
                let _ = i.defer_update().await;
                return Ok(());
            }
            let clicker = i.user().id.to_string();
            // The handlers of two clicks run concurrently: every decision tests and sets `settled` in one step, and a
            // click that lost the race is acknowledged like one that came after (`if (settled) deferUpdate()`).
            let claim = || !std::mem::replace(&mut self.lock().settled, true);
            if clicker == self.challenger {
                if action == "decline" {
                    // The challenger "declining" cancels their own challenge.
                    if !claim() {
                        let _ = i.defer_update().await;
                        return Ok(());
                    }
                    stopper.stop("cancel");
                    self.release();
                    let embed = guild_embed(self.settings.as_deref(), Some("⭕ Tic-Tac-Toe"), Some(&format!("**{}** cancelled the challenge.", self.challenger_name)), None);
                    i.update(CreateInteractionResponseMessage::new().content("").embed(embed).components(Vec::new())).await?;
                    return Ok(());
                }
                i.reply(CreateInteractionResponseMessage::new().content(format!("⏳ Waiting for **{}** to accept.", self.target_name)).ephemeral(true)).await?;
                return Ok(());
            }
            if clicker != self.target {
                i.reply(CreateInteractionResponseMessage::new().content("❌ This challenge isn't for you!").ephemeral(true)).await?;
                return Ok(());
            }
            if action == "decline" {
                if !claim() {
                    let _ = i.defer_update().await;
                    return Ok(());
                }
                stopper.stop("decline");
                self.release();
                let embed = guild_embed(self.settings.as_deref(), Some("⭕ Tic-Tac-Toe"), Some(&format!("{} declined the challenge.", self.target_name)), None);
                i.update(CreateInteractionResponseMessage::new().content("").embed(embed).components(Vec::new())).await?;
                return Ok(());
            }
            if action != "accept" {
                let _ = i.defer_update().await;
                return Ok(());
            }

            // Accept (R090): decide before any await, so a double tap starts one game.
            let won = {
                let mut g = self.lock();
                let won = !std::mem::replace(&mut g.settled, true);
                if won {
                    g.current = self.challenger.clone(); // the challenger is X and goes first
                }
                won
            };
            if !won {
                let _ = i.defer_update().await;
                return Ok(());
            }
            stopper.stop("accept");

            // The game collector exists before the update lands, so no click is ever left unhandled.
            let game_stopper = Stopper::new();
            let (t1, t2, gs1) = (self.clone(), self.clone(), game_stopper.clone());
            drive(
                &self.app,
                id_filter("ttt", self.id.clone()),
                Timers { idle: Some(Duration::from_millis(TTT_MOVE_IDLE_MS)), time: Duration::from_millis(TTT_MAX_MS) },
                game_stopper.clone(),
                move |gi| t1.clone().on_move(gi, gs1.clone()),
                move |reason, _| t2.on_game_end(reason),
            );
            self.lock().game_started = true;

            let (embed, rows) = {
                let g = self.lock();
                (self.turn_embed(&g.current), build_board(&g.board, &self.id, false))
            };
            if let Err(e) = i.update(CreateInteractionResponseMessage::new().content("").embed(embed).components(rows)).await {
                if is_interaction_gone(&e) {
                    bot_util::log_debug!("tictactoe: accept interaction gone", err_arg(&e));
                } else {
                    bot_util::log_error!("Tic-Tac-Toe start failed", err_arg(&e));
                }
                game_stopper.stop("error"); // releases the locks through its end handler
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            if is_interaction_gone(&e) {
                bot_util::log_debug!("tictactoe: interaction gone", err_arg(&e));
            } else {
                bot_util::log_error!("Tic-Tac-Toe challenge failed", err_arg(&e));
            }
            let (settled, started) = {
                let g = self.lock();
                (g.settled, g.game_started)
            };
            if settled && !started {
                self.release(); // decided, but the game never got its collector
            }
            if !i.is_acknowledged() {
                let _ = i.defer_update().await;
            }
        }
    }

    async fn on_challenge_end(self: Arc<Self>, reason: String) {
        if reason == "accept" {
            return; // the game collector owns the locks now
        }
        self.release();
        if reason == "time" {
            let edit = EditInteractionResponse::new()
                .content("")
                .embed(warn_embed("Challenge Timed Out", Some(&format!("{} did not respond in time.", self.target_name)), None))
                .components(Vec::new());
            let _ = self.responder.edit_reply(edit).await;
        }
    }

    async fn on_move(self: Arc<Self>, gi: ComponentCtx, stopper: Stopper) {
        let r: Result<(), bot_core::interaction::InteractionError> = async {
            if self.lock().finished {
                let _ = gi.defer_update().await; // R242
                return Ok(());
            }
            let clicker = gi.user().id.to_string();
            if clicker != self.challenger && clicker != self.target {
                // R241
                gi.reply(CreateInteractionResponseMessage::new().content("❌ You're not a player in this game!").ephemeral(true)).await?;
                return Ok(());
            }
            let current = self.lock().current.clone();
            if clicker != current {
                gi.reply(CreateInteractionResponseMessage::new().content(format!("⏳ It's **{}**'s turn!", self.name_of(&current))).ephemeral(true)).await?;
                return Ok(());
            }

            let idx = gi.parsed().1.get(1).and_then(|p| p.parse::<usize>().ok());
            enum Next {
                Ignore,
                /// A concurrent click of the same player moved first: it is now the other player's turn.
                NotYourTurn(String),
                Draw(Vec<CreateActionRow>),
                Win(String, Vec<CreateActionRow>),
                Turn(Box<CreateEmbed>, Vec<CreateActionRow>),
            }
            let next = {
                let mut g = self.lock();
                match idx {
                    // The turn and the end are decided again under the lock: two clicks of the same player run
                    // concurrently and must not both move (the JS decides each click on its own, one after the other).
                    _ if g.finished => Next::Ignore,
                    _ if g.current != current => Next::NotYourTurn(self.name_of(&g.current).to_string()),
                    Some(n) if n <= 8 && g.board[n].is_empty() => {
                        g.board[n] = self.mark_of(&current).to_string();
                        match check_winner(&g.board) {
                            Some(r) if r == "draw" => {
                                g.finished = true;
                                Next::Draw(build_board(&g.board, &self.id, true))
                            }
                            Some(r) => {
                                g.finished = true;
                                Next::Win(r, build_board(&g.board, &self.id, true))
                            }
                            None => {
                                g.current = if current == self.challenger { self.target.clone() } else { self.challenger.clone() };
                                Next::Turn(Box::new(self.turn_embed(&g.current)), build_board(&g.board, &self.id, false))
                            }
                        }
                    }
                    _ => Next::Ignore,
                }
            };
            match next {
                Next::Ignore => {
                    let _ = gi.defer_update().await;
                }
                Next::NotYourTurn(name) => {
                    gi.reply(CreateInteractionResponseMessage::new().content(format!("⏳ It's **{name}**'s turn!")).ephemeral(true)).await?;
                }
                Next::Draw(rows) => {
                    stopper.stop("done");
                    let embed = guild_embed(self.settings.as_deref(), Some("🤝 Tic-Tac-Toe — Draw!"), Some("It's a draw! Well played, both of you."), None);
                    gi.update(CreateInteractionResponseMessage::new().embed(embed).components(rows)).await?;
                }
                Next::Win(mark, rows) => {
                    stopper.stop("done");
                    let embed = success_embed("Tic-Tac-Toe — Winner!", Some(&format!("🎉 **{}** wins with **{mark}**s!", self.name_of(&current))), None);
                    gi.update(CreateInteractionResponseMessage::new().embed(embed).components(rows)).await?;
                }
                Next::Turn(embed, rows) => {
                    gi.update(CreateInteractionResponseMessage::new().embed(*embed).components(rows)).await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            if is_interaction_gone(&e) {
                bot_util::log_debug!("tictactoe: interaction gone", err_arg(&e));
            } else {
                bot_util::log_error!("Tic-Tac-Toe move failed", err_arg(&e));
            }
            if !gi.is_acknowledged() {
                let _ = gi.defer_update().await;
            }
        }
    }

    async fn on_game_end(self: Arc<Self>, reason: String) {
        self.release();
        if reason != "idle" && reason != "time" {
            return;
        }
        let (text, rows) = {
            let g = self.lock();
            let text = if reason == "idle" {
                format!("**{}** didn't move within 60 seconds. Game over.", self.name_of(&g.current))
            } else {
                "The game reached its 10-minute limit.".to_string()
            };
            (text, build_board(&g.board, &self.id, true))
        };
        let edit = EditInteractionResponse::new().content("").embed(warn_embed("Tic-Tac-Toe — Timed Out", Some(&text), None)).components(rows);
        let _ = self.responder.edit_reply(edit).await;
    }
}

async fn execute_tictactoe(ctx: CommandCtx) -> anyhow::Result<()> {
    let challenger = ctx.user().clone();
    let Some(target) = ctx.get_user("target") else { anyhow::bail!("Required option \"target\" not found.") };
    let target_member = ctx.get_member("target");

    if target.id == challenger.id {
        ctx.reply_text("❌ You cannot challenge yourself!", true).await?;
        return Ok(());
    }
    if target.bot {
        ctx.reply_text("❌ You cannot challenge a bot!", true).await?;
        return Ok(());
    }
    if target_member.is_none() {
        ctx.reply_text("❌ That user isn't in this server.", true).await?;
        return Ok(());
    }
    let st = games_state(&ctx.app);
    let (cid, tid) = (challenger.id.to_string(), target.id.to_string());
    let busy = guard(&st.ttt).contains(&cid);
    if busy {
        ctx.reply_text("⚠️ You already have an active Tic-Tac-Toe game!", true).await?;
        return Ok(());
    }
    let challenger_name = shown_name(ctx.member(), &challenger);
    let target_name = shown_name(target_member.as_ref(), &target);
    let busy = guard(&st.ttt).contains(&tid);
    if busy {
        ctx.reply_text(format!("⚠️ **{target_name}** is already in a game!"), true).await?;
        return Ok(());
    }

    let settings = load_settings(&ctx).await;
    // The settings lookup may have yielded: re-check, then take both locks with no await in between.
    let refusal = {
        let mut set = guard(&st.ttt);
        if set.contains(&cid) {
            Some("⚠️ You already have an active Tic-Tac-Toe game!".to_string())
        } else if set.contains(&tid) {
            Some(format!("⚠️ **{target_name}** is already in a game!"))
        } else {
            set.insert(cid.clone());
            set.insert(tid.clone());
            None
        }
    };
    if let Some(text) = refusal {
        ctx.reply_text(text, true).await?;
        return Ok(());
    }

    let id = ctx.interaction.id.to_string();
    let game = Arc::new(Ttt {
        app: ctx.app.clone(),
        id: id.clone(),
        settings: settings.clone(),
        challenger: cid,
        target: tid,
        challenger_name: challenger_name.clone(),
        target_name,
        responder: ctx.responder.clone(),
        released: AtomicBool::new(false),
        state: Mutex::new(TttGame { settled: false, game_started: false, finished: false, board: vec![String::new(); 9], current: String::new() }),
    });

    // Phase 1: the challenge.
    let row = CreateActionRow::Buttons(vec![
        CreateButton::new(bot_config::custom_id!("ttt-ch", id, "accept")?).label("✅ Accept").style(ButtonStyle::Success),
        CreateButton::new(bot_config::custom_id!("ttt-ch", id, "decline")?).label("❌ Decline").style(ButtonStyle::Danger),
    ]);
    let reply = CreateInteractionResponseMessage::new()
        .content(format!("<@{}>", target.id))
        .allowed_mentions(serenity::all::CreateAllowedMentions::new().users([target.id]))
        .embed(guild_embed(
            settings.as_deref(),
            Some("⭕ Tic-Tac-Toe Challenge"),
            Some(&format!("**{challenger_name}** challenges you to Tic-Tac-Toe! You have **30 seconds** to respond.")),
            None,
        ))
        .components(vec![row]);
    if let Err(e) = ctx.reply(reply).await {
        // R089: the locks belong to the collectors once they exist; release them if anything fails before.
        game.release();
        return Err(e.into());
    }

    // Filters check only this game's prefix; users are checked inside the handlers, so a wrong user's click gets a
    // friendly reply instead of the router's "expired" fallback.
    let stopper = Stopper::new();
    let (g1, g2, s1) = (game.clone(), game.clone(), stopper.clone());
    drive(
        &ctx.app,
        id_filter("ttt-ch", id),
        Timers { idle: None, time: Duration::from_millis(TTT_CHALLENGE_MS) },
        stopper,
        move |i| g1.clone().on_challenge_click(i, s1.clone()),
        move |reason, _| g2.on_challenge_end(reason),
    );
    Ok(())
}
