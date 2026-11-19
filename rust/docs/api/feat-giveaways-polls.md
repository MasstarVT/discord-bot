# feat-giveaways-polls: public API (from its author)

The crate was empty after the crash, so all of it was written in this session. JS file to Rust file (all under rust/crates/feat-giveaways-polls/src):
- src/services/giveawayService.js -> giveaway_service.rs: every export, including drawWinners (lazy A-ES), makeResolver, start/end/reroll/cancel/restart/updateOdds, the Participate toggle, the 10 s count refresh, the delete hooks and the giveaway.end job, plus RAW_SQL.
- src/services/pollService.js -> poll_service.rs: every export, including parsers, canVote, applyVote (advisory lock + FOR SHARE), the 5 s render throttle, postPoll, endPoll, the hooks, the poll.end job, the command queries and RAW_SQL.
- src/plugins/giveaways.js -> plugins/giveaways.rs; src/plugins/polls.js -> plugins/polls.rs.
- src/commands/community/giveaway.js -> commands/giveaway.rs; src/commands/community/poll.js -> commands/poll.rs.
- Shared helpers: discord_ops.rs (the REST seam: send/edit/delete/fetch message, fetch channel, fetch member, as raw JSON bodies through serenity's Http) and guild_view.rs (bot and executor permissions on cached or fetched channels, role pings, guild availability).
- The JS polls use the bot's own embed + button/select messages, not Discord's native poll object, so no native poll payload is needed. Every channel message body is the raw JSON discord.js sends.

Tests: one file, tests/giveaways_polls.rs, 47 tests, plus two Node scripts:
- rust/fixtures/gen/feat-giveaways-polls-golden.mjs writes rust/fixtures/golden/feat-giveaways-polls/golden.json. It holds join rows, giveaway embeds in all statuses, list lines, announcement bodies (via MessagePayload.resolveBody), normalize/pack/ref/endsAt cases, 40 seeded draws, poll components/embeds/payloads/results/modal/list lines, parser sweeps, the canVote matrix, toggles, selections, customIds, dedupe keys and job types.
- rust/fixtures/gen/feat-giveaways-polls-js.mjs is the JS side of the cross-language tests.
- Cross-language, JS writes and Rust reads: a JS-started giveaway with JS button entries gets a Rust Participate click, and its JS end job runs under the Rust scheduler. A JS-posted poll gets JS votes and a Rust select vote, and its JS job ends it under Rust.
- Cross-language, Rust writes and JS reads: Prisma reads the Rust giveaway and poll rows, entries and jobs JSON-equal. The JS bot rerolls a Rust-ended giveaway and votes on and ends a Rust poll.
- The ported JS DB cases cover the job-attempt matrix (no access, unresolved entrants, unavailable guild), the resolver, reroll CONFLICT and RESOLVE_FAILED, concurrent ends paying once, the poll cap under parallel creates, a vote racing the end, endPoll with a deleted message or channel, a send failure or an unavailable guild, every hook, and the /giveaway and /poll subcommands through the router, including the cancel confirm click.
- Final state: cargo check, cargo test and cargo clippy --all-targets are clean for the crate. The suite passed three times in a row with both mock variables set, before a last small command fix; after that fix the suite passed once more, but clippy was not rerun.

## API

Entry point: feat_giveaways_polls::register(&mut Registry), the only entry point.

giveaway_service, all functions take &App and string ids:
- Flows: start_giveaway(app, StartGiveaway{..}) -> Result<Started{giveaway, message}, GiveawayError>; end_giveaway(app, guild, id, EndSource::{Job,Command}, attempt) -> anyhow::Result<Result<Ended, GiveawayError>> (the dashboard passes EndSource::Command, as the JS 'command'); reroll_giveaway(app, guild, id, count); cancel_giveaway(app, guild, id); restart_giveaway(app, guild, id, GuildChannel, ends_at, settings) -> Result<SentMessage, GiveawayError>; update_odds(app, guild, id, OddsOp::{Add{weight},Remove}, role, settings).
- Button, hooks and job: toggle_entry / handle_join_button, schedule_count_refresh, refresh_entry_count, reset_count_refresh, handle_giveaway_message_delete, handle_giveaway_bulk_delete, handle_end_job.
- Discord: fetch_channel, locate_message, pay_prizes, announce_winners / announcement_bodies.
- Pure helpers: normalize_role_weights, member_weight, meets_requirements, held_denied_role, draw_winners, make_resolver, max_weight_for, pack_mentions, parse_giveaway_ref, resolve_giveaway_ref, validate_ends_at -> Result<(), (title, message)>, reward_unavailable_message, format_list_line, build_join_row, build_giveaway_embed, giveaway_message_body.
- Queries: find_giveaway, entry_count, list_giveaways(db, guild, ListFilter), list_entrants, find_giveaways_for_autocomplete.
- Constants: END_JOB_TYPE, end_job_key, END_MAX_ATTEMPTS, MIN/MAX_DURATION_MS, MAX_ROLE_WEIGHTS, MAX_WEIGHT, MAX_PRIZE_AMOUNT, POST_NEEDS, status_text / status_text_short / status_emoji, RAW_SQL.
- GiveawayError::code() returns the JS codes (LIMIT, SEND_FAILED, DB_ERROR, NOT_FOUND, NOT_ACTIVE, NOT_DUE, DRAFTED, NO_ACCESS, RESOLVE_FAILED, NOT_ENDED, NO_ENTRANTS, CONFLICT, NOT_OPEN, NOT_DRAFT, NOT_SET).

poll_service:
- Constants: the poll module (POLL), END_JOB_TYPE, end_job_key, RAW_SQL.
- Pure helpers: parse_duration_option -> Result<Option<ms>, DurationError>, validate_question, parse_poll_options -> (options, errors), can_vote, apply_vote_toggle, normalize_selection, Tally / empty_tally / tally_from_groups, winners, percent, format_option_lines, button_label, build_poll_components, build_poll_embed, build_poll_payload(_default) (message JSON), build_results_embed, jump_url, build_create_modal, channel_problem(app, Option<&GuildChannel>, Option<&Member>) -> Option<CreateEmbed>, send_failure_reason.
- Pending forms: PendingStore and PendingPolls (app.ext).
- Database and flows: get_tally, create_poll_row / create_poll -> Option<Poll> (None = LIMIT), reconcile_deleted_channels, post_poll / post_poll_with(.., log_prefix) -> anyhow::Result<Result<Posted{poll, timer_failed}, PostError::{Limit,Send,Save}>>, apply_vote(app, poll, user, max, VoteInput::{Toggle,Set}) -> VoteOutcome, end_poll(app, id, EndReason::{Manual,Timer,Expired,Deleted}, Option<guild>) -> anyhow::Result<Result<PollEnded{poll, tally, posted, message_gone}, EndPollError::{NotFound,AlreadyEnded}>>.
- Live render: request_render, cancel_render, render_poll_message, set_render_impl.
- Hooks, job and command queries: the on_* hooks, handle_end_job, find_polls_for_autocomplete, list_active_polls, poll_list_lines.

discord_ops: DiscordOps trait plus set_discord_ops(app, Arc<dyn DiscordOps>) for a host without a gateway, or for tests; ops(app) returns the active implementation.

## Known gaps

- giveawayService.js makeResolver: uncached entrants are looked up over REST (GET /guilds/{g}/members/{u}, 5 at a time, 15 s timeout each) instead of one gateway chunk request per 100 ids. Unknown Member / Unknown User (10007 / 10013) means the member left. Any other error, or a timeout, leaves only that id unknown; in the JS a failed chunk request leaves the whole batch unknown. Under rate limits serenity queues the lookups, a lookup that times out counts as unknown, the job retries, and the final attempt ends without those entrants (a command answers RESOLVE_FAILED).
- commands/community/poll.js create (and giveaway.js optionChannel): a picked channel that is not in the serenity cache is refused ('Channel Not Found' for /poll, 'Unsupported Channel' for /giveaway). The JS runs its checks on the raw API channel object.
- pollService.js postPoll: the JS deps test seams (createPoll, buildPollPayload, emptyTally, getSettings, db, schedule) are replaced by the app-wide DiscordOps seam (discord_ops::set_discord_ops) plus App. logPrefix is kept through poll_service::post_poll_with(.., log_prefix). __setRenderImpl is poll_service::set_render_impl(app, Option<RenderFn>).
- Message JSON (giveawayService.js / pollService.js embeds): serenity adds type: "rich" to every embed and writes timestamps without milliseconds when they fall on a whole second (2026-05-02T12:00:00Z). Discord treats both the same; the golden tests compare with those two differences normalised away.
- giveawayService.js endGiveaway / pollService.js guildFor: a guild missing from the process's guild directory counts as unavailable, the same as guild.available === false.
- giveawayService.js scheduleCountRefresh / pollService.js requestRender: the timers are one-shot tokio tasks. resetCountRefresh and cancelRender invalidate them by generation or token instead of clearTimeout, so a cancelled task still wakes up, sees it was cancelled and does nothing.
