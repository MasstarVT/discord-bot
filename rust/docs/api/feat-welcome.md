# feat-welcome: public API (from its author)

Ported in full (the crash had left only the skeleton, so the work was redone from scratch):
- src/services/welcomeService.js -> src/welcome_service.rs: truth table, healing and serialisation, card style, payload building, card memo, senders with the 50013/50035/50007 fallbacks, flood caps, autoroles (immediate, delayed job, screening watch, captcha gate), hook bodies, the welcome.autorole job, diagnostics, settings writers, status embed, setup modals and submit, test previews, command helpers.
- src/services/applicationService.js -> src/application_service.rs: pure rules and builders, DB access (raw SQL in RAW_SQL plus bot-db builders), the per-form KeyedMutex in app.ext, the panel, the apply flow (pages, Continue, claim, revert), the review flow (accept/deny, roles, DM, outcome) and the cleanup hooks.
- src/plugins/welcome.js -> src/plugins/welcome.rs; src/plugins/applications.js -> src/plugins/applications.rs.
- src/commands/admin/welcome.js -> src/commands/welcome.rs; src/commands/community/application.js -> src/commands/application.rs.
- New src/io.rs: one seam (DiscordIo) for every REST call, used by serenity in production and by a recording fake in tests.
- Fixtures: rust/fixtures/gen/feat-welcome-golden.mjs writes rust/fixtures/golden/feat-welcome/golden.json (303 payloads, 150 template cases, modals, embeds, rules, flood key). rust/fixtures/gen/feat-welcome-js.mjs is the JS side of the cross-language tests.
- Tests: tests/welcome.rs and tests/applications.rs (shared helpers in tests/common/mod.rs). Both command payloads are byte-identical to commands.json and both plugins match plugins.json. Cross-language in both directions: settings rows, the delayed {userId} job and the {userId, watch:true} job, forms, drafts, submissions and Prisma's case-insensitive lookups.

## API

Entry point: feat_welcome::register(&mut Registry). Modules are pub: welcome_service, application_service, io, plugins::{welcome, applications}, commands::{welcome, application}.

For feat-captcha:
- welcome_service::assign_autorole(app: &App, member: &Member, force: bool) (never fails).
- welcome_service::is_captcha_gated(guild: Option<&Guild>, member: &Member, settings: Option<&GuildSettings>) -> bool. Role-persist uses this too.
- Contract: ctx.state[welcome_service::CAPTCHA_PENDING_KEY = "captchaPending"] must be a bool. Captcha depends on this crate, so it cannot import captcha's type.
- application_service::build_apply_button(form_id: &str, label: Option<&str>) -> CreateButton; apply_custom_id(form_id).
- find_form_by_id(pool, guild_id, id) -> anyhow::Result<Option<ApplicationForm>>; find_form(pool, guild_id, ref); find_forms_for_autocomplete(pool, guild_id, query, enabled_only) -> Vec<FormChoice{id,name,enabled}>.
- judge_apply(&ApplyFacts{form, member_role_ids, review_channel_problem, pending, last_denied_at, now}) -> Result<(), Refusal>; refusal_embed(&Refusal).
- review_channel_issue(app, guild_id: GuildId, form) -> Option<String>.

For the dashboard:
- welcome_service::heal_embed(Option<&Value>) -> Option<Value>.
- materialise_welcome(cur: &GuildSettings, data: &mut SettingsPatch).
- parse_card_style(Option<&Value>) -> CardStyle (to_json, to_card_style); serialize_card_style(&CardStyle) -> Value.
- welcome_card_enabled(Option<&GuildSettings>).
- Also resolve_template, serialize_embed, has_embed_content, describe_* and status_fields.

Other welcome writers (all return anyhow; refusals downcast to WelcomeConfigError{message,title}):
- update_message, update_embed_options(EmbedOptions), update_card_style(CardStyleOptions), add_autorole(app, guild_id, executor, role_id, delay) -> (Settings, already), remove_autorole, set_autorole_delay, prune_missing_autoroles.
- Job: run_autorole_job(Job, JobCtx). Payloads are {userId} and {userId, watch:true}; dedupe key autorole_job_key(g,u) = "welcome.autorole:{g}:{u}". Flood key flood_key(g,kind) = "welcome:rate:{g}:{kind}".

Application writers:
- create_form, update_form(app, g, form_id, Map of Prisma fields), delete_form, add_question(Question), remove_question, change_role(RoleList), post_panel, refresh_panel, pending_count, list_forms -> Vec<FormWithPending>, start_draft.
- Results are anyhow::Result<Result<T, Refusal>>.
- Prisma insensitive semantics were checked against the JS: equals escapes _ and %, contains treats them as wildcards.

Tests can replace every Discord REST call with io::set_io(app, Arc<dyn DiscordIo>); io::io(app) returns the active one. Card rendering and background validation go through the same trait.

## Known gaps

- welcomeService.js resolveChannel / checkConfigChannel and applicationService.js deleteMessageQuietly: channels are looked up in the serenity cache only. The JS falls back to guild.channels.fetch(id) when a channel is uncached; a channel missing from the cache counts as not found.
- welcomeService.js buildWelcomePayload: the catch-all branch (a rendering exception falls back to the default text) has no Rust equivalent, because rendering cannot fail. The 50035 default-text retry in sendWithFallback / sendWelcomeDm is ported.
- welcomeService.js addAutorole (via commands/admin/welcome.js 'autorole add'): the role is checked against the cached guild (check_assignable_role_cached). A role missing from the cache answers 'Role not found.', where the JS used the resolved option role.
- applicationService.js decide: the applicant is fetched over REST only (DiscordIo::fetch_member). The JS guild.members.fetch reads the cache first. Same result, one extra REST call.
- commands/community/application.js runPanel: JS behaviour kept on purpose, do not 'fix' it as a Rust bug. `(await usableChannel(...)) ?? interaction.channel` falls through to the current channel after a refused channel option has already been answered, so the panel is still posted there.
- Serialization only, the same to Discord: allowed_mentions carries `roles: []`; buttons carry `disabled: false` and no `emoji.animated: false`; review embed fields carry `inline: false`; thumbnails carry null proxy_url/height/width. The tests compare after dropping these defaults; everything else is JSON-equal to the JS.
- welcomeService.js toDbData: kept as a pass-through for API parity. bot-db already binds null on a Json? column as SQL NULL (Prisma.DbNull); a test proves a reset reads back as SQL NULL.
