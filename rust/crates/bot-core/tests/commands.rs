//! commandLoader.js: payload building (byte-identical to discord.js for all 79 golden commands), buildCommandPayload,
//! the deploy hash against the JS hash of the real command set, localeCompare order, and command validation.

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::command::{
    AutoDeployOutcome, AutoDeployResult, CommandDef, CommandOption, DeployConfig, SlashCommand, auto_deploy_commands, auto_deploy_with_timeout,
    command_name_compare, compute_deploy_hash, option_type,
};
use bot_core::testing::{json_diff, load_golden};
use serde_json::{Value, json};
use serenity::all::Permissions;

fn golden_commands() -> Vec<Value> {
    load_golden("commands.json").as_array().cloned().unwrap()
}

#[test]
fn every_golden_payload_round_trips_byte_identically() {
    let commands = golden_commands();
    assert_eq!(commands.len(), 79);
    for c in &commands {
        let payload = &c["payload"];
        let data = SlashCommand::from_payload(payload).unwrap();
        let def = CommandDef::new("test", data, None, |_| async { Ok(()) });
        let def = if c["allowInDMs"] == json!(true) { def.allow_in_dms() } else { def };
        let ours = serde_json::to_string(&def.payload()).unwrap();
        let theirs = serde_json::to_string(payload).unwrap();
        assert_eq!(ours, theirs, "/{}: {:?}", c["name"], json_diff(&def.payload(), payload, true));
        assert!(def.data.validate().is_empty(), "/{}: {:?}", c["name"], def.data.validate());
    }
}

#[test]
fn built_commands_use_discord_js_key_order() {
    // A command written with the builder, compared with the JSON discord.js produces for the same builder calls.
    let data = SlashCommand::new("probe", "Probe command")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            CommandOption::sub_command("create", "Create one")
                .option(CommandOption::string("name", "Name").required(true).max_length(50))
                .option(CommandOption::string("kind", "Kind").required(true).choice("Messages sent", "MESSAGES"))
                .option(CommandOption::role("role", "Role"))
                .option(CommandOption::integer("xp", "XP").min_value(1).max_value(1_000_000))
                .option(CommandOption::channel("channel", "Channel").channel_types(&[0, 5]))
                .option(CommandOption::string("search", "Search").autocomplete(true).max_length(100).min_length(1)),
        );
    let def = CommandDef::new("admin", data, Some(ModuleKey::Achievements), |_| async { Ok(()) }).gated(false).permission_level(PermissionLevel::Admin);
    let expected = r#"{"options":[{"type":1,"name":"create","description":"Create one","options":[{"type":3,"name":"name","description":"Name","required":true,"max_length":50},{"type":3,"choices":[{"name":"Messages sent","value":"MESSAGES"}],"name":"kind","description":"Kind","required":true},{"name":"role","description":"Role","required":false,"type":8},{"max_value":1000000,"min_value":1,"type":4,"name":"xp","description":"XP","required":false},{"channel_types":[0,5],"name":"channel","description":"Channel","required":false,"type":7},{"autocomplete":true,"type":3,"name":"search","description":"Search","required":false,"max_length":100,"min_length":1}]}],"name":"probe","description":"Probe command","contexts":[0],"default_member_permissions":"32","integration_types":[0],"type":1}"#;
    assert_eq!(serde_json::to_string(&def.payload()).unwrap(), expected);
    assert_eq!(def.metadata()["permissionLevel"], json!(2));
    assert_eq!(def.metadata()["module"], json!("achievements"));
}

#[test]
fn build_command_payload_makes_commands_guild_only_unless_allow_in_dms() {
    let data = SlashCommand::new("probe", "probe");
    let guild_only = CommandDef::new("utility", data.clone(), None, |_| async { Ok(()) });
    assert_eq!(guild_only.payload()["contexts"], json!([0]));
    assert_eq!(guild_only.payload()["integration_types"], json!([0]));
    let dm = CommandDef::new("utility", data.clone(), None, |_| async { Ok(()) }).allow_in_dms();
    assert!(dm.payload().get("contexts").is_none());
    assert!(dm.payload().get("integration_types").is_none());
    // A command that sets its own contexts keeps them.
    let own = CommandDef::new("utility", data.contexts(&[0, 1, 2]), None, |_| async { Ok(()) });
    assert_eq!(own.payload()["contexts"], json!([0, 1, 2]));
}

#[test]
fn deploy_hash_equals_the_js_hash_of_the_real_command_set() {
    let fixture = load_golden("bot-core/deploy-hash.json");
    let payloads: Vec<Value> = golden_commands().iter().map(|c| c["payload"].clone()).collect();
    let client = fixture["clientId"].as_str().unwrap();
    assert_eq!(compute_deploy_hash(client, None, &payloads), fixture["global"].as_str().unwrap());
    assert_eq!(compute_deploy_hash(client, fixture["guildId"].as_str(), &payloads), fixture["guild"].as_str().unwrap());
    let mut reversed = payloads.clone();
    reversed.reverse();
    assert_eq!(compute_deploy_hash(client, None, &reversed), fixture["reversedGlobal"].as_str().unwrap(), "order never matters");
    let small = vec![json!({ "name": "a", "description": "a" }), json!({ "name": "b", "description": "b" })];
    assert_eq!(compute_deploy_hash("c1", None, &small), fixture["small"]["base"].as_str().unwrap());
    assert_eq!(compute_deploy_hash("c1", Some(""), &small), fixture["small"]["emptyGuild"].as_str().unwrap(), "no guild = global");
}

#[test]
fn the_deploy_hash_covers_the_deploy_target_and_ignores_payload_order() {
    let a = json!({ "name": "a", "description": "a" });
    let b = json!({ "name": "b", "description": "b" });
    let base = compute_deploy_hash("c1", None, &[a.clone(), b.clone()]);
    assert_eq!(base, compute_deploy_hash("c1", None, &[b.clone(), a.clone()]));
    assert_eq!(base, compute_deploy_hash("c1", Some(""), &[a.clone(), b.clone()]));
    assert_ne!(base, compute_deploy_hash("c2", None, &[a.clone(), b.clone()]));
    assert_ne!(base, compute_deploy_hash("c1", Some("123"), &[a.clone(), b.clone()]));
    assert_ne!(base, compute_deploy_hash("c1", None, &[a]));
}

#[test]
fn command_names_sort_like_locale_compare() {
    let fixture = load_golden("bot-core/deploy-hash.json");
    let mut names: Vec<String> = golden_commands().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
    names.sort_by(|a, b| command_name_compare(a, b));
    let expected: Vec<String> = fixture["sortedNames"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(names, expected);
    let mut synthetic: Vec<String> = expected_synthetic(&fixture);
    synthetic.reverse();
    synthetic.sort_by(|a, b| command_name_compare(a, b));
    assert_eq!(synthetic, expected_synthetic(&fixture));
}

fn expected_synthetic(fixture: &Value) -> Vec<String> {
    fixture["syntheticSorted"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[test]
fn subcommand_permissions_must_name_real_subcommands() {
    let data = SlashCommand::new("cfg", "Config")
        .option(CommandOption::sub_command("show", "Show"))
        .option(CommandOption::sub_command_group("danger", "Danger").option(CommandOption::sub_command("wipe", "Wipe")));
    let ok = CommandDef::new("admin", data.clone(), None, |_| async { Ok(()) })
        .subcommand_permission("show", PermissionLevel::Moderator)
        .subcommand_permission("danger wipe", PermissionLevel::Admin);
    assert!(ok.validate().is_empty(), "{:?}", ok.validate());
    assert_eq!(ok.required_level(Some("danger"), Some("wipe")), PermissionLevel::Admin);
    assert_eq!(ok.required_level(None, Some("show")), PermissionLevel::Moderator);
    assert_eq!(ok.required_level(None, Some("other")), PermissionLevel::Member);
    let bad = CommandDef::new("admin", data, None, |_| async { Ok(()) }).subcommand_permission("nope", PermissionLevel::Admin);
    assert!(bad.validate().iter().any(|e| e.contains("nope")));
}

#[test]
fn validation_catches_discord_limits() {
    let mut data = SlashCommand::new("Bad Name", "");
    for i in 0..26 {
        data = data.option(CommandOption::string(format!("o{i}"), "x"));
    }
    let errors = data.validate();
    assert!(errors.iter().any(|e| e.contains("invalid name")));
    assert!(errors.iter().any(|e| e.contains("description")));
    assert!(errors.iter().any(|e| e.contains("options (max 25)")));
    let order = SlashCommand::new("ok", "ok").option(CommandOption::string("a", "a")).option(CommandOption::string("b", "b").required(true));
    assert!(order.validate().iter().any(|e| e.contains("after an optional one")));
    assert_eq!(option_type::ATTACHMENT, 11);
}

#[tokio::test]
async fn auto_deploy_skips_without_credentials_and_skips_an_unchanged_set() {
    let none = DeployConfig::default();
    assert_eq!(auto_deploy_commands(&none, vec![]).await, AutoDeployResult::Skipped);

    let dir = std::env::temp_dir().join(format!("bot-core-deploy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(".deploy-hash");
    let payloads: Vec<Value> = golden_commands().iter().map(|c| c["payload"].clone()).collect();
    // The hash file the JS bot wrote for this command set and target: nothing is deployed.
    let fixture = load_golden("bot-core/deploy-hash.json");
    std::fs::write(&file, format!("{}\n", fixture["global"].as_str().unwrap())).unwrap();
    let cfg = DeployConfig { token: Some("token".into()), client_id: Some(fixture["clientId"].as_str().unwrap().into()), guild_id: None, hash_file: file.clone() };
    assert_eq!(auto_deploy_commands(&cfg, payloads).await, AutoDeployResult::Unchanged);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn auto_deploy_with_timeout_resolves_done_failed_or_timeout() {
    assert_eq!(auto_deploy_with_timeout(async { AutoDeployResult::Unchanged }, 1000).await, AutoDeployOutcome::Done);
    assert_eq!(auto_deploy_with_timeout(async { AutoDeployResult::Failed }, 1000).await, AutoDeployOutcome::Failed);
    let stuck = async {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        AutoDeployResult::Deployed
    };
    assert_eq!(auto_deploy_with_timeout(stuck, 50).await, AutoDeployOutcome::Timeout);
    let crash = async {
        panic!("boom");
    };
    assert_eq!(auto_deploy_with_timeout(crash, 1000).await, AutoDeployOutcome::Failed);
}
