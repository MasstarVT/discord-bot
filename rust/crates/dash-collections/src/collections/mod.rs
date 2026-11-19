//! The collection definitions' code: one module per JS file of `src/dashboard/collections/` (several small ones share
//! `small.rs`, giveaways and polls share a file, levels and balances too). Their descriptors are data
//! (`collections_data.json`); [`attach_hooks`] gives each definition its Rust `check` and ref-key hooks.

pub mod automations;
pub mod cases;
pub mod embeds;
pub mod feeds;
pub mod giveaways_polls;
pub mod levels;
pub mod reaction_roles;
pub mod reminders;
pub mod small;
pub mod tags;
pub mod ticket_panels;
pub mod translate_stats;
pub mod triggers;

use crate::index::Collection;

/// Attaches the server-only hooks of the JS definition (`check`, an action's `check`, `refKeys`, `refKeyOf`) to a
/// collection loaded from the data file.
pub fn attach_hooks(c: Collection) -> Collection {
    match c.key.as_str() {
        "tags" => c.with_check(tags::check_tag),
        "triggers" => c.with_check(triggers::check_trigger),
        "reminders" => c.with_check(reminders::check_reminder),
        "feeds" => c.with_check(feeds::check_feed),
        "levelRoles" => c.with_check(small::check_level_role),
        "shopItems" => c.with_check(small::check_shop_item),
        "achievements" => c.with_check(small::check_achievement),
        "starboards" => c.with_check(small::check_starboard),
        "tempVoiceHubs" => c.with_check(small::check_temp_voice_hub),
        "statChannels" => c.with_check(small::check_stat_channel),
        "cases" => c.with_check(cases::check_case),
        "giveaways" => c
            .with_check(giveaways_polls::check_giveaway)
            .with_action_check("restart", giveaways_polls::check_giveaway_restart),
        "polls" => c.with_check(giveaways_polls::check_poll),
        "reactionRoles" => c.with_check(reaction_roles::check_reaction_roles),
        "ticketPanels" => c.with_check(ticket_panels::check_ticket_panel),
        "automations" => c
            .with_check(automations::check_automation)
            .with_ref_keys(automations::action_role_keys, automations::action_role_key_of),
        _ => c,
    }
}
