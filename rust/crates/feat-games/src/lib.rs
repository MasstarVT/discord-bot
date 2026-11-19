//! feat-games: the games package.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/scrabbleService.js` | [`scrabble_service`] |
//! | `src/plugins/games-fixes.js` | [`games_fixes`] |
//! | `src/commands/games/blackjack.js` | [`commands::blackjack`] |
//! | `src/commands/games/games.js` | [`commands::games`] |
//! | `src/commands/games/roulette.js` | [`commands::roulette`] |
//! | `src/commands/games/scrabble.js` | [`commands::scrabble`] |
//! | `src/commands/games/slots.js` | [`commands::slots`] |
//!
//! [`casino`] holds the start sequence the three casino commands share and their test seam; [`collector_driver`]
//! gives bot-core's collectors the discord.js `idle` / `time` / `stop(reason)` semantics.

pub mod casino;
pub mod collector_driver;
pub mod commands;
pub mod games_fixes;
pub mod scrabble_service;

/// Registers this crate's commands and plugins.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::blackjack::command());
    reg.command(commands::games::command());
    reg.command(commands::roulette::command());
    reg.command(commands::scrabble::command());
    reg.command(commands::slots::command());
    reg.plugin(games_fixes::plugin());
    // /games numguess reads the player's typed guesses (the JS channel message collector).
    reg.on_boot("games numguess listener", commands::games::register_numguess_listener);
}
