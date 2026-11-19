# Writing a feature crate

This is the guide for porting one group of modules (`src/commands/<category>/*.js`, `src/plugins/<id>.js`,
`src/services/*.js`) into a `feat-*` crate. Read `rust/PORTING.md` first: it sets the rules; this file shows how to
follow them with `bot-core`. The JS is the specification: copy every user-visible string verbatim.

```rust
use bot_core::prelude::*;
```

brings in the types used below. The rustdoc of each `bot_core` module names the JS file it ports.

| JS | Rust |
|---|---|
| `client` (and module-level singletons) | `bot_core::App` (`ctx.app` everywhere) |
| `prisma` | `app.db()` (a `PgPool`, used through `bot-db`) |
| `cache` (redis.js) | `app.cache()` |
| settingsService.js | `app.settings()` |
| scheduler.js | `app.scheduler()` |
| rewards.js | `app.rewards()` |
| `runHooks(event, fields)` | `app.run_hooks(payload).await` |
| `client.commands` / `client.buttonHandlers` ... | `app.commands()` / `app.components()` |
| embedBuilder.js | `bot_core::embed_builder` (`error_embed`, `success_embed`, ...) |
| permissionChecker.js | `bot_core::permission_checker` and `ctx.permission_level().await` |
| logger.js | `bot_util::log_info!`, `log_warn!`, `log_error!`, `log_debug!`, `log_success!` |
| `test/*.test.js` helpers | `bot_core::testing` |

## 1. The crate and its one entry point

The JS discovers files on disk. Rust needs an explicit registry: every feature crate exposes exactly one function.

```rust
// crates/feat-polls/src/lib.rs
mod commands;   // commands/poll.rs  <- src/commands/utility/poll.js
mod plugin;     // plugin.rs         <- src/plugins/polls.js
mod service;    // poll_service.rs   <- src/services/pollService.js

pub fn register(reg: &mut bot_core::Registry) {
    reg.command(commands::poll::command());
    reg.plugin(plugin::plugin());
}
```

The bot binary calls every crate's `register`, then `Registry::install(&app)`. That validates the plugins
(`collectPlugins`: a bad entry is dropped and logged, never fatal), installs hooks, components (module-gated), job
handlers and reward providers, runs each plugin's `init`, registers the core `noop` button, then loads the commands.
This is the JS boot order.

Mirror the JS file names in snake_case so a reviewer can compare the two files side by side.

## 2. The application context (`App`)

`App` is a cheap `Arc` clone, passed to every command, autocomplete, component, hook and job. There are no globals.

| Method | What |
|---|---|
| `app.db()` | the PostgreSQL pool |
| `app.cache()` | the never-failing cache (section 9) |
| `app.settings()` | GuildSettings (section 8) |
| `app.scheduler()` | ScheduledJob (section 11) |
| `app.rewards()` | the XP and coins bus (section 12) |
| `app.run_hooks(payload)` | dispatch a hook point (internal points `LevelUp`, `EconomyPurchase`) |
| `app.discord()` | `Option<&DiscordHandle>`: serenity `http` and `cache` once connected; `None` offline |
| `app.http()` | `Option<Arc<Http>>` |
| `app.collectors()` | live component collectors (section 6) |
| `app.clock()` | `Date.now()` (`now_ms()`, `now()`); tests move it |
| `app.config()` | `BOT_OWNER_ID`, `CLIENT_ID`, `DEPLOY_GUILD_ID`, `NODE_ENV` |
| `app.ext::<T>()` | per-app state of your feature (see below) |

**Feature state.** What the JS keeps in module-level variables goes in a `Default` struct fetched with
`app.ext::<MyState>()`. Examples are a 60 s memo, a per-guild throttle, or an index. You get one instance per app and
type. Never use `static` / `OnceLock` / `lazy_static` for mutable state: tests build many apps in parallel.

```rust
#[derive(Default)]
struct TagMemo { by_guild: std::sync::Mutex<std::collections::HashMap<String, (Vec<Tag>, i64)>> }

let memo = app.ext::<TagMemo>();
```

**Discord.** Read the guild, members, channels and roles from `app.discord()`'s serenity cache. Call Discord REST
through `d.http`. Both are absent offline: degrade the way the JS degrades when a fetch fails (log and skip, or answer
the user). Never `unwrap()` them.

More Discord helpers (bot-core):

* `app.ready_at_ms()` is `client.readyTimestamp` (set at the first shard's `ClientReady`; `None` before).
* `interaction_helpers::with_cached_guild(&app, id, |g| ..)` reads the cached guild without cloning it (`cached_guild`
  clones). The closure runs under the cache lock: never touch the serenity cache inside it.
* `interaction_helpers::fetch_channel(&app, guild, channel).await` is `guild.channels.fetch(id)`: the cache, else REST,
  then cached; a channel of another guild is `Err(Unowned)`. `resolve_channel` is the same as an `Option`.
* `discord::cache_created_channel(&app, &channel)` / `cache_created_role(&app, guild, &role)` put what the bot just
  created (or fetched) over REST into the cache (`channels._add`); `new_targets` already does this for what it creates.
* `guild_members::fetch_members_by_ids(&app, guild, &ids, time_ms).await` is `guild.members.fetch({ user: ids, time })`
  over the gateway (`REQUEST_GUILD_MEMBERS`, 100 ids per request, cached); without a running gateway it falls back to
  one REST call per id.
* `discord::ban_with_delete_seconds(http, guild, user, Some(secs), reason)` is `guild.bans.create(user,
  { deleteMessageSeconds, reason })`; `discord::fetch_bans_page(http, guild, 1000, cursor)` pages bans 1000 at a time;
  `discord::set_role_positions(http, guild, &[(role, pos)], reason)` is the whole-body `PATCH /guilds/{g}/roles`;
  `discord::encode_audit_reason` is discord.js's `X-Audit-Log-Reason` encoding.
* `interaction_helpers::components_to_builders(&message.components)` turns a message's rows back into builders
  (`ActionRowBuilder.from`), e.g. to resend them disabled.
* `adapters`: `ImageUrlOptions` + `user_display_avatar_url_with` / `member_display_avatar_url_with` /
  `guild_icon_url_with` are discord.js's `displayAvatarURL(options)` / `iconURL(options)`;
  `display_avatar_url_default` and `guild_icon_url_default` are the no-argument forms (webp, gif when animated);
  `member_static_png_avatar_url` is `{ extension: 'png', forceStatic: true, size: 256 }`.
  `clean_content(&app, &message)` / `clean_content_in(&app, guild, text)` are `message.cleanContent`.
* `markdown::escape_markdown_with(text, EscapeOptions { heading: true, ..Default::default() })` takes every
  `escapeMarkdown` option (`escape_markdown` is the defaults).
* `clock::clock_throttle(app.clock(), ms)` is `createThrottle(ms)` on the app clock (tests move it with the
  `ManualClock`).

## 3. Commands

### Declaring

A command is a `CommandDef`: the category (the JS folder under `src/commands/`, used by `/help`), the data, the
module, and `execute`. The data builder produces exactly what discord.js's `toJSON()` produced, key order included. The
deploy hash depends on it: if the payload is identical, switching from the JS bot does not redeploy commands.

```rust
use bot_core::command::{CommandDef, CommandOption as Opt, SlashCommand, channel_type};
use serenity::all::Permissions;

pub fn command() -> CommandDef {
    let data = SlashCommand::new("poll", "Create and manage polls")
        .default_member_permissions(Permissions::MANAGE_GUILD) // only for ADMIN commands, as the JS does
        .option(
            Opt::sub_command("create", "Create a poll")
                .option(Opt::string("question", "The question").required(true).max_length(200))
                .option(Opt::channel("channel", "Where to post").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
                .option(Opt::integer("hours", "Duration").min_value(1).max_value(720))
                .option(Opt::string("mode", "Mode").choice("Single choice", "single").choice("Multiple", "multi")),
        )
        .option(
            Opt::sub_command_group("config", "Settings")
                .option(Opt::sub_command("show", "Show the settings")),
        );
    CommandDef::new("utility", data, Some(ModuleKey::Polls), |ctx| async move { execute(ctx).await })
        .permission_level(PermissionLevel::Member)                         // permissionLevel
        .subcommand_permission("config show", PermissionLevel::Admin)      // subcommandPermissions { 'config show': 2 }
        .autocomplete(|ctx| async move { autocomplete(ctx).await })
}
```

* Add options in the JS order. Option types: `string`, `integer`, `number`, `boolean`, `user`, `channel`, `role`,
  `mentionable`, `attachment`, `sub_command`, `sub_command_group`. Option methods: `.required(true)`,
  `.autocomplete(true)`, `.choice(name, value)` (string), `.num_choice(name, n)`, `.min_value`, `.max_value`,
  `.min_length`, `.max_length`, `.channel_types(&[..])`, `.option(..)`.
* `module` is required and explicit: `Some(ModuleKey::X)` or `None` for a core command.
* `.gated(false)` for ADMIN config commands (the JS `gated = false`). `.allow_in_dms()` only for DM-safe commands.
  Every other command is registered for servers only and fails closed outside a cached guild.
* The router applies the one gate (`checkCommandAccess`) before `execute`: the guild guard, then the module toggle,
  then the permission level, where `'group sub'` beats `'sub'` beats `permission_level`. Autocomplete gets the same
  gate and answers `[]` on any refusal. Do not repeat these checks.

### What `execute` gets

`CommandCtx` derefs to its `Responder`:

| Field or method | JS |
|---|---|
| `ctx.app` | `client` |
| `ctx.interaction` | the serenity `CommandInteraction` |
| `ctx.guild_id()`, `ctx.channel_id()`, `ctx.user()`, `ctx.member()` | `interaction.guildId` ... |
| `ctx.subcommand()`, `ctx.subcommand_group()` | `options.getSubcommand(false)` ... |
| `ctx.get_string("x")`, `get_integer`, `get_number`, `get_boolean`, `get_channel_id`, `get_role`, ... (section 17) | `options.getString('x')` ... |
| `ctx.options()` | resolved leaf options (users, members, roles, channels, attachments) |
| `ctx.settings` | the settings the router loaded for its gate (only when it needed them) |
| `ctx.permission_level().await` | `resolvePermissionLevel(member, settings)` |
| `ctx.ack_budget_ms()` | `ackBudgetMs(interaction)` |
| `ctx.channel_nsfw()` | `interaction.channel?.nsfw` (a thread: its parent's), from the cache; `None` when uncached |

### Replying and deferring

Every answer goes through the `Responder` (or the helpers of section 16 built on it). It follows discord.js's
`replied` / `deferred` / `ephemeral` state machine exactly, because the router's error reply and the collector grace
period read that state. **Never call serenity's `create_response` (or `.execute`) on the raw interaction.**

| discord.js | Responder |
|---|---|
| `reply({ content, ephemeral })` | `ctx.reply(CreateInteractionResponseMessage::new().content(..).ephemeral(true))`, `ctx.reply_text(s, ephemeral)`, `ctx.reply_embed(e, ephemeral)` |
| `deferReply({ ephemeral })` | `ctx.defer_reply(ephemeral)` |
| `editReply(opts)` | `ctx.edit_reply(EditInteractionResponse::new()...)` (returns the `Message`) |
| `followUp(opts)` | `ctx.follow_up(CreateInteractionResponseFollowup::new()...)` |
| `deleteReply()` / `fetchReply()` | `ctx.delete_reply()` / `ctx.fetch_reply()` |
| `deferUpdate()` / `update(opts)` | `ctx.defer_update()` / `ctx.update(..)` (components) |
| `showModal(modal)` | `ctx.show_modal(CreateModal::new(..))` |
| `respond(choices)` | `ctx.respond(vec![AutocompleteChoice::new(name, value)])` (at most 25) |

`reply`, the defers, `update` and `show_modal` fail with `AlreadyReplied` after an acknowledgement. `edit_reply`,
`follow_up` and `delete_reply` fail with `NotReplied` before one, like discord.js. Where the JS wrote
`.catch(() => {})`, write `let _ = ...await;`.

Acknowledge within 3 s. Bound any lookup before the acknowledgement with `ctx.ack_budget_ms()` and
`bot_util::async_util::with_timeout`, and use `app.settings().peek_settings(g)` where the JS uses `peekSettings`.

Embeds come from `bot_core::embed_builder` (`success_embed(title, Some(desc), None)`, `error_embed`, `info_embed`,
`warn_embed`, `neutral_embed`, `guild_embed`, `apply_guild_color`). They add the JS emoji prefixes, colours, timestamp
and clamping.

The client-wide default mentions (`{ parse: ['users'], repliedUser: false }`) are set on serenity's HTTP client by the
boot (`bot_core::discord::default_allowed_mentions()`). Set `allowed_mentions` yourself only where the JS does: for
example, roles via `pingableRoleIds` when an admin configured a ping role. Never parse `@everyone`.

### Errors

Return `Err` (`anyhow::bail!`, `?`) where the JS throws. The router logs `Error in command "<name>"` and answers the
JS "Command Error" embed, without overwriting a panel that was already acknowledged. A panic is caught the same way,
but do not rely on that: no `unwrap()` on anything that can be absent.

## 4. Autocomplete

```rust
async fn autocomplete(ctx: AutocompleteCtx) -> BotResult {
    let (option, typed) = ctx.focused().unwrap_or_default(); // getFocused(true)
    let choices = search(&ctx.app, ctx.guild_id(), &typed).await; // filter in the database, at most 25
    let _ = ctx.respond(choices).await;
    Ok(())
}
```

The router gates autocomplete like `execute` and answers `[]` on a refusal, so the handler never runs for a caller who
may not use the command. A command with an `autocomplete(true)` option must declare `.autocomplete(..)`; the
validation checks this.

`AutocompleteCtx` also reads the other options: `get_string("panel")` (the focused option gives its typed text),
`get_integer`, `get_number`, `get_boolean`, `get_user_id`, `get_channel_id`, `get_role_id`, `option(name)`, plus
`user()`, `member()`, `channel_id()`, `member_access()`, `permission_level().await` and `ack_budget_ms()`.

## 5. Plugins

`src/plugins/<id>.js` becomes a `Plugin`. `id` is exactly the JS file name (`reaction-roles`); `module` is the JS
`plugin.module`.

```rust
pub fn plugin() -> Plugin {
    Plugin::new("polls", Some(ModuleKey::Polls))
        .hook(Hook::<MessageDelete>::new("polls", |ctx| async move { on_message_delete(ctx).await })
            .always()
            .filter(|ctx| is_poll_message(&ctx.app, ctx.data.message_id)))   // sync, no I/O
        .hook(Hook::<MessageDeleteBulk>::new("polls", |ctx| async move { on_bulk_delete(ctx).await }).always())
        .hook(Hook::<ChannelDelete>::new("polls", |ctx| async move { on_channel_delete(ctx).await }).always())
        .button("poll", |ctx| async move { on_vote(ctx).await })
        .select("poll-select", |ctx| async move { on_select(ctx).await })
        .modal("poll-modal", |ctx| async move { on_modal(ctx).await })
        .job(JobType::PollEnd, job_handler(|job, ctx| async move { end_poll(job, ctx).await }))
}
```

### Hooks

* The payload type selects the event. Every hook point has one type in `bot_core::hooks::events` with exactly the
  fields the JS passes to `runHooks`. Examples: `MessageCreate { message, member, channel_id, channel }`,
  `GuildMemberUpdate { old_member, member, old_known }`, and
  `MessageReactionAdd` (`ensure_message(&app)`, `get_member(&app)`, `emoji_key`). Every type has a `guild_id`; the
  `client` and `guild` of the JS are `ctx.app` and `ctx.guild_id`.
* `ctx: Arc<HookCtx<E>>` has `ctx.data`, `ctx.app`, `ctx.guild_id`, `ctx.event`, `ctx.state`,
  `ctx.get_settings().await` (memoised, never fails), `ctx.stop(reason)` and `ctx.stopped()`. **Only automod and
  custom-commands call `stop`.**
* `ctx.state` is shared by every hook of one dispatch: `ctx.state.insert("invite", value)` and
  `ctx.state.get::<T>("invite")`. The keys in use are `invite`, `captchaPending`, `customCommand` and `automodAction`.
  The plugins that share a key must agree on the stored Rust type: put it in the crate that writes it and re-export it.
* Hook declarations:
  * The id must be listed in `HOOK_ORDER` for that event and be `"<plugin id>"` or `"<plugin id>:<name>"`.
  * `.filter(f)` is a cheap synchronous check. Use `app.settings().peek_settings(..)` there, never I/O.
    A panic in the filter skips the hook.
  * `.always()` runs whatever the module toggle says.
  * `.ungated()` is the JS `module: null`; `.module(ModuleRef::Module(k))` is a different module.
  * `.timeout_ms(n)` is the JS `timeoutMs`.
* Runner semantics, as in the JS:
  * hooks run in `HOOK_ORDER`;
  * a hook gated on a module is skipped unless the module is enabled, and no settings counts as disabled;
  * a failing or timed-out hook is logged and the next one runs; a timed-out hook is abandoned, not cancelled.
* Dispatch an internal point from your service:
  `app.run_hooks(LevelUp { guild_id, member, user_id, old_level, new_level, channel_id, source: "message".into() }).await;`

### Components and customIds

* Build customIds with `bot_config::custom_id!("poll", poll_id, option)?` (or `build_custom_id`).
  Parse them with `ctx.parsed()` -> `(prefix, params)`.
  Every prefix must be in `src/config/customIds.js` (`bot_config::CUSTOM_ID_PREFIXES`); a test scans all Rust sources
  for literal prefixes.
* Register only **persistent** prefixes your plugin owns, through `.button` / `.select` / `.modal`.
  `.component_module(ModuleRef::Ungated)` after one of them is the JS `module: null` on that component.
  A component with a module is wrapped in the module gate: memo first, a bounded load, and the JS notices.
* `ComponentCtx`:
  * `custom_id()`, `parsed()`, `values()` (selects of every kind), `interaction.message`;
  * `member_access()`, `permission_level().await`.
  * **Persistent components re-check permission levels themselves** (the router does not).
* `ModalCtx::value("field-id")` is `fields.getTextInputValue`.

## 6. Collector flows (paginator, confirm prompts, games)

A `collector` prefix (`paginator`, `confirm`, `rps`, `ttt`, `bj`, ...) has no registered handler. The router hands
every click on such a prefix to the app's `CollectorHub`. A live collector whose filter matches receives the same
`ComponentCtx` (the same acknowledgement state). After 2.5 s, if nothing acknowledged the click, the router answers
"This menu has expired.", as in the JS.

```rust
let msg = ctx.fetch_reply().await?;                      // the message the buttons are on
let mut collector = ctx.app.collectors().collect(
    CollectorFilter::message(msg.id).prefixes(["confirm"]).user(ctx.user().id));
match collector.next(Duration::from_secs(30)).await {  // awaitMessageComponent({ time: 30_000 })
    Some(click) => {
        click.defer_update().await.ok();                 // collectors acknowledge first
        // ...
    }
    None => { /* timed out: edit the reply as the JS does */ }
}
```

`await_component(filter, timeout)` is the one-shot form. A collector stops when dropped. Never use serenity's own
collectors: the router cannot see them.

## 7. Jobs

* A job handler gets `(job: Job, ctx: JobCtx)`:
  * `job.payload` is the JSON the JS stored; `job.payload_as::<T>()` deserializes it;
  * `ctx.app`, `ctx.guild_id`, `ctx.late_by_ms`, `ctx.attempt` (1-based).
* It returns `Ok(JobOutcome::Done)` (the row is deleted) or
  `Ok(JobOutcome::Reschedule { next_run_at, payload: Some(..) | None })`.
* A plain `Err` retries with back-off (30 s x 2^n, max 1 h) up to `max_attempts`.
  `Err(PermanentJobError::new("...").into())` fails at once.
* Handlers must be idempotent: a claim can repeat after a crash.
* The guild is cached and available when a handler runs. A guild that is not available makes an ordinary job wait,
  and an interactive one fail.

## 8. Settings

The only way to read or write `GuildSettings`. Never query the table yourself, and never touch `guild:{g}:settings`.

```rust
let s = app.settings().get_settings(&guild_id).await?;    // Result<Arc<GuildSettings>, SettingsError>
let s = app.settings().get_settings_safe(&guild_id).await; // Option, logs at most once a minute per guild
let s = app.settings().peek_settings(&guild_id);           // memo only, synchronous
app.settings().ensure_guild_settings(&guild_id).await?;    // before creating ANY per-guild row (FK)

// Scalars: field names are the Prisma names, values are JSON as the JS passed them.
app.settings().update_settings(&guild_id, SettingsPatch::new().set("pollsEnabled", true).set("modLogChannelId", id)).await?;

// Arrays and JSON: a row-locked read-modify-write; the closure is pure (no Discord calls, no awaits).
app.settings().update_settings_with(&guild_id, move |cur| {
    let mut ids = cur.mod_role_ids.clone();
    if ids.contains(&role) { anyhow::bail!(AlreadyThere) }   // your error comes back unchanged: downcast it
    ids.push(role);
    Ok(SettingsPatch::new().set("modRoleIds", serde_json::json!(ids)))
}).await?;
```

* Rows are `Arc<GuildSettings>`, shared and immutable (the JS deep freeze): clone a field to change it.
* `bot_core::settings::is_module_enabled(Some(&s), Some(ModuleKey::Polls))` is `isModuleEnabled(settings, 'polls')`.
* The row is cached in Redis with the JS format (`_cacheV` 2), so the JS and Rust bots read each other's copies.
* `app.settings().peek_or_load(id, ms).await` is `peekSettings(id) ?? withTimeout(getSettingsSafe(id), ms)`.
* `bot_db::GuildSettings::default()` has Prisma's column defaults (`guild_id` empty), for pure tests:
  `GuildSettings { guild_id: "1".into(), xp_rate: 20, ..Default::default() }`. `GuildSettings` implements
  `bot_config::ModuleFlags`, so `is_module_enabled_in(Some(&row), Some("polls"))` works on a typed row.

## 9. Cache

`app.cache()` never fails. It returns the JS fallbacks: `get` -> `None`, `set`/`del`/`hset`/`hdel` -> `false`,
`set_nx` -> `None` (Redis down), `incr`/`sliding_window`/`push_recent` -> `None`, `invalidate` -> the keys deleted so
far.

```rust
let hit: Option<MyShape> = app.cache().get_as(&key).await;   // a value that does not fit is a miss
app.cache().set(&key, &value, 300).await;                     // always pass a TTL (seconds, any number type)
match app.cache().set_nx(&key, &1, 30).await {                // fail-closed: == Some(true); fail-open: != Some(false)
    Some(true) => { /* first */ }
    _ => { /* cooldown, or Redis down */ }
}
let hits = app.cache().sliding_window(&key, 5_000, None).await;
```

* Key names are byte-identical to the JS (see the key table in `docs/claude/architecture.md`).
* Values must serialize to exactly what `JSON.stringify` wrote. Use the Prisma field names (`#[serde(rename_all =
  "camelCase")]` or explicit renames), dates as `2026-01-02T03:04:05.678Z` (`bot_db::serde_js::date`), and leave
  absent optional fields absent or `null` as the JS did.
* A value the JS wrote must read in Rust, and the reverse: test it (section 14).
* Redis is a cache only. A miss is never a deletion of data.

## 10. SQL with bot-db

* `app.db()` is the pool. For model rows use the `bot_db` builders (`InsertBuilder`, `UpdateBuilder`,
  `SelectBuilder`, `delete_where`) and the generated models (`bot_db::Poll`, ...). They apply Prisma's client defaults
  (`cuid()`, `@updatedAt`) and refuse unknown columns.
* Port raw SQL statement for statement, with `$n` binds:
  * `SQL_NOW` / `(now() AT TIME ZONE 'UTC')`, never a bare `now()` (a test scans every SQL string);
  * `::bigint` maths, and clamp Int columns with `bot_config::XP_MAX` / `COIN_MAX`;
  * bind times as `SqlValue::timestamp(dt)` or `dt.naive_utc()`, never a `DateTime<Utc>` (that is TIMESTAMPTZ).
* Transactions use `bot_db::transaction(app.db(), TxOptions::default(), |tx| Box::pin(async move { ... }))`. The JS
  `$transaction` default is 5 s; pass the JS `timeout` / `maxWait` when it sets them. Lock helpers are
  `lock_guild_settings`, `advisory_xact_lock(tx, key)` (the JS `lockSql`) and `FOR UPDATE SKIP LOCKED`.
* Export your raw statements as a `RAW_SQL`-style list (name, SQL) and run each one in a DB test inside a rolled-back
  transaction, as `test/db.test.js` does.

## 11. Scheduling

```rust
app.scheduler().schedule_job(
    ScheduleJob::new(guild_id, JobType::PollEnd, ends_at)
        .dedupe_key(format!("poll.end:{poll_id}"))     // the JS dedupe key, byte for byte
        .payload(json!({ "pollId": poll_id }))         // the JS payload shape (or .payload_of(&struct)?)
        .on_conflict(OnConflict::Earliest)             // Replace (default) | Keep | Earliest | merge(|cur, inc| ..)
        .max_attempts(3),
).await?;                                              // calls ensure_guild_settings first

app.scheduler().cancel_jobs(CancelJobs { dedupe_key: Some(key), ..Default::default() }).await;
let job = app.scheduler().get_job(&key).await;                 // Option<ScheduledJob>
let jobs = app.scheduler().get_jobs_by_keys(&keys).await;      // None on a database error
```

* Jobs the JS bot left in the table run under Rust: keep the type, the dedupe key and the payload shape identical.
* `merge` plus `settle_merged_job(&job, |live| next, run_at)` is the `ticket.managersync` pattern.
* Never use a timer loop for recurring work (the JS rule "no setInterval"); use a recurring job type that reschedules
  itself.
* `scheduler.try_get_job(key).await` returns the database error (the JS `getJob` throws); `get_job` logs it and
  returns `None`. Fail closed with `try_get_job`.

## 12. Rewards

```rust
let r = app.rewards().grant(&app, RewardKind::Coins, RewardArgs::new(guild_id, user_id, 50).reason("giveaway")).await;
if !r.ok { /* r.reason: invalid-amount | unavailable | module-disabled | error */ }
app.rewards().is_reward_available(RewardKind::Xp, settings.as_deref());
```

The levels and economy plugins provide the kinds with `.provides(RewardKind::Xp, Arc::new(MyProvider))`, where
`MyProvider` implements `RewardProvider` (`grant` / `take`).

## 13. Logging and async rules

* Log with `bot_util::log_info!` and the other `log_*!` macros, using the JS text, and
  `bot_core::error::log_arg(&err)` for an `anyhow::Error`. Never `println!`.
* Handler futures must be `Send + 'static`:
  * never hold a `std::sync::MutexGuard` across `.await`; compute first, then lock;
  * clone what you need into `async move`.
* Serialise per key with `bot_core::sync::KeyedMutex` (`createKeyedMutex`), kept in your `app.ext` state: its job is
  queued when `run` is called and the future it returns is `'static`, so a hook can return it (section 15). Bound slow
  work with `bot_util::async_util::with_timeout`. Fan out with `map_limit`.
* User URLs go through `bot_util::safe_fetch`, user regexes through `bot_util::safe_regex`, admin text through
  `bot_util::placeholder_parser`.
* `bot_util::async_util::Throttle::with_clock(ms, now_fn)` runs a throttle on any clock (`allow_at(key, now_ms)` takes
  the time explicitly); bot-core's `clock_throttle` wires the app clock.
* `bot_util::js::date_parse` is V8's whole `Date.parse` (ISO, then the legacy forms: zone names, offsets, month names);
  `bot_util::v8_date` holds the parser.
* `with_timeout` still needs a `'static` future (it keeps the work running after a timeout, as the JS does): clone what
  the future needs, or use `peek_or_load` for settings.

## 14. Testing

Tests are offline by default; database and Redis tests read `TEST_DATABASE_URL` / `TEST_REDIS_URL` and return early
when they are not set. Run them with the mock servers:
`eval "$(node rust/mockdb/mockdb.mjs db <your_crate>)"` (see PORTING.md).

### Offline: `TestApp`

```rust
use bot_core::testing::{FakeInteraction, TestApp};

#[tokio::test]
async fn refuses_a_member() {
    let t = TestApp::new().with_guilds(["900000000000000001"]);   // no guild is available until you say so
    let mut reg = bot_core::Registry::new();
    feat_polls::register(&mut reg);
    t.install(reg);
    bot_core::router::route_interaction(&t, FakeInteraction::slash("poll").guild("900000000000000001").subcommand("create")
        .option("question", bot_core::command::option_type::STRING, serde_json::json!("Why?")).build()).await;
    let call = &t.responses.calls()[0];            // what Discord would have received
    assert_eq!(call.method, "deferReply");
    assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
}
```

* `t.responses` (`RecordingBackend`) records every answer as JSON. Use `names()`, `calls()`, and on a call
  `.content()`, `.embed_title()`, `.embed_description()`, `.ephemeral()`, `.choices()`. `set_failing(true)`
  simulates an expired interaction.
* `t.settings_store` (rows, `put`, `row`, `set_available(false)` for a database outage), `t.job_store` (`all()`,
  `put`, `modify`, `claims()`), `t.cache_backend` (`raw`, `put_raw`, `pttl`, `keys`, `calls`, `set_available(false)`,
  `fail_ops(&["set"])`). `t.clock.advance(ms)` moves time for TTLs, the 30 s memo and back-off.
* `FakeInteraction` builds slash commands, autocompletes, buttons, selects, user selects and modals from Discord JSON:
  `.guild(id)`, `.user(id)`, `.permissions(bits)`, `.roles(&[..])`, `.subcommand(..)`, `.group(g, s)`,
  `.option(..)`, `.focused(..)`, `.channel(id)`, `.message_id(id)`.
* With `TestApp::new().with_discord_guilds(&[guild_create_json(g, owner, roles, members)])` the serenity cache has
  the guild, and permission levels come from it as in production.
* Dispatch hooks with `t.run_hooks(payload).await` and run jobs with `t.scheduler().tick(&t, false).await`.
* Collector flows: spawn the command's route, wait until the collector is registered (`t.collectors().len()`), then
  route the click with `FakeInteraction::button(..).message_id(..)`.

### With PostgreSQL and Redis: `DbTestApp` and cross-language tests

```rust
let Some(url) = bot_core::testing::database_url() else { return };
let t = DbTestApp::new(&url).await.with_guilds([G]);   // PostgreSQL stores, the system clock
t.reset_guilds(&[G]).await;                             // before and after: deletes the guild's rows (cascade)
```

Every crate with SQL or cache code proves compatibility both ways (PORTING.md):

* **JS writes, Rust reads.** Write `rust/fixtures/gen/<crate>-js.mjs`, which uses the JS bot's own services to create
  rows, cache entries and jobs. `rust/fixtures/gen/bot-core-js.mjs` is the model: the test variables become
  `DATABASE_URL` / `REDIS_URL`, and it prints one `RESULT <json>` line. Call it with
  `bot_core::testing::run_node("rust/fixtures/gen/<crate>-js.mjs", &[..])`, then read the data through your Rust code.
* **Rust writes, JS reads.** Write through Rust, then call the script to read it back through the JS service, and
  compare the JSON.
* Fixtures that do not need the servers (a rendered value, a cached shape) go in `rust/fixtures/golden/<crate>/`,
  produced by a script under `rust/fixtures/gen/`.

### The golden checks every crate runs

```rust
#[test]
fn commands_match_the_js() {
    let mut reg = bot_core::Registry::new();
    feat_polls::register(&mut reg);
    bot_core::testing::assert_commands_match_golden(&reg, &["poll"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "polls");
}
```

* `assert_commands_match_golden` checks:
  * the payload is byte-identical to `commands.json` (key order included: it is the deploy hash);
  * the metadata matches: `category`, `module`, `gated`, `permissionLevel`, `subcommandPermissions`, `allowInDMs`,
    `hasAutocomplete`;
  * Discord's limits;
  * the registry declares exactly the listed commands.
* `assert_plugin_matches_golden` checks the module and hooks against `plugins.json`. Hooks are compared on event, id,
  module (absent = inherited, `null` = ungated), always, timeoutMs and hasFilter, in order. It also checks components,
  job types, providers and init, and runs the plugin validation.
* Port the JS tests for your files (`test/<feature>.test.js`) wherever they test logic. Keep decisions, parsing and
  formatting in pure functions and test those directly.


## 15. The worked example: `feat-counting`

`rust/crates/feat-counting` is the reference port. Read it next to the JS files before you start your own crate.

| JS | Rust | What it shows |
|---|---|---|
| `src/services/countingService.js` | `src/counting_service.rs` | pure rules (`parse_count`, `evaluate_math`, `judge_count`, `current_state`), SQL through bot-db (`load_state`, `save_state` after `ensure_guild_settings`), the per-guild lock in `app.ext` |
| `src/plugins/counting.js` | `src/plugin.rs` | a gated hook with a sync filter that takes its lock *synchronously* (gateway order); the decision returned as plain `Action`s and sent afterwards; bot-permission checks before every Discord write; "gone" REST codes |
| `src/commands/admin/counting.js` | `src/commands/counting.rs` | an ADMIN config command (`.gated(false)`, public `safe_defer`), subcommands, `new_channel_option` + `resolve_new_options`, `check_channel_usable`, `update_settings`, `guild_embed` with fields |
| `test/counting.test.js` | `tests/counting.rs` | ported JS tests, golden checks, a parser sweep against Node, DB handler tests, the command through the router with an offline guild, both cross-language directions |
| (none) | `rust/fixtures/gen/feat-counting-js.mjs` | the JS side of the cross-language tests |

The bot needs only `pub fn register`. The modules are `pub` too, so other crates (the dashboard, another feature)
can call the service directly (section 24).

The hook, as every order-sensitive hook should be written:

```rust
pub fn on_message(ctx: Arc<HookCtx<MessageCreate>>) -> impl Future<Output = BotResult> + Send + 'static {
    let guild_id = ctx.data.guild_id.to_string();
    let message = CountMessage::of(&ctx.data.message);
    let (c, g, m) = (ctx.clone(), guild_id.clone(), message.clone());
    // The place in the guild's lock is taken here, before any await: the order hooks are called is the order
    // messages are judged.
    let judged = run_locked(&ctx.app, &guild_id, move || async move {
        let settings = c.get_settings().await;
        judge_message(&c.app, settings, &g, &m).await       // decides and saves; returns the actions
    });
    let app = ctx.app.clone();
    async move {
        if let Some(actions) = judged.await? {
            run_actions(&app, &guild_id, &message, actions).await;   // reactions and notes, outside the lock
        }
        Ok(())
    }
}

Hook::<MessageCreate>::new("counting", on_message).filter(|ctx| {
    looks_like_count(Some(&ctx.data.message.content))
        && in_counting_channel(&ctx.app, &ctx.data.guild_id.to_string(), &ctx.data.channel_id.to_string())
})
```

`run_locked` is `app.ext::<CountingLock>().0.run(guild_id, f)` with a `bot_core::sync::KeyedMutex`: the job is queued
when `run` is called and the returned future is `'static`. Use it (not `bot_util`'s, whose future borrows the key)
whenever a hook returns the locked future. The gateway calls `run_hooks` in gateway order, and the hook runner starts
the first hook during that call when it is ungated, `always`, or gated on a module the memoised settings show enabled
(section 18). A hook that queues itself synchronously therefore keeps the gateway order.

## 16. Replying: the interaction helpers

`bot_core::interaction_helpers` ports `interactionHelpers.js`. It works on any context: `CommandCtx`, `ComponentCtx`,
`ModalCtx` and `AutocompleteCtx` implement `Interactive` and deref to `Responder`.

```rust
use bot_core::interaction_helpers::{Payload, reply_error, safe_defer, safe_reply};

if !safe_defer(&ctx.responder, false).await { return Ok(()); }       // public deferReply; false = interaction gone
safe_reply(&ctx.responder, Payload::embed(success_embed("Saved", Some("Done."), None)).no_mentions()).await;
reply_error(&ctx.responder, "Not Found", Some("No such tag."), true).await;   // an ephemeral error
```

* `safe_reply` picks the route from the state, like the JS:
  * not acknowledged: `reply`;
  * after `deferReply`: `editReply`, falling back to `followUp` when the original was deleted (10008);
  * after a reply, `deferUpdate` or `update`: `followUp` (the clicked panel is never overwritten).

  Gone interactions (10062, 40060, 10015, 50027) are logged at debug level. It returns `Option<Sent>`;
  `safe_reply_message` returns the `Message` (it fetches `@original` after a reply).
* `Payload` is `{ content, embeds, components, files, ephemeral, allowedMentions }`. Unset fields are left out, so an
  edit keeps them.
  * Builders: `Payload::text(s)`, `Payload::embed(e)`, `.content(..)`, `.embeds(vec)`, `.components(rows)`,
    `.file(CreateAttachment)`, `.ephemeral(true)`, `.allowed_mentions(..)`, `.no_mentions()` (`{ parse: [] }`).
  * Serenity builders: `to_reply()`, `to_edit()`, `to_follow_up()`, `to_update()`, `to_message()`,
    `to_message_edit()`.
* Ephemeral: `reply` and `followUp` take `.ephemeral(true)`; an `editReply` keeps the visibility of the defer.
  Moderation commands `safe_defer(.., true)`, then `delete_reply()` and a public `follow_up`. Admin config commands use
  a public defer (counting).
* Modals: `ctx.show_modal(CreateModal::new(id, "Title").components(rows))` before any other answer. Read the submit
  with `ModalCtx::value("field-id")`. Message templates have ready-made modals (section 21).
* Discord REST outside the interaction (a channel message, a reaction, a role) goes through `app.http()`, which is
  `None` offline: degrade.
  * `bot_core::discord::is_gone(&err)` covers the "gone" codes (10003, 10008, 10011, 50001, 50013): degrade and clean
    up, never retry.
  * `discord::error_code(&err)` and `error_message(&err)` read the rest.

```rust
let Some(http) = app.http() else { return };
if let Err(e) = channel_id.send_message(http.as_ref(), Payload::embed(embed).no_mentions().to_message()).await
    && !bot_core::discord::is_gone(&e)
{
    bot_util::log_warn!(format!("[counting] {guild_id}: could not post: {}", bot_core::discord::error_message(&e)));
}
```

**Mentions.** The HTTP client's default is `{ parse: ['users'], repliedUser: false }`.
* Notes and logs that quote user text use `.no_mentions()`.
* Ping a role only where an admin configured one: filter with `pingable_role_ids` (section 19) and pass
  `CreateAllowedMentions::new().roles(allowed)`.
* Never `everyone(true)`.

## 17. Options: declaring and reading

Declare options with `CommandOption` (section 3). The JSON key order is discord.js's, so
`assert_commands_match_golden` passes byte for byte. Read them in `execute`:

| discord.js | `CommandCtx` |
|---|---|
| `getSubcommandGroup(false)` / `getSubcommand(false)` | `subcommand_group()` / `subcommand()` |
| `getString` / `getInteger` / `getNumber` / `getBoolean` | `get_string` / `get_integer` / `get_number` / `get_boolean` |
| `getUser` / `getMember` | `get_user_id`, `get_user` (resolved `User`), `get_member` (a full `Member`) |
| `getChannel` | `get_channel_id`, `get_channel` (a `PartialChannel`; the full one is `interaction_helpers::cached_channel(app, g, id)`) |
| `getRole` | `get_role_id`, `get_role` |
| `getMentionable` / `getAttachment` | `get_mentionable_id`, `get_attachment` |
| `options.get(name) !== null` | `has_option(name)` |

**"Or create a new one" companions** (`newTargets.js`). Declare the companion right after its picker, and call
`resolve_new_options` after the defer, before you read the picker. It does three things:
* reuses an exact-name match, unless the role has dangerous permissions, or the option is `private` and more people
  can see the channel than a new one would allow;
* otherwise creates the role or channel;
* rewrites the option, so `get_channel_id("channel")` returns the result.

```rust
Opt::sub_command("set", "Pick the counting channel and turn counting on.")
    .option(Opt::channel("channel", "The counting channel.").channel_types(&[channel_type::GUILD_TEXT, channel_type::GUILD_ANNOUNCEMENT]))
    .option(new_channel_option("channel", "channel"))          // new_role_option("role") for a role picker

if !resolve_new_options(&mut ctx, NewOptions { required: &["channel"], private: &[] }).await { return Ok(()); }
let channel = ctx.get_channel_id("channel").ok_or_else(|| anyhow::anyhow!("Required option \"channel\" not found."))?;
// For a private option, `reuse_note(&ctx)` is the "Reused the existing ..." line to add to the reply.
```

## 18. Hooks, event by event

`bot_core::gateway` ports every gateway event, with the JS pre-filters. The payload types are in
`bot_core::hooks::events`, which the prelude re-exports. The JS `client` and `guild` are `ctx.app` and `ctx.guild_id`;
read the guild from `app.discord()`'s cache.

| Point | Payload fields | When it reaches the hooks (pre-filter) |
|---|---|---|
| `ready` | `Ready {}` | once per shard, after its READY guilds arrived (or after 15 s) |
| `guildReady` | `guild_id, reason` (`Startup`/`Join`/`Available`) | startup (4 at a time), a join, or a guild back from an outage (2 at a time, 10 min cooldown); the settings row exists by then |
| `guildDelete` | `guild_id, guild` | the bot left (not an outage); the cache keys are purged and recurring jobs cancelled first |
| `messageCreate` | `message, member, channel_id, channel` | a real user message (default or reply) in a guild |
| `botMessage` | `message, channel_id, channel, edited, was_loading` | another bot's or a webhook's guild message, before the type filter; also their edits |
| `ownMessage` | `message, channel_id, channel` | the bot's own guild messages |
| `directMessage` | `message, channel_id, user` | a DM from a user (`guild_id` is `None`) |
| `messageUpdate` | `old_message, message, old_known, member, channel_id, channel` | a content change of a user message (or a fresh edit of an uncached one) |
| `messageDelete` | `message_id, channel_id, message, partial, channel` | any guild message, bots included (`message` when it was cached) |
| `messageDeleteBulk` | `channel_id, message_ids, messages, channel` | a guild channel |
| `messageReactionAdd` / `Remove` | `ReactionEvent`: `reaction, user, emoji_key`, `ensure_message(&app)`, `get_member(&app)` | not the bot, not a bot user, a cached guild |
| `messageReactionRemoveAll` / `RemoveEmoji` | `channel_id, message_id, reactions` / `reaction, emoji_key`, both with `ensure_message(&app)` | a cached guild |
| `voiceStateUpdate` | `old_state, new_state, member, change` (`join`/`leave`/`move`/`state`) | not a bot |
| `guildMemberAdd` / `Remove` / `Update` | `member, user` / `member` (`None` when uncached)`, user` / `old_member, member, old_known` | every member, bots included; never diff when `old_known` is false |
| `presenceUpdate` | `old_presence, new_presence, member` | a cached, non-bot member; filter cheaply, it is the busiest event |
| `userUpdate` | `old_user, user, member` | a cached user's name, avatar or flags changed; once per guild that has the member |
| `channel*`, `thread*`, `role*`, `emoji*` | as named (`old_*` when it was cached) | `threadCreate` only for a newly created thread |
| `guildBanAdd` / `Remove`, `guildUpdate`, `invite*`, `guildScheduledEvent*` | as named | invites only for a cached guild |

```rust
Hook::<MessageReactionAdd>::new("starboard", |ctx| async move {
    let Some(message) = ctx.data.ensure_message(&ctx.app).await else { return Ok(()) };   // fetched once per dispatch
    let member = ctx.data.get_member(&ctx.app).await;
    // ...
    Ok(())
})
.filter(|ctx| ctx.data.emoji_key == "⭐")
```

Automod records what it did in the dispatch state with `ctx.state.set_automod_action(&AutomodAction { rule, deleted })`
(`ctx.state.automodAction = { rule, deleted }`); later hooks read `ctx.state.automod_action()` and skip a deleted
message. It is stored as the JS object, so `get::<serde_json::Value>("automodAction")` keeps working.

## 19. Permission checks before Discord writes

The command gate checks the *member's* level. Before the bot writes, check the *bot's* permissions:

```rust
use bot_core::interaction_helpers::{PermTarget, check_assignable_role_cached, check_channel_usable, missing_bot_permissions, require_bot_permissions};

if !require_bot_permissions(&ctx, &["ManageRoles"], None, true).await { return Ok(()); }   // answered "Missing Permissions"
let usable = check_channel_usable(&ctx.app, guild_id, Some(channel_id), &["ViewChannel", "SendMessages", "EmbedLinks"]);
if !usable.ok { /* usable.reason: "I need **Embed Links** in <#..>." */ }
let check = check_assignable_role_cached(&ctx.app, guild_id, role_id, ctx.member(), false);
if !check.ok { /* check.reason: "That role is above my highest role." ... */ }
if !missing_bot_permissions(&app, PermTarget::Channel(guild_id, channel_id), &["AddReactions"]).is_empty() { return; }
```

* Keys are discord.js `PermissionFlagsBits` names. Messages list them with `permission_names` ("Manage Server").
* `bot_core::permissions` computes discord.js's `permissionsFor`: owner or Administrator, `@everyone`, then role and
  member overwrites; a thread uses its parent. The functions are `member_permissions_in`, `role_permissions_in`,
  `guild_permissions`, `role_position` and `highest_position`.
* `bot_core::privacy_guards` has `everyone_can_view`, `copy_leaks_private`, `member_channel_problem` and
  `role_hierarchy_problem`, for features that copy or configure content across channels.
* Persistent components re-check the clicker's level with `ctx.permission_level().await`.
* `member_role_ids(&guild, &member)` is `member.roles.cache` keys: `@everyone` first, then the member's roles that
  still exist.
* `check_channel_usable_for(&app, guild, &channel, needs)` judges a channel object in hand (fetched over REST);
  `check_channel_usable_fetch(&app, guild, Some(id), needs).await` fetches an uncached channel first.

## 20. Buttons, selects, collectors, confirms and pagination

Persistent components (registered by a plugin, they survive restarts) are in section 5. For a one-off prompt, use a
`collector` prefix and the helpers. All of them go through the app's `CollectorHub`:

```rust
use bot_core::interaction_helpers::{ConfirmOptions, await_confirm};
use bot_core::paginator::{PaginateOptions, paginate};

let ok = await_confirm(&ctx, ConfirmOptions { title: Some("Delete every tag?".into()), ..Default::default() }).await;
if !ok { return Ok(()); }                      // cancelled, timed out, or the prompt could not be sent

paginate(&ctx, &pages, PaginateOptions::default()).await?;   // Prev / Stop / Next for the invoker; others are told no
```

To write your own collector:
1. Start it with `ctx.app.collectors().collect(CollectorFilter::message(msg.id).prefixes(["ttt"]))`.
2. Call `next(timeout)` in a loop, and acknowledge each click first (`click.update(..)` or `click.defer_update()`).
3. Answer strangers with `interaction_helpers::answer_stranger(&click)`, and keep your own idle deadline: a stranger's
   click must not extend it.

Build ids with `bot_config::custom_id!`.

## 21. Message templates and placeholders

Admin-written messages are `customEmbed` templates: `{ content, embed, buttons }`, stored as JSON.

```rust
use bot_core::custom_embed::{RenderOptions, build_template_modal, parse_template_modal, render_template, validate_template};

let check = validate_template(&tpl, true);                         // { ok, errors }, with the JS texts
let ctx = bot_core::adapters::render_context(&app, Some(guild_id), &user, member.as_ref(), Some(channel_id));
if let Some(r) = render_template(&tpl, &ctx, RenderOptions::default()) {
    channel_id.send_message(http.as_ref(), r.to_payload().to_message()).await?;
}
let modal = build_template_modal("welcome-tpl:edit", Some("Edit the welcome message"), &tpl, false)?;
let (template, errors) = parse_template_modal(|id| modal_ctx.value(id), &tpl, true);
```

* `normalize_template` returns the JS shape with the JS key order; store that.
* `template_to_json` and `template_from_json` export and import.
* `bot_core::adapters` turns serenity users, members, guilds and channels into the placeholder structs, with
  discord.js's CDN URLs (`displayAvatarURL({ extension: 'png', size: 256 })`).

## 22. Typical SQL

```rust
use bot_db::query::delete_where;
use bot_db::{COUNTING_STATE_INFO, CountingState, InsertBuilder, SelectBuilder, UpdateBuilder};

// Insert: the builder fills cuid() ids and @updatedAt. Upsert = ON CONFLICT DO UPDATE (updatedAt too).
app.settings().ensure_guild_settings(&guild_id).await?;            // every per-guild row has an FK to GuildSettings
let row: CountingState = InsertBuilder::new(&COUNTING_STATE_INFO)
    .value("guildId", json!(guild_id))?
    .values([("channelId", json!(channel)), ("count", json!(0))])?
    .on_conflict_update(&["guildId"], [("channelId", json!(channel)), ("count", json!(0))])?
    .build()?
    .fetch_one(app.db())
    .await?;

// Select, update, delete.
let row: Option<CountingState> =
    SelectBuilder::new(&COUNTING_STATE_INFO).filter("guildId", json!(guild_id))?.build()?.fetch_optional(app.db()).await?;
UpdateBuilder::new(&COUNTING_STATE_INFO).set("count", json!(5))?.filter("guildId", json!(guild_id))?.build()?.execute(app.db()).await?;
delete_where(&COUNTING_STATE_INFO, [("guildId", json!(guild_id))])?.execute(app.db()).await?;

// A transaction with a row lock or an advisory lock (the JS lockSql); raw SQL with $n binds and SQL_NOW.
let gid = guild_id.clone();
bot_db::transaction(app.db(), bot_db::TxOptions::default(), move |tx| {
    Box::pin(async move {
        bot_db::lock_guild_settings(tx, &gid).await?;                   // SELECT ... FOR UPDATE on GuildSettings
        bot_db::advisory_xact_lock(tx, &format!("infraction:{gid}")).await?;
        let sql = format!(r#"UPDATE "CountingState" SET "count" = LEAST("count"::bigint + $1, 999999999)::int, "updatedAt" = {} WHERE "guildId" = $2"#, bot_db::SQL_NOW);
        sqlx::query(sqlx::AssertSqlSafe(sql)).bind(1_i64).bind(&gid).execute(&mut *tx).await.map_err(bot_db::DbError::from)?;
        Ok::<_, bot_db::DbError>(())
    })
})
.await?;
```

* Bind times as `dt.naive_utc()` (a `timestamp(3)` column).
* Clamp `Int` columns and keep `::bigint` maths.
* Never write a bare `now()`.

## 23. Cache keys and values, in detail

* Build keys with the exact JS template (`format!("rr:rate:{g}:{user}")`), and test each against a key the JS built.
* `set` serialises with serde: use the Prisma field names, and `bot_db::serde_js::date` for dates.
* Read with `get_as::<T>()`, where `T` tolerates what the JS wrote (`#[serde(default)]`, `Option`, numbers as `f64`
  where the JS wrote floats). A value that does not fit is a miss.
* Fail-open or fail-closed, as the JS comments say: `set_nx(..) != Some(false)` is open, `== Some(true)` is closed.
* `guildDelete` purges `guild:{g}:*`, `xp:cooldown:{g}:*` and `voice:start:{g}:*`. Other keys expire by their TTL, as
  in the JS.

## 24. Exposing functions to other crates

Other features and the dashboard call services directly, never through a command.
* Make the service module `pub` (`feat_counting::counting_service::load_state(app.db(), &g)`).
* Take `&App` (or `&PgPool` for read-only SQL) and plain ids, and return plain data or `anyhow::Result`.
* A crate that needs another feature's types lists that crate in its `Cargo.toml`. Shared hook-state types live in the
  crate that writes them.
* Keep Discord out of the service where you can: return what to do (counting's `Action`s) and let the caller send it,
  so the dashboard can reuse the decision.

## 25. Testing it all offline

```rust
let guild = FakeGuild::new(G, OWNER)
    .everyone(Permissions::VIEW_CHANNEL)
    .role("820000000000000001", 1, Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS)
    .member(TEST_BOT_ID, &["820000000000000001"])                      // the bot itself (the cache's current user)
    .member(ADMIN, &["820000000000000002"])
    .channel(CH, 0, "general", &[(G, false, Permissions::empty(), Permissions::SEND_MESSAGES)]);   // (id, is_member, allow, deny)
let t = TestApp::new().with_guilds([G]).with_fake_guilds(&[guild]);   // or DbTestApp + t.set_discord(offline_discord_as(&[guild.json()]))
t.install(registry());
route_interaction(&t, FakeInteraction::slash("counting").guild(G).user(ADMIN).roles(&["820000000000000002"]).subcommand("set")
    .option("channel", option_type::CHANNEL, json!(CH)).build()).await;
assert_eq!(t.responses.names(), vec!["deferReply", "editReply"]);
```

* Bot permission checks, channel lookups, role positions and the `newTargets` reuse all work offline against the fake
  guild. Any REST call fails (there is no token), so creating, posting and reacting are skipped or logged. Keep those
  in a thin layer and test the decision: counting's `judge_message` returns the actions.
* `t.responses.fail_next("editReply", 10008)` and `set_failing(true)` (10062) simulate Discord errors.
* Collector flows:
  1. Spawn the call.
  2. Wait for `t.collectors().len()`.
  3. Route the clicks with `FakeInteraction::button(id).message_id("1000000000000000100")`, the id `fetch_reply`
     fabricates.
* Hooks: `t.run_hooks(MessageCreate { .. message: fake_message(id, g, ch, user, "text") .. })`.
* bot-core covers the gateway itself (`tests/gateway.rs`); features test their hooks with payloads.
* SQL: `DbTestApp::new(&url)`, with `install` and `reset_guilds`.
* The JS side: `run_node(script, args)`. Test both directions, as counting does: JS `saveState` then Rust
  `load_state`; Rust `save_state` then JS `loadState`, compared as JSON.
* REST: `let rest = FakeRest::new();` then `TestApp::new().with_fake_guilds_rest(&[guild], &rest)` (or `.with_rest(&rest)`,
  or `app.set_discord(rest.discord(&guild_json))` on a `DbTestApp`). Real serenity calls reach it; assert with
  `rest.calls_to("POST", "/channels/*/messages")` (`method`, `path`, `query_param`, `reason`, `body`, `files`), script
  answers with `rest.on(..)`, `on_times`, `on_with` and errors with `rest.fail(method, pattern, 403, 50013, "Missing
  Permissions")`. Unscripted: message creates / edits echo a message, DMs open, GETs of channels, messages, members and
  roles are Discord's 404s, the rest `204`.
* `FakeInteraction::option(name, USER, id)` fills `data.resolved` (a default user, and a member in a guild); with
  `.resolve_from(&guild)` USER, ROLE, CHANNEL and MENTIONABLE options resolve from that fake guild (defaults for ids it
  lacks). Entries written by hand are kept. `.message_timestamp(..)` sets a component's
  message time (default: now).
* `FakeGuild` setters: `name`, `afk`, `premium_tier`, `premium_count`, `features`, `vanity_code`, `field(key, json)`,
  `bot_member`, `member_with`, `channel_with`, `edit_channel`, `channel_parent`, `channel_nsfw`, `channel_user_limit`,
  `channel_bitrate`, `thread_owner`, `voice_state`, `voice_state_with`, `presence`. `with_discord_guilds` registers the
  bot user too.
* `TestApp::with_failing_jobs()` / `DbTestApp::with_failing_jobs(url)` return a `FailingJobStore`
  (`fail_next_schedules(n)`, `set_failing(true)`). `persistent_handler_problems_for(&registry, &["plugin-id"])` keeps
  only your plugins' prefixes and job types. `bot_util::logger::capture(fut).await` / `capture_sync(|| ..)` collect the
  log lines of one test only.

## 26. Boot and shutdown (bins/bot)

```rust
let app = App::builder().production(pool, redis_url.as_deref())?.build();
let registry = Registry::with(&[feat_counting::register /* , every feature */]);
let bot = bot_core::client::start_bot(app.clone(), registry, &token, bot_util::gateway_features::presences_enabled()).await?;
// On SIGTERM / SIGINT: bot.shutdown().await (scheduler 5 s, feature flushes, shards, pool, Redis); exit within 25 s.
// bot.wait().await: an Err with bot_core::client::fatal_code(&e) == Some(4004 | 4013 | 4014) parks instead of retrying.
```

A feature that buffers writes registers a flush, which runs at shutdown after the scheduler stops:
`bot_core::client::on_shutdown(&app, 20, "log queue flush", 4000, "Log queue was not flushed before shutdown", || async { flush().await })`.
Achievement counts use order 10 and 3000 ms.
From a feature's `register`, without a plugin `init` (so `hasInit` stays as in `plugins.json`):
`reg.on_shutdown(20, "log queue flush", 4000, "Log queue was not flushed before shutdown", |app| async move { flush(&app).await })`.
`reg.on_boot("label", |app| { ..; Ok(()) })` runs once at install.

## Checklist before reporting done

* [ ] `pub fn register(reg: &mut Registry)`, with the service modules `pub` for other crates, and no other entry point.
* [ ] Every command declares its category, module, `.gated(false)` for ADMIN config commands, levels,
      `subcommand_permission` and DM flag; `assert_commands_match_golden` passes (byte-identical payload).
* [ ] Every plugin has the JS id and module, and hooks in `HOOK_ORDER` with the JS `always` / `ungated` /
      `timeout_ms` / filters; `assert_plugin_matches_golden` passes.
* [ ] Every customId prefix, hook id and job type exists in `bot-config`. Dedupe keys, job payloads, cache keys and
      cached values are byte-identical to the JS, tested against JS output.
* [ ] Every user-visible string is copied verbatim. Titles keep the factory emoji: `"❌  Count Ruined"` in tests.
* [ ] Replies go through `Responder` / `interaction_helpers` only: `safe_defer`, then `safe_reply`, ephemeral as the
      JS.
* [ ] The bot's permissions are checked before every Discord write; "gone" REST codes degrade silently; mentions are
      as the JS.
* [ ] Order-sensitive hooks take their `sync::KeyedMutex` place synchronously.
* [ ] No globals (use `app.ext`), no `println!`, no `unwrap()` on absent data, no serenity `create_response` or
      collectors, no bare `now()` in SQL, no timer loops (use recurring jobs).
* [ ] Tests: the ported JS tests, the golden checks, and DB tests in both cross-language directions with the mock
      servers (`eval "$(node rust/mockdb/mockdb.mjs db <crate>)"`), all run with the variables set.
* [ ] `cargo test -p <crate>` passes, and `cargo clippy -p <crate> --tests` shows no warnings in your crate.
* [ ] The report lists every gap (with the JS file and function) and every `core_requests` item.
