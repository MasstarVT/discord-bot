# feat-games: public API (from its author)

All seven JS files are ported, each JS file to one Rust file:
- src/services/scrabbleService.js -> src/scrabble_service.rs: every export, covering the rules engine, game registry, turn timers, rendering, rack button and dictionary memo.
- src/plugins/games-fixes.js -> src/games_fixes.rs: plugin `games-fixes`, module games, the persistent `scrabble-rack` button.
- src/commands/games/blackjack.js -> src/commands/blackjack.rs.
- src/commands/games/games.js -> src/commands/games.rs: list, rps, numguess, tictactoe.
- src/commands/games/roulette.js -> src/commands/roulette.rs, autocomplete included.
- src/commands/games/scrabble.js -> src/commands/scrabble.rs: start, word, pass, exchange, resign, help.
- src/commands/games/slots.js -> src/commands/slots.rs.

Supporting files in the crate:
- src/casino.rs holds the start sequence the three casino files each repeat (`begin`, `refusal`, `goneOrWarn`, `loadSettings`) and the JS `DEFAULTS` test seam.
- src/collector_driver.rs gives bot-core's collectors the discord.js `idle`/`time`/`stop(reason)`/`end` behaviour. Each click's handler is spawned, so overlapping clicks are decided by the same synchronous flags as in the JS (settled, busy, played).

Randomness:
- The JS `crypto.randomInt` uses (deck, wheel, reels, rps, numguess secret) become an OS CSPRNG (`rand` SysRng) through `casino::secure_int`.
- Scrabble keeps `Math.random` semantics: a plain f64 source stored in the game, because exchange reshuffles with it.
- Both are injectable.

State:
- None of these JS files uses Redis or the database. Scrabble games and every per-user lock live in memory, here per App through `app.ext`.
- PORTING check (b), cache keys and values, does not apply. There are no jobs either.
- The game state serialises exactly like the JS object (`Game::state_json`).

Tests: one binary, tests/games.rs, 51 tests.
- JS-produced fixture: rust/fixtures/gen/feat-games-js.mjs writes rust/fixtures/golden/feat-games/pure.json.
- Seven scripted Scrabble games are replayed step by step with mulberry32. Bag, racks, board, scores, results and rendered embeds are byte-identical to the JS.
- Sweeps compare against the JS: parseSpace, spaceReturn and autocomplete; hand values, outcomes and the seeded deck; grids, payouts and rendering; the /games list embeds; the help embed; and every customId and button row.
- The ported tests are games-fixes.test.js, economy-casino.test.js and the slots RTP test from review-economy.test.js. All five commands and the rack button run through the router.
- DB tests run both ways. JS seeds through settingsService/economyService, Rust plays slots, roulette and blackjack, and JS reads the balance. JS plays slots and blackjack through its own execute(), and Rust reads them through feat_economy::get_account and compares the result embed.

## API

Entry point: `feat_games::register(&mut Registry)` registers 5 commands, the `games-fixes` plugin and the numguess listener boot step.

`scrabble_service` (pub). Rules take a locked `&mut Game`: lock it through `GameRef = Arc<Mutex<Game>>` with `scrabble_service::lock(&game_ref)`. The registry and dictionary take `&App`.
- Constants: BOARD_SIZE, CENTER, RACK_SIZE, BINGO_BONUS, TURN_TIMEOUT_MS, CHALLENGE_TIMEOUT_MS, MAX_SCORELESS_TURNS, MIN_BAG_FOR_EXCHANGE, COLS, BLANK, MAX_WORD_LENGTH, TILE_COUNTS, `tile_points(char)`, DICTIONARY_PATH, DICTIONARY_MAX_BYTES.
- Pure helpers: `create_board`, `shuffle(&mut [T], rng)`, `create_tile_bag`, `draw_tiles`, `rack_value`, `parse_word_input` -> Result<Vec<WordTile>, String>, `parse_tiles_input`, `describe_words`, `format_rack`, `current_player_id`, `other_player_id`.
- `create_game(CreateGame { id, guild_id, channel_id, players: [(id, name); 2], dictionary, settings, rng: PlainRng })` -> GameRef. `math_random()` is the default rng.
- Registry: `is_channel_busy`, `reserve_channel`, `release_channel`, `pending_challenge`, `register_game(&App, &GameRef) -> bool`, `get_game`, `find_game_by_id`.
- Rules: `finish_game(&mut Game, EndReason, loser, mover) -> GameResult`, `clear_turn_timer`, `arm_turn_timer`, `handle_turn_timeout(GameRef, seq)`, `compute_play(&Game, ..) -> Result<Play, String>`, `play_word(&mut Game, user, raw, row0, col0, dir) -> Result<WordPlayed, String>`, `pass_turn -> Result<Option<GameResult>, String>`, `exchange_tiles -> Result<(count, Option<GameResult>), String>`, `resign_game`.
- Rendering and posting: `render_board`, `build_game_embed(&Game, finished)`, `build_result_embed`, `rack_row`, `post_board(&GameRef, &Responder, content, mention_ids) -> bool`, `post_result`, `rack_button_text`, `handle_rack_button(ComponentCtx)`.
- Dictionary: `parse_dictionary`, `load_dictionary(&App)` (a Shared future), `get_dictionary`, `dictionary_status -> DictStatus { state, size }`, `dictionary_path`. Test hooks are in `scrabble_service::testing` (reset, set_dictionary, set_path, clear_registry).
- `Game::state_json()` is the JS object shape. `GameResult::to_json()`.

Commands:
- `commands::{blackjack, roulette, slots}` each have `data()`, `command()` and `command_with(CasinoDeps)`, the test seam. `casino::CasinoDeps { escrow: Arc<dyn Escrow>, load_settings, new_deck, spin_number, spin_grid, sleep }`. `casino::Escrow` is the injectable economy; `EconomyEscrow` is the feat-economy implementation.
- Pure helpers:
  - Blackjack: `build_deck(rng)`, `hand_value`, `is_soft`, `is_natural`, `should_dealer_hit`, `natural_outcome`, `final_outcome`, `result_text`, `build_buttons`, `progress_embed`, `final_embed`, `forfeit_embed`.
  - Roulette: `parse_space`, `space_return`, `wheel_color`, `spin_number`, `autocomplete_choices`.
  - Slots: SYMBOLS, LINES, TOTAL_WEIGHT, `spin_symbol`, `spin_grid`, `check_wins`, `slots_payout`, `render_grid`.
- `commands::games`: `collect_game_commands(&[CommandDef])`, `build_games_embed`, `game_emoji`, `decide_rps`, `build_rps_row`, `evaluate_guess`, `Numguess` (the guessing state machine), `build_board`, `check_winner`, `shown_name`, `offer_guess` / `has_guess_game` (the message feed), `register_numguess_listener`.
- `commands::scrabble`: `build_help_embed`, `dictionary_line`.

Shared helpers: `casino::secure_int(n)` is crypto.randomInt. `collector_driver::{drive, Stopper, Timers, id_filter}`.

## Known gaps

- games.js executeNumguess (channel.createMessageCollector): bot-core has no message collector. The guesses come instead from an ungated messageCreate hook `games-fixes:numguess`, registered with Registry::on_boot outside the plugin, so plugins.json still matches. Four consequences: (1) the hook id is not in bot-config HOOK_ORDER, which breaks the FRAMEWORK checklist item 'every hook id exists in bot-config'; (2) it sorts after every listed hook, so a guess that automod or custom-commands stop() never reaches the game; (3) it sees only the messages the gateway hands to hooks, meaning real user messages of the default and reply types; (4) when an earlier hook of the first dispatch is slow, two rapid guesses can be handled out of gateway order.
- Collector end reasons messageDelete, channelDelete, threadDelete and guildDelete (blackjack.js collector 'end'; games.js rps, tictactoe and numguess; scrabble.js challenge) cannot be observed in Rust. A hand whose message was deleted is forfeited at the idle (60 s) or total timeout, not at once. Numguess and ttt in a deleted channel end by idle or time, and their end message fails silently.
- scrabbleService.js DICTIONARY_PATH: `assets/dictionaries/words.txt` resolves against the process working directory, not the source file. It is the same file when the bot runs from the repository root. The file is absent in the repo, so word checking is off, as in the JS.
- blackjack.js execute: the branch where createMessageComponentCollector throws after the reserve cannot happen in Rust. The edit-failure branch (refund once) is ported and tested.
- The collectors in blackjack.js, games.js and scrabble.js filter on the customId (`<prefix>:<interactionId>:`), not on the message the JS collector was attached to. They match the same clicks, and no fetchReply is needed.
- Embed fields without `inline` (game embed Status/Turn, the hand fields, help and /games list): serenity always sends `inline: false` where discord.js leaves the key out. Discord renders both the same.
- scrabbleService.js stripOldBoard: serenity's webhook edit builder always sends `embeds`, so the old board's embed is resent unchanged with `components: []`. The JS sent components only. The visible result is identical.
- scrabbleService.js activeGames/pendingChallenges/gameMutex/dictionary memo and the casino lock (economyService): kept per App (app.ext) rather than per process. With one App per process this is the same.
- games.js tictactoe/rps/scrabble.js challenge: collector-level `filter` errors and discord.js-internal collector throws have no Rust equivalent. Handler errors are logged with the JS text and the click is acknowledged, as in the JS catch blocks.
