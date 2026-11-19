//! The JS loaders discover `src/commands/**`, `src/plugins/*.js` and the services on disk; Rust lists the feature
//! crates here. Every crate exposes one `register`.
//!
//! Order: `collect_plugins` sorts the plugins by id (the JS loader sorts `src/plugins/*.js`), and hooks run in
//! `HOOK_ORDER`, so neither depends on the order below. Commands are sorted into the JS `walkDir` order of
//! `src/commands/<category>/<file>.js` ([`JS_COMMAND_ORDER`]) by [`build_registry`].

use bot_core::{RegisterFn, Registry};

/// Every feature crate, by crate name.
pub const FEATURES: [(&str, RegisterFn); 27] = [
    ("feat-achievements", feat_achievements::register),
    ("feat-automations", feat_automations::register),
    ("feat-automod", feat_automod::register),
    ("feat-backups", feat_backups::register),
    ("feat-captcha", feat_captcha::register),
    ("feat-community-a", feat_community_a::register),
    ("feat-community-b", feat_community_b::register),
    ("feat-counting", feat_counting::register),
    ("feat-dashboard-tasks", feat_dashboard_tasks::register),
    ("feat-economy", feat_economy::register),
    ("feat-embeds", feat_embeds::register),
    ("feat-events", feat_events::register),
    ("feat-feeds", feat_feeds::register),
    ("feat-games", feat_games::register),
    ("feat-general", feat_general::register),
    ("feat-giveaways-polls", feat_giveaways_polls::register),
    ("feat-levels", feat_levels::register),
    ("feat-logging", feat_logging::register),
    ("feat-moderation", feat_moderation::register),
    ("feat-reactionroles", feat_reactionroles::register),
    ("feat-search", feat_search::register),
    ("feat-starboard-stats", feat_starboard_stats::register),
    ("feat-tags", feat_tags::register),
    ("feat-tempvoice", feat_tempvoice::register),
    ("feat-tickets", feat_tickets::register),
    ("feat-translation", feat_translation::register),
    ("feat-welcome", feat_welcome::register),
];

/// Command names in the order the JS loader finds their files (`src/commands/<category>/<file>.js`, sorted), as in
/// `rust/fixtures/golden/commands.json`.
pub const JS_COMMAND_ORDER: [&str; 79] = [
    "achievement-config",
    "antiraid",
    "automation",
    "automod",
    "autothread",
    "autotime",
    "autotranslate",
    "backup",
    "captcha",
    "counting",
    "economy-admin",
    "embed",
    "emoji",
    "feed",
    "levelconfig",
    "liverole",
    "logging",
    "modmail",
    "module",
    "reactionrole",
    "reminder",
    "rolepersist",
    "settings",
    "starboard",
    "stats",
    "sticky",
    "ticket-panel",
    "voice-hub",
    "voicerole",
    "welcome",
    "achievements",
    "afk",
    "application",
    "birthday",
    "boost",
    "bumps",
    "confession",
    "event",
    "giveaway",
    "poll",
    "suggestion",
    "ticket",
    "voice",
    "balance",
    "daily",
    "inventory",
    "pay",
    "pity",
    "richest",
    "shop",
    "use",
    "work",
    "blackjack",
    "games",
    "roulette",
    "scrabble",
    "slots",
    "leaderboard",
    "rank",
    "ban",
    "case",
    "kick",
    "modconfig",
    "purge",
    "slowmode",
    "softban",
    "timeout",
    "unban",
    "unmute",
    "warn",
    "search",
    "help",
    "info",
    "invites",
    "owner",
    "tag",
    "timestamp",
    "timezone",
    "translate",
];

/// A command's position in [`JS_COMMAND_ORDER`] (unknown names last, keeping their registration order).
pub fn js_command_rank(name: &str) -> usize {
    JS_COMMAND_ORDER.iter().position(|n| *n == name).unwrap_or(usize::MAX)
}

/// The register functions alone.
pub fn register_fns() -> Vec<RegisterFn> {
    FEATURES.iter().map(|(_, f)| *f).collect()
}

/// The whole bot: every command and plugin of the JS bot (`loadCommands` + `collectPlugins` input).
/// Commands come out in the JS `walkDir` order ([`JS_COMMAND_ORDER`]).
pub fn build_registry() -> Registry {
    let mut reg = Registry::with(&register_fns());
    reg.sort_commands_by(|a, b| js_command_rank(a.name()).cmp(&js_command_rank(b.name())));
    reg
}
