//! dash-records-b: eleven of the web dashboard's record stores (`src/dashboard/collections/*.store.js`), each an
//! implementation of dash-collections' [`RecordStore`]. They read and write the same PostgreSQL rows as the bot,
//! through the feature crates that port the bot services the JS stores import, and trigger the same cache
//! invalidations, scheduled jobs and dashboard tasks.
//!
//! [`register`] puts all of them into a [`StoreRegistry`]; bins/dashboard calls it with the dashboard's [`App`].

use std::sync::Arc;

use bot_core::App;
use dash_collections::{RecordStore, StoreRegistry};

pub mod common;
pub mod reaction_roles_store;
pub mod reminders_store;
pub mod shop_items_store;
pub mod starboards_store;
pub mod stat_channels_store;
pub mod tags_store;
pub mod temp_voice_hubs_store;
pub mod ticket_panels_store;
pub mod translate_channels_store;
pub mod translate_stats_store;
pub mod triggers_store;

pub use reaction_roles_store::ReactionRolesStore;
pub use reminders_store::RemindersStore;
pub use shop_items_store::ShopItemsStore;
pub use starboards_store::StarboardsStore;
pub use stat_channels_store::StatChannelsStore;
pub use tags_store::TagsStore;
pub use temp_voice_hubs_store::TempVoiceHubsStore;
pub use ticket_panels_store::TicketPanelsStore;
pub use translate_channels_store::TranslateChannelsStore;
pub use translate_stats_store::TranslateStatsStore;
pub use triggers_store::TriggersStore;

/// The collection keys this crate serves, in the order [`stores`] answers them.
pub const KEYS: [&str; 11] = [
    "reactionRoles",
    "reminders",
    "shopItems",
    "starboards",
    "statChannels",
    "tags",
    "tempVoiceHubs",
    "ticketPanels",
    "translateChannels",
    "translateStats",
    "triggers",
];

/// Every store of this crate with its collection key (recordsBridge.js `STORES` entries).
pub fn stores(app: &App) -> Vec<(&'static str, Arc<dyn RecordStore>)> {
    let a = || app.clone();
    vec![
        ("reactionRoles", Arc::new(ReactionRolesStore::new(a())) as Arc<dyn RecordStore>),
        ("reminders", Arc::new(RemindersStore::new(a()))),
        ("shopItems", Arc::new(ShopItemsStore::new(a()))),
        ("starboards", Arc::new(StarboardsStore::new(a()))),
        ("statChannels", Arc::new(StatChannelsStore::new(a()))),
        ("tags", Arc::new(TagsStore::new(a()))),
        ("tempVoiceHubs", Arc::new(TempVoiceHubsStore::new(a()))),
        ("ticketPanels", Arc::new(TicketPanelsStore::new(a()))),
        ("translateChannels", Arc::new(TranslateChannelsStore::new(a()))),
        ("translateStats", Arc::new(TranslateStatsStore::new(a()))),
        ("triggers", Arc::new(TriggersStore::new(a()))),
    ]
}

/// Registers every store of this crate into `registry`. `app` is the dashboard's application context: built on the
/// same PostgreSQL database and Redis as the bot (`App::production(pool, REDIS_URL)`), so the cache invalidations and
/// scheduled jobs the stores write reach the bot. `Err` names a key the registry's collections do not know.
pub fn register(registry: &mut StoreRegistry, app: &App) -> Result<(), String> {
    for (key, store) in stores(app) {
        registry.register(key, store)?;
    }
    Ok(())
}
