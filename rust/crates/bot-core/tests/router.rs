//! interactionRouter.js (test/registry.test.js "Router" and "Autocomplete gating", test/review-core.test.js): the
//! guild guard, the module gate, permission levels with subcommandPermissions, autocomplete gating, component routing,
//! the collector grace period, and the error reply that never overwrites an acknowledged panel.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_config::{ModuleKey, PermissionLevel};
use bot_core::collectors::CollectorFilter;
use bot_core::command::option_type;
use bot_core::interaction::AckState;
use bot_core::router::route_interaction;
use bot_core::testing::{FakeInteraction, TestApp};
use bot_core::{AppConfig, CommandDef, CommandOption, SettingsPatch, SlashCommand};
use serde_json::json;
use serenity::all::Permissions;
use serenity::builder::{AutocompleteChoice, CreateInteractionResponseMessage};

const G: &str = "900000000000000123";

fn app() -> TestApp {
    TestApp::new().with_guilds([G])
}

fn cmd(name: &str) -> SlashCommand {
    SlashCommand::new(name, "test command")
}

fn counter() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn counting(name: &str, ran: &Arc<Mutex<Vec<String>>>, module: Option<ModuleKey>) -> CommandDef {
    let r = ran.clone();
    let n = name.to_string();
    CommandDef::new("test", cmd(name), module, move |ctx| {
        let r = r.clone();
        let n = n.clone();
        async move {
            r.lock().unwrap().push(format!("{n}:{}:{}", ctx.subcommand().unwrap_or_default(), ctx.user().id));
            Ok(())
        }
    })
}

#[tokio::test]
async fn the_router_fails_closed_outside_a_cached_guild() {
    let t = app();
    let ran = counter();
    t.commands().insert(counting("ban", &ran, None).permission_level(PermissionLevel::Admin));
    t.commands().insert(counting("ping", &ran, None).allow_in_dms());
    t.commands().insert(counting("both", &ran, None).allow_in_dms().permission_level(PermissionLevel::Moderator));

    route_interaction(&t, FakeInteraction::slash("ban").build()).await;
    assert_eq!(t.responses.calls()[0].title_or_content(), Some("❌  Server Only"));
    route_interaction(&t, FakeInteraction::slash("ban").guild("999").build()).await;
    assert_eq!(t.responses.calls()[1].title_or_content(), Some("❌  Bot Not In Server"));
    assert!(t.responses.calls().iter().all(|c| c.ephemeral()));
    assert!(ran.lock().unwrap().is_empty());
    // A DM-safe command runs in a DM...
    route_interaction(&t, FakeInteraction::slash("ping").build()).await;
    assert_eq!(ran.lock().unwrap().len(), 1);
    // ...but one that also needs a permission level cannot prove it there.
    route_interaction(&t, FakeInteraction::slash("both").build()).await;
    assert_eq!(t.responses.calls().last().unwrap().title_or_content(), Some("❌  Server Only"));
    assert_eq!(
        t.responses.calls().last().unwrap().embed_description(),
        Some("This command can only be used in a server where I am a member.")
    );
    assert_eq!(ran.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ungated_member_commands_load_no_settings_and_unknown_commands_are_reported() {
    let t = app();
    t.settings_store.set_available(false); // any settings load would fail
    let ran = counter();
    t.commands().insert(counting("hello", &ran, None));
    route_interaction(&t, FakeInteraction::slash("hello").guild(G).build()).await;
    assert_eq!(ran.lock().unwrap().len(), 1);
    assert!(t.responses.calls().is_empty());
    route_interaction(&t, FakeInteraction::slash("nope").guild(G).build()).await;
    let c = t.responses.calls()[0].clone();
    assert_eq!(c.title_or_content(), Some("❌  Unknown Command"));
    assert_eq!(c.embed_description(), Some("This command no longer exists. Try `/` to see available commands."));
}

#[tokio::test]
async fn a_gated_command_is_refused_when_settings_cannot_be_loaded_or_the_module_is_off() {
    let t = app();
    let ran = counter();
    t.commands().insert(counting("slots", &ran, Some(ModuleKey::Games)));
    t.commands().insert(counting("setup", &ran, Some(ModuleKey::Games)).gated(false));
    t.settings_store.set_available(false);
    route_interaction(&t, FakeInteraction::slash("slots").guild(G).build()).await;
    assert_eq!(t.responses.calls()[0].title_or_content(), Some("❌  Temporarily Unavailable"));
    assert!(ran.lock().unwrap().is_empty());
    t.settings_store.set_available(true);
    t.settings().update_settings(G, SettingsPatch::new().set("gamesEnabled", false)).await.unwrap();
    route_interaction(&t, FakeInteraction::slash("slots").guild(G).build()).await;
    let c = t.responses.calls()[1].clone();
    assert_eq!(c.title_or_content(), Some("⚠️  Module Disabled"));
    assert_eq!(c.embed_description(), Some("The **Games** module is turned off on this server. An admin can enable it with `/module enable games`."));
    route_interaction(&t, FakeInteraction::slash("setup").guild(G).build()).await;
    assert_eq!(ran.lock().unwrap().len(), 1, "gated = false commands are not module-gated");
}

#[tokio::test]
async fn slash_commands_refuse_one_level_below_and_run_at_it() {
    let t = app();
    t.settings().update_settings(G, SettingsPatch::new().set("modRoleIds", json!(["700"]))).await.unwrap();
    let ran = counter();
    t.commands().insert(counting("warn", &ran, None).permission_level(PermissionLevel::Moderator));
    let data = cmd("config").option(CommandOption::sub_command("reset", "Reset")).option(CommandOption::sub_command("show", "Show"));
    let r = ran.clone();
    t.commands().insert(
        CommandDef::new("test", data, None, move |ctx| {
            let r = r.clone();
            async move {
                r.lock().unwrap().push(format!("config {}:{}", ctx.subcommand().unwrap(), ctx.user().id));
                Ok(())
            }
        })
        .permission_level(PermissionLevel::Moderator)
        .subcommand_permission("reset", PermissionLevel::Admin),
    );
    let plain = || FakeInteraction::slash("warn").guild(G).user("1");
    route_interaction(&t, plain().build()).await;
    assert_eq!(t.responses.calls()[0].title_or_content(), Some("❌  Insufficient Permissions"));
    assert_eq!(t.responses.calls()[0].embed_description(), Some("You don't have permission to use this command."));
    assert!(ran.lock().unwrap().is_empty());
    route_interaction(&t, FakeInteraction::slash("warn").guild(G).user("2").permissions(Permissions::KICK_MEMBERS).build()).await;
    route_interaction(&t, FakeInteraction::slash("warn").guild(G).user("3").roles(&["700"]).build()).await;
    route_interaction(&t, FakeInteraction::slash("warn").guild(G).user("4").permissions(Permissions::ADMINISTRATOR).build()).await;
    assert_eq!(*ran.lock().unwrap(), vec!["warn::2", "warn::3", "warn::4"]);
    ran.lock().unwrap().clear();
    let before = t.responses.calls().len();
    route_interaction(&t, FakeInteraction::slash("config").guild(G).user("2").permissions(Permissions::KICK_MEMBERS).subcommand("reset").build()).await;
    route_interaction(&t, FakeInteraction::slash("config").guild(G).user("3").roles(&["700"]).subcommand("reset").build()).await;
    assert_eq!(t.responses.calls().len(), before + 2, "a MODERATOR cannot run an ADMIN subcommand");
    route_interaction(&t, FakeInteraction::slash("config").guild(G).user("2").permissions(Permissions::KICK_MEMBERS).subcommand("show").build()).await;
    route_interaction(&t, FakeInteraction::slash("config").guild(G).user("4").permissions(Permissions::MANAGE_GUILD).subcommand("reset").build()).await;
    assert_eq!(*ran.lock().unwrap(), vec!["config show:2", "config reset:4"]);
}

#[tokio::test]
async fn bot_owner_id_is_owner_everywhere() {
    let t = TestApp::with_config(AppConfig { bot_owner_id: Some("1234".into()), ..Default::default() }).with_guilds([G]);
    let ran = counter();
    t.commands().insert(counting("owner", &ran, None).permission_level(PermissionLevel::Owner));
    route_interaction(&t, FakeInteraction::slash("owner").guild(G).user("1235").permissions(Permissions::ADMINISTRATOR).build()).await;
    route_interaction(&t, FakeInteraction::slash("owner").guild(G).user("1234").build()).await;
    assert_eq!(*ran.lock().unwrap(), vec!["owner::1234"]);
}

#[tokio::test]
async fn a_slash_command_that_replied_and_then_failed_gets_an_ephemeral_follow_up() {
    let t = app();
    t.commands().insert(CommandDef::new("test", cmd("boom"), None, |ctx| async move {
        ctx.reply(CreateInteractionResponseMessage::new().content("done")).await?;
        anyhow::bail!("boom")
    }));
    route_interaction(&t, FakeInteraction::slash("boom").guild(G).build()).await;
    assert_eq!(t.responses.names(), vec!["reply", "followUp"]);
    let f = &t.responses.calls()[1];
    assert!(f.ephemeral());
    assert_eq!(f.title_or_content(), Some("❌  Command Error"));
    // A panic is a failure like any other.
    t.commands().insert(CommandDef::new("test", cmd("panic"), None, |_| async move { panic!("kaboom") }));
    route_interaction(&t, FakeInteraction::slash("panic").guild(G).build()).await;
    assert_eq!(t.responses.calls().last().unwrap().title_or_content(), Some("❌  Command Error"));
}

#[tokio::test]
async fn components_nobody_handles_are_answered_at_once_and_collectors_get_a_grace_period() {
    tokio::time::pause();
    let t = app();
    route_interaction(&t, FakeInteraction::button("unknown-prefix:1").build()).await;
    route_interaction(&t, FakeInteraction::select("unknown-prefix:1", &["a"]).build()).await;
    route_interaction(&t, FakeInteraction::modal("unknown-prefix:1", &[]).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].content(), Some("This button is no longer active."));
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].content(), Some("This menu is no longer active."));
    assert_eq!(calls[2].content(), Some("This form has expired. Please run the command again."));
    t.responses.clear();

    route_interaction(&t, FakeInteraction::button("rps:123:rock").build()).await; // a collector-only prefix
    assert!(t.responses.calls().is_empty(), "the live collector gets the first chance");
    tokio::time::sleep(Duration::from_millis(2600)).await; // paused clock: auto-advances
    assert_eq!(t.responses.calls()[0].content(), Some("This menu has expired."));
    t.responses.clear();

    // A live collector answers in time: no expiry notice.
    let mut collector = t.collectors().collect(CollectorFilter::default().prefixes(["rps"]));
    route_interaction(&t, FakeInteraction::button("rps:123:paper").build()).await;
    let click = collector.next(Duration::from_secs(1)).await.expect("the collector received the click");
    assert_eq!(click.parsed(), ("rps".to_string(), vec!["123".to_string(), "paper".to_string()]));
    click.defer_update().await.unwrap();
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert_eq!(t.responses.names(), vec!["deferUpdate"]);
    drop(collector);
    assert!(t.collectors().is_empty());
}

#[tokio::test]
async fn components_route_by_prefix_and_survive_a_failing_handler() {
    let t = app();
    let seen = counter();
    let (a, b) = (seen.clone(), seen.clone());
    t.components().buttons.set(
        "noop",
        bot_core::component_registry::component_fn(move |ctx| {
            let a = a.clone();
            async move {
                a.lock().unwrap().push(format!("button:{}", ctx.custom_id()));
                Ok(())
            }
        }),
    );
    t.components().selects.set(
        "rr-select",
        bot_core::component_registry::component_fn(move |ctx| {
            let b = b.clone();
            async move {
                b.lock().unwrap().push(format!("select:{}:{}", ctx.custom_id(), ctx.values().join(",")));
                Ok(())
            }
        }),
    );
    t.components().modals.set("welcome-setup", bot_core::component_registry::modal_fn(|_| async { anyhow::bail!("handler boom") }));
    route_interaction(&t, FakeInteraction::button("noop:a:b").build()).await;
    route_interaction(&t, FakeInteraction::user_select("rr-select:g", &["5"]).build()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["button:noop:a:b", "select:rr-select:g:5"]);
    route_interaction(&t, FakeInteraction::modal("welcome-setup", &[("title", "x")]).build()).await;
    let c = t.responses.calls().last().cloned().unwrap();
    assert_eq!(c.title_or_content(), Some("❌  Modal Error"));
    assert_eq!(c.embed_description(), Some("Something went wrong processing your submission."));
}

#[tokio::test]
async fn the_error_reply_never_overwrites_an_acknowledged_panel() {
    let t = app();
    // deferUpdate then throw: a follow-up, not an edit of the clicked panel.
    t.components().buttons.set(
        "giveaway",
        bot_core::component_registry::component_fn(|ctx| async move {
            ctx.defer_update().await?;
            anyhow::bail!("boom")
        }),
    );
    route_interaction(&t, FakeInteraction::button("giveaway:1").build()).await;
    assert_eq!(t.responses.names(), vec!["deferUpdate", "followUp"]);
    assert!(t.responses.calls()[1].ephemeral());
    assert_eq!(t.responses.calls()[1].title_or_content(), Some("❌  Button Error"));
    t.responses.clear();

    // update() then throw: a follow-up.
    t.components().selects.set(
        "rr-select",
        bot_core::component_registry::component_fn(|ctx| async move {
            ctx.update(CreateInteractionResponseMessage::new().content("x")).await?;
            anyhow::bail!("boom")
        }),
    );
    route_interaction(&t, FakeInteraction::select("rr-select:1", &["a"]).build()).await;
    assert_eq!(t.responses.names(), vec!["update", "followUp"]);
    t.responses.clear();

    // deferReply then throw: the deferred placeholder is replaced.
    t.components().modals.set(
        "welcome-setup",
        bot_core::component_registry::modal_fn(|ctx| async move {
            ctx.defer_reply(true).await?;
            anyhow::bail!("boom")
        }),
    );
    route_interaction(&t, FakeInteraction::modal("welcome-setup", &[]).build()).await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(t.responses.calls()[1].title_or_content(), Some("❌  Modal Error"));
    t.responses.clear();

    // Throwing before acknowledging: an ephemeral reply.
    t.components().buttons.set("poll", bot_core::component_registry::component_fn(|_| async { anyhow::bail!("boom") }));
    route_interaction(&t, FakeInteraction::button("poll:1").build()).await;
    assert_eq!(t.responses.names(), vec!["reply"]);
    assert!(t.responses.calls()[0].ephemeral());
}

#[tokio::test]
async fn the_responder_follows_the_discord_js_state_machine() {
    let t = app();
    let backend = t.responses.clone();
    let r = bot_core::Responder::new(backend.clone(), serenity::all::InteractionId::new(1), "tok");
    assert!(r.edit_reply(serenity::builder::EditInteractionResponse::new().content("x")).await.is_err(), "NotReplied");
    assert!(r.follow_up(serenity::builder::CreateInteractionResponseFollowup::new().content("x")).await.is_err());
    r.defer_reply(true).await.unwrap();
    assert_eq!(r.state(), AckState { replied: false, deferred: true, ephemeral: Some(true), responded: false });
    assert!(r.reply_text("again", false).await.is_err(), "AlreadyReplied");
    let msg = r.edit_reply(serenity::builder::EditInteractionResponse::new().content("done")).await.unwrap();
    assert_eq!(msg.content, "done");
    assert!(r.replied());
    let r2 = bot_core::Responder::new(backend.clone(), serenity::all::InteractionId::new(2), "tok");
    r2.defer_update().await.unwrap();
    assert_eq!(r2.ephemeral(), None, "deferUpdate leaves ephemeral null");
    let r3 = bot_core::Responder::new(backend.clone(), serenity::all::InteractionId::new(3), "tok");
    r3.reply(CreateInteractionResponseMessage::new().content("hi").ephemeral(true)).await.unwrap();
    assert_eq!(r3.ephemeral(), Some(true));
    // A failed request leaves the state as it was.
    backend.set_failing(true);
    let r4 = bot_core::Responder::new(backend.clone(), serenity::all::InteractionId::new(4), "tok");
    assert!(r4.reply_text("x", true).await.is_err());
    assert!(!r4.is_acknowledged());
}

fn ac_app() -> (TestApp, Arc<Mutex<usize>>) {
    let t = app();
    let handled = Arc::new(Mutex::new(0usize));
    let choices = || vec![AutocompleteChoice::new("secret role", "r1")];
    let mk = |name: &str, h: &Arc<Mutex<usize>>| {
        let h = h.clone();
        CommandDef::new("test", cmd(name).option(CommandOption::string("q", "q").autocomplete(true)), None, |_| async { Ok(()) }).autocomplete(move |ctx| {
            let h = h.clone();
            async move {
                *h.lock().unwrap() += 1;
                ctx.respond(choices()).await?;
                Ok(())
            }
        })
    };
    t.commands().insert(mk("config", &handled).permission_level(PermissionLevel::Admin));
    t.commands().insert(mk("open", &handled));
    let mixed = {
        let h = handled.clone();
        CommandDef::new(
            "test",
            cmd("mixed")
                .option(CommandOption::sub_command("setup", "s"))
                .option(CommandOption::sub_command("list", "l"))
                .option(CommandOption::sub_command_group("danger", "d").option(CommandOption::sub_command("wipe", "w"))),
            None,
            |_| async { Ok(()) },
        )
        .subcommand_permission("setup", PermissionLevel::Admin)
        .subcommand_permission("danger wipe", PermissionLevel::Admin)
        .autocomplete(move |ctx| {
            let h = h.clone();
            async move {
                *h.lock().unwrap() += 1;
                ctx.respond(vec![AutocompleteChoice::new("x", "x")]).await?;
                Ok(())
            }
        })
    };
    t.commands().insert(mixed);
    (t, handled)
}

#[tokio::test]
async fn autocomplete_is_gated_like_execute() {
    let (t, handled) = ac_app();
    route_interaction(&t, FakeInteraction::autocomplete("config").guild(G).focused("q", "").build()).await;
    assert!(t.responses.calls()[0].choices().is_empty(), "a MEMBER sees no admin-only suggestions");
    assert_eq!(*handled.lock().unwrap(), 0);
    route_interaction(&t, FakeInteraction::autocomplete("config").guild(G).permissions(Permissions::ADMINISTRATOR).focused("q", "").build()).await;
    assert_eq!(t.responses.calls()[1].choices(), vec![json!({ "name": "secret role", "value": "r1" })]);
    route_interaction(&t, FakeInteraction::autocomplete("open").guild(G).focused("q", "").build()).await;
    assert_eq!(t.responses.calls()[2].choices().len(), 1, "a MEMBER command still autocompletes for a MEMBER");
    assert_eq!(*handled.lock().unwrap(), 2);
}

#[tokio::test]
async fn autocomplete_honours_subcommand_permissions() {
    let (t, handled) = ac_app();
    route_interaction(&t, FakeInteraction::autocomplete("mixed").guild(G).subcommand("setup").build()).await;
    route_interaction(&t, FakeInteraction::autocomplete("mixed").guild(G).group("danger", "wipe").build()).await;
    assert!(t.responses.calls().iter().all(|c| c.choices().is_empty()));
    assert_eq!(*handled.lock().unwrap(), 0);
    route_interaction(&t, FakeInteraction::autocomplete("mixed").guild(G).subcommand("list").build()).await;
    route_interaction(&t, FakeInteraction::autocomplete("mixed").guild(G).subcommand("setup").permissions(Permissions::ADMINISTRATOR).build()).await;
    assert_eq!(t.responses.calls()[2].choices().len(), 1);
    assert_eq!(t.responses.calls()[3].choices().len(), 1);
    assert_eq!(*handled.lock().unwrap(), 2);
}

#[tokio::test]
async fn autocomplete_of_a_disabled_module_or_without_settings_answers_nothing() {
    let t = app();
    let mk = |name: &str, gated: bool| {
        CommandDef::new("test", cmd(name), Some(ModuleKey::Games), |_| async { Ok(()) })
            .gated(gated)
            .autocomplete(|ctx| async move {
                ctx.respond(vec![AutocompleteChoice::new("bet", "10")]).await?;
                Ok(())
            })
    };
    t.commands().insert(mk("slots", true));
    t.commands().insert(mk("ungated", false));
    t.commands().insert(CommandDef::new("test", cmd("boom"), None, |_| async { Ok(()) }).autocomplete(|_| async { anyhow::bail!("autocomplete boom") }));
    t.settings().update_settings(G, SettingsPatch::new().set("gamesEnabled", false)).await.unwrap();
    route_interaction(&t, FakeInteraction::autocomplete("slots").guild(G).build()).await;
    assert!(t.responses.calls()[0].choices().is_empty());
    route_interaction(&t, FakeInteraction::autocomplete("ungated").guild(G).build()).await;
    assert_eq!(t.responses.calls()[1].choices()[0]["value"], json!("10"));
    t.settings().update_settings(G, SettingsPatch::new().set("gamesEnabled", true)).await.unwrap();
    route_interaction(&t, FakeInteraction::autocomplete("slots").guild(G).build()).await;
    assert_eq!(t.responses.calls()[2].choices().len(), 1);
    // Settings cannot be loaded: [] within the answer window.
    let t2 = app();
    t2.commands().insert(mk("slots", true));
    t2.settings_store.set_available(false);
    let started = std::time::Instant::now();
    route_interaction(&t2, FakeInteraction::autocomplete("slots").guild(G).build()).await;
    assert!(t2.responses.calls()[0].choices().is_empty());
    assert!(started.elapsed() < Duration::from_millis(2500));
    route_interaction(&t, FakeInteraction::autocomplete("boom").guild(G).build()).await; // logged, never thrown
}

#[tokio::test]
async fn autocomplete_outside_a_cached_guild_answers_nothing_unless_dm_safe() {
    let t = app();
    let handled = Arc::new(Mutex::new(0usize));
    let mk = |name: &str| {
        let h = handled.clone();
        CommandDef::new("test", cmd(name), None, |_| async { Ok(()) }).autocomplete(move |ctx| {
            let h = h.clone();
            async move {
                *h.lock().unwrap() += 1;
                ctx.respond(vec![AutocompleteChoice::new("x", "x")]).await?;
                Ok(())
            }
        })
    };
    t.commands().insert(mk("guildOnly"));
    t.commands().insert(mk("dmSafe").allow_in_dms());
    t.commands().insert(mk("dmAdmin").allow_in_dms().permission_level(PermissionLevel::Moderator));
    route_interaction(&t, FakeInteraction::autocomplete("guildOnly").build()).await;
    route_interaction(&t, FakeInteraction::autocomplete("guildOnly").guild("555").build()).await;
    route_interaction(&t, FakeInteraction::autocomplete("dmAdmin").build()).await;
    assert!(t.responses.calls().iter().all(|c| c.choices().is_empty()));
    assert_eq!(t.responses.calls().len(), 3);
    assert_eq!(*handled.lock().unwrap(), 0);
    route_interaction(&t, FakeInteraction::autocomplete("dmSafe").build()).await;
    assert_eq!(t.responses.calls()[3].choices().len(), 1);
}

#[tokio::test]
async fn command_contexts_read_options_and_subcommands() {
    let t = app();
    let seen = counter();
    let s = seen.clone();
    let data = cmd("opts").option(
        CommandOption::sub_command_group("form", "f")
            .option(CommandOption::sub_command("create", "c").option(CommandOption::string("name", "n")).option(CommandOption::integer("n", "n"))),
    );
    t.commands().insert(CommandDef::new("test", data, None, move |ctx| {
        let s = s.clone();
        async move {
            s.lock().unwrap().push(format!(
                "{:?} {:?} {:?} {:?} {}",
                ctx.subcommand_group(),
                ctx.subcommand(),
                ctx.get_string("name"),
                ctx.get_integer("n"),
                ctx.options().len()
            ));
            Ok(())
        }
    }));
    route_interaction(
        &t,
        FakeInteraction::slash("opts")
            .guild(G)
            .group("form", "create")
            .option("name", option_type::STRING, json!("Alpha"))
            .option("n", option_type::INTEGER, json!(5))
            .build(),
    )
    .await;
    assert_eq!(*seen.lock().unwrap(), vec![r#"Some("form") Some("create") Some("Alpha") Some(5) 2"#]);
}

#[tokio::test]
async fn a_handler_that_panics_before_returning_its_future_is_caught() {
    let t = app();
    let fail: Option<u8> = std::hint::black_box(None);
    t.commands().insert(CommandDef::new("test", cmd("early"), None, move |_| {
        let _x = fail.expect("unwrap before the async block");
        async { Ok(()) }
    }));
    route_interaction(&t, FakeInteraction::slash("early").guild(G).build()).await;
    assert_eq!(t.responses.calls()[0].title_or_content(), Some("❌  Command Error"));
    t.components().buttons.set(
        "poll",
        Arc::new(move |_ctx| -> bot_core::BoxFuture<bot_core::BotResult<()>> {
            let _x = fail.expect("unwrap before the async block");
            Box::pin(async { Ok(()) })
        }),
    );
    route_interaction(&t, FakeInteraction::button("poll:1").build()).await;
    assert_eq!(t.responses.calls()[1].title_or_content(), Some("❌  Button Error"));
}

#[tokio::test]
async fn component_and_modal_contexts_read_ids_values_and_levels() {
    let t = app();
    t.settings().update_settings(G, SettingsPatch::new().set("modRoleIds", json!(["700"]))).await.unwrap();
    let seen = counter();
    let s = seen.clone();
    t.components().buttons.set(
        "poll",
        bot_core::component_registry::component_fn(move |ctx| {
            let s = s.clone();
            async move {
                let level = ctx.permission_level().await;
                s.lock().unwrap().push(format!("{:?} {:?} {}", ctx.parsed(), level, ctx.interaction.message.id));
                Ok(())
            }
        }),
    );
    let s = seen.clone();
    t.components().modals.set(
        "poll-modal",
        bot_core::component_registry::modal_fn(move |ctx| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push(format!("{:?} {:?} {:?}", ctx.parsed(), ctx.value("poll-question"), ctx.value("missing")));
                Ok(())
            }
        }),
    );
    route_interaction(&t, FakeInteraction::button("poll:9:x").guild(G).roles(&["700"]).message_id("123").build()).await;
    route_interaction(&t, FakeInteraction::modal("poll-modal:9", &[("poll-question", "Why?")]).guild(G).build()).await;
    assert_eq!(
        *seen.lock().unwrap(),
        vec![r#"("poll", ["9", "x"]) Moderator 123"#, r#"("poll-modal", ["9"]) Some("Why?") None"#]
    );
}

#[tokio::test]
async fn permission_levels_come_from_the_cached_guild_when_discord_is_connected() {
    use bot_core::testing::guild_create_json;
    let guild = guild_create_json(G, "42", &[("800", Permissions::ADMINISTRATOR.bits()), ("801", Permissions::KICK_MEMBERS.bits())], &[("7", &["800"]), ("8", &["801"]), ("9", &[])]);
    let t = TestApp::new().with_discord_guilds(&[guild]);
    assert_eq!(t.guild_directory().available_guild_ids(), vec![G.to_string()]);
    let ran = counter();
    t.commands().insert(counting("admin", &ran, None).permission_level(PermissionLevel::Admin));
    t.commands().insert(counting("mod", &ran, None).permission_level(PermissionLevel::Moderator));
    // The interaction says ADMINISTRATOR (channel permissions), but the cached guild says otherwise: the cache wins.
    route_interaction(&t, FakeInteraction::slash("admin").guild(G).user("9").permissions(Permissions::ADMINISTRATOR).build()).await;
    route_interaction(&t, FakeInteraction::slash("admin").guild(G).user("42").build()).await; // the guild owner
    route_interaction(&t, FakeInteraction::slash("admin").guild(G).user("7").roles(&["800"]).build()).await;
    route_interaction(&t, FakeInteraction::slash("mod").guild(G).user("8").roles(&["801"]).build()).await;
    assert_eq!(*ran.lock().unwrap(), vec!["admin::42", "admin::7", "mod::8"]);
}
