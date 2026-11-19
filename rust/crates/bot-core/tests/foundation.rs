//! test/foundation.test.js (permission levels, the rewards bus) plus the embed factories and the golden helpers every
//! feature crate calls.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bot_config::{JobType, ModuleKey, PermissionLevel};
use bot_core::embed_builder::{apply_guild_color, error_embed, guild_embed, neutral_embed, success_embed, warn_embed};
use bot_core::hooks::events::{ChannelDelete, MessageDelete, MessageDeleteBulk};
use bot_core::permission_checker::{MemberAccess, has_permission_level, resolve_permission_level};
use bot_core::rewards::{RewardArgs, RewardKind, RewardProvider, RewardResult, provider_module};
use bot_core::scheduler::{JobOutcome, job_handler};
use bot_core::settings::MemorySettingsStore;
use bot_core::testing::{TestApp, assert_commands_match_golden, assert_plugin_matches_golden, load_golden};
use bot_core::{App, BotResult, CommandDef, Hook, Plugin, Registry, SlashCommand};
use serde_json::json;
use serenity::all::{GuildId, Permissions};

fn member(id: &str, owner: &str, perms: Permissions, roles: &[&str]) -> MemberAccess {
    MemberAccess { user_id: id.into(), guild_owner_id: Some(owner.into()), permissions: Some(perms), role_ids: roles.iter().map(|r| r.to_string()).collect() }
}

#[test]
fn resolve_permission_level_like_the_js() {
    let mut settings = MemorySettingsStore::new_row("g", serde_json::Map::new()).unwrap();
    settings.mod_role_ids = vec!["mod-role".into()];
    let s = Some(&settings);
    let lvl = |m: &MemberAccess, s: Option<&bot_db::GuildSettings>| resolve_permission_level(Some(m), s, None);
    assert_eq!(lvl(&member("5", "5", Permissions::empty(), &[]), s), PermissionLevel::Admin, "guild owner");
    assert_eq!(lvl(&member("10", "99", Permissions::ADMINISTRATOR, &[]), s), PermissionLevel::Admin);
    assert_eq!(lvl(&member("10", "99", Permissions::MANAGE_GUILD, &[]), s), PermissionLevel::Admin);
    assert_eq!(lvl(&member("10", "99", Permissions::MODERATE_MEMBERS, &[]), s), PermissionLevel::Moderator);
    assert_eq!(lvl(&member("10", "99", Permissions::BAN_MEMBERS, &[]), s), PermissionLevel::Moderator);
    assert_eq!(lvl(&member("10", "99", Permissions::empty(), &["mod-role"]), s), PermissionLevel::Moderator, "mod role");
    assert_eq!(lvl(&member("10", "99", Permissions::empty(), &["other"]), s), PermissionLevel::Member);
    assert_eq!(lvl(&member("10", "99", Permissions::KICK_MEMBERS, &[]), None), PermissionLevel::Moderator, "null settings");
    assert_eq!(lvl(&member("10", "99", Permissions::empty(), &["mod-role"]), None), PermissionLevel::Member);
    assert_eq!(lvl(&member("10", "99", Permissions::SEND_MESSAGES, &[]), s), PermissionLevel::Member);
    assert_eq!(resolve_permission_level(None, s, None), PermissionLevel::Member);
    assert_eq!(resolve_permission_level(Some(&member("1234", "99", Permissions::empty(), &[])), s, Some("1234")), PermissionLevel::Owner);
    assert_eq!(resolve_permission_level(Some(&member("1235", "99", Permissions::empty(), &[])), s, Some("1234")), PermissionLevel::Member);
    assert!(has_permission_level(Some(&member("10", "99", Permissions::KICK_MEMBERS, &[])), PermissionLevel::Moderator, s, None));
    assert!(!has_permission_level(Some(&member("10", "99", Permissions::KICK_MEMBERS, &[])), PermissionLevel::Admin, s, None));
}

#[test]
fn embed_factories_match_embed_builder_js() {
    let e = serde_json::to_value(error_embed("Server Only", Some("This command can only be used in a server."), None)).unwrap();
    assert_eq!(e["title"], json!("❌  Server Only"));
    assert_eq!(e["color"], json!(0xed4245));
    assert!(e.get("timestamp").is_some());
    let w = serde_json::to_value(warn_embed("Module Disabled", None, Some("foot"))).unwrap();
    assert_eq!(w["title"], json!("⚠️  Module Disabled"));
    assert_eq!(w["footer"]["text"], json!("foot"));
    assert!(w.get("description").is_none(), "an empty description is left out");
    let s = serde_json::to_value(success_embed(&"x".repeat(300), Some(""), None)).unwrap();
    assert_eq!(s["title"].as_str().unwrap().encode_utf16().count(), 256, "clamped");
    assert!(s.get("description").is_none());
    let n = serde_json::to_value(neutral_embed(None, Some("d"), None)).unwrap();
    assert!(n.get("title").is_none());
    assert_eq!(n["color"], json!(0x99aab5));
    let mut row = MemorySettingsStore::new_row("g", serde_json::Map::new()).unwrap();
    row.embed_color = Some(0x123456);
    assert_eq!(serde_json::to_value(guild_embed(Some(&row), Some("t"), None, None)).unwrap()["color"], json!(0x123456));
    row.embed_color = Some(0x1000000);
    assert_eq!(serde_json::to_value(apply_guild_color(neutral_embed(None, None, None), Some(&row))).unwrap()["color"], json!(0x99aab5), "out of range is ignored");
    assert_eq!(serde_json::to_value(guild_embed(None, None, None, None)).unwrap()["color"], json!(0x5865f2));
}

struct Coins {
    calls: Mutex<Vec<(String, RewardArgs)>>,
}

#[async_trait]
impl RewardProvider for Coins {
    async fn grant(&self, _: &App, args: RewardArgs) -> BotResult<RewardResult> {
        let broke = args.user_id == "broke";
        self.calls.lock().unwrap().push(("grant".into(), args));
        Ok(if broke { RewardResult::fail("module-disabled") } else { RewardResult::ok(Some(105)) })
    }
    async fn take(&self, _: &App, args: RewardArgs) -> BotResult<RewardResult> {
        self.calls.lock().unwrap().push(("take".into(), args));
        anyhow::bail!("provider exploded")
    }
}

#[tokio::test]
async fn rewards_bus() {
    let t = TestApp::new();
    let g = GuildId::new(1);
    assert_eq!(RewardKind::ALL.map(|k| k.as_str()), ["xp", "coins"]);
    assert_eq!(provider_module("coins"), Some(ModuleKey::Economy));
    assert_eq!(provider_module("xp"), Some(ModuleKey::Leveling));
    assert_eq!(provider_module("gems"), None);
    let bus = t.rewards();
    assert!(!bus.has_provider(RewardKind::Coins));
    let mut on = MemorySettingsStore::new_row("1", serde_json::Map::new()).unwrap();
    on.economy_enabled = true;
    assert!(!bus.is_reward_available(RewardKind::Coins, Some(&on)), "no provider yet");
    assert_eq!(bus.grant(&t, RewardKind::Coins, RewardArgs::new(g, "1", 5)).await, RewardResult::fail("unavailable"));
    assert_eq!(bus.take(&t, RewardKind::Coins, RewardArgs::new(g, "1", 5)).await, RewardResult::fail("unavailable"));

    let coins = Arc::new(Coins { calls: Mutex::new(Vec::new()) });
    bus.register_provider(RewardKind::Coins, coins.clone(), Some("economy")).unwrap();
    let err = bus.register_provider(RewardKind::Coins, coins.clone(), Some("again")).unwrap_err();
    assert!(err.contains("already registered by economy"));
    assert!(bus.has_provider(RewardKind::Coins));
    assert!(bus.is_reward_available(RewardKind::Coins, Some(&on)));
    let mut off = on.clone();
    off.economy_enabled = false;
    assert!(!bus.is_reward_available(RewardKind::Coins, Some(&off)));
    assert!(!bus.is_reward_available(RewardKind::Coins, None));
    assert!(!bus.is_reward_available(RewardKind::Xp, Some(&on)), "xp has no provider");

    assert_eq!(bus.grant(&t, RewardKind::Coins, RewardArgs::new(g, "1", 5).reason("test")).await, RewardResult::ok(Some(105)));
    {
        let calls = coins.calls.lock().unwrap();
        assert_eq!(calls[0].1.reason, "test");
        assert!(calls[0].1.member.is_none() && calls[0].1.channel_id.is_none());
    }
    assert_eq!(bus.grant(&t, RewardKind::Coins, RewardArgs::new(g, "broke", 5)).await, RewardResult::fail("module-disabled"));
    assert_eq!(bus.take(&t, RewardKind::Coins, RewardArgs::new(g, "1", 5)).await, RewardResult::fail("error"), "provider errors are caught");
    for amount in [0, -1, 1_000_000_001] {
        assert_eq!(bus.grant(&t, RewardKind::Coins, RewardArgs::new(g, "1", amount)).await, RewardResult::fail("invalid-amount"));
    }
    assert_eq!(bus.grant(&t, RewardKind::Coins, RewardArgs::new(g, "1", 1_000_000_000)).await, RewardResult::ok(Some(105)));
    assert_eq!(serde_json::to_value(RewardResult::fail("unavailable")).unwrap(), json!({ "ok": false, "reason": "unavailable" }));
    assert_eq!(serde_json::to_value(RewardResult::ok(Some(5))).unwrap(), json!({ "ok": true, "total": 5 }));
}

#[test]
fn the_golden_command_helper_accepts_the_js_commands_and_rejects_differences() {
    let golden = load_golden("commands.json");
    let mut reg = Registry::new();
    let mut names = Vec::new();
    for c in golden.as_array().unwrap().iter().take(12) {
        let data = SlashCommand::from_payload(&c["payload"]).unwrap();
        let module = c["module"].as_str().map(|m| ModuleKey::from_name(m).unwrap());
        let category: &'static str = Box::leak(c["category"].as_str().unwrap().to_string().into_boxed_str());
        let mut def = CommandDef::new(category, data, module, |_| async { Ok(()) })
            .gated(c["gated"].as_bool().unwrap())
            .permission_level(PermissionLevel::from_u8(c["permissionLevel"].as_u64().unwrap() as u8).unwrap());
        for (k, v) in c["subcommandPermissions"].as_object().unwrap() {
            def = def.subcommand_permission(k, PermissionLevel::from_u8(v.as_u64().unwrap() as u8).unwrap());
        }
        if c["allowInDMs"] == json!(true) {
            def = def.allow_in_dms();
        }
        if c["hasAutocomplete"] == json!(true) {
            def = def.autocomplete(|_| async { Ok(()) });
        }
        names.push(c["name"].as_str().unwrap().to_string());
        reg.command(def);
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_commands_match_golden(&reg, &refs);

    // A changed description fails.
    let mut bad = Registry::new();
    let first = &golden[0];
    let mut data = SlashCommand::from_payload(&first["payload"]).unwrap();
    data.description.push('!');
    bad.command(CommandDef::new("admin", data, Some(ModuleKey::Achievements), |_| async { Ok(()) }).gated(false).permission_level(PermissionLevel::Admin));
    let name = first["name"].as_str().unwrap();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_commands_match_golden(&bad, &[name])));
    assert!(r.is_err());
}

#[test]
fn the_golden_plugin_helper_checks_hooks_components_and_jobs() {
    let polls = || {
        Plugin::new("polls", Some(ModuleKey::Polls))
            .hook(Hook::<MessageDelete>::new("polls", |_| async { Ok(()) }).always().filter(|_| true))
            .hook(Hook::<MessageDeleteBulk>::new("polls", |_| async { Ok(()) }).always())
            .hook(Hook::<ChannelDelete>::new("polls", |_| async { Ok(()) }).always())
            .button("poll", |_| async { Ok(()) })
            .select("poll-select", |_| async { Ok(()) })
            .modal("poll-modal", |_| async { Ok(()) })
            .job(JobType::PollEnd, job_handler(|_, _| async { Ok(JobOutcome::Done) }))
    };
    let mut reg = Registry::new();
    reg.plugin(polls());
    assert_plugin_matches_golden(&reg, "polls");
    // Missing `always` on one hook fails.
    let mut bad = Registry::new();
    bad.plugin(
        Plugin::new("polls", Some(ModuleKey::Polls))
            .hook(Hook::<MessageDelete>::new("polls", |_| async { Ok(()) }).filter(|_| true))
            .hook(Hook::<MessageDeleteBulk>::new("polls", |_| async { Ok(()) }).always())
            .hook(Hook::<ChannelDelete>::new("polls", |_| async { Ok(()) }).always())
            .button("poll", |_| async { Ok(()) })
            .select("poll-select", |_| async { Ok(()) })
            .modal("poll-modal", |_| async { Ok(()) })
            .job(JobType::PollEnd, job_handler(|_, _| async { Ok(JobOutcome::Done) })),
    );
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_plugin_matches_golden(&bad, "polls"))).is_err());
    // An explicit `module: null` differs from an inherited module.
    let mut ungated = Registry::new();
    ungated.plugin(polls().component_module(bot_core::ModuleRef::Ungated));
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_plugin_matches_golden(&ungated, "polls"))).is_err());
}

/// `resolvePermissionLevel` reads `member.roles.cache`: @everyone (the guild id) is in it and roles the guild no
/// longer has are not, so a modRoleIds entry for @everyone counts and one for a deleted role does not.
#[tokio::test]
async fn member_access_from_the_cache_lists_roles_like_roles_cache() {
    const G: &str = "900000000000000777";
    let guild = bot_core::testing::FakeGuild::new(G, "600000000000000001")
        .everyone(Permissions::VIEW_CHANNEL)
        .role("800000000000000001", 1, Permissions::empty())
        .member("600000000000000005", &["800000000000000001", "800000000000000099"]);
    let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild]);
    let d = t.discord().expect("discord");
    let gid = GuildId::new(G.parse().unwrap());
    let m = d.cache.guild(gid).unwrap().members.get(&serenity::all::UserId::new(600000000000000005)).cloned().unwrap();
    let access = MemberAccess::from_cache(Some(&d.cache), gid, &m);
    assert_eq!(access.role_ids, vec![G.to_string(), "800000000000000001".to_string()]);
    let mut settings = MemorySettingsStore::new_row(G, serde_json::Map::new()).unwrap();
    settings.mod_role_ids = vec!["800000000000000099".into()];
    assert_eq!(resolve_permission_level(Some(&access), Some(&settings), None), PermissionLevel::Member, "a deleted role");
    settings.mod_role_ids = vec![G.into()];
    assert_eq!(resolve_permission_level(Some(&access), Some(&settings), None), PermissionLevel::Moderator, "@everyone");
}
