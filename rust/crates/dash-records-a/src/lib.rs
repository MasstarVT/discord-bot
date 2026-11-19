//! dash-records-a: the dashboard record stores of 11 collections (ports of `src/dashboard/collections/*.store.js`),
//! each an implementation of dash-collections' [`RecordStore`]. They read and write through the bot's own services
//! (the feature crates) and bot-db, so the bot process sees every change exactly as it sees its own.
//!
//! [`register_stores`] puts them all into a [`StoreRegistry`].

use std::sync::Arc;

use bot_core::App;
use dash_collections::{RecordStore, StoreRegistry};

pub mod achievements_store;
pub mod automations_store;
pub mod balances_store;
pub mod cases_store;
pub mod common;
pub mod economy_boosts_store;
pub mod embeds_store;
pub mod feeds_store;
pub mod giveaways_store;
pub mod level_roles_store;
pub mod levels_store;
pub mod polls_store;

pub use achievements_store::AchievementsStore;
pub use automations_store::AutomationsStore;
pub use balances_store::BalancesStore;
pub use cases_store::CasesStore;
pub use economy_boosts_store::EconomyBoostsStore;
pub use embeds_store::EmbedsStore;
pub use feeds_store::FeedsStore;
pub use giveaways_store::GiveawaysStore;
pub use level_roles_store::LevelRolesStore;
pub use levels_store::LevelsStore;
pub use polls_store::PollsStore;

/// The collection keys this crate serves, in registration order.
pub const STORE_KEYS: [&str; 11] = ["embeds", "feeds", "levelRoles", "achievements", "economyBoosts", "cases", "levels", "balances", "giveaways", "polls", "automations"];

/// Builds the store of one key (None for a key this crate does not serve).
pub fn store_for(key: &str, app: &App) -> Option<Arc<dyn RecordStore>> {
    let app = app.clone();
    Some(match key {
        "levelRoles" => Arc::new(LevelRolesStore::new(app)),
        "embeds" => Arc::new(EmbedsStore::new(app)),
        "feeds" => Arc::new(FeedsStore::new(app)),
        "achievements" => Arc::new(AchievementsStore::new(app)),
        "economyBoosts" => Arc::new(EconomyBoostsStore::new(app)),
        "cases" => Arc::new(CasesStore::new(app)),
        "levels" => Arc::new(LevelsStore::new(app)),
        "balances" => Arc::new(BalancesStore::new(app)),
        "giveaways" => Arc::new(GiveawaysStore::new(app)),
        "polls" => Arc::new(PollsStore::new(app)),
        "automations" => Arc::new(AutomationsStore::new(app)),
        _ => return None,
    })
}

/// Registers the stores of this crate (`STORE_KEYS`) in `registry`, each on `app` (its database pool, cache,
/// settings and scheduler). `Err` names a key the registry's collections do not have.
pub fn register_stores(registry: &mut StoreRegistry, app: &App) -> Result<(), String> {
    for key in STORE_KEYS {
        let store = store_for(key, app).ok_or_else(|| format!("no store for {key}"))?;
        registry.register(key, store)?;
    }
    Ok(())
}

/// The raw statements of the stores (the `RAW_SQL` exports of the JS stores), with sample text parameters:
/// (name, SQL, parameters). A database test runs each one inside a rolled-back transaction, as test/db.test.js does.
pub fn raw_sql() -> Vec<(&'static str, String, Vec<String>)> {
    vec![
        ("dashboard.lockGuildSettings", common::LOCK_GUILD_SETTINGS_SQL.into(), vec!["0".into()]),
        ("dashboard.lockAchievement", achievements_store::ACHIEVEMENT_LOCK_SQL.into(), vec!["0".into(), "0".into()]),
        ("dashboard.embeds.lockGuild", common::ADVISORY_LOCK_SQL.into(), vec!["savedembed:0".into()]),
        ("dashboard.feeds.lockGuild", common::ADVISORY_LOCK_SQL.into(), vec!["feed-create:0".into()]),
        ("dashboard.polls.voters", polls_store::voters_sql(1), vec!["0".into()]),
    ]
}
