//! The whole bot, offline: the registry against the JS golden files and `test/integration.test.js`, the deploy hash
//! against the JS hash, `.env` parsing against dotenv, and the index.js helpers.

use std::collections::{BTreeMap, HashSet};

use bot_config::{CUSTOM_ID_PREFIXES, CustomIdKind, HookPoint, JobType, MODULES, ModuleKey, PermissionLevel};
use bot_core::Registry;
use bot_core::command::{AutoDeployResult, DeployConfig, auto_deploy_commands, compute_deploy_hash};
use bot_core::hooks::ModuleRef;
use bot_core::rewards::RewardKind;
use bot_core::testing::{
    TestApp, assert_registry_matches_golden, json_diff, load_golden, persistent_handler_problems, plugin_manifest, repo_root, run_node,
};
use serde_json::{Value, json};

use bot::index::{Env, Lifecycle, Phase, parse_health_port, park_text, readiness_of};
use bot::registry::{FEATURES, build_registry};

// ── The command table of test/integration.test.js (EXPECTED_COMMANDS) ────────────────────────────────────────────

/// (name, file, module, gated (None = core command), level).
type CommandRow = (&'static str, &'static str, Option<&'static str>, Option<bool>, &'static str);

const EXPECTED_COMMANDS: &[CommandRow] = &[
    ("achievement-config", "src/commands/admin/achievement-config.js", Some("achievements"), Some(false), "ADMIN"),
    ("automation", "src/commands/admin/automation.js", Some("automations"), Some(false), "ADMIN"),
    ("automod", "src/commands/admin/automod.js", Some("automod"), Some(false), "ADMIN"),
    ("autotranslate", "src/commands/admin/autotranslate.js", Some("translation"), Some(false), "ADMIN"),
    ("counting", "src/commands/admin/counting.js", Some("counting"), Some(false), "ADMIN"),
    ("autotime", "src/commands/admin/autotime.js", Some("autotime"), Some(false), "ADMIN"),
    ("suggestion", "src/commands/community/suggestion.js", Some("suggestions"), Some(false), "MEMBER"),
    ("modmail", "src/commands/admin/modmail.js", Some("modmail"), Some(false), "MODERATOR"),
    ("application", "src/commands/community/application.js", Some("applications"), Some(false), "MEMBER"),
    ("sticky", "src/commands/admin/sticky.js", Some("sticky"), Some(false), "ADMIN"),
    ("afk", "src/commands/community/afk.js", Some("afk"), Some(true), "MEMBER"),
    ("antiraid", "src/commands/admin/antiraid.js", Some("antiraid"), Some(false), "ADMIN"),
    ("event", "src/commands/community/event.js", Some("events"), Some(true), "MEMBER"),
    ("rolepersist", "src/commands/admin/rolepersist.js", Some("rolepersist"), Some(false), "ADMIN"),
    ("boost", "src/commands/community/boost.js", Some("boosts"), Some(false), "MEMBER"),
    ("backup", "src/commands/admin/backup.js", Some("backups"), Some(false), "ADMIN"),
    ("confession", "src/commands/community/confession.js", Some("confessions"), Some(false), "MEMBER"),
    ("autothread", "src/commands/admin/autothread.js", Some("autothreads"), Some(false), "ADMIN"),
    ("voicerole", "src/commands/admin/voicerole.js", Some("voiceroles"), Some(false), "ADMIN"),
    ("bumps", "src/commands/community/bumps.js", Some("bumps"), Some(false), "MEMBER"),
    ("captcha", "src/commands/admin/captcha.js", Some("captcha"), Some(false), "ADMIN"),
    ("economy-admin", "src/commands/admin/economy-admin.js", Some("economy"), Some(false), "ADMIN"),
    ("embed", "src/commands/admin/embed.js", Some("embeds"), Some(true), "ADMIN"),
    ("emoji", "src/commands/admin/emoji.js", None, None, "ADMIN"),
    ("feed", "src/commands/admin/feed.js", Some("feeds"), Some(false), "ADMIN"),
    ("levelconfig", "src/commands/admin/levelconfig.js", Some("leveling"), Some(false), "ADMIN"),
    ("liverole", "src/commands/admin/liverole.js", Some("liverole"), Some(false), "ADMIN"),
    ("logging", "src/commands/admin/logging.js", Some("logging"), Some(false), "ADMIN"),
    ("module", "src/commands/admin/module.js", None, None, "ADMIN"),
    ("reactionrole", "src/commands/admin/reactionrole.js", Some("reactionroles"), Some(false), "ADMIN"),
    ("reminder", "src/commands/admin/reminder.js", Some("reminders"), Some(false), "ADMIN"),
    ("settings", "src/commands/admin/settings.js", None, None, "ADMIN"),
    ("starboard", "src/commands/admin/starboard.js", Some("starboard"), Some(false), "ADMIN"),
    ("stats", "src/commands/admin/stats.js", Some("stats"), Some(false), "ADMIN"),
    ("ticket-panel", "src/commands/admin/ticket-panel.js", Some("tickets"), Some(false), "ADMIN"),
    ("voice-hub", "src/commands/admin/voice-hub.js", Some("tempvoice"), Some(false), "ADMIN"),
    ("welcome", "src/commands/admin/welcome.js", Some("welcome"), Some(false), "ADMIN"),
    ("achievements", "src/commands/community/achievements.js", Some("achievements"), Some(true), "MEMBER"),
    ("birthday", "src/commands/community/birthday.js", Some("birthdays"), Some(false), "MEMBER"),
    ("giveaway", "src/commands/community/giveaway.js", Some("giveaways"), Some(true), "MODERATOR"),
    ("poll", "src/commands/community/poll.js", Some("polls"), Some(true), "MODERATOR"),
    ("ticket", "src/commands/community/ticket.js", Some("tickets"), Some(true), "MEMBER"),
    ("voice", "src/commands/community/voice.js", Some("tempvoice"), Some(true), "MEMBER"),
    ("balance", "src/commands/economy/balance.js", Some("economy"), Some(true), "MEMBER"),
    ("daily", "src/commands/economy/daily.js", Some("economy"), Some(true), "MEMBER"),
    ("inventory", "src/commands/economy/inventory.js", Some("economy"), Some(true), "MEMBER"),
    ("pay", "src/commands/economy/pay.js", Some("economy"), Some(true), "MEMBER"),
    ("pity", "src/commands/economy/pity.js", Some("economy"), Some(true), "MEMBER"),
    ("richest", "src/commands/economy/richest.js", Some("economy"), Some(true), "MEMBER"),
    ("shop", "src/commands/economy/shop.js", Some("economy"), Some(true), "MEMBER"),
    ("use", "src/commands/economy/use.js", Some("economy"), Some(true), "MEMBER"),
    ("work", "src/commands/economy/work.js", Some("economy"), Some(true), "MEMBER"),
    ("blackjack", "src/commands/games/blackjack.js", Some("economy"), Some(true), "MEMBER"),
    ("games", "src/commands/games/games.js", Some("games"), Some(true), "MEMBER"),
    ("roulette", "src/commands/games/roulette.js", Some("economy"), Some(true), "MEMBER"),
    ("scrabble", "src/commands/games/scrabble.js", Some("games"), Some(true), "MEMBER"),
    ("slots", "src/commands/games/slots.js", Some("economy"), Some(true), "MEMBER"),
    ("leaderboard", "src/commands/leveling/leaderboard.js", Some("leveling"), Some(true), "MEMBER"),
    ("rank", "src/commands/leveling/rank.js", Some("leveling"), Some(true), "MEMBER"),
    ("ban", "src/commands/moderation/ban.js", Some("moderation"), Some(true), "MODERATOR"),
    ("case", "src/commands/moderation/case.js", Some("moderation"), Some(true), "MODERATOR"),
    ("kick", "src/commands/moderation/kick.js", Some("moderation"), Some(true), "MODERATOR"),
    ("modconfig", "src/commands/moderation/modconfig.js", Some("moderation"), Some(false), "ADMIN"),
    ("purge", "src/commands/moderation/purge.js", Some("moderation"), Some(true), "MODERATOR"),
    ("slowmode", "src/commands/moderation/slowmode.js", Some("moderation"), Some(true), "MODERATOR"),
    ("softban", "src/commands/moderation/softban.js", Some("moderation"), Some(true), "MODERATOR"),
    ("timeout", "src/commands/moderation/timeout.js", Some("moderation"), Some(true), "MODERATOR"),
    ("unban", "src/commands/moderation/unban.js", Some("moderation"), Some(true), "MODERATOR"),
    ("unmute", "src/commands/moderation/unmute.js", Some("moderation"), Some(true), "MODERATOR"),
    ("warn", "src/commands/moderation/warn.js", Some("moderation"), Some(true), "MODERATOR"),
    ("search", "src/commands/search/search.js", Some("search"), Some(true), "MEMBER"),
    ("help", "src/commands/utility/help.js", None, None, "MEMBER"),
    ("info", "src/commands/utility/info.js", None, None, "MEMBER"),
    ("invites", "src/commands/utility/invites.js", Some("invites"), Some(true), "MEMBER"),
    ("owner", "src/commands/utility/owner.js", None, None, "MEMBER"),
    ("tag", "src/commands/utility/tag.js", Some("customcommands"), Some(false), "MEMBER"),
    ("translate", "src/commands/utility/translate.js", None, None, "MEMBER"),
    ("timestamp", "src/commands/utility/timestamp.js", None, None, "MEMBER"),
    ("timezone", "src/commands/utility/timezone.js", None, None, "MEMBER"),
];

/// EXPECTED_PLUGIN_MODULE of test/integration.test.js.
const EXPECTED_PLUGIN_MODULE: &[(&str, Option<&str>)] = &[
    ("moderation", Some("moderation")),
    ("automod", Some("automod")),
    ("logging", Some("logging")),
    ("welcome", Some("welcome")),
    ("levels", Some("leveling")),
    ("invites", Some("invites")),
    ("translation-fixes", Some("translation")),
    ("games-fixes", Some("games")),
    ("embeds", Some("embeds")),
    ("reminders", Some("reminders")),
    ("giveaways", Some("giveaways")),
    ("polls", Some("polls")),
    ("starboard", Some("starboard")),
    ("reaction-roles", Some("reactionroles")),
    ("tickets", Some("tickets")),
    ("temp-voice", Some("tempvoice")),
    ("stat-channels", Some("stats")),
    ("birthdays", Some("birthdays")),
    ("custom-commands", Some("customcommands")),
    ("economy", Some("economy")),
    ("achievements", Some("achievements")),
    ("automations", Some("automations")),
    ("captcha", Some("captcha")),
    ("feeds", Some("feeds")),
    ("liverole", Some("liverole")),
    ("counting", Some("counting")),
    ("autotime", Some("autotime")),
    ("suggestions", Some("suggestions")),
    ("modmail", Some("modmail")),
    ("applications", Some("applications")),
    ("sticky", Some("sticky")),
    ("afk", Some("afk")),
    ("antiraid", Some("antiraid")),
    ("events", Some("events")),
    ("role-persist", Some("rolepersist")),
    ("boosts", Some("boosts")),
    ("backups", Some("backups")),
    ("confessions", Some("confessions")),
    ("auto-threads", Some("autothreads")),
    ("voice-roles", Some("voiceroles")),
    ("bumps", Some("bumps")),
    ("dashboard-tasks", None),
    ("bot-joins", None),
];

/// Owners that are not plugins (CORE_OWNERS).
const CORE_OWNERS: &[&str] = &["foundation", "foundation-runtime", "foundation-utils"];

/// The one hook outside HOOK_ORDER: feat-games registers `games-fixes:numguess` with `Registry::on_boot` (see
/// rust/docs/api/feat-games.md); it sorts after every listed messageCreate hook.
const BOOT_REGISTERED_HOOKS: &[(HookPoint, &str)] = &[(HookPoint::MessageCreate, "games-fixes:numguess")];

fn level_of(name: &str) -> PermissionLevel {
    match name {
        "MEMBER" => PermissionLevel::Member,
        "MODERATOR" => PermissionLevel::Moderator,
        "ADMIN" => PermissionLevel::Admin,
        _ => PermissionLevel::Owner,
    }
}

fn effective(m: ModuleRef, plugin: Option<ModuleKey>) -> Option<ModuleKey> {
    match m {
        ModuleRef::Inherit => plugin,
        ModuleRef::Ungated => None,
        ModuleRef::Module(k) => Some(k),
    }
}

fn expect_none(problems: Vec<String>, what: &str) {
    assert!(problems.is_empty(), "{what} ({}):\n  {}", problems.len(), problems.join("\n  "));
}

// ── (1) The full registry ──────────────────────────────────────────────────────────────────────────────────────

#[test]
fn every_feature_crate_is_registered_once() {
    assert_eq!(FEATURES.len(), 27);
    let names: HashSet<&str> = FEATURES.iter().map(|(n, _)| *n).collect();
    assert_eq!(names.len(), 27, "a feature crate is listed twice");
    let groups: Value = serde_json::from_str(&std::fs::read_to_string(repo_root().join("rust/docs/groups.json")).expect("groups.json reads"))
        .expect("groups.json parses");
    let expected: HashSet<&str> = groups.as_object().expect("groups.json is an object").keys().map(String::as_str).collect();
    assert_eq!(names, expected, "FEATURES must be exactly the feature crates of rust/docs/groups.json");
}

#[test]
fn the_registry_matches_the_js_bot() {
    let registry = build_registry();
    assert_eq!(registry.commands().len(), 79);
    assert_eq!(registry.plugins().len(), 43);
    // Every command payload byte-identical to commands.json with its routing metadata, every plugin equal to
    // plugins.json, every persistent customId prefix and job type with exactly one handler.
    assert_registry_matches_golden(&registry);
    expect_none(persistent_handler_problems(&registry), "persistent handler problems");
    expect_none(registry.validate_commands(), "command validation problems");
}

#[test]
fn plugins_install_in_the_js_order() {
    // collectPlugins sorts src/plugins/*.js; collect_plugins sorts by id. Compare the manifests in that order with
    // plugins.json (which dump-golden.mjs wrote in the JS load order).
    let golden = load_golden("plugins.json");
    let golden: Vec<Value> = golden
        .as_array()
        .expect("plugins.json is an array")
        .iter()
        .map(|p| {
            let mut p = p.clone();
            p.as_object_mut().expect("plugin entry").remove("file");
            p
        })
        .collect();
    let collected = bot_core::plugin::collect_plugins(build_registry_plugins());
    assert!(collected.errors.is_empty(), "collectPlugins errors: {:?}", collected.errors);
    let ours: Vec<Value> = collected.plugins.iter().map(plugin_manifest).collect();
    assert_eq!(ours.len(), golden.len());
    for (i, (o, g)) in ours.iter().zip(&golden).enumerate() {
        if let Some(d) = json_diff(o, g, false) {
            panic!("plugin #{i} ({}) differs from plugins.json #{i} ({}): {d}", o["id"], g["id"]);
        }
    }
    // The install order is independent of the order the crates register in.
    let mut reversed = Registry::new();
    for (_, f) in FEATURES.iter().rev() {
        f(&mut reversed);
    }
    let again: Vec<&str> = bot_core::plugin::collect_plugins(take_plugins(reversed)).plugins.iter().map(|p| p.id).collect();
    let first: Vec<&str> = collected.plugins.iter().map(|p| p.id).collect();
    assert_eq!(again, first);

}

/// The plugins of a fresh registry (collect_plugins consumes them).
fn build_registry_plugins() -> Vec<bot_core::Plugin> {
    take_plugins(build_registry())
}

fn take_plugins(registry: Registry) -> Vec<bot_core::Plugin> {
    registry.into_parts().1
}

#[test]
fn commands_match_the_integration_table() {
    let registry = build_registry();
    let golden = load_golden("commands.json");
    let files: BTreeMap<String, String> = golden
        .as_array()
        .expect("commands.json is an array")
        .iter()
        .map(|c| (c["name"].as_str().unwrap_or_default().to_string(), c["file"].as_str().unwrap_or_default().to_string()))
        .collect();
    let mut problems = Vec::new();
    let declared: HashSet<&str> = registry.commands().iter().map(|c| c.name()).collect();
    let expected: HashSet<&str> = EXPECTED_COMMANDS.iter().map(|c| c.0).collect();
    for n in expected.difference(&declared) {
        problems.push(format!("/{n}: expected but not declared"));
    }
    for n in declared.difference(&expected) {
        problems.push(format!("/{n}: missing from EXPECTED_COMMANDS"));
    }
    assert_eq!(EXPECTED_COMMANDS.len(), 79);
    for &(name, file, module, gated, level) in EXPECTED_COMMANDS {
        let Some(def) = registry.command_named(name) else { continue };
        // A2: the JS file (the category is its folder).
        if files.get(name).map(String::as_str) != Some(file) {
            problems.push(format!("/{name}: commands.json file {:?} (expected {file})", files.get(name)));
        }
        let folder = file.rsplit('/').nth(1).unwrap_or_default();
        if def.category != folder {
            problems.push(format!("/{name}: category {} (expected {folder})", def.category));
        }
        // A4
        if def.module.map(|m| m.as_str()) != module {
            problems.push(format!("/{name}: module {:?} (expected {module:?})", def.module.map(|m| m.as_str())));
        }
        // A5: effective gating, core commands excluded.
        if let Some(g) = gated
            && def.module.is_some()
            && def.gated != g
        {
            problems.push(format!("/{name}: effectively gated={} (expected {g})", def.gated));
        }
        // A6
        if def.permission_level != level_of(level) {
            problems.push(format!("/{name}: level {:?} (expected {level})", def.permission_level));
        }
        let payload = def.payload();
        // A7: subcommandPermissions keys name real subcommands.
        let mut keys = HashSet::new();
        for opt in payload["options"].as_array().into_iter().flatten() {
            match opt["type"].as_u64() {
                Some(1) => {
                    keys.insert(opt["name"].as_str().unwrap_or_default().to_string());
                }
                Some(2) => {
                    for sub in opt["options"].as_array().into_iter().flatten().filter(|s| s["type"] == json!(1)) {
                        keys.insert(format!("{} {}", opt["name"].as_str().unwrap_or_default(), sub["name"].as_str().unwrap_or_default()));
                    }
                }
                _ => {}
            }
        }
        for (k, _) in &def.subcommand_permissions {
            if !keys.contains(k) {
                problems.push(format!("/{name}: subcommandPermissions key \"{k}\" is not a subcommand"));
            }
        }
        // A8: MODERATOR and ADMIN commands restrict default member permissions.
        if def.permission_level >= PermissionLevel::Moderator && !payload["default_member_permissions"].as_str().is_some_and(|s| !s.is_empty()) {
            problems.push(format!("/{name}: level {:?} without default_member_permissions", def.permission_level));
        }
    }
    // A9: every module has an existing config command of that module (or a core one).
    for m in MODULES.iter() {
        let cfg = m.config_command;
        let Some(cmd_name) = cfg.strip_prefix('/') else {
            problems.push(format!("{}: configCommand {cfg:?} is not \"/name\"", m.key));
            continue;
        };
        match registry.command_named(cmd_name) {
            None => problems.push(format!("{}: configCommand {cfg} does not exist", m.key)),
            Some(def) if def.module.is_some() && def.module != Some(m.key) => {
                problems.push(format!("{}: {cfg} belongs to module {:?}", m.key, def.module.map(|k| k.as_str())))
            }
            _ => {}
        }
    }
    expect_none(problems, "command table mismatches");
}

#[test]
fn hooks_components_jobs_and_owners_agree_with_bot_config() {
    let registry = build_registry();
    let plugins = registry.plugins();
    let plugin_ids: HashSet<&str> = plugins.iter().map(|p| p.id).collect();
    let mut problems = Vec::new();

    // E1 / E3: exactly the expected plugins, with their modules.
    let expected_ids: HashSet<&str> = EXPECTED_PLUGIN_MODULE.iter().map(|p| p.0).collect();
    for id in expected_ids.difference(&plugin_ids) {
        problems.push(format!("expected plugin {id} is not declared"));
    }
    for id in plugin_ids.difference(&expected_ids) {
        problems.push(format!("unexpected plugin {id}"));
    }
    for &(id, module) in EXPECTED_PLUGIN_MODULE {
        if let Some(p) = registry.plugin_named(id)
            && p.module.map(|m| m.as_str()) != module
        {
            problems.push(format!("{id}: module {:?} (expected {module:?})", p.module.map(|m| m.as_str())));
        }
    }

    // B1: every HOOK_ORDER entry is declared by exactly one plugin hook. B2: every hook is listed under its plugin.
    for point in HookPoint::ALL {
        for id in point.order() {
            let n = plugins.iter().flat_map(|p| p.hooks.iter()).filter(|h| h.event() == point && h.id() == *id).count();
            if n != 1 {
                problems.push(format!("{point}:{id} is declared {n} time(s)"));
            }
            // E4: every HOOK_ORDER owner is a plugin.
            let owner = id.split(':').next().unwrap_or_default();
            if !plugin_ids.contains(owner) {
                problems.push(format!("HOOK_ORDER.{point}: \"{id}\" has no plugin \"{owner}\""));
            }
        }
    }
    for p in plugins {
        for h in &p.hooks {
            if !h.event().order().contains(&h.id()) {
                problems.push(format!("{}: {}:{} is not in HOOK_ORDER", p.id, h.event(), h.id()));
            }
            if h.id() != p.id && !h.id().starts_with(&format!("{}:", p.id)) {
                problems.push(format!("{}: {}:{} must be \"{}\" or start with \"{}:\"", p.id, h.event(), h.id(), p.id, p.id));
            }
            // B3: timeouts within the JS ceiling.
            let max = if h.event() == HookPoint::GuildReady && h.id() == "liverole" { 120_000 } else { 60_000 };
            if let Some(t) = h.timeout_ms()
                && !(1..=max).contains(&t)
            {
                problems.push(format!("{}: {}:{} timeoutMs {t} is not in 1..{max}", p.id, h.event(), h.id()));
            }
        }
        // C1: every component is a registered persistent prefix of its owner, with that type.
        for c in &p.components {
            let tag = format!("{}: {} \"{}\"", p.id, c.kind.as_str(), c.prefix);
            match bot_config::prefix_info(c.prefix) {
                None => problems.push(format!("{tag}: prefix is not in CUSTOM_ID_PREFIXES")),
                Some(info) => {
                    if info.kind != CustomIdKind::Persistent {
                        problems.push(format!("{tag}: registry kind is {}", info.kind.as_str()));
                    }
                    if info.owner != p.id {
                        problems.push(format!("{tag}: registry owner is {}", info.owner));
                    }
                    if !info.types.contains(&c.kind.component_type()) {
                        problems.push(format!("{tag}: registry types do not include it"));
                    }
                }
            }
        }
    }

    // C6: collector and field prefixes are never registered.
    for info in CUSTOM_ID_PREFIXES.iter().filter(|i| i.kind != CustomIdKind::Persistent) {
        if plugins.iter().flat_map(|p| p.components.iter()).any(|c| c.prefix == info.prefix) {
            problems.push(format!("{} \"{}\" is registered", info.kind.as_str(), info.prefix));
        }
    }
    // E4: persistent prefixes belong to a plugin or a core registration (noop).
    for info in CUSTOM_ID_PREFIXES.iter().filter(|i| i.kind == CustomIdKind::Persistent) {
        let core = CORE_OWNERS.contains(&info.owner) && info.prefix == "noop";
        if !plugin_ids.contains(info.owner) && !core {
            problems.push(format!("CUSTOM_ID_PREFIXES[\"{}\"]: owner \"{}\" is neither a plugin nor a core registration", info.prefix, info.owner));
        }
    }

    // D1: every job type has exactly one handler, declared by its owner. D2 holds by construction (JobType is the
    // registry).
    for job in JobType::ALL {
        let decls: Vec<&str> = plugins.iter().filter(|p| p.jobs.iter().any(|j| j.0 == job)).map(|p| p.id).collect();
        let count: usize = plugins.iter().map(|p| p.jobs.iter().filter(|j| j.0 == job).count()).sum();
        if count != 1 {
            problems.push(format!("{job}: {count} handler declaration(s)"));
        } else if decls[0] != job.info().owner {
            problems.push(format!("{job}: declared by {}, owner is {}", decls[0], job.info().owner));
        }
    }

    // F1 / K8: one provider per reward kind, from the right module (levels: xp, economy: coins).
    for (kind, plugin) in [(RewardKind::Xp, "levels"), (RewardKind::Coins, "economy")] {
        let providers: Vec<&str> = plugins.iter().filter(|p| p.provides.iter().any(|(k, _)| *k == kind)).map(|p| p.id).collect();
        if providers != [plugin] {
            problems.push(format!("{}: provided by {providers:?} (expected {plugin})", kind.as_str()));
        }
        if registry.plugin_named(plugin).and_then(|p| p.module) != Some(kind.module()) {
            problems.push(format!("{}: {plugin} is not of module {}", kind.as_str(), kind.module()));
        }
    }
    // F3: every internal hook point has hooks.
    for point in bot_config::INTERNAL_HOOK_POINTS {
        if point.order().is_empty() {
            problems.push(format!("internal hook point {point} has no hooks"));
        }
    }
    expect_none(problems, "registry problems");
}

#[test]
fn the_architecture_registry_names_every_module_flag_job_type_and_plugin() {
    // H3 of test/integration.test.js, against bot-config's tables and the Rust plugin ids.
    let doc = std::fs::read_to_string(repo_root().join("docs/claude/architecture.md")).expect("architecture.md reads");
    let in_ticks = |s: &str| doc.contains(&format!("`{s}`"));
    let mut problems = Vec::new();
    for m in MODULES.iter() {
        if !in_ticks(m.key.as_str()) {
            problems.push(format!("module key `{}`", m.key));
        }
        if !in_ticks(m.flag) {
            problems.push(format!("module flag `{}` ({})", m.flag, m.key));
        }
    }
    for job in JobType::ALL {
        if !in_ticks(job.as_str()) {
            problems.push(format!("job type `{job}`"));
        }
    }
    for p in build_registry().plugins() {
        if !in_ticks(&format!("src/plugins/{}.js", p.id)) {
            problems.push(format!("plugin file `src/plugins/{}.js`", p.id));
        }
    }
    expect_none(problems, "missing from the architecture registry");
    // The bot-config module table is the JS one (config.json, dumped from src/config/modules.js).
    let golden = load_golden("config.json");
    let js_modules = golden["modules"]["MODULES"].as_object().expect("config.json modules.MODULES");
    let rust_keys: Vec<&str> = MODULES.iter().map(|m| m.key.as_str()).collect();
    let js_keys: Vec<&str> = js_modules.keys().map(String::as_str).collect();
    assert_eq!(rust_keys, js_keys, "module keys and their order");
    for m in MODULES.iter() {
        let js = &js_modules[m.key.as_str()];
        assert_eq!(js["configCommand"], json!(m.config_command), "{}: configCommand", m.key);
        assert_eq!(js["label"], json!(m.label), "{}: label", m.key);
    }
}

#[tokio::test]
async fn the_install_matches_the_declarations() {
    // Z1-Z5 of test/integration.test.js: one install into an offline app.
    let t = TestApp::new();
    let registry = build_registry();
    let declared_hooks: usize = registry.plugins().iter().map(|p| p.hooks.len()).sum();
    let declared_components: usize = registry.plugins().iter().map(|p| p.components.len()).sum();
    let declared_jobs: usize = registry.plugins().iter().map(|p| p.jobs.len()).sum();
    let declared_providers: usize = registry.plugins().iter().map(|p| p.provides.len()).sum();
    let decls: Vec<(HookPoint, &'static str, Option<ModuleKey>, bool)> = registry
        .plugins()
        .iter()
        .flat_map(|p| p.hooks.iter().map(move |h| (h.event(), h.id(), effective(h.module(), p.module), h.always())))
        .collect();

    let (report, lines) = bot_util::logger::capture_sync(|| t.install(registry));
    // Z1
    assert!(report.plugins.errors.is_empty(), "install errors: {:?}", report.plugins.errors);
    assert!(report.command_errors.is_empty(), "command errors: {:?}", report.command_errors);
    let errors: Vec<&String> = lines.iter().filter(|(l, _)| *l == bot_util::logger::Level::Error).map(|(_, s)| s).collect();
    assert!(errors.is_empty(), "the install logged errors: {errors:?}");
    // Z2
    assert_eq!(report.plugins.plugins, 43);
    assert_eq!(report.commands, 79);
    assert_eq!(
        (report.plugins.hooks, report.plugins.components, report.plugins.jobs, report.plugins.providers),
        (declared_hooks, declared_components, declared_jobs, declared_providers)
    );
    // Z3: installed hooks run in exactly the HOOK_ORDER order, with the declared module and always flag.
    let mut problems = Vec::new();
    for point in HookPoint::ALL {
        let installed = t.hooks().list(point);
        let mut expected: Vec<&str> = point.order().to_vec();
        expected.extend(BOOT_REGISTERED_HOOKS.iter().filter(|(p, _)| *p == point).map(|(_, id)| *id));
        let ids: Vec<&str> = installed.iter().map(|h| h.id.as_str()).collect();
        if ids != expected {
            problems.push(format!("{point}: installed {ids:?}, expected {expected:?}"));
            continue;
        }
        for h in &installed {
            if BOOT_REGISTERED_HOOKS.contains(&(point, h.id.as_str())) {
                continue;
            }
            if h.plugin_id != h.id.split(':').next().unwrap_or_default() {
                problems.push(format!("{point}:{}: pluginId {}", h.id, h.plugin_id));
            }
            match decls.iter().find(|d| d.0 == point && d.1 == h.id) {
                None => problems.push(format!("{point}:{}: no declaration", h.id)),
                Some(d) => {
                    if h.module != d.2 {
                        problems.push(format!("{point}:{}: module {:?} (declared {:?})", h.id, h.module, d.2));
                    }
                    if h.always != d.3 {
                        problems.push(format!("{point}:{}: always {} (declared {})", h.id, h.always, d.3));
                    }
                }
            }
        }
    }
    expect_none(problems, "installed hook problems");
    // Z4: the component maps hold exactly the persistent prefixes.
    let maps = [
        (bot_config::ComponentType::Button, t.components().buttons.prefixes()),
        (bot_config::ComponentType::Select, t.components().selects.prefixes()),
        (bot_config::ComponentType::Modal, t.components().modals.prefixes()),
    ];
    for (ty, got) in maps {
        let mut expected: Vec<String> = CUSTOM_ID_PREFIXES
            .iter()
            .filter(|i| i.kind == CustomIdKind::Persistent && i.types.contains(&ty))
            .map(|i| i.prefix.to_string())
            .collect();
        expected.sort();
        let mut got = got;
        got.sort();
        assert_eq!(got, expected, "{} handlers", ty.as_str());
    }
    // Z5: the reward providers are registered.
    assert!(t.rewards().has_provider(RewardKind::Xp));
    assert!(t.rewards().has_provider(RewardKind::Coins));
    let on = bot_db::GuildSettings { guild_id: "1".into(), xp_enabled: true, economy_enabled: true, ..Default::default() };
    let off = bot_db::GuildSettings { guild_id: "1".into(), xp_enabled: false, economy_enabled: false, ..Default::default() };
    assert!(t.rewards().is_reward_available(RewardKind::Xp, Some(&on)));
    assert!(!t.rewards().is_reward_available(RewardKind::Xp, Some(&off)));
    assert!(t.rewards().is_reward_available(RewardKind::Coins, Some(&on)));
    assert!(!t.rewards().is_reward_available(RewardKind::Coins, None));
    // The commands are loaded.
    assert_eq!(t.commands().all().len(), 79);
}

// ── (3) The deploy hash ────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn the_deploy_hash_equals_the_js_hash() {
    let payloads = build_registry().payloads();
    for (client, guild) in [("123456789012345678", None), ("123456789012345678", Some("900000000000000077"))] {
        let mut args = vec!["deploy-hash", client];
        if let Some(g) = guild {
            args.push(g);
        }
        let js = run_node("rust/fixtures/gen/bot-bin-js.mjs", &args);
        assert!(js.get("error").is_none(), "the JS side failed: {js}");
        let rust = compute_deploy_hash(client, guild, &payloads);
        assert_eq!(json!(rust), js["hash"], "deploy hash for guild {guild:?}");
        assert_eq!(js["hash"], js["unsortedHash"]);
        // The payload set is the JS one, and the JS sort order is the Rust one.
        let mut sorted = payloads.clone();
        bot_core::command::sort_payloads(&mut sorted);
        let names: Vec<Value> = sorted.iter().map(|p| p["name"].clone()).collect();
        assert_eq!(Value::Array(names), js["sortedNames"]);
    }
}

#[tokio::test]
async fn the_first_rust_start_does_not_redeploy() {
    // The JS bot wrote DEPLOY_HASH_FILE: the Rust auto-deploy finds it unchanged and never reaches Discord (the
    // token is fake and the client id would be refused).
    let client = "123456789012345678";
    let js = run_node("rust/fixtures/gen/bot-bin-js.mjs", &["deploy-hash", client]);
    let dir = std::env::temp_dir().join(format!("bot-deploy-hash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join(".deploy-hash");
    std::fs::write(&file, js["hash"].as_str().expect("hash")).expect("hash file writes");
    let cfg = DeployConfig { token: Some("not-a-token".into()), client_id: Some(client.into()), guild_id: None, hash_file: file.clone() };
    let (result, lines) = bot_util::logger::capture(auto_deploy_commands(&cfg, build_registry().payloads())).await;
    assert_eq!(result, AutoDeployResult::Unchanged, "{lines:?}");
    assert!(lines.iter().any(|(_, l)| l.contains("Commands unchanged — skipping deploy.")));
    // The file keeps the JS format (the bare hex digest), and a different target is a change.
    assert_eq!(std::fs::read_to_string(&file).expect("reads"), js["hash"].as_str().expect("hash"));
    let other = compute_deploy_hash(client, Some("900000000000000077"), &build_registry().payloads());
    assert_ne!(json!(other), js["hash"]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── .env ───────────────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn dotenv_parses_like_the_js_dotenv() {
    let sample = repo_root().join("rust/fixtures/golden/bot/dotenv-sample.env");
    let js = run_node("rust/fixtures/gen/bot-bin-js.mjs", &["dotenv-parse", sample.to_str().expect("utf-8 path")]);
    assert_eq!(js["version"], json!("16.6.1"));
    let text = String::from_utf8(std::fs::read(&sample).expect("sample reads")).expect("utf-8");
    let ours = bot::dotenv::parse(&text);
    let theirs: BTreeMap<String, String> = js["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| (e[0].as_str().unwrap_or_default().to_string(), e[1].as_str().unwrap_or_default().to_string()))
        .collect();
    let ours_sorted: BTreeMap<String, String> = ours.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(ours_sorted, theirs);
    // No $VAR expansion (dotenvy would expand ${HOME}).
    assert_eq!(ours["DOLLAR"], "pa$$word${HOME}");
    assert_eq!(ours["DOUBLE"], "double\nnewline\rcr");
}

#[test]
fn dotenv_options_and_populate() {
    let env = |k: &str| match k {
        "DOTENV_CONFIG_PATH" => Some("config/.env.local".to_string()),
        "DOTENV_CONFIG_OVERRIDE" => Some("false".to_string()),
        _ => None,
    };
    // Boolean("false") is true.
    let o = bot::dotenv::options(env, &[]);
    assert_eq!(o, bot::dotenv::Options { path: Some("config/.env.local".into()), override_existing: true });
    // An argument wins over the variable.
    let o = bot::dotenv::options(env, &["bot".into(), "dotenv_config_path=other.env".into()]);
    assert_eq!(o.path.as_deref(), Some("other.env"));
    let o = bot::dotenv::options(|_| None, &[]);
    assert_eq!(o, bot::dotenv::Options::default());

    let dir = std::env::temp_dir().join(format!("bot-dotenv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join(".env"), "A=from-file\nB=b\n").expect("writes");
    let (set, err) = bot::dotenv::plan(&dir, &o, |k| k == "A");
    assert!(err.is_none());
    assert_eq!(set, vec![("B".to_string(), "b".to_string())], "an existing variable wins");
    let over = bot::dotenv::Options { override_existing: true, ..Default::default() };
    let (set, _) = bot::dotenv::plan(&dir, &over, |k| k == "A");
    assert_eq!(set.len(), 2);
    let (set, err) = bot::dotenv::plan(&dir.join("missing"), &o, |_| false);
    assert!(set.is_empty() && err.is_some(), "a missing .env is not fatal");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── index.js helpers ───────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn health_port_parses_like_number() {
    assert_eq!(parse_health_port(None), Ok(8081));
    assert_eq!(parse_health_port(Some("")), Ok(8081));
    assert_eq!(parse_health_port(Some(" 9000 ")), Ok(9000));
    assert_eq!(parse_health_port(Some("0x1F91")), Ok(8081));
    assert_eq!(parse_health_port(Some("9000.0")), Ok(9000));
    assert!(parse_health_port(Some("abc")).is_err());
    assert!(parse_health_port(Some("70000")).is_err());
    assert!(parse_health_port(Some("80.5")).is_err());
}

#[test]
fn env_paths_and_required_keys() {
    let env = Env { root: "/srv/bot".into(), ..Default::default() };
    assert_eq!(env.missing(), vec!["DISCORD_TOKEN", "CLIENT_ID", "DATABASE_URL", "REDIS_URL"]);
    assert_eq!(env.state_dir(), std::path::PathBuf::from("/srv/bot").join("state"));
    assert_eq!(env.deploy_config().hash_file, std::path::PathBuf::from("/srv/bot").join(".deploy-hash"));
    let env = Env {
        root: "/srv/bot".into(),
        discord_token: Some("t".into()),
        client_id: Some("1".into()),
        redis_url: Some("redis://x".into()),
        state_dir: Some("/var/state".into()),
        deploy_hash_file: Some("/data/.deploy-hash".into()),
        ..Default::default()
    };
    assert_eq!(env.missing(), vec!["DATABASE_URL"]);
    assert_eq!(env.state_dir(), std::path::PathBuf::from("/var/state"));
    assert_eq!(env.deploy_config().hash_file, std::path::PathBuf::from("/data/.deploy-hash"));
}

#[tokio::test]
async fn readiness_follows_the_lifecycle() {
    let lc = Lifecycle::new();
    let r = lc.readiness().await.expect("no error");
    assert!(!r.ready);
    assert_eq!(r.details["reason"], json!("starting"));
    lc.set_brake_until(1_767_225_600_000.0);
    lc.set_phase(Phase::BrakeDelay);
    let r = lc.readiness().await.expect("no error");
    assert_eq!(r.details["reason"], json!("brake-delay"));
    assert_eq!(r.details["until"], json!("2026-01-01T00:00:00.000Z"));
    // Spawned without a client is still starting.
    lc.set_phase(Phase::Spawned);
    assert_eq!(lc.readiness().await.expect("no error").details["reason"], json!("starting"));
    let (_, lines) = bot_util::logger::capture(async { lc.park("4014", None) }).await;
    assert_eq!(lc.parked().as_deref(), Some("4014"));
    assert!(lines[0].1.contains("PARKED: Discord closed the gateway with 4014 (Disallowed intents). Enable the privileged intents"));
    assert!(lines[1].1.contains("Not retrying. /healthz stays 200 and /readyz returns 503 \"parked:4014\" until the bot is restarted."));
    assert_eq!(lc.readiness().await.expect("no error").details["reason"], json!("parked:4014"));
    // A second park keeps the first reason.
    lc.park("missing-env", Some("x".into()));
    assert_eq!(lc.parked().as_deref(), Some("4014"));

    assert!(readiness_of(&[true, true]).ready);
    assert_eq!(readiness_of(&[true, true]).details["shards"], json!(2));
    let r = readiness_of(&[true, false]);
    assert!(!r.ready);
    assert_eq!(r.details["reason"], json!("shard-not-ready"));
    assert!(!readiness_of(&[]).ready);
    assert_eq!(park_text("4004"), "Discord closed the gateway with 4004 (Authentication failed). Discord rejected DISCORD_TOKEN. Reset the token in the Developer Portal (Bot → Reset Token), put the new one in DISCORD_TOKEN and restart the bot.");
}

#[test]
fn intents_match_bot_js() {
    use serenity::all::GatewayIntents as I;
    let all = bot_core::client::intents(true);
    for i in [
        I::GUILDS,
        I::GUILD_MEMBERS,
        I::GUILD_MODERATION,
        I::GUILD_EMOJIS_AND_STICKERS,
        I::GUILD_INTEGRATIONS,
        I::GUILD_WEBHOOKS,
        I::GUILD_INVITES,
        I::GUILD_VOICE_STATES,
        I::GUILD_SCHEDULED_EVENTS,
        I::GUILD_PRESENCES,
        I::GUILD_MESSAGES,
        I::GUILD_MESSAGE_REACTIONS,
        I::DIRECT_MESSAGES,
        I::MESSAGE_CONTENT,
    ] {
        assert!(all.contains(i), "{i:?}");
    }
    assert_eq!(all.bits().count_ones(), 14);
    assert!(!bot_core::client::intents(false).contains(I::GUILD_PRESENCES));
}

#[test]
fn commands_load_in_the_js_walkdir_order() {
    let golden: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/golden/commands.json")).unwrap()).unwrap();
    let want: Vec<String> = golden.iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
    let got: Vec<String> = build_registry().commands().iter().map(|c| c.name().to_string()).collect();
    assert_eq!(got, want);
    let (commands, plugins) = build_registry().into_parts();
    assert_eq!(commands.len(), 79);
    assert_eq!(plugins.len(), 43);
}
