# feat-dashboard-tasks: public API (from its author)

All ten JS files are ported to rust/crates/feat-dashboard-tasks/src:
- shared.js -> shared.rs: constants, task_result_key, task_dedupe_key, is_task_kind (TASK_KIND_RE), TaskError, and TaskResultValue (the Redis value {ok,message,data}).
- index.js -> index.rs: TASKS (22 kinds, in JS order), TaskMap, resolve_actor, execute_task (test-double map, as the JS), run_dashboard_task, clamp_message.
- channelAccess.js -> channel_access.rs; members.js -> members.rs; moderation.js -> moderation.rs; giveaways.js -> giveaways.rs; polls.js -> polls.rs; reactionRoles.js -> reaction_roles.rs; tickets.js -> tickets.rs.
- plugins/dashboard-tasks.js -> plugin.rs (module null, job dashboard.task).
- util.rs holds the shared JS-semantics helpers: SNOWFLAKE_RE, Number.isInteger, String() coercion, guild/member/channel/role lookups, permission level.
- Every task calls the sibling crates' services: feat-levels xp_service, feat-moderation moderation_service, feat-giveaways-polls giveaway_service and poll_service, feat-reactionroles reaction_role_service and role_template_setup, feat-tickets ticket_service.
- The result-to-text mappings are pure public functions, checked against JS output.

Fixtures:
- rust/fixtures/gen/feat-dashboard-tasks-golden.mjs writes rust/fixtures/golden/feat-dashboard-tasks/golden.json. It holds the kinds, constants and keys, a TASK_KIND_RE sweep, sweeps of readActionPayload, readXpPayload and readAccessPayload, and the mod.action, xp.adjust, giveaway end/reroll/start/restart and poll start/end texts over stubbed JS service results. It also holds executeTask's refusals.
- rust/fixtures/gen/feat-dashboard-tasks-js.mjs is the JS side of the cross-language tests: the real dashboard runner from src/dashboard/tasks.js, the real JS bot runDashboardTask, and the JS services that seed and read rows.

Tests: tests/offline.rs has 25 and tests/db.rs has 13.
- Cross-language, JS writes and Rust reads: the full JS createTaskRunner().run queues a job, the Rust interactive scheduler runs it, and the JS runner reads 'done' with Rust's Redis result.
- JS-queued refusals, unknown kinds and permission failures come back through the FAILED row's lastError.
- Cross-language, Rust writes and JS reads: a job Rust queues with the shared constants runs under the JS bot, and Rust reads the JS-written Redis result.
- Domain round trips:
  - JS-written infractions are edited and pardoned by Rust; Prisma reads the result back.
  - JS-written XP is changed by Rust.
  - A JS-started giveaway is cancelled by Rust, and a JS-posted poll is deleted by Rust.
  - The rr and ticket panel rows that Rust writes are read by Prisma.
  - A JS-queued ticket.managersync job is merged by the Rust ticket.refresh.

## API

Entry point: feat_dashboard_tasks::register(&mut Registry), which adds only the dashboard-tasks plugin.

Protocol, in shared:
- TASK_JOB_TYPE, TASK_RESULT_TTL_SECONDS (900), TASK_MESSAGE_MAX (500)
- task_result_key(job_id) -> "dashboard:task:{id}"
- task_dedupe_key(uuid) -> "dashboard.task:{uuid}"
- is_task_kind(kind), the TASK_KIND_RE check
- TaskResultValue { ok, message, data }: the Redis value; read it with app.cache().get_as::<TaskResultValue>(key)
- TaskError(String) and task_error(msg), for a refusal the admin sees
- A dashboard queues ScheduleJob::new(guild, JobType::DashboardTask, now).payload({kind, actorId, payload}).dedupe_key(task_dedupe_key(uuid)).max_attempts(1), the same as tasks.js.

Runner, in index:
- tasks() -> &TaskMap, with kinds() and get(kind)
- TaskMap::from_groups
- DashboardTask::new(async fn(TaskCtx) -> anyhow::Result<TaskOutcome>)
- TaskCtx { app, guild_id, actor: Actor { id, raw_id, member, is_bot_owner }, payload, task_id }
- TaskOutcome { message, data }
- resolve_actor(app, guild, actor_id)
- execute_task(&TaskMap, &Job, &App, guild_id): stores the result, or returns a PermanentJobError with the JS text
- run_dashboard_task(job, ctx), clamp_message

Per-area public task functions, each taking a TaskCtx:
- channel_access::access_channels
- members::adjust_xp
- moderation::{mod_action, edit_case, clear_warnings}
- giveaways::{start, end, reroll, cancel, restart, odds}
- polls::{start, end, remove}
- reaction_roles::{publish, repost, remove, role_template}
- tickets::{check, publish, refresh, remove}
Each module's tasks() lists its kinds.

Pure helpers:
- Payload readers: read_access_payload, read_xp_payload, read_action_payload (ActionPayload), read_edit_payload.
- Moderation text: action_message(ActionResult), edit_notes, pardon_reason, pardon_message.
- XP text: change_text, give_message, take_message, set_message, reset_message.
- Giveaway text: end_text, reroll_text, start_failure_text, restart_failure_text, name_list_with, plain, end_time, odds_plan, is_cuid.
- Poll text: post_failure_text, start_message, end_text, duration_error_text, embed_text.
- Reaction roles: needs_for, emojis_to_remove, reaction_failures, is_panel_id, UNPOSTED.
- Tickets: CATEGORY_PERMS, TRANSCRIPT_PERMS, MAX_CATEGORIES.
- Constants: XP_OPS, XP_STEP_MAX, DASHBOARD_MOD_ACTIONS, COMMAND_REASON_MAX, ACCESS_NEEDS, ACCESS_CHECKS_MAX.

## Known gaps

- channel_access.rs access_channels: when the serenity cache has no guild but the channel was found over REST, the check fails closed: every need counts as missing, and the guild owner and Administrators get no exemption. In the JS the guild object always exists. This difference is intentional, because it is a privacy gate.
- polls.rs remove (polls.js poll.delete): the message is deleted over REST (feat-giveaways-polls DiscordOps) without fetching the channel first. markBotDeletion runs even when the JS would have failed at guild.channels.fetch. The outcome is the same: a gone channel or message counts as already deleted.
- index.rs clamp_message and the unknown-kind text (index.js clampMessage, String(kind).slice(0,40)): bot_util's utf16_prefix never splits a surrogate pair, and JS slice can.
- moderation.rs edit_case (moderation.js mod.editCase): the case-log notes go through feat-moderation post_mod_log. That function reads the log channel from the serenity cache only (a gap in feat-moderation), so an uncached log channel gets no note. mod.action also inherits feat-moderation's documented gaps.
- giveaways.rs and polls.rs: they inherit feat-giveaways-polls' gaps. The REST resolver replaces the gateway chunk request in end/reroll. A channel that is neither cached nor fetchable is refused.
- reaction_roles.rs: inherits feat-reactionroles' gaps. Role verification uses REST and does not fill the cache. fetch_panel_message always goes to REST. Template roles are created with an explicit REST body.
- tickets.rs: inherits feat-tickets' gaps. publish_panel edits without fetching first. A refresh of a channel found only over REST skips the bot-permission check.
- util.rs fetch_member (index.js resolveActor, guild.members.fetch): a member that is not cached is fetched over REST GET /guilds/{g}/members/{u}, not over the gateway. A failure counts as 'not a member', as in the JS .catch(() => null).
