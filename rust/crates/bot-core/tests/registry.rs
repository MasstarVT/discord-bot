//! pluginLoader.js, hookRegistry.js and componentRegistry.js (test/registry.test.js sections 6, 10, 11, 14, 15).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bot_config::{HookPoint, JobType, ModuleKey};
use bot_core::component_registry::{ComponentRegistry, component_fn, noop_handler, register_core_components};
use bot_core::discord::StaticGuilds;
use bot_core::hooks::events::{GuildMemberAdd, GuildReady, GuildReadyReason, MessageCreate, MessageDelete, MessageDeleteBulk, Ready};
use bot_core::hooks::{DEFAULT_HOOK_TIMEOUT_MS, Hook, HookCtx, HookInfo, ModuleRef, ReadyGate, run_guild_available, run_guild_ready};
use bot_core::plugin::{collect_plugins, install_plugins};
use bot_core::rewards::{RewardArgs, RewardKind, RewardProvider, RewardResult};
use bot_core::scheduler::{JobOutcome, job_handler};
use bot_core::testing::{FakeInteraction, TestApp};
use bot_core::{App, BotResult, Plugin, Registry};
use serenity::all::{ChannelId, GuildId, Interaction, MessageId};

fn msg_delete(guild: u64) -> MessageDelete {
    MessageDelete { guild_id: GuildId::new(guild), message_id: MessageId::new(1), channel_id: ChannelId::new(2), message: None, partial: true, channel: None }
}

struct Provider;

#[async_trait]
impl RewardProvider for Provider {
    async fn grant(&self, _: &App, _: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(Some(1)))
    }
    async fn take(&self, _: &App, _: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(Some(0)))
    }
}

#[test]
fn collect_plugins_validates_declarations_and_drops_only_the_offending_entry() {
    let noop = |_ctx: Arc<HookCtx<MessageDelete>>| async { Ok(()) };
    let giveaways = Plugin::new("giveaways", Some(ModuleKey::Giveaways))
        .hook(Hook::<MessageDelete>::new("giveaways", noop))
        .hook(Hook::<MessageDelete>::new("polls", noop)) // another package's id
        .hook(Hook::<MessageDelete>::new("giveaways", noop)) // duplicate
        .hook(Hook::<MessageDeleteBulk>::new("giveaways:extra", |_| async { Ok(()) })) // not in HOOK_ORDER
        .button("giveaway", |_| async { Ok(()) })
        .select("giveaway", |_| async { Ok(()) }) // registry lists button only
        .button("rr", |_| async { Ok(()) }) // owned by reaction-roles
        .button("paginator", |_| async { Ok(()) }) // collector, not persistent
        .button("no-such-prefix", |_| async { Ok(()) })
        .job(JobType::GiveawayEnd, job_handler(|_, _| async { Ok(JobOutcome::Done) }))
        .job(JobType::PollEnd, job_handler(|_, _| async { Ok(JobOutcome::Done) })) // owned by polls
        .provides(RewardKind::Xp, Arc::new(Provider));
    let levels = Plugin::new("levels", Some(ModuleKey::Leveling)).provides(RewardKind::Xp, Arc::new(Provider)); // duplicate provider
    let wrong = Plugin::new("Wrong_Id", None);
    let dup = Plugin::new("giveaways", None);
    let collected = collect_plugins(vec![giveaways, levels, wrong, dup]);
    let ids: Vec<&str> = collected.plugins.iter().map(|p| p.id).collect();
    assert_eq!(ids, vec!["giveaways", "levels"]);
    let p = &collected.plugins[0];
    assert_eq!(p.hooks.iter().map(|h| format!("{}:{}", h.event(), h.id())).collect::<Vec<_>>(), vec!["messageDelete:giveaways"]);
    assert_eq!(p.components.iter().map(|c| format!("{}:{}", c.kind.as_str(), c.prefix)).collect::<Vec<_>>(), vec!["button:giveaway"]);
    assert_eq!(p.jobs.iter().map(|j| j.0.as_str()).collect::<Vec<_>>(), vec!["giveaway.end"]);
    assert_eq!(p.provides.len(), 1);
    assert!(collected.plugins[1].provides.is_empty(), "a second xp provider is dropped");
    let text = collected.errors.join("\n");
    for expected in [
        "\"polls\"",
        "duplicate hook",
        "not listed in HOOK_ORDER",
        "for button",
        "owned by \"reaction-roles\"",
        "paginator",
        "not in CUSTOM_ID_PREFIXES",
        "poll.end",
        "duplicate provider",
        "Wrong_Id",
        "already taken",
    ] {
        assert!(text.contains(expected), "an error mentions {expected}:\n{text}");
    }
    assert!(collect_plugins(vec![]).errors.is_empty());
}

#[tokio::test]
async fn install_registers_hooks_components_jobs_and_wraps_module_gated_components() {
    let t = TestApp::new();
    let handled = Arc::new(Mutex::new(Vec::<String>::new()));
    let (h1, h2) = (handled.clone(), handled.clone());
    let plugin = Plugin::new("polls", Some(ModuleKey::Polls))
        .hook(Hook::<MessageDelete>::new("polls", |_| async { Ok(()) }))
        .button("poll", move |_| {
            let h = h1.clone();
            async move {
                h.lock().unwrap().push("poll".into());
                Ok(())
            }
        })
        .select("poll-select", move |_| {
            let h = h2.clone();
            async move {
                h.lock().unwrap().push("select".into());
                Ok(())
            }
        })
        .component_module(ModuleRef::Ungated)
        .modal("poll-modal", |_| async { Ok(()) })
        .job(JobType::PollEnd, job_handler(|_, _| async { Ok(JobOutcome::Done) }))
        .init(|_| Err(anyhow::anyhow!("init exploded")));
    let collected = collect_plugins(vec![plugin]);
    let r = install_plugins(&t, &collected);
    assert_eq!((r.hooks, r.components, r.jobs), (1, 3, 1));
    assert_eq!(r.errors.len(), 1, "the init failure is recorded, nothing else is: {:?}", r.errors);
    assert!(r.errors[0].contains("init exploded"));
    assert_eq!(t.hooks().list(HookPoint::MessageDelete), vec![HookInfo { id: "polls".into(), plugin_id: "polls".into(), module: Some(ModuleKey::Polls), always: false }]);
    assert!(t.scheduler().has_handler("poll.end"));

    // The ungated select runs in a guild whose settings cannot be read; the gated button does not.
    t.settings_store.set_available(false);
    let select = FakeInteraction::select("poll-select:1", &["a"]).guild("1").build();
    let button = FakeInteraction::button("poll:1").guild("1").build();
    bot_core::router::route_interaction(&t, select).await;
    bot_core::router::route_interaction(&t, button).await;
    assert_eq!(*handled.lock().unwrap(), vec!["select"]);
    let notice = t.responses.calls().last().cloned().unwrap();
    assert_eq!(notice.content(), Some("Server settings are temporarily unavailable. Please try again in a moment."));
    assert!(notice.ephemeral());
    // Outside a guild there are no settings to check: a gated component passes straight through.
    bot_core::router::route_interaction(&t, FakeInteraction::button("poll:1").build()).await;
    assert_eq!(*handled.lock().unwrap(), vec!["select", "poll"]);
    // With the module off the notice names it.
    t.settings_store.set_available(true);
    t.settings().update_settings("2", bot_core::SettingsPatch::new().set("pollsEnabled", false)).await.unwrap();
    bot_core::router::route_interaction(&t, FakeInteraction::button("poll:1").guild("2").build()).await;
    assert_eq!(t.responses.calls().last().unwrap().content(), Some("The **Polls** module is turned off on this server."));
}

fn record(seen: &Arc<Mutex<Vec<String>>>, name: &str) -> impl Fn(Arc<HookCtx<Ready>>) -> futures::future::Ready<BotResult<()>> + Send + Sync + 'static {
    let seen = seen.clone();
    let name = name.to_string();
    move |_| {
        seen.lock().unwrap().push(name.clone());
        futures::future::ready(Ok(()))
    }
}

fn mc_recorder(seen: &Arc<Mutex<Vec<String>>>, name: &str) -> impl Fn(Arc<HookCtx<MessageCreateLite>>) -> futures::future::Ready<BotResult<()>> + Send + Sync + 'static {
    let seen = seen.clone();
    let name = name.to_string();
    move |_| {
        seen.lock().unwrap().push(name.clone());
        futures::future::ready(Ok(()))
    }
}

/// `messageCreate` hooks need a serenity Message; the ordering tests only need the hook point, so they use `ready`
/// (no hooks in HOOK_ORDER) for unlisted-id ordering and this alias for messageCreate ids.
type MessageCreateLite = MessageCreate;

fn mc_payload() -> MessageCreate {
    let message: serenity::all::Message = serde_json::from_value(serde_json::json!({
        "id": "10", "channel_id": "2", "guild_id": "1",
        "author": { "id": "3", "username": "u", "discriminator": "0", "avatar": null },
        "content": "hi", "timestamp": "2026-01-01T00:00:00.000000+00:00", "edited_timestamp": null, "tts": false,
        "mention_everyone": false, "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0
    }))
    .unwrap();
    MessageCreate { guild_id: GuildId::new(1), message, member: None, channel_id: ChannelId::new(2), channel: None }
}

#[tokio::test]
async fn hooks_run_in_hook_order_regardless_of_registration_order() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let order = HookPoint::MessageCreate.order();
    for id in order.iter().rev() {
        t.hooks().register_hook(Hook::<MessageCreate>::new(id, mc_recorder(&seen, id)), "test", None).unwrap();
    }
    assert_eq!(t.hooks().list(HookPoint::MessageCreate).iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), order.to_vec());
    t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), order.iter().map(|s| s.to_string()).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_failing_or_panicking_hook_does_not_stop_later_hooks() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    t.hooks().register_hook(Hook::<MessageCreate>::new("automod", |_| async { panic!("sync boom") }), "test", None).unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("logging", |_| async { Err(anyhow::anyhow!("async boom")) }), "test", None).unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("levels", mc_recorder(&seen, "levels")), "test", None).unwrap();
    let ctx = t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["levels"]);
    assert!(!ctx.stopped());
}

#[tokio::test]
async fn stop_stops_later_hooks_and_state_is_shared_within_a_dispatch() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    t.hooks()
        .register_hook(
            Hook::<MessageCreate>::new("automod", |ctx| async move {
                ctx.state.insert("deleted", true);
                ctx.stop("automod");
                Ok(())
            }),
            "test",
            None,
        )
        .unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("levels", mc_recorder(&seen, "levels")), "test", None).unwrap();
    let ctx = t.run_hooks(mc_payload()).await;
    assert!(seen.lock().unwrap().is_empty());
    assert!(ctx.stopped());
    assert_eq!(ctx.stop_reason().as_deref(), Some("automod"));
    assert_eq!(ctx.state.get::<bool>("deleted"), Some(true));
    assert_eq!(ctx.state.get::<String>("deleted"), None, "typed reads");
}

#[tokio::test]
async fn a_module_gated_hook_is_skipped_without_settings_unless_always_or_ungated() {
    let t = TestApp::new();
    t.settings_store.set_available(false); // no settings can be read
    let seen = Arc::new(Mutex::new(Vec::new()));
    t.hooks().register_hook(Hook::<MessageCreate>::new("automod", mc_recorder(&seen, "gated")), "test", Some(ModuleKey::Automod)).unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("logging", mc_recorder(&seen, "always")).always(), "test", Some(ModuleKey::Logging)).unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("levels", mc_recorder(&seen, "ungated")).ungated(), "test", Some(ModuleKey::Leveling)).unwrap();
    t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["always", "ungated"]);
    // With settings: the gated hook runs when its module is on.
    t.settings_store.set_available(true);
    t.settings().update_settings("1", bot_core::SettingsPatch::new().set("automodEnabled", true)).await.unwrap();
    seen.lock().unwrap().clear();
    let ctx = t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["gated", "always", "ungated"]);
    assert!(ctx.get_settings().await.unwrap().automod_enabled, "ctx.getSettings() memoised");
}

#[tokio::test]
async fn a_sync_filter_that_returns_false_or_panics_skips_the_hook() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    t.hooks().register_hook(Hook::<MessageCreate>::new("automod", mc_recorder(&seen, "no")).filter(|_| false), "test", None).unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("logging", mc_recorder(&seen, "no")).filter(|_| panic!("bad filter")), "test", None).unwrap();
    t.hooks()
        .register_hook(Hook::<MessageCreate>::new("levels", mc_recorder(&seen, "levels")).filter(|ctx| ctx.data.message.content == "hi"), "test", None)
        .unwrap();
    t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["levels"]);
}

#[tokio::test]
async fn a_hook_that_exceeds_its_timeout_is_abandoned_and_the_next_hook_runs() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    t.hooks()
        .register_hook(Hook::<MessageCreate>::new("automod", |_| futures::future::pending::<BotResult<()>>()).timeout_ms(50), "test", None)
        .unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("logging", mc_recorder(&seen, "logging")), "test", None).unwrap();
    let started = std::time::Instant::now();
    t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["logging"]);
    assert!(started.elapsed() < Duration::from_millis(DEFAULT_HOOK_TIMEOUT_MS), "the soft timeout applied");
}

#[tokio::test]
async fn the_context_carries_the_core_keys() {
    let t = TestApp::new();
    let ctx = t.run_hooks(msg_delete(42)).await;
    assert_eq!(ctx.event, HookPoint::MessageDelete);
    assert_eq!(ctx.guild_id, Some(GuildId::new(42)));
    assert!(!ctx.stopped());
    assert!(t.run_hooks(Ready {}).await.guild_id.is_none());
    assert!(t.run_hooks(Ready {}).await.get_settings().await.is_none(), "no guild: no settings");
}

#[tokio::test]
async fn register_hook_refuses_a_duplicate_event_id() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    t.hooks().register_hook(Hook::<Ready>::new("x", record(&seen, "a")), "test", None).unwrap();
    assert!(t.hooks().register_hook(Hook::<Ready>::new("x", record(&seen, "b")), "test", None).is_err());
    assert_eq!(t.hooks().list(HookPoint::Ready).len(), 1);
    // Hooks not in HOOK_ORDER sort after the listed ones, in registration order.
    t.hooks().register_hook(Hook::<Ready>::new("y", record(&seen, "y")), "test", None).unwrap();
    t.run_hooks(Ready {}).await;
    assert_eq!(*seen.lock().unwrap(), vec!["a", "y"]);
    t.hooks().clear();
    assert!(t.hooks().list(HookPoint::Ready).is_empty());
}

#[tokio::test]
async fn the_first_always_or_ungated_hook_starts_during_the_run_call() {
    let t = TestApp::new();
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    t.hooks()
        .register_hook(
            Hook::<GuildMemberAdd>::new("invites", move |_| {
                r.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .always(),
            "test",
            Some(ModuleKey::Invites),
        )
        .unwrap();
    let member: serenity::all::Member = serde_json::from_value(serde_json::json!({
        "user": { "id": "5", "username": "u", "discriminator": "0", "avatar": null },
        "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0, "guild_id": "1"
    }))
    .unwrap();
    let user = member.user.clone();
    let pending = t.run_hooks(GuildMemberAdd { guild_id: GuildId::new(1), member, user }); // deliberately not awaited
    assert_eq!(ran.load(Ordering::SeqCst), 1, "run(ctx) was called before the dispatch future was polled");
    pending.await;
}

#[tokio::test]
async fn guild_ready_skips_an_unavailable_guild_and_ensures_the_row_first() {
    let t = TestApp::new();
    let seen = Arc::new(AtomicUsize::new(0));
    let s = seen.clone();
    t.hooks()
        .register_hook(
            Hook::<GuildReady>::new("invites", move |_| {
                s.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
            .ungated(),
            "test",
            None,
        )
        .unwrap();
    t.set_guilds(StaticGuilds { unavailable: vec!["5".into()], shard_count: 1, ..Default::default() });
    run_guild_ready(&t, GuildId::new(5), GuildReadyReason::Startup).await;
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    t.set_guilds(StaticGuilds::available(["5"]));
    run_guild_ready(&t, GuildId::new(5), GuildReadyReason::Startup).await;
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    assert!(t.settings_store.row("5").is_some(), "the settings row was ensured");
    // The startup run counts for the guildAvailable cooldown.
    assert!(!run_guild_available(&t, GuildId::new(5)).await);
    t.set_guilds(StaticGuilds { unavailable: vec!["6".into()], shard_count: 1, ..Default::default() });
    assert!(!run_guild_available(&t, GuildId::new(6)).await, "an unavailable guild never runs");
}

#[tokio::test]
async fn guild_available_gate_one_rerun_per_cooldown_and_the_cap_holds() {
    let gate = ReadyGate::new(1, 60_000);
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    assert!(gate.run("g1", move || async move { r.fetch_add(1, Ordering::SeqCst); }).await);
    let r = runs.clone();
    assert!(!gate.run("g1", move || async move { r.fetch_add(1, Ordering::SeqCst); }).await, "inside the cooldown");
    gate.record("g2");
    let r = runs.clone();
    assert!(!gate.run("g2", move || async move { r.fetch_add(1, Ordering::SeqCst); }).await);
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    let gate = Arc::new(gate);
    let order = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let (g, o) = (gate.clone(), order.clone());
    let first = tokio::spawn(async move {
        g.run("a", move || async move {
            o.lock().unwrap().push("a start");
            let _ = rx.await;
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (g, o) = (gate.clone(), order.clone());
    let second = tokio::spawn(async move { g.run("b", move || async move { o.lock().unwrap().push("b start") }).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*order.lock().unwrap(), vec!["a start"], "b waits for the only slot");
    tx.send(()).unwrap();
    assert!(first.await.unwrap() && second.await.unwrap());
    assert_eq!(*order.lock().unwrap(), vec!["a start", "b start"]);
}

#[test]
fn handler_map_refuses_a_second_different_handler() {
    let reg = ComponentRegistry::new();
    let a = component_fn(|_| async { Ok(()) });
    let b = component_fn(|_| async { Ok(()) });
    assert!(reg.buttons.set("x", a.clone()));
    assert!(!reg.buttons.set("x", b));
    assert!(Arc::ptr_eq(&reg.buttons.get("x").unwrap(), &a));
    assert!(reg.buttons.set("x", a), "the same handler again is fine");
    assert_eq!(reg.buttons.len(), 1);
}

#[tokio::test]
async fn register_core_components_registers_noop_once_and_it_defers_the_update() {
    let t = TestApp::new();
    register_core_components(t.components());
    register_core_components(t.components());
    assert_eq!(t.components().buttons.prefixes(), vec!["noop"]);
    assert!(Arc::ptr_eq(&t.components().buttons.get("noop").unwrap(), &noop_handler()));
    bot_core::router::route_interaction(&t, FakeInteraction::button("noop").build()).await;
    assert_eq!(t.responses.names(), vec!["deferUpdate"]);
}

#[test]
fn registry_install_runs_the_js_boot_order() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let t = TestApp::new();
        let mut reg = Registry::new();
        reg.plugin(Plugin::new("polls", Some(ModuleKey::Polls)).button("poll", |_| async { Ok(()) }));
        reg.command(bot_core::CommandDef::new("utility", bot_core::SlashCommand::new("ping", "Ping"), None, |_| async { Ok(()) }));
        reg.command(bot_core::CommandDef::new("utility", bot_core::SlashCommand::new("ping", "Ping again"), None, |_| async { Ok(()) }));
        let report = t.install(reg);
        assert_eq!(report.commands, 1, "a duplicate name is skipped");
        assert!(report.command_errors.iter().any(|e| e.contains("Duplicate command name")));
        assert_eq!(report.plugins.components, 1);
        assert!(t.components().buttons.has("noop"), "core components registered");
        assert_eq!(t.commands().category("ping"), Some("utility"));
        let _: Interaction = FakeInteraction::slash("ping").build();
    });
}

#[tokio::test]
async fn a_hook_that_panics_before_returning_its_future_does_not_stop_the_dispatch() {
    let t = TestApp::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let fail: Option<u8> = std::hint::black_box(None);
    t.hooks()
        .register_hook(
            Hook::<MessageCreate>::new("automod", move |_| {
                let _x = fail.expect("unwrap before the async block");
                async { Ok(()) }
            })
            .ungated(),
            "test",
            None,
        )
        .unwrap();
    t.hooks().register_hook(Hook::<MessageCreate>::new("levels", mc_recorder(&seen, "levels")), "test", None).unwrap();
    t.run_hooks(mc_payload()).await;
    assert_eq!(*seen.lock().unwrap(), vec!["levels"]);
}

#[test]
fn persistent_prefixes_and_job_types_need_exactly_one_handler() {
    let reg = Registry::new();
    let problems = bot_core::testing::persistent_handler_problems(&reg);
    assert!(problems.iter().any(|p| p.contains("\"poll\"")));
    assert!(problems.iter().any(|p| p.contains("job type poll.end")));
    assert!(!problems.iter().any(|p| p.contains("\"noop\"")), "the core owns noop");
}
