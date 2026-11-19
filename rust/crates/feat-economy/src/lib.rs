//! feat-economy: the Economy module (per-server coins, `/daily`, `/work`, `/pity`, `/pay`, the shop, inventories, coin
//! boosts and `/economy-admin`) and the economy service the casino games (feat-games) build on.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/economyService.js` | [`economy_service`] |
//! | `src/plugins/economy.js` | [`plugin`] |
//! | `src/commands/admin/economy-admin.js` | [`commands::economy_admin`] |
//! | `src/commands/economy/balance.js` | [`commands::balance`] |
//! | `src/commands/economy/daily.js` | [`commands::daily`] |
//! | `src/commands/economy/inventory.js` | [`commands::inventory`] |
//! | `src/commands/economy/pay.js` | [`commands::pay`] |
//! | `src/commands/economy/pity.js` | [`commands::pity`] |
//! | `src/commands/economy/richest.js` | [`commands::richest`] |
//! | `src/commands/economy/shop.js` | [`commands::shop`] |
//! | `src/commands/economy/use.js` | [`commands::use_`] |
//! | `src/commands/economy/work.js` | [`commands::work`] |
//!
//! Other crates (feat-games, the dashboard) call [`economy_service`] directly.

pub mod commands;
pub mod economy_service;
pub mod plugin;

/// Registers the economy commands and the `economy` plugin.
pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::balance::command());
    reg.command(commands::daily::command());
    reg.command(commands::inventory::command());
    reg.command(commands::pay::command());
    reg.command(commands::pity::command());
    reg.command(commands::richest::command());
    reg.command(commands::shop::command());
    reg.command(commands::use_::command());
    reg.command(commands::work::command());
    reg.command(commands::economy_admin::command());
    reg.plugin(plugin::plugin());
}
