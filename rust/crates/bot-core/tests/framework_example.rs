//! The snippets of `rust/docs/FRAMEWORK.md`, compiled and run, so the guide cannot drift from the API.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bot_core::collectors::CollectorFilter;
use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand, channel_type};
use bot_core::prelude::*;
use bot_core::testing::{FakeInteraction, TestApp};
use serde_json::json;
use serenity::all::Permissions;
use serenity::builder::{AutocompleteChoice, CreateInteractionResponseMessage};

// ── Section 2: feature state ──

#[derive(Default)]
struct TagMemo {
    by_guild: std::sync::Mutex<std::collections::HashMap<String, (Vec<String>, i64)>>,
}

// ── Section 3: a command ──

async fn execute(ctx: CommandCtx) -> BotResult {
    let memo = ctx.app.ext::<TagMemo>();
    memo.by_guild.lock().unwrap().insert("g".into(), (vec![ctx.get_string("question").unwrap_or_default()], 0));
    ctx.defer_reply(true).await?;
    let _ = ctx.edit_reply(serenity::builder::EditInteractionResponse::new().embed(success_embed("Poll Created", Some("Done."), None))).await;
    Ok(())
}

async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (_option, typed) = ctx.focused().unwrap_or_default();
    let _ = ctx.respond(vec![AutocompleteChoice::new(format!("match {typed}"), typed.clone())]).await;
    Ok(())
}

fn command() -> CommandDef {
    let data = SlashCommand::new("poll", "Create and manage polls")
        .default_member_permissions(Permissions::MANAGE_GUILD)
        .option(
            Opt::sub_command("create", "Create a poll")
                .option(Opt::string("question", "The question").required(true).max_length(200))
                .option(Opt::channel("channel", "Where to post").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                .option(Opt::integer("hours", "Duration").min_value(1).max_value(720))
                .option(Opt::string("mode", "Mode").choice("Single choice", "single").choice("Multiple", "multi"))
                .option(Opt::string("search", "Search").autocomplete(true)),
        )
        .option(Opt::sub_command_group("config", "Settings").option(Opt::sub_command("show", "Show the settings")));
    CommandDef::new("utility", data, Some(ModuleKey::Polls), |ctx| async move { execute(ctx).await })
        .permission_level(PermissionLevel::Member)
        .subcommand_permission("config show", PermissionLevel::Admin)
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}

// ── Section 5: a plugin ──

fn is_poll_message(_app: &App, id: serenity::all::MessageId) -> bool {
    id.get() != 1
}

async fn on_message_delete(_ctx: Arc<HookCtx<MessageDelete>>) -> BotResult {
    Ok(())
}

async fn on_channel_delete(_ctx: Arc<HookCtx<ChannelDelete>>) -> BotResult {
    Ok(())
}

async fn on_vote(ctx: ComponentCtx) -> BotResult {
    let (_prefix, _params) = ctx.parsed();
    if ctx.permission_level().await < PermissionLevel::Member {
        return Ok(());
    }
    ctx.defer_update().await?;
    Ok(())
}

async fn on_select(ctx: ComponentCtx) -> BotResult {
    let _ = ctx.values();
    Ok(())
}

async fn on_modal(ctx: ModalCtx) -> BotResult {
    let _ = ctx.value("poll-question");
    Ok(())
}

async fn end_poll(job: Job, ctx: JobCtx) -> BotResult<JobOutcome> {
    let _poll_id = job.payload["pollId"].as_str().map(str::to_string);
    let _ = (ctx.late_by_ms, ctx.attempt);
    Ok(JobOutcome::Done)
}

fn plugin() -> Plugin {
    Plugin::new("polls", Some(ModuleKey::Polls))
        .hook(
            Hook::<MessageDelete>::new("polls", |ctx| async move { on_message_delete(ctx).await })
                .always()
                .filter(|ctx| is_poll_message(&ctx.app, ctx.data.message_id)),
        )
        .hook(Hook::<MessageDeleteBulk>::new("polls", |_| async { Ok(()) }).always())
        .hook(Hook::<ChannelDelete>::new("polls", |ctx| async move { on_channel_delete(ctx).await }).always())
        .button("poll", |ctx| async move { on_vote(ctx).await })
        .select("poll-select", |ctx| async move { on_select(ctx).await })
        .modal("poll-modal", |ctx| async move { on_modal(ctx).await })
        .job(JobType::PollEnd, job_handler(|job, ctx| async move { end_poll(job, ctx).await }))
}

fn register(reg: &mut Registry) {
    reg.command(command());
    reg.plugin(plugin());
}

// ── Section 12: a reward provider ──

struct MyProvider;

#[async_trait]
impl RewardProvider for MyProvider {
    async fn grant(&self, _app: &App, _args: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(Some(50)))
    }
    async fn take(&self, _app: &App, _args: RewardArgs) -> BotResult<RewardResult> {
        Ok(RewardResult::ok(Some(0)))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("already there")]
struct AlreadyThere;

#[tokio::test]
async fn the_guide_snippets_work() {
    const G: &str = "900000000000000001";
    let t = TestApp::new().with_guilds([G]);
    let mut reg = Registry::new();
    register(&mut reg);
    let report = t.install(reg);
    assert!(report.plugins.errors.is_empty(), "{:?}", report.plugins.errors);
    assert!(report.command_errors.is_empty(), "{:?}", report.command_errors);
    bot_core::testing::assert_plugin_matches_golden(&{
        let mut r = Registry::new();
        r.plugin(plugin());
        r
    }, "polls");

    // Section 14: a command through the router.
    bot_core::router::route_interaction(
        &t,
        FakeInteraction::slash("poll").guild(G).subcommand("create").option("question", bot_core::command::option_type::STRING, json!("Why?")).build(),
    )
    .await;
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
    assert_eq!(t.responses.calls()[1].embed_title(), Some("✅  Poll Created"));

    // Section 8: settings.
    let app: &App = &t;
    let guild_id = G.to_string();
    let role = "700".to_string();
    app.settings().update_settings(&guild_id, SettingsPatch::new().set("pollsEnabled", true).set("modLogChannelId", "123")).await.unwrap();
    let r2 = role.clone();
    app.settings()
        .update_settings_with(&guild_id, move |cur| {
            let mut ids = cur.mod_role_ids.clone();
            if ids.contains(&r2) {
                anyhow::bail!(AlreadyThere)
            }
            ids.push(r2);
            Ok(SettingsPatch::new().set("modRoleIds", json!(ids)))
        })
        .await
        .unwrap();
    let again = app
        .settings()
        .update_settings_with(&guild_id, move |cur| {
            if cur.mod_role_ids.contains(&role) {
                anyhow::bail!(AlreadyThere)
            }
            Ok(SettingsPatch::new())
        })
        .await;
    assert!(again.unwrap_err().downcast_ref::<AlreadyThere>().is_some());
    let s = app.settings().peek_settings(&guild_id);
    assert!(is_module_enabled(s.as_deref(), Some(ModuleKey::Polls)));

    // Section 9: the cache.
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct MyShape {
        n: i64,
    }
    let key = format!("guild:{G}:polls");
    app.cache().set(&key, &MyShape { n: 1 }, 300).await;
    let hit: Option<MyShape> = app.cache().get_as(&key).await;
    assert_eq!(hit, Some(MyShape { n: 1 }));
    assert_eq!(app.cache().set_nx(&format!("{key}:cd"), &1, 30).await, Some(true));
    assert_eq!(app.cache().sliding_window(&format!("{key}:w"), 5_000, None).await, Some(1));

    // Section 11: scheduling.
    let ends_at = chrono::Utc::now() - chrono::Duration::seconds(1);
    app.scheduler()
        .schedule_job(
            ScheduleJob::new(G, JobType::PollEnd, ends_at)
                .dedupe_key("poll.end:p1")
                .payload(json!({ "pollId": "p1" }))
                .on_conflict(OnConflict::Earliest)
                .max_attempts(3),
        )
        .await
        .unwrap();
    assert!(app.scheduler().get_job("poll.end:p1").await.is_some());
    assert_eq!(app.scheduler().tick(app, false).await, 1);
    assert!(app.scheduler().get_job("poll.end:p1").await.is_none(), "end_poll returned Done");
    assert_eq!(app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some("poll.end:none".into()), ..Default::default() }).await, 0);

    // Section 12: rewards.
    app.rewards().register_provider(RewardKind::Coins, Arc::new(MyProvider), Some("economy")).unwrap();
    let r = app.rewards().grant(app, RewardKind::Coins, RewardArgs::new(serenity::all::GuildId::new(1), "5", 50).reason("giveaway")).await;
    assert!(r.ok);

    // Section 6: a collector flow (the command waits on a confirm button).
    let waiter = {
        let a = app.clone();
        tokio::spawn(async move {
            let mut collector = a.collectors().collect(CollectorFilter::message(serenity::all::MessageId::new(555)).prefixes(["confirm"]));
            match collector.next(Duration::from_secs(5)).await {
                Some(click) => {
                    click.defer_update().await.ok();
                    true
                }
                None => false,
            }
        })
    };
    while t.collectors().is_empty() {
        tokio::task::yield_now().await;
    }
    bot_core::router::route_interaction(&t, FakeInteraction::button("confirm:yes").message_id("555").build()).await;
    assert!(waiter.await.unwrap());

    // Section 5: dispatch an internal point.
    let ctx = app
        .run_hooks(LevelUp {
            guild_id: serenity::all::GuildId::new(1),
            member: None,
            user_id: serenity::all::UserId::new(5),
            old_level: 1,
            new_level: 2,
            channel_id: None,
            source: "message".into(),
        })
        .await;
    assert!(!ctx.stopped());
    let _ = CreateInteractionResponseMessage::new();
}

// ── Sections 15-26 (part B): compiled, not run (they need Discord or a database) ──

#[allow(dead_code)]
mod part_b {
    use bot_core::command::{CommandOption as Opt, SlashCommand, channel_type};
    use bot_core::custom_embed::{RenderOptions, build_template_modal, parse_template_modal, render_template, validate_template};
    use bot_core::interaction_helpers::{
        ConfirmOptions, Payload, PermTarget, await_confirm, check_assignable_role_cached, check_channel_usable, missing_bot_permissions, reply_error,
        require_bot_permissions, safe_defer, safe_reply,
    };
    use bot_core::new_targets::{NewOptions, new_channel_option, reuse_note};
    use bot_core::paginator::{PaginateOptions, paginate};
    use bot_core::prelude::*;
    use bot_db::query::delete_where;
    use bot_db::{COUNTING_STATE_INFO, CountingState, InsertBuilder, SelectBuilder, UpdateBuilder};
    use serde_json::json;
    use serenity::all::{ChannelId, GuildId, RoleId};

    fn data() -> SlashCommand {
        SlashCommand::new("counting", "A channel where members count up from 1, one number each.").option(
            Opt::sub_command("set", "Pick the counting channel and turn counting on.")
                .option(Opt::channel("channel", "The counting channel.").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                .option(new_channel_option("channel", "channel")),
        )
    }

    async fn replies(mut ctx: CommandCtx, guild_id: GuildId, channel_id: ChannelId, role_id: RoleId, pages: Vec<serenity::builder::CreateEmbed>) -> BotResult {
        if !safe_defer(&ctx.responder, false).await {
            return Ok(());
        }
        if !bot_core::new_targets::resolve_new_options(&mut ctx, NewOptions { required: &["channel"], private: &[] }).await {
            return Ok(());
        }
        let _channel = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
        let _note = reuse_note(&ctx);
        safe_reply(&ctx.responder, Payload::embed(success_embed("Saved", Some("Done."), None)).no_mentions()).await;
        reply_error(&ctx.responder, "Not Found", Some("No such tag."), true).await;
        if !require_bot_permissions(&ctx, &["ManageRoles"], None, true).await {
            return Ok(());
        }
        let usable = check_channel_usable(&ctx.app, guild_id, Some(channel_id), &["ViewChannel", "SendMessages", "EmbedLinks"]);
        let check = check_assignable_role_cached(&ctx.app, guild_id, role_id, ctx.member(), false);
        let _ = (usable.ok, check.ok, missing_bot_permissions(&ctx.app, PermTarget::Channel(guild_id, channel_id), &["AddReactions"]));
        let ok = await_confirm(&ctx, ConfirmOptions { title: Some("Delete every tag?".into()), ..Default::default() }).await;
        if ok {
            paginate(&ctx, &pages, PaginateOptions::default()).await?;
        }
        Ok(())
    }

    async fn post(app: &App, guild_id: &str, channel_id: ChannelId, embed: serenity::builder::CreateEmbed) {
        let Some(http) = app.http() else { return };
        if let Err(e) = channel_id.send_message(http.as_ref(), Payload::embed(embed).no_mentions().to_message()).await
            && !bot_core::discord::is_gone(&e)
        {
            bot_util::log_warn!(format!("[counting] {guild_id}: could not post: {}", bot_core::discord::error_message(&e)));
        }
    }

    async fn templates(app: &App, modal_ctx: &ModalCtx, tpl: serde_json::Value, guild_id: GuildId, channel_id: ChannelId) -> BotResult {
        let _check = validate_template(&tpl, true);
        let user = modal_ctx.user().clone();
        let member = modal_ctx.member().cloned();
        let ctx = bot_core::adapters::render_context(app, Some(guild_id), &user, member.as_ref(), Some(channel_id));
        if let (Some(r), Some(http)) = (render_template(&tpl, &ctx, RenderOptions::default()), app.http()) {
            channel_id.send_message(http.as_ref(), r.to_payload().to_message()).await?;
        }
        let _modal = build_template_modal("welcome-tpl:edit", Some("Edit the welcome message"), &tpl, false)?;
        let (_template, _errors) = parse_template_modal(|id| modal_ctx.value(id), &tpl, true);
        Ok(())
    }

    async fn sql(app: &App, guild_id: String, channel: String) -> BotResult {
        app.settings().ensure_guild_settings(&guild_id).await?;
        let _row: CountingState = InsertBuilder::new(&COUNTING_STATE_INFO)
            .value("guildId", json!(guild_id))?
            .values([("channelId", json!(channel)), ("count", json!(0))])?
            .on_conflict_update(&["guildId"], [("channelId", json!(channel)), ("count", json!(0))])?
            .build()?
            .fetch_one(app.db())
            .await?;
        let _row: Option<CountingState> = SelectBuilder::new(&COUNTING_STATE_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_optional(app.db()).await?;
        UpdateBuilder::new(&COUNTING_STATE_INFO).set("count", json!(5))?.filter("guildId", json!(guild_id))?.build()?.execute(app.db()).await?;
        delete_where(&COUNTING_STATE_INFO, [("guildId", json!(guild_id))])?.execute(app.db()).await?;
        let gid = guild_id.clone();
        bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
            Box::pin(async move {
                bot_db::lock_guild_settings(tx, &gid).await?;
                bot_db::advisory_xact_lock(tx, &format!("infraction:{gid}")).await?;
                let sql = format!(r#"UPDATE "CountingState" SET "count" = LEAST("count"::bigint + $1, 999999999)::int, "updatedAt" = {} WHERE "guildId" = $2"#, bot_db::SQL_NOW);
                sqlx::query(sqlx::AssertSqlSafe(sql)).bind(1_i64).bind(&gid).execute(&mut *tx).await.map_err(bot_db::DbError::from)?;
                Ok::<_, bot_db::DbError>(())
            })
        })
        .await?;
        Ok(())
    }

    fn hooks() -> Plugin {
        Plugin::new("starboard", Some(ModuleKey::Starboard)).hook(
            Hook::<MessageReactionAdd>::new("starboard", |ctx| async move {
                let Some(_message) = ctx.data.ensure_message(&ctx.app).await else { return Ok(()) };
                let _member = ctx.data.get_member(&ctx.app).await;
                Ok(())
            })
            .filter(|ctx| ctx.data.emoji_key == "⭐"),
        )
    }

    fn shutdown(app: &App) {
        bot_core::client::on_shutdown(app, 20, "log queue flush", 4000, "Log queue was not flushed before shutdown", || async { Ok(()) });
    }

    #[test]
    fn the_part_b_snippets_build_payloads() {
        assert_eq!(data().to_json()["options"][0]["options"][1]["name"], "new_channel");
        assert_eq!(hooks().hooks.len(), 1);
    }
}
