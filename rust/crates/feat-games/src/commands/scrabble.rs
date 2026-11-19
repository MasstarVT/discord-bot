//! Port of `src/commands/games/scrabble.js`: /scrabble (pkg-games). `start` challenges a player, `word|pass|exchange|
//! resign` are the moves and `help` explains the game. The game itself (board, bag, scoring, timers) lives in
//! [`crate::scrabble_service`].

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand};
use bot_core::embed_builder::{error_embed, guild_embed, success_embed, warn_embed};
use bot_core::interaction::{CommandCtx, ComponentCtx, InteractionError, Responder};
use bot_core::interaction_helpers::{ConfirmOptions, Payload, await_confirm, is_interaction_gone, safe_defer, safe_reply};
use bot_core::prelude::{ModuleKey, PermissionLevel};
use bot_core::settings::Settings;
use bot_core::App;
use bot_db::GuildSettings;
use serenity::all::{
    ButtonStyle, CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateInteractionResponseFollowup, CreateInteractionResponseMessage,
    EditInteractionResponse,
};

use crate::collector_driver::{Stopper, Timers, drive, id_filter};
use crate::commands::games::shown_name;
use crate::scrabble_service::{
    self as svc, CHALLENGE_TIMEOUT_MS, COLS, CreateGame, DictState, DictStatus, EndReason, GameRef, current_player_id, describe_words, dictionary_status,
    exchange_tiles, finish_game, format_rack, get_dictionary, get_game, is_channel_busy, load_dictionary, lock, pass_turn, play_word, post_board,
    post_result, register_game, release_channel, reserve_channel, resign_game,
};

/// The data: `/scrabble start|word|pass|exchange|resign|help`.
pub fn data() -> SlashCommand {
    let mut col = Opt::string("col", "Column A-O").required(true);
    for c in COLS.chars() {
        col = col.choice(c.to_string(), c.to_string());
    }
    SlashCommand::new("scrabble", "Play Scrabble: challenge a player, place words, pass, exchange tiles or resign.")
        .option(
            Opt::sub_command("start", "Challenge another player to a game of Scrabble!")
                .option(Opt::user("target", "The player you want to challenge").required(true)),
        )
        .option(
            Opt::sub_command("word", "Place a word")
                .option(Opt::string("word", "Letters; ?X = blank played as X").required(true).min_length(1).max_length(17))
                .option(Opt::integer("row", "Row 1-15").required(true).min_value(1).max_value(15))
                .option(col)
                .option(Opt::string("direction", "Direction").required(true).choice("Horizontal", "h").choice("Vertical", "v")),
        )
        .option(Opt::sub_command("pass", "Skip your turn"))
        .option(
            Opt::sub_command("exchange", "Swap tiles with the bag")
                .option(Opt::string("tiles", "Letters to swap; ? = blank").required(true).min_length(1).max_length(7)),
        )
        .option(Opt::sub_command("resign", "Forfeit the game"))
        .option(Opt::sub_command("help", "Learn how to play Scrabble and use its commands."))
}

/// The command.
pub fn command() -> CommandDef {
    CommandDef::new("games", data(), Some(ModuleKey::Games), |ctx| async move { execute(ctx).await }).permission_level(PermissionLevel::Member)
}

/// `/scrabble <subcommand>`.
pub async fn execute(ctx: CommandCtx) -> anyhow::Result<()> {
    let sub = ctx.subcommand().unwrap_or_default();
    match sub.as_str() {
        "start" => execute_start(ctx).await,
        "help" => execute_help(ctx).await,
        _ => execute_play(ctx, sub).await,
    }
}

// ── /scrabble start ──────────────────────────────────────────────────────────

/// How long an accepted challenge may wait for the dictionary to finish loading (it started at challenge time).
const DICTIONARY_WAIT_MS: u64 = 5000;
const BUSY_TEXT: &str = "⚠️ There is already a Scrabble game or a pending challenge in this channel!";

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

struct Challenge {
    app: App,
    id: String,
    guild_id: String,
    channel_id: String,
    settings: Option<Settings>,
    challenger: String,
    target: String,
    challenger_name: String,
    target_name: String,
    responder: Responder,
    settled: Mutex<bool>,
}

impl Challenge {
    fn settled(&self) -> bool {
        *self.settled.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Claims the decision: `false` when another click already settled the challenge. The handlers of two clicks run
    /// concurrently, so the test and the set are one step.
    fn settle(&self) -> bool {
        !std::mem::replace(&mut *self.settled.lock().unwrap_or_else(|e| e.into_inner()), true)
    }

    async fn on_click(self: Arc<Self>, i: ComponentCtx, stopper: Stopper) {
        let r: Result<(), InteractionError> = async {
            let action = i.parsed().1.get(1).cloned().unwrap_or_default();
            if self.settled() {
                let _ = i.defer_update().await;
                return Ok(());
            }
            let clicker = i.user().id.to_string();
            if clicker == self.challenger {
                if action == "decline" {
                    // The challenger "declining" cancels their own challenge.
                    if !self.settle() {
                        let _ = i.defer_update().await;
                        return Ok(());
                    }
                    stopper.stop("cancel");
                    release_channel(&self.app, &self.channel_id, &self.id);
                    let embed = guild_embed(self.settings.as_deref(), Some("🔤 Scrabble"), Some(&format!("**{}** cancelled the challenge.", self.challenger_name)), None);
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
                if !self.settle() {
                    let _ = i.defer_update().await;
                    return Ok(());
                }
                stopper.stop("decline");
                release_channel(&self.app, &self.channel_id, &self.id);
                let embed =
                    guild_embed(self.settings.as_deref(), Some("🔤 Scrabble"), Some(&format!("{} declined the Scrabble challenge.", self.target_name)), None);
                i.update(CreateInteractionResponseMessage::new().content("").embed(embed).components(Vec::new())).await?;
                return Ok(());
            }
            if action != "accept" {
                let _ = i.defer_update().await;
                return Ok(());
            }

            // Accept (R021, R086, R087): decide synchronously, so a second Accept can't start a second game.
            if !self.settle() {
                let _ = i.defer_update().await;
                return Ok(());
            }
            stopper.stop("accept");

            // Acknowledge first (00-overview rule 21). The channel stays reserved meanwhile, and `settled` (plus the
            // ended collector) blocks repeat clicks, so nothing can start a second game.
            i.defer_update().await?;

            // The dictionary load began when the challenge was issued, so this is normally instant.
            let dictionary = if dictionary_status(&self.app).state == DictState::Loading {
                let fut = load_dictionary(&self.app);
                tokio::time::timeout(Duration::from_millis(DICTIONARY_WAIT_MS), fut).await.ok().flatten()
            } else {
                get_dictionary(&self.app)
            };

            let game = svc::create_game(CreateGame {
                id: self.id.clone(),
                guild_id: self.guild_id.clone(),
                channel_id: self.channel_id.clone(),
                settings: self.settings.clone(),
                dictionary,
                players: [(self.challenger.clone(), self.challenger_name.clone()), (self.target.clone(), self.target_name.clone())],
                rng: svc::math_random(),
            });
            if !register_game(&self.app, &game) {
                release_channel(&self.app, &self.channel_id, &self.id);
                i.follow_up(CreateInteractionResponseFollowup::new().content("⚠️ A Scrabble game already started in this channel.").ephemeral(true)).await?;
                return Ok(());
            }
            lock(&game).last_interaction = Some(i.responder.clone());

            let mut started = false;
            let embed = guild_embed(
                self.settings.as_deref(),
                Some("🔤 Scrabble"),
                Some(&format!("**{}** accepted **{}**'s challenge. Good luck!", self.target_name, self.challenger_name)),
                None,
            );
            match i.edit_reply(EditInteractionResponse::new().content("").embed(embed).components(Vec::new())).await {
                Ok(_) => {
                    started = post_board(
                        &game,
                        &i.responder,
                        &format!("<@{}>, you go first! Play with `/scrabble word` — you have 3 minutes per turn.", self.challenger),
                        std::slice::from_ref(&self.challenger),
                    )
                    .await;
                }
                Err(e) => {
                    if is_interaction_gone(&e) {
                        bot_util::log_debug!("scrabble: accept interaction gone", err_arg(&e));
                    } else {
                        bot_util::log_error!("Scrabble start failed", err_arg(&e));
                    }
                }
            }
            if !started {
                // A game that never got its board would lock the channel until its timer ran out.
                finish_game(&mut lock(&game), EndReason::Aborted, None, None);
                let _ = i
                    .follow_up(CreateInteractionResponseFollowup::new().content("⚠️ I couldn't start the game. Please try `/scrabble start` again.").ephemeral(true))
                    .await;
            }
            Ok(())
        }
        .await;
        if let Err(e) = r {
            if is_interaction_gone(&e) {
                bot_util::log_debug!("scrabble: interaction gone", err_arg(&e));
            } else {
                bot_util::log_error!("Scrabble challenge collect failed", err_arg(&e));
            }
            release_channel(&self.app, &self.channel_id, &self.id);
            if !i.is_acknowledged() {
                let _ = i.defer_update().await;
            }
        }
    }

    async fn on_end(self: Arc<Self>, reason: String) {
        if reason == "accept" || reason == "decline" || reason == "cancel" {
            return;
        }
        release_channel(&self.app, &self.channel_id, &self.id);
        if reason == "time" {
            let edit = EditInteractionResponse::new()
                .content("")
                .embed(warn_embed("Challenge Timed Out", Some(&format!("{} did not respond in time.", self.target_name)), None))
                .components(Vec::new());
            let _ = self.responder.edit_reply(edit).await;
        }
    }
}

async fn execute_start(ctx: CommandCtx) -> anyhow::Result<()> {
    let challenger = ctx.user().clone();
    let Some(target) = ctx.get_user("target") else { anyhow::bail!("Required option \"target\" not found.") };
    let channel_id = ctx.channel_id().to_string();

    if target.id == challenger.id {
        ctx.reply_text("❌ You cannot challenge yourself!", true).await?;
        return Ok(());
    }
    if target.bot {
        ctx.reply_text("❌ You cannot challenge a bot!", true).await?;
        return Ok(());
    }
    let target_member = ctx.get_member("target");
    if target_member.is_none() {
        ctx.reply_text("❌ That user isn't in this server.", true).await?;
        return Ok(());
    }
    if is_channel_busy(&ctx.app, &channel_id) {
        ctx.reply_text(BUSY_TEXT, true).await?;
        return Ok(());
    }

    drop(load_dictionary(&ctx.app)); // starts the lazy load; memoised, never fails, deliberately not awaited
    let settings = load_settings(&ctx).await;
    let challenger_name = shown_name(ctx.member(), &challenger);
    let target_name = shown_name(target_member.as_ref(), &target);

    // R085: the reservation is taken synchronously with the busy check, so two challenges can't coexist.
    let id = ctx.interaction.id.to_string();
    if !reserve_channel(&ctx.app, &channel_id, &id) {
        ctx.reply_text(BUSY_TEXT, true).await?;
        return Ok(());
    }

    let row = CreateActionRow::Buttons(vec![
        CreateButton::new(bot_config::custom_id!("scrabble-ch", id, "accept")?).label("✅ Accept").style(ButtonStyle::Success),
        CreateButton::new(bot_config::custom_id!("scrabble-ch", id, "decline")?).label("❌ Decline").style(ButtonStyle::Danger),
    ]);
    let reply = CreateInteractionResponseMessage::new()
        .content(format!("<@{}>", target.id))
        .allowed_mentions(CreateAllowedMentions::new().users([target.id]))
        .embed(guild_embed(
            settings.as_deref(),
            Some("🔤 Scrabble Challenge"),
            Some(&format!("**{challenger_name}** challenges you to Scrabble! You have **30 seconds** to respond.")),
            None,
        ))
        .components(vec![row]);
    if let Err(e) = ctx.reply(reply).await {
        // R089: the reservation belongs to the collector once it exists; release it if anything fails before.
        release_channel(&ctx.app, &channel_id, &id);
        return Err(e.into());
    }

    let challenge = Arc::new(Challenge {
        app: ctx.app.clone(),
        id: id.clone(),
        guild_id: ctx.guild_id().map(|g| g.to_string()).unwrap_or_default(),
        channel_id,
        settings,
        challenger: challenger.id.to_string(),
        target: target.id.to_string(),
        challenger_name,
        target_name,
        responder: ctx.responder.clone(),
        settled: Mutex::new(false),
    });
    // The filter checks only this challenge's prefix; users are checked inside the handler.
    let stopper = Stopper::new();
    let (c1, c2, s1) = (challenge.clone(), challenge.clone(), stopper.clone());
    drive(
        &ctx.app,
        id_filter("scrabble-ch", id),
        Timers { idle: None, time: Duration::from_millis(CHALLENGE_TIMEOUT_MS) },
        stopper,
        move |i| c1.clone().on_click(i, s1.clone()),
        move |reason, _| c2.on_end(reason),
    );
    Ok(())
}

// ── /scrabble word, pass, exchange, resign ───────────────────────────────────

fn no_game() -> CreateEmbed {
    error_embed("No Active Game", Some("There is no Scrabble game running in this channel. Start one with `/scrabble start`."), None)
}

fn not_player() -> CreateEmbed {
    error_embed("Not Your Game", Some("You're not a player in this game."), None)
}

/// Ephemeral edit of the deferred reply; the move is already committed, so a failure is only logged.
async fn tell(responder: &Responder, embed: CreateEmbed) {
    if let Err(e) = responder.edit_reply(EditInteractionResponse::new().embed(embed)).await {
        if is_interaction_gone(&e) {
            bot_util::log_debug!("scrabble: interaction gone", err_arg(&e));
        } else {
            bot_util::log_warn!("scrabble: reply failed", err_arg(&e));
        }
    }
}

/// Posts the public follow-up after a committed move: the next board (with a real mention of the next player, R084)
/// or the final result. The ephemeral reply is edited FIRST, because the first follow-up after a deferred reply
/// would otherwise become that reply and stay private.
async fn announce(game: &GameRef, responder: &Responder, game_over: Option<&svc::GameResult>, text: &str) {
    if let Some(result) = game_over {
        post_result(game, Some(responder), Some(result)).await;
        return;
    }
    let next = current_player_id(&lock(game)).to_string();
    post_board(game, responder, &format!("<@{next}>, it's your turn! {text}"), std::slice::from_ref(&next)).await;
}

/// The options of a move.
#[derive(Debug, Clone, Default)]
pub struct MoveOptions {
    pub word: Option<String>,
    pub row: Option<i64>,
    pub col: Option<String>,
    pub direction: Option<String>,
    pub tiles: Option<String>,
}

/// `handleMove(interaction, sub)`, run under the channel's game mutex.
async fn handle_move(app: App, responder: Responder, channel_id: String, user_id: String, sub: String, opts: MoveOptions) {
    let Some(game) = get_game(&app, &channel_id) else {
        tell(&responder, no_game()).await;
        return;
    };
    let my_name = lock(&game).player(&user_id).map(|p| p.name.clone());
    let Some(my_name) = my_name else {
        tell(&responder, not_player()).await;
        return;
    };

    // The service calls below are synchronous (validate + commit with no await in between); the mutex only keeps
    // the Discord calls of consecutive moves in order.
    match sub.as_str() {
        "word" => {
            let col0 = opts.col.as_deref().and_then(|c| COLS.find(c).filter(|_| c.len() == 1)).map(|i| i as i64).unwrap_or(-1);
            let row0 = opts.row.map(|r| r - 1).unwrap_or(-1);
            let outcome = {
                let mut g = lock(&game);
                play_word(&mut g, &user_id, opts.word.as_deref().unwrap_or(""), row0, col0, opts.direction.as_deref().unwrap_or("")).map(|r| {
                    let rack = g.player(&user_id).map(|p| format_rack(&p.rack)).unwrap_or_default();
                    (r, rack)
                })
            };
            let (r, rack) = match outcome {
                Ok(v) => v,
                Err(e) => {
                    tell(&responder, error_embed("Invalid Move", Some(&e), None)).await;
                    return;
                }
            };
            let words = describe_words(&r.words);
            tell(
                &responder,
                success_embed("Word Played!", Some(&format!("{words}{} = **{}** pts.\n🎒 Your tiles: {rack}", if r.bingo { " + **50** bingo 🎉" } else { "" }, r.total)), None),
            )
            .await;
            announce(&game, &responder, r.game_over.as_ref(), &format!("**{my_name}** played {words} for **{}** points.", r.total)).await;
        }
        "pass" => {
            let r = {
                let mut g = lock(&game);
                pass_turn(&mut g, &user_id)
            };
            match r {
                Err(e) => tell(&responder, error_embed("Invalid Move", Some(&e), None)).await,
                Ok(over) => {
                    tell(&responder, success_embed("Turn Passed", Some("You passed your turn."), None)).await;
                    announce(&game, &responder, over.as_ref(), &format!("**{my_name}** passed.")).await;
                }
            }
        }
        "exchange" => {
            let outcome = {
                let mut g = lock(&game);
                exchange_tiles(&mut g, &user_id, opts.tiles.as_deref().unwrap_or("")).map(|r| {
                    let rack = g.player(&user_id).map(|p| format_rack(&p.rack)).unwrap_or_default();
                    (r, rack)
                })
            };
            match outcome {
                Err(e) => tell(&responder, error_embed("Invalid Move", Some(&e), None)).await,
                Ok(((count, over), rack)) => {
                    let s = if count == 1 { "" } else { "s" };
                    tell(&responder, success_embed("Tiles Exchanged", Some(&format!("You swapped **{count}** tile{s}.\n🎒 Your tiles: {rack}")), None)).await;
                    // Never reveal which tiles were swapped.
                    announce(&game, &responder, over.as_ref(), &format!("**{my_name}** exchanged {count} tile{s}.")).await;
                }
            }
        }
        _ => {}
    }
}

async fn handle_resign(ctx: &CommandCtx) {
    let channel_id = ctx.channel_id().to_string();
    let user_id = ctx.user().id.to_string();

    let Some(game) = get_game(&ctx.app, &channel_id) else {
        tell(&ctx.responder, no_game()).await;
        return;
    };
    let game_id = {
        let g = lock(&game);
        g.player(&user_id).map(|_| g.id.clone())
    };
    let Some(game_id) = game_id else {
        tell(&ctx.responder, not_player()).await;
        return;
    };

    // Outside the mutex, so other moves are not blocked while the player decides.
    let ok = await_confirm(
        ctx,
        ConfirmOptions {
            title: Some("Resign this game?".into()),
            description: Some("Your opponent will be declared the winner.".into()),
            confirm_label: "Resign".into(),
            cancel_label: "Keep playing".into(),
            danger: true,
            timeout_ms: 30_000.0,
            ephemeral: true,
        },
    )
    .await;
    if !ok {
        return;
    }

    let app = ctx.app.clone();
    let responder = ctx.responder.clone();
    svc::state(&ctx.app)
        .game_mutex
        .run(&channel_id.clone(), move || async move {
            let g = get_game(&app, &channel_id).filter(|g| lock(g).id == game_id);
            let Some(g) = g else {
                safe_reply(&responder, Payload::embed(error_embed("No Active Game", Some("That game has already ended."), None)).ephemeral(true)).await;
                return;
            };
            let result = resign_game(&mut lock(&g), &user_id);
            post_result(&g, Some(&responder), Some(&result)).await;
        })
        .await;
}

async fn execute_play(ctx: CommandCtx, sub: String) -> anyhow::Result<()> {
    // R083: acknowledge first, then serialise every move of the channel.
    if !safe_defer(&ctx.responder, true).await {
        return Ok(());
    }
    if sub == "resign" {
        handle_resign(&ctx).await;
        return Ok(());
    }
    let opts = MoveOptions {
        word: ctx.get_string("word"),
        row: ctx.get_integer("row"),
        col: ctx.get_string("col"),
        direction: ctx.get_string("direction"),
        tiles: ctx.get_string("tiles"),
    };
    let channel_id = ctx.channel_id().to_string();
    let (app, responder, user_id, sub2) = (ctx.app.clone(), ctx.responder.clone(), ctx.user().id.to_string(), sub.clone());
    let ch = channel_id.clone();
    let run = svc::state(&ctx.app).game_mutex.run(&channel_id, move || async move {
        tokio::spawn(handle_move(app, responder, ch, user_id, sub2, opts)).await
    });
    if let Err(e) = run.await {
        bot_util::log_error!(format!("/scrabble {sub} failed"), err_arg(&e));
        safe_reply(&ctx.responder, Payload::embed(error_embed("Something Went Wrong", Some("That move could not be completed. Please try again."), None)).ephemeral(true))
            .await;
    }
    Ok(())
}

// ── /scrabble help ───────────────────────────────────────────────────────────

/// `dictionaryLine(status)`: the "Rules" bullet that tells players whether words are checked.
pub fn dictionary_line(status: DictStatus) -> String {
    if status.state == DictState::Loaded {
        format!("Words are checked against a **{}-word** list.", bot_util::text::format_number(status.size as f64))
    } else {
        "Word checking is **off**: any letters are accepted.".into()
    }
}

/// `buildHelpEmbed(settings, status)`: every field value stays within Discord's 1,024-character limit.
pub fn build_help_embed(settings: Option<&GuildSettings>, status: DictStatus) -> CreateEmbed {
    guild_embed(
        settings,
        Some("🔤 How to Play Scrabble"),
        Some("A 2-player word game on a 15×15 board. Take turns placing words; the highest score at the end wins."),
        Some("Tip: use /scrabble word with direction:Vertical to build down from an existing tile."),
    )
    .field(
        "📋 Commands",
        [
            "`/scrabble start` — challenge someone (30 s to accept)",
            "`/scrabble word` — place a word",
            "`/scrabble pass` — skip your turn",
            "`/scrabble exchange` — swap tiles with the bag (needs 7+ in the bag)",
            "`/scrabble resign` — forfeit the game",
            "`/scrabble help` — this help",
            "Click **🎒 View My Tiles** on the board to see your rack privately.",
        ]
        .join("\n"),
        false,
    )
    .field(
        "📍 Placing Words",
        [
            "Give the **starting square** and a **direction**:",
            "• **Row** `1`–`15` (top → bottom) · **Column** `A`–`O` (left → right)",
            "• **Direction** — Horizontal (→) or Vertical (↓)",
            "Example: `/scrabble word word:HELLO row:8 col:H direction:Horizontal`",
            "Tiles touching your word are joined to it automatically.",
            "**Blanks:** put `?` before a letter to play a blank as that letter, e.g. `QU?IZ`. Blanks score 0 and show in lowercase.",
        ]
        .join("\n"),
        false,
    )
    .field(
        "📏 Rules",
        [
            "• The first word must cover the centre square **H8** (★) and be at least 2 letters.".to_string(),
            "• Later words must touch or pass through tiles already on the board.".to_string(),
            "• Every word your tiles form counts — the main word and any cross-words.".to_string(),
            "• Using all 7 tiles in one move earns a **+50 bingo**.".to_string(),
            "• You have **3 minutes** per turn; running out of time forfeits the game.".to_string(),
            format!("• {}", dictionary_line(status)),
        ]
        .join("\n"),
        false,
    )
    .field(
        "🗺️ Board Squares",
        [
            "`TW` Triple Word score",
            "`DW` Double Word score",
            "`TL` Triple Letter score",
            "`DL` Double Letter score",
            "`★ ` Centre square (counts as Double Word on first play)",
            "`· ` Empty square",
            "`a` lowercase letter = blank tile",
        ]
        .join("\n"),
        false,
    )
    .field(
        "🔢 Scoring",
        [
            "Each letter has a point value (e.g. A=1, Q=10, Z=10).",
            "Letter multipliers (`DL`/`TL`) apply first, then word multipliers (`DW`/`TW`) multiply the total.",
            "Multipliers only apply when a **new tile** is placed on that square — they are consumed after use.",
            "Cross-words formed by your new tiles are scored too.",
        ]
        .join("\n"),
        false,
    )
    .field(
        "🏁 Ending the Game",
        [
            "• **Out:** the bag is empty and a player uses their last tile — they gain the value of the opponent's tiles, which the opponent loses.",
            "• **Six scoreless turns** in a row (passes/exchanges): each player loses the value of their own tiles.",
            "• **Resign or time-out:** the other player wins.",
        ]
        .join("\n"),
        false,
    )
}

async fn execute_help(ctx: CommandCtx) -> anyhow::Result<()> {
    // Acknowledge first (a public deferred reply), then take the time the dictionary status may need.
    if !safe_defer(&ctx.responder, false).await {
        return Ok(());
    }
    let settings = match &ctx.settings {
        Some(s) => Some(s.clone()),
        None => match ctx.guild_id() {
            Some(g) => {
                let gid = g.to_string();
                match ctx.app.settings().peek_settings(&gid) {
                    Some(s) => Some(s),
                    None => ctx.app.settings().get_settings_safe(&gid).await,
                }
            }
            None => None,
        },
    };
    // Starts the lazy load if nobody has yet, and waits briefly so the status line is accurate.
    let _ = tokio::time::timeout(Duration::from_millis(2000), load_dictionary(&ctx.app)).await;
    let status = dictionary_status(&ctx.app);
    ctx.edit_reply(EditInteractionResponse::new().embed(build_help_embed(settings.as_deref(), status))).await?;
    Ok(())
}
