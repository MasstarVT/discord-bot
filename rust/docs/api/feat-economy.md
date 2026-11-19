# feat-economy: public API (from its author)

Picked up from the empty skeleton (the previous run had left nothing) and ported all 12 JS files.
- src/services/economyService.js -> src/economy_service.rs: every export. The raw SQL is ported statement for statement as `sql::*` builders, with `raw_sql()` mirroring the JS `RAW_SQL`. The Prisma client calls are written out as SQL and listed in `client_sql()`, using Prisma's own ILIKE semantics, confirmed from Prisma's query log.
- src/plugins/economy.js -> src/plugin.rs (`economy` plugin providing `coins` through `CoinsProvider`).
- src/commands/admin/economy-admin.js -> src/commands/economy_admin.rs: all 20 subcommands, both autocompletes, the private-flow confirm prompts, and the new_role companion via resolve_new_options.
- src/commands/economy/{balance,daily,inventory,pay,pity,richest,shop,use,work}.js -> src/commands/{balance,daily,inventory,pay,pity,richest,shop,use,work}.rs. use.rs is mounted as `commands::use_`. Shop covers view/info/buy, the autocomplete, the per-member KeyedMutex for use-on-buy role buys (`RoleBuys` in app.ext), the refund on a failed grant, and the economyPurchase hook.

Tests: one test binary, tests/economy.rs, with 55 tests. JS side: rust/fixtures/gen/feat-economy-js.mjs (pure/seed/read/sql), which also generates rust/fixtures/golden/feat-economy/pure.json.
- Golden checks: the pure helpers (about 240 period keys, emoji and name validation sweeps, config clamps, formatting, ordering, ownership, game-channel checks) and the RAW_SQL text, which is byte-identical to Prisma's statements once `$n` becomes `?`, with the same parameters.
- Command and plugin golden checks pass.
- All of test/economy.test.js is ported, plus the economy parts of review-economy.test.js.
- Commands are tested through the router, including the confirm collectors.
- Cross-language tests run both ways: JS writes then Rust reads, and Rust writes then JS reads, compared as JSON.

Not tested: the successful role-grant paths (shop use-on-buy granting the role, /use on a role item). Offline REST always fails, so only the refund and item-return paths are exercised. The JS "two concurrent role buys charge once" test is ported in its offline form: both buys are refunded and serialised, and the balance ends unchanged.

This crate builds no Redis keys, cached values, scheduled jobs or customIds, so PORTING checks (b) and (c) do not apply.

## API

Entry point: `feat_economy::register(&mut Registry)`. Public modules are `economy_service`, `plugin` and `commands`.

Conventions for every `economy_service` function:
- `app: &App` comes first.
- Settings are passed as `Option<&GuildSettings>` (None means all defaults). This replaces the JS partial-settings objects.
- Ids are `&str`; amounts are `i64`.
- Results are `anyhow::Result`. Expected failures are an `EconomyError { code: EconomyCode, data: Map }` inside it; read them with `economy_error(&err)` or `is_code(&err, EconomyCode::X)`, and use `e.data_i64("max")` or `e.reason()` for the details.

Pure helpers:
- Config and formatting: `economy_config` (an `EconomyConfig` that serialises like the JS), `format_coins(n, s, bold)`, `format_net`, `fmt_num`, `clamp_int`, `big`.
- Daily streak: `streak_tenths`, `streak_multiplier_label`, `daily_amount`, `next_daily_streak`, `active_streak`.
- Game channels: `game_channel_check(settings, channel_id, thread_parent, exists_fn)`, and `game_channel_check_cached(app, settings, guild_id, Option<&PartialChannel>, channel_id)` for feat-games. Both return a `GameChannelCheck` enum with a JS-shaped `to_json`.
- Payout and boosts: `clamp_payout`, `boosted_amount`, `boost_period_key`, `order_boosts`.
- Validation: `validate_currency_name`, `validate_item_name`, `validate_boost_name` return `Result<String, reason>`; `validate_currency_emoji` returns `Result<(value, Option<customId>), reason>`.
- Items: `effective_max_owned`, `max_owned_of`, `goes_to_inventory`, `goes_to_inventory_of`, `item_vars(name, price, quantity, total, settings)` (no defaults), `item_label(name)`, `item_type_label`, `boost_period_label`.
- Constants: all the JS constants, plus `raw_sql()`, `client_sql()` and `Stmt::query()`.

Accounts:
- `get_account` (the lazy global copy; returns `EconomyAccount`), `peek_account` (returns `AccountPeek`), `get_rank`.
- `credit` -> `Credited { balance, capped }`; `debit` -> `Debited { ok, balance }`.
- `transfer` -> the `Transfer::{Done, Refused}` enum.
- `set_balance` -> i64; `remove_coins` -> `Removed`; `clamp_balances_to_cap` -> u64; `top_accounts(app, g, take)`.

Casino (for feat-games):
- `try_lock_casino`, `unlock_casino`, `is_casino_locked(app, g, u)` are synchronous.
- `reserve_bet` -> the `Reserved::{Ok{balance, before}, Invalid, MaxBet{max_bet}, Insufficient{balance}}` enum, with `to_json`.
- `settle_bet(app, g, u, payout, settings, stake)`: `stake` is a required positional argument. Returns `Settled { balance, capped, payout_capped }` and retries once only when `failed_before_send`.

Claims:
- `claim_daily(app, g, u, settings, &member_role_ids)` -> the `DailyClaim::{Claimed, NotYet}` enum.
- `claim_work` -> the `WorkClaim` enum.
- `claim_pity` -> the `PityClaim::{Claimed, TooRich, NotYet}` enum, with `to_json`.

Shop:
- Lookups: `resolve_item(app, g, value, enabled_only)`, `search_items(app, g, q, enabled_only, take)`, `list_items(app, g, enabled_only, sort_by_name)`, `count_items`, `find_item_by_name_insensitive(.., exclude_id)`, `get_owned_quantity`.
- Inventory: `get_inventory` and `search_inventory(.., usable_only)` return `Vec<InventoryEntry>`, which serialises like Prisma's include.
- Buying: `purchase_item(app, g, u, item_id, qty, settings)` -> `Purchase`; `refund_purchase(app, Refund{..})`.
- Copies: `consume_item` -> remaining; `restore_item`; `give_item` -> owned; `take_item` -> `Taken`.
- Items: `create_item` and `update_item` take a `serde_json::Map` with Prisma field names; `delete_item` returns the number of copies removed.

Boosts:
- `list_boosts` -> `Vec<BoostWithUses>` (serialises with `_count.uses`), `search_boosts`, `resolve_boost`.
- `create_boost(app, g, NewBoost{..})`, `delete_boost` -> bool, `set_boost_enabled` -> `Option<EconomyBoost>`.

Reset: `reset_economy(app, g, Option<user>, ResetWhat, settings)` -> `ResetCounts`.

Rewards: `grant_coins_reward` and `take_coins_reward(app, &RewardArgs)` return a `RewardResult` and never fail; `CoinsProvider` implements `RewardProvider`.

UI: `reply_fail(responder, title, desc)` and `reply_fail_embed(responder, embed)`.

Command helpers also exposed for the dashboard: `commands::economy_admin::{channel_list_text, limits_summary, item_details, reset_prompt, streak_table, boost_list_line, item_list_line, period_text}`, `commands::shop::{view_field, stock_text, limit_text}`, `commands::richest::lines`, `commands::inventory::line`.

## Known gaps

- economyService.js failedBeforeSend: Prisma P1001, P2024 and PrismaClientInitializationError have no exact sqlx equivalent. The Rust version treats only sqlx PoolTimedOut and PoolClosed (also when wrapped in DbError) as 'never sent'. A connect failure that sqlx reports as an Io error is treated as 'may have been sent', so it is not retried.
- economyService.js orderBoosts: the last tiebreak (`String(id).localeCompare`) is a byte comparison in Rust. This is identical for the lowercase cuid/uuid ids in use, but differs from ICU collation for mixed case.
- economyService.js validateCurrencyEmoji: the Extended_Pictographic, Regional_Indicator and Emoji_Modifier properties come from the regex crate's Unicode tables, not V8's. The golden sweep matches, but a very new emoji could differ if the Unicode versions diverge.
- economyService.js casino lock (tryLockCasino/unlockCasino/isCasinoLocked): kept per App in app.ext (CasinoLocks) instead of a module-level Set. In production this is the same, since there is one App per process.
- economyService.js replyFail: split into `reply_fail(responder, title, description)` and `reply_fail_embed(responder, embed)`, which is the `{ embed }` option. The state machine is unchanged.
- economyService.js settleBet / every Prisma `throws`: functions return anyhow::Result, and the expected failures are an `EconomyError` inside it. Inputs that JS validated as non-integer (1.5, NaN, '10') cannot occur with typed i64 arguments, so only the range checks remain.
- shop.js checkRoleItem / use.js role branch / economy-admin.js item create|edit: the role is looked up in the serenity cache. Without a cached guild (offline, or a guild not yet cached) the answer is 'Item Broken' ('This item's role no longer exists') or 'Role not found.', where the JS used interaction.guild.
- shop.js and use.js messageCtx (`interaction.channel ?? { id }`): when the channel is not in the serenity cache, the placeholder channel has only its id. The name and type come only from the cache.
- economy-admin.js itemCreate `embed.setThumbnail` in a try/catch: serenity does not validate the URL, so there is no catch. The URL is already http(s)-validated before this point.
- richest.js / inventory.js / shop.js / economy-admin.js addFields without `inline`: serenity always serialises `inline: false`, where discord.js left the key out. Discord renders both the same.
