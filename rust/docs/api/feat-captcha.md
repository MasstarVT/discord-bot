# feat-captcha: public API (from its author)

All six JS files are ported in full.
- src/services/captchaService.js -> src/captcha_service.rs, split into submodules:
  - the main file: constants, MSG, Reply, pure helpers, rate limits and SQL;
  - captcha_service/image.rs: the captcha image, drawn with tiny-skia and bot-cards' embedded Nunito;
  - captcha_service/panel.rs: panel and channel, lock-down, verify-existing batch, diagnostics, status embed, fallback form;
  - captcha_service/flow.rs: challenge and verification, the join, leave and update hooks, the captcha.timeout job, buttons and modals.
- src/services/stickyService.js -> src/sticky_service.rs: debounce scheduler (tokio timers), channel memo, rendering, re-send, admin operations, cleanup.
- src/plugins/captcha.js -> src/plugins/captcha.rs; src/plugins/sticky.js -> src/plugins/sticky.rs (filters, hooks, sticky-modal form, applySet, texts).
- src/commands/admin/captcha.js -> src/commands/captcha.rs: all 12 subcommands plus autocomplete.
- src/commands/admin/sticky.js -> src/commands/sticky.rs: 5 subcommands plus autocomplete.

Both command payloads are byte-identical to commands.json, and both plugins match plugins.json.

Fixtures:
- rust/fixtures/gen/feat-captcha-golden.mjs writes rust/fixtures/golden/feat-captcha/golden.json, with JS sweeps of every pure helper, payload, text and form.
- rust/fixtures/gen/feat-captcha-js.mjs is the JS side of the cross-language tests.

Tests: tests/captcha.rs (35) and tests/sticky.rs (15). Cross-language coverage in both directions:
- Join rows and timeout jobs: JS writes and the Rust scheduler runs them; Rust schedules and the JS reads them.
- Challenges: Rust judges a JS-issued challenge, and the JS accepts the code of a Rust-stored challenge.
- GuildSettings captcha fields.
- StickyMessage rows and the channel map.
- The shared captcha:img Redis sliding window.

## API

Entry point: `feat_captcha::register(reg)`. The modules are pub: captcha_service (with flow, image, panel re-exported), sticky_service, plugins::{captcha, sticky}, commands::{captcha, sticky}.

Captcha, for the dashboard and other crates (`captcha_service`):
- Constants: CAPTCHA_ALPHABET, CODE_LENGTH, MAX_ATTEMPTS, TIMEOUT_MIN_MINUTES / TIMEOUT_MAX_MINUTES, BUTTON_STYLES, STYLE_NAMES, DEFAULT_VERIFY_LABEL / STYLE, PANEL_NEEDS, custom_ids::*, timeout_job_key(g, u) = "captcha.timeout:{g}:{u}".
- Validation and templates:
  - validate_timeout_minutes(i64) / validate_timeout_value(&Value) -> Result<i64, String>
  - parse_verify_button(Option<&Value>) -> VerifyButton{label, style}
  - panel_template(Option<&GuildSettings>) / panel_template_of(Option<&Value>)
  - default_panel_template()
  - build_panel_payload(settings, &RenderContext) -> Rendered
  - guild_render_context(guild, channel): a context with no user
- Role checks:
  - check_verified_role(guild, role, bot_id, settings, executor) and check_verified_role_cached(app, guild_id, role_id, settings, executor) -> Check{ok, reason}
  - check_required_role, missing_requirement, plan_required_roles(&RequireChange) -> Result<RolePlan, String>
  - fallback_form_problem, inspect_fallback_form(app, guild_id, settings, member_roles) -> FallbackForm
  - describe_requirements
- Lock-down: is_locked, everyone_can_view, bot_sees_without_everyone, lock_plan, is_kick_exempt, apply_lock(app, g, role, channel, plan, reason) -> LockResult, remove_lock.
- Answers: normalize_answer, hash_answer, answer_matches, evaluate_answer(Option<&Challenge>, g, u, input, now_ms) -> Evaluation, generate_code.
- Panel and status:
  - publish_panel(app, g, channel, settings, prev_channel, prev_message) -> anyhow Message; refusals downcast to CaptchaConfigError{message, title}
  - refresh_panel(app, g, settings) -> Result<(), reason>
  - create_verification_channel, diagnose_captcha, build_status_embed
- Data: count_pending(pool, g), reset_panel_template(app, g) (SQL NULL), find_row / delete_row / record_pending / store_challenge / count_wrong_try / clear_challenge / pending_user_ids, cancel_timeout(app, g, u), and RAW_SQL.
- Flow:
  - complete_verification(app, &Member, RoleId) -> Completion
  - issue_challenge(app, g, u, settings, content) -> Reply
  - Hook bodies: on_member_join / on_member_leave / on_member_update
  - run_timeout_job(Job, JobCtx)
  - Component handlers: handle_captcha_button / handle_captcha_modal
  - Kick seam: set_mod_action(app, Some(ModActionFn)), where ModActionFn returns KickOutcome
- verify-existing: start_verify_existing(BatchReporter, settings, executor, include_pending) -> anyhow::Result<Option<JoinHandle>>, get_bulk_run, is_bulk_target, stop_bulk_run, progress_text.
- Cache keys: guild_image_key, user_image_key, kick_dm_key, and check_image_limits (fails open).
- Shared hook state: on join the captcha hook sets the bool feat_welcome::welcome_service::CAPTCHA_PENDING_KEY.

Sticky (`sticky_service`):
- set_sticky(app, SetArgs{guild_id, channel_id, user_id, content, embed, keep_embed}) -> SetResult{created, has_embed, status}; refusals downcast to StickyError{code, title, message}.
- remove_sticky, set_sticky_enabled -> EnabledResult, resend_sticky(app, g, c, force) -> ResendStatus.
- Reads: list_stickies, get_sticky, count_stickies, find_by_channel.
- Channel memo: get_channels / peek_channels / invalidate.
- Scheduling and cleanup: note_message, cancel_pending, has_pending, clear_last_message, drop_channel, consume_self_deleted.
- Helpers: embed_part, render_sticky, sticky_needs, missing_for, run_exclusive, Debouncer (the generic createScheduler), RAW_SQL.

Sticky plugin (`plugins::sticky`): apply_set, set_result_embed, variable_notes, build_sticky_modal, STICKY_FIELD, and the filters may_stick / may_stick_own / may_be_sticky / may_have_sticky.

## Known gaps

- captchaService.js renderCaptchaImage: drawn with tiny-skia and the embedded Nunito face, not Skia. textBaseline 'middle' is approximated by centring each glyph's outline bounds, and bold is a stroke of fontSize/24. The noise, curves, rotation, sizes, colours, 320x120 size and PNG format follow the JS; the output is not pixel-identical.
- captchaService.js isImageCaptchaAvailable / MSG.FONT_MISSING: the font is compiled into the binary, so this is always true. The font-missing fallback and its texts exist but cannot trigger.
- captchaService.js publishPanel / refreshPanel: the stored panel is edited with PATCH directly, without a messages.fetch first. A 10008 still re-posts and permission codes still map to CaptchaConfigError, so the outcome is the same with one REST call fewer.
- captchaService.js diagnoseCaptcha: the panel check is a REST GET with a 3 s timeout. discord.js read its message cache first.
- captchaService.js / stickyService.js / commands: channels, roles and members are read from the serenity cache only (cached_channel / cached_guild). An uncached channel counts as deleted or unusable, as in other wave-1 crates.
- captchaService.js discordErrorText: the default branch uses serenity's error message text, so its wording can differ from DiscordAPIError.message. Offline, the text is 'Discord is not connected'.
- captchaService.js startVerifyExisting: 'guild.available' is read from app.guild_directory() (GuildState::Available). The batch runs in a tokio::spawn task; the JS used an un-awaited promise.
- captchaService.js applyLock / removeLock and the role creation in setup: if Discord applies a role edit but serenity cannot parse the returned Role, it is reported as a failure.
- captchaService.js buildStatusEmbed and other embeds with fields: fields the JS leaves without `inline` serialize as inline:false. Discord renders them the same.
- commands/admin/captcha.js doRequire: uses the resolved option role (options.getRole), falling back to the cached role when Discord sent no resolved data.
- stickyService.js createScheduler: the debounce uses tokio timers (one-shot sleeps with AbortHandles). The JS `now` parameter is replaced by tokio::time::Instant, which tests drive with paused time. The `_scheduler` export is has_pending(app, channel).
- plugins/sticky.js mayBeSticky: an uncached deleted message counts as 'partial' (MessageDelete.message is None or partial is true), matching discord.js.
