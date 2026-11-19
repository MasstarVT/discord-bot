//! Port of `src/plugins/economy.js`: the plugin declaration. It registers the `coins` provider on the rewards bus, so
//! giveaways, achievements and automations grant and take coins without depending on this crate. Declaration only: no
//! hooks, components or jobs. `bj` (blackjack) is a collector prefix and is never registered as a persistent component.

use std::sync::Arc;

use bot_core::prelude::*;

use crate::economy_service::CoinsProvider;

/// `export const plugin = { id: 'economy', module: 'economy', provides: { coins } }`.
pub fn plugin() -> Plugin {
    Plugin::new("economy", Some(ModuleKey::Economy)).provides(RewardKind::Coins, Arc::new(CoinsProvider))
}
