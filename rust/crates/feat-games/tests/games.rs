//! feat-games: the ported JS tests (`test/games-fixes.test.js`, `test/economy-casino.test.js`, the slots part of
//! `test/review-economy.test.js`), the golden checks against JS-produced fixtures, the commands through the router,
//! and the database tests in both cross-language directions.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bot_core::command::option_type;
use bot_core::interaction::RecordedCall;
use bot_core::router::route_interaction;
use bot_core::testing::{DbTestApp, FakeGuild, FakeInteraction, TEST_BOT_ID, TestApp, database_url, fake_message, run_node};
use bot_core::{App, Registry};
use bot_db::{ChannelListMode, GuildSettings};
use feat_economy::economy_service::{Reserved, Settled, clamp_payout, economy_config};
use feat_games::casino::{CasinoDeps, Escrow};
use feat_games::commands::blackjack::{self as bj, Card};
use feat_games::commands::games as games_cmd;
use feat_games::commands::roulette;
use feat_games::commands::scrabble as scrabble_cmd;
use feat_games::commands::slots::{self, SYMBOLS, Symbol};
use feat_games::scrabble_service::{self as svc, CreateGame, EndReason, GameRef, Tile, lock};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use serenity::all::Permissions;

// ── helpers ───────────────────────────────────────────────────────────────────

fn golden() -> &'static Value {
    static G: OnceLock<Value> = OnceLock::new();
    G.get_or_init(|| {
        let path = bot_core::testing::golden_dir().join("feat-games").join("pure.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("pure.json")).expect("json")
    })
}

/// The JS test PRNG.
fn mulberry32(seed: u32) -> impl FnMut() -> f64 + Send {
    let mut a = seed;
    move || {
        a = a.wrapping_add(0x6d2b79f5);
        let mut t = a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// An embed or component as JSON with what the two libraries serialise differently taken out: the timestamp, the
/// `type: rich` serenity adds, `inline: false` (discord.js leaves the key out) and `disabled: false`.
fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, x) in m {
                if k == "timestamp" || (k == "type" && x == "rich") || x.is_null() {
                    continue;
                }
                if (k == "inline" || k == "disabled") && x == &json!(false) {
                    continue;
                }
                out.insert(k.clone(), norm(x));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        x => x.clone(),
    }
}

fn js<T: serde::Serialize>(x: &T) -> Value {
    norm(&serde_json::to_value(x).expect("serialize"))
}

const A: &str = "111111111111111111";
const B: &str = "222222222222222222";

fn mk_game(app: Option<&App>, channel: &str, id: &str, seed: u32, dictionary: Option<HashSet<String>>) -> GameRef {
    let _ = app;
    svc::create_game(CreateGame {
        id: id.into(),
        guild_id: "guild-1".into(),
        channel_id: channel.into(),
        players: [(A.into(), "Alice".into()), (B.into(), "Bob".into())],
        dictionary: dictionary.map(Arc::new),
        settings: None,
        rng: Box::new(mulberry32(seed)),
    })
}

fn scores(game: &GameRef) -> (i64, i64) {
    let g = lock(game);
    (g.player(A).expect("A").score, g.player(B).expect("B").score)
}

fn set_rack(game: &GameRef, user: &str, letters: &str) {
    lock(game).player_mut(user).expect("player").rack = letters.chars().map(Tile::of).collect();
}

/// Waits (real time) until `f` holds.
async fn wait_for(what: &str, f: impl Fn() -> bool) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn settle_tasks() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
}

fn calls_named<'a>(calls: &'a [RecordedCall], name: &str) -> Vec<&'a RecordedCall> {
    calls.iter().filter(|c| c.method == name).collect()
}

fn field<'a>(call: &'a RecordedCall, name: &str) -> Option<&'a str> {
    call.body["embeds"][0]["fields"].as_array()?.iter().find(|f| f["name"] == name || f["name"].as_str().is_some_and(|n| n.starts_with(&format!("{name} (")))).and_then(|f| f["value"].as_str())
}

// ── golden: commands and plugin ───────────────────────────────────────────────

#[test]
fn commands_and_plugin_match_the_js() {
    let mut reg = Registry::new();
    feat_games::register(&mut reg);
    bot_core::testing::assert_commands_match_golden(&reg, &["blackjack", "games", "roulette", "scrabble", "slots"]);
    bot_core::testing::assert_plugin_matches_golden(&reg, "games-fixes");
    assert!(bot_core::testing::persistent_handler_problems_for(&reg, &["games-fixes"]).is_empty());
}

#[test]
fn payload_budgets_of_the_commands() {
    for (cmd, budget) in [
        (bj::command(), 100),
        (roulette::command(), 160),
        (slots::command(), 70),
        (games_cmd::command(), 340),
        (scrabble_cmd::command(), 520),
    ] {
        assert!(cmd.data.char_count() <= budget, "{} is {} characters", cmd.name(), cmd.data.char_count());
    }
    let col = &scrabble_cmd::data().options[1].options[2];
    assert_eq!(col.name, "col");
}

// ── scrabble: the scripted games against the JS ───────────────────────────────

fn outcome_json(game: &GameRef, op: &[Value]) -> Value {
    let s = |i: usize| op[i].as_str().unwrap_or("").to_string();
    let n = |i: usize| op[i].as_i64().unwrap_or(-1);
    let mut g = lock(game);
    let err = |e: String| json!({ "ok": false, "error": e });
    let over = |r: &Option<svc::GameResult>| r.as_ref().map(svc::GameResult::to_json).unwrap_or(Value::Null);
    match op[0].as_str().unwrap_or("") {
        "setRack" => {
            g.player_mut(&s(1)).expect("player").rack = s(2).chars().map(Tile::of).collect();
            Value::Null
        }
        "setBag" => {
            g.bag.truncate(n(1) as usize);
            Value::Null
        }
        "play" => match svc::play_word(&mut g, &s(1), &s(2), n(3), n(4), &s(5)) {
            Ok(r) => json!({ "ok": true, "words": r.words, "total": r.total, "bingo": r.bingo, "placements": r.placements, "gameOver": over(&r.game_over) }),
            Err(e) => err(e),
        },
        "compute" => match svc::parse_word_input(&s(2)) {
            Err(e) => err(e),
            Ok(tiles) => match svc::compute_play(&g, &s(1), &tiles, n(3), n(4), &s(5)) {
                Ok(p) => {
                    let mut v = serde_json::to_value(&p).expect("play");
                    v["ok"] = json!(true);
                    v
                }
                Err(e) => err(e),
            },
        },
        "pass" => match svc::pass_turn(&mut g, &s(1)) {
            Ok(r) => json!({ "ok": true, "gameOver": over(&r) }),
            Err(e) => json!({ "ok": false, "error": e, "gameOver": null }),
        },
        "exchange" => match svc::exchange_tiles(&mut g, &s(1), &s(2)) {
            Ok((count, r)) => json!({ "ok": true, "count": count, "gameOver": over(&r) }),
            Err(e) => json!({ "ok": false, "error": e, "gameOver": null }),
        },
        "resign" => svc::resign_game(&mut g, &s(1)).to_json(),
        "finish" => {
            let reason = match s(1).as_str() {
                "timeout" => EndReason::Timeout,
                "resign" => EndReason::Resign,
                "out" => EndReason::Out,
                "scoreless" => EndReason::Scoreless,
                _ => EndReason::Aborted,
            };
            let loser = op[2].as_str().map(str::to_string);
            let mover = op[3].as_str().map(str::to_string);
            svc::finish_game(&mut g, reason, loser.as_deref(), mover.as_deref()).to_json()
        }
        other => panic!("unknown op {other}"),
    }
}

#[test]
fn scrabble_games_replay_exactly_like_the_js() {
    for script in golden()["scrabble"]["games"].as_array().expect("games") {
        let seed = script["seed"].as_u64().expect("seed") as u32;
        let dictionary = script["dictionary"].as_array().map(|a| a.iter().filter_map(|w| w.as_str().map(str::to_string)).collect());
        let game = mk_game(None, &format!("ch-{seed}"), &format!("game-{seed}"), seed, dictionary);
        {
            let mut g = lock(&game);
            g.guild_id = "900000000000000001".into();
        }
        assert_eq!(lock(&game).state_json(), script["initial"], "seed {seed}: initial state");
        for (i, step) in script["steps"].as_array().expect("steps").iter().enumerate() {
            let op = step["op"].as_array().expect("op");
            let out = outcome_json(&game, op);
            assert_eq!(out, step["out"], "seed {seed} step {i} {op:?}: outcome");
            assert_eq!(lock(&game).state_json(), step["state"], "seed {seed} step {i} {op:?}: state");
        }
        let g = lock(&game);
        assert_eq!(svc::render_board(&g.board), script["board"].as_str().expect("board"));
        assert_eq!(js(&svc::build_game_embed(&g, false)), norm(&script["embed"]), "seed {seed}: embed");
        assert_eq!(js(&svc::build_game_embed(&g, true)), norm(&script["finishedEmbed"]), "seed {seed}: finished embed");
        match &g.result {
            Some(r) => assert_eq!(js(&svc::build_result_embed(&g, r)), norm(&script["resultEmbed"]), "seed {seed}: result embed"),
            None => assert!(script["resultEmbed"].is_null()),
        }
        for p in &g.players {
            assert_eq!(svc::format_rack(&p.rack), script["racks"][&p.id].as_str().expect("rack"));
        }
    }
}

#[test]
fn scrabble_pure_helpers_match_the_js() {
    let s = &golden()["scrabble"];
    for b in s["bags"].as_array().expect("bags") {
        let mut rng = mulberry32(b["seed"].as_u64().expect("seed") as u32);
        let letters: String = svc::create_tile_bag(&mut rng).iter().map(|t| t.letter).collect();
        assert_eq!(letters, b["letters"].as_str().expect("letters"));
    }
    for c in s["parseWord"].as_array().expect("parseWord") {
        let got = match svc::parse_word_input(c["input"].as_str().expect("input")) {
            Ok(tiles) => json!({ "ok": true, "tiles": tiles }),
            Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(got, c["out"], "{}", c["input"]);
    }
    for c in s["parseTiles"].as_array().expect("parseTiles") {
        let got = match svc::parse_tiles_input(c["input"].as_str().expect("input")) {
            Ok(l) => json!({ "ok": true, "letters": l.iter().map(|c| c.to_string()).collect::<Vec<_>>() }),
            Err(e) => json!({ "ok": false, "error": e }),
        };
        assert_eq!(got, c["out"], "{}", c["input"]);
    }
    let mut dict: Vec<String> = svc::parse_dictionary("cat\r\n# c\nQI\nx\nfoo-bar\n  dog  \nABCDEFGHIJKLMNOP\nABCDEFGHIJKLMNO\n\n").into_iter().collect();
    dict.sort();
    assert_eq!(json!(dict), s["dictionary"]);
    let words = [svc::FormedWord { text: "OX".into(), score: 17 }, svc::FormedWord { text: "CO".into(), score: 4 }];
    assert_eq!(json!([svc::describe_words(&words), svc::describe_words(&[])]), s["describe"]);
    assert_eq!(json!([svc::format_rack(&[]), svc::format_rack(&[Tile { letter: '?', points: 0 }, Tile { letter: 'Q', points: 10 }])]), s["formatRack"]);
    assert_eq!(js(&svc::rack_row("1234567890123456789")), norm(&s["rackRow"]));
    assert_eq!(svc::render_board(&svc::create_board()), s["emptyBoard"].as_str().expect("board"));
    for h in s["help"].as_array().expect("help") {
        let status = svc::DictStatus {
            state: if h["status"]["state"] == "loaded" { svc::DictState::Loaded } else { svc::DictState::Missing },
            size: h["status"]["size"].as_u64().expect("size") as usize,
        };
        assert_eq!(scrabble_cmd::dictionary_line(status), h["line"].as_str().expect("line"));
        let embed = js(&scrabble_cmd::build_help_embed(None, status));
        assert_eq!(embed, norm(&h["embed"]));
        assert!(embed["fields"].as_array().expect("fields").iter().all(|f| bot_util::js::utf16_len(f["value"].as_str().unwrap_or("")) <= 1024));
    }
}

// Ported: test/games-fixes.test.js "tile bag and parsing", "scoring", "turns, racks, exchange, endings".

#[test]
fn the_bag_has_100_tiles_with_2_blanks() {
    let bag = svc::create_tile_bag(&mut mulberry32(7));
    assert_eq!(bag.len(), 100);
    let blanks: Vec<_> = bag.iter().filter(|t| t.letter == '?').collect();
    assert_eq!(blanks.len(), 2);
    assert!(blanks.iter().all(|t| t.points == 0));
    for (letter, count) in svc::TILE_COUNTS {
        assert_eq!(bag.iter().filter(|t| t.letter == letter).count(), count, "{letter}");
    }
}

#[test]
fn scoring_cross_words_and_extension() {
    let g = mk_game(None, "c1", "g1", 1, None);
    set_rack(&g, A, "CATXYZE");
    let r = svc::play_word(&mut lock(&g), A, "CAT", 7, 7, "h").expect("CAT");
    assert_eq!(r.total, 10);
    assert_eq!(lock(&g).placed_count, 3);
    set_rack(&g, B, "OXQJKLM");
    let r = svc::play_word(&mut lock(&g), B, "OX", 8, 7, "h").expect("OX");
    let summary: Vec<String> = r.words.iter().map(|w| format!("{} {}", w.text, w.score)).collect();
    assert_eq!(summary, ["OX 17", "CO 4", "AX 17"]);
    assert_eq!(r.total, 38);

    let play = |letters: &str, word: &str, r: i64, c: i64, dir: &str| {
        let g = mk_game(None, "c2", "g2", 1, None);
        set_rack(&g, A, "CATXYZE");
        svc::play_word(&mut lock(&g), A, "CAT", 7, 7, "h").expect("CAT");
        set_rack(&g, B, letters);
        let tiles = svc::parse_word_input(word).expect("word");
        let gg = lock(&g);
        svc::compute_play(&gg, B, &tiles, r, c, dir)
    };
    let words = |p: Result<svc::Play, String>| p.expect("play").words.iter().map(|w| format!("{} {}", w.text, w.score)).collect::<Vec<_>>();
    assert_eq!(words(play("SQJKLMN", "S", 7, 10, "h")), ["CATS 6"]);
    assert_eq!(words(play("SQJKLMN", "S", 7, 10, "v")), ["CATS 6"]);
    assert_eq!(words(play("DOGQJKL", "DOG", 7, 10, "h")), ["CATDOG 11"]);
    assert_eq!(words(play("EQJKLMN", "ATE", 7, 8, "h")), ["CATE 6"]);
    assert!(play("XUQJKLM", "XU", 0, 0, "h").unwrap_err().contains("touch or pass through"));
}

#[test]
fn a_blank_and_a_bingo() {
    let g = mk_game(None, "c3", "g3", 1, None);
    set_rack(&g, A, "C?ATXYZ");
    let r = svc::play_word(&mut lock(&g), A, "C?AT", 7, 7, "h").expect("C?AT");
    assert_eq!(r.total, 8);
    assert_eq!(lock(&g).board[7][8], svc::Cell::Tile(svc::PlacedTile { letter: 'A', points: 0, blank: true }));
    let g = mk_game(None, "c4", "g4", 1, None);
    set_rack(&g, A, "ABCDEFG");
    let r = svc::play_word(&mut lock(&g), A, "ABCDEFG", 7, 7, "h").expect("bingo");
    assert!(r.bingo);
    assert_eq!(r.total, 84);
}

#[test]
fn turns_exchange_and_endings() {
    let g = mk_game(None, "c5", "g5", 1, None);
    set_rack(&g, A, "CATXYZE");
    svc::play_word(&mut lock(&g), A, "CAT", 7, 7, "h").expect("CAT");
    let snap = |g: &GameRef| {
        let gg = lock(g);
        (gg.player(A).expect("A").rack.clone(), gg.state_json()["board"].clone())
    };
    let before = snap(&g);
    let again = svc::play_word(&mut lock(&g), A, "XE", 8, 7, "h").unwrap_err();
    assert!(again.contains("It's not your turn"));
    assert_eq!(snap(&g), before);

    let g = mk_game(None, "c6", "g6", 1, None);
    lock(&g).bag.truncate(6);
    assert!(svc::exchange_tiles(&mut lock(&g), A, "AB").unwrap_err().contains("at least 7 tiles (it has **6**)"));
    let g = mk_game(None, "c7", "g7", 1, None);
    set_rack(&g, A, "ABCDEFG");
    let bag_before = lock(&g).bag.len();
    let (count, over) = svc::exchange_tiles(&mut lock(&g), A, "AB").expect("exchange");
    assert_eq!((count, over), (2, None));
    let gg = lock(&g);
    assert_eq!((gg.player(A).expect("A").rack.len(), gg.bag.len(), gg.current_turn, gg.scoreless_turns), (7, bag_before, 1, 1));
    drop(gg);

    let g = mk_game(None, "c8", "g8", 1, None);
    lock(&g).bag.clear();
    set_rack(&g, A, "CAT");
    set_rack(&g, B, "QZ");
    let r = svc::play_word(&mut lock(&g), A, "CAT", 7, 7, "h").expect("CAT");
    let over = r.game_over.expect("over");
    assert_eq!(over.reason, EndReason::Out);
    assert_eq!(over.mover_id.as_deref(), Some(A));
    assert_eq!(scores(&g), (30, -20));
    assert_eq!((over.adjustment(A), over.adjustment(B)), (20, -20));

    let g = mk_game(None, "c9", "g9", 1, None);
    let res = svc::resign_game(&mut lock(&g), B);
    assert_eq!((res.reason, res.winner_id.as_deref(), res.loser_id.as_deref()), (EndReason::Resign, Some(A), Some(B)));
    assert_eq!(svc::resign_game(&mut lock(&g), A), res, "idempotent");
}

#[test]
fn rack_errors_suggest_a_blank_only_when_one_is_held() {
    let g = mk_game(None, "c10", "g10", 1, None);
    set_rack(&g, A, "ABC?EFG");
    let gg = lock(&g);
    let hint = svc::compute_play(&gg, A, &svc::parse_word_input("QA").expect("QA"), 7, 7, "h").unwrap_err();
    assert!(hint.contains("`?Q`"));
    drop(gg);
    set_rack(&g, A, "ABCDEFG");
    let gg = lock(&g);
    assert!(svc::compute_play(&gg, A, &svc::parse_word_input("QA").expect("QA"), 7, 7, "h").unwrap_err().contains("You don't have the tile **Q**"));
    assert!(svc::compute_play(&gg, A, &svc::parse_word_input("A?B").expect("A?B"), 7, 7, "h").unwrap_err().contains("don't have a blank"));
}

// ── scrabble: registry and timers (R021, R087, R167) ──────────────────────────

#[tokio::test(start_paused = true)]
async fn six_scoreless_turns_end_a_registered_game() {
    let t = TestApp::new();
    let g = mk_game(Some(&t), "sc-1", "sc-1", 1, None);
    assert!(svc::register_game(&t, &g));
    assert!(lock(&g).timer.is_some(), "a registered game always has a timer");
    let racks = {
        let gg = lock(&g);
        (svc::rack_value(&gg.player(A).expect("A").rack), svc::rack_value(&gg.player(B).expect("B").rack))
    };
    let mut last = None;
    for _ in 0..6 {
        let cur = svc::current_player_id(&lock(&g)).to_string();
        last = svc::pass_turn(&mut lock(&g), &cur).expect("pass");
    }
    let over = last.expect("game over");
    assert_eq!(over.reason, EndReason::Scoreless);
    assert_eq!(scores(&g), (-racks.0, -racks.1));
    assert!(svc::get_game(&t, "sc-1").is_none());
    let gg = lock(&g);
    assert!(gg.timer.is_none() && gg.ended);
}

#[tokio::test(start_paused = true)]
async fn a_stale_game_or_sequence_never_ends_a_newer_game_and_the_real_timer_forfeits() {
    let t = TestApp::new();
    let game1 = mk_game(Some(&t), "tm-1", "one", 1, None);
    assert!(svc::register_game(&t, &game1));
    let seq1 = lock(&game1).turn_seq;
    svc::finish_game(&mut lock(&game1), EndReason::Resign, Some(A), None);
    let game2 = mk_game(Some(&t), "tm-1", "two", 2, None);
    assert!(svc::register_game(&t, &game2));

    svc::handle_turn_timeout(game1.clone(), seq1).await;
    assert!(svc::get_game(&t, "tm-1").is_some_and(|g| Arc::ptr_eq(&g, &game2)));
    assert!(!lock(&game2).ended);

    let seq0 = lock(&game2).turn_seq;
    let cur = svc::current_player_id(&lock(&game2)).to_string();
    svc::pass_turn(&mut lock(&game2), &cur).expect("pass");
    svc::handle_turn_timeout(game2.clone(), seq0).await;
    assert!(!lock(&game2).ended);

    let mover = svc::current_player_id(&lock(&game2)).to_string();
    tokio::time::sleep(Duration::from_millis(svc::TURN_TIMEOUT_MS - 1)).await;
    assert!(!lock(&game2).ended);
    tokio::time::sleep(Duration::from_millis(2)).await;
    settle_paused().await;
    let g = lock(&game2);
    assert!(g.ended);
    let r = g.result.clone().expect("result");
    assert_eq!((r.reason, r.loser_id.as_deref()), (EndReason::Timeout, Some(mover.as_str())));
    assert_eq!(r.winner_id.as_deref(), Some(svc::other_player_id(&g, &mover)));
    drop(g);
    assert!(svc::get_game(&t, "tm-1").is_none());
}

async fn settle_paused() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn reservations_only_the_owner_releases_and_register_honours_them() {
    let t = TestApp::new();
    assert!(svc::reserve_channel(&t, "c", "x"));
    assert!(svc::is_channel_busy(&t, "c"));
    assert!(!svc::reserve_channel(&t, "c", "y"));
    svc::release_channel(&t, "c", "y");
    assert!(svc::is_channel_busy(&t, "c"));
    assert!(!svc::register_game(&t, &mk_game(Some(&t), "c", "other", 1, None)));
    svc::release_channel(&t, "c", "x");
    assert!(!svc::is_channel_busy(&t, "c"));
    assert!(svc::reserve_channel(&t, "c", "mine"));
    let mine = mk_game(Some(&t), "c", "mine", 1, None);
    assert!(svc::register_game(&t, &mine));
    assert!(svc::pending_challenge(&t, "c").is_none());
    assert!(!svc::register_game(&t, &mk_game(Some(&t), "c", "late", 1, None)));
    svc::finish_game(&mut lock(&mine), EndReason::Aborted, None, None);
    let g = lock(&mine);
    assert!(g.timer.is_none());
    assert_eq!(g.result.as_ref().map(|r| (r.reason, r.winner_id.clone())), Some((EndReason::Aborted, None)));
}

#[tokio::test]
async fn the_rack_button_answers_no_game_a_stranger_and_a_player() {
    let t = TestApp::new();
    assert!(svc::rack_button_text(&t, "scrabble-rack:999", "x", A).contains("no longer active"));
    let g = mk_game(Some(&t), "rack-ch", "rack-game", 1, None);
    svc::register_game(&t, &g);
    assert!(svc::rack_button_text(&t, "scrabble-rack:rack-game", "x", "333333333333333333").contains("You're not a player"));
    let mine = svc::rack_button_text(&t, "scrabble-rack:rack-game", "x", A);
    assert!(mine.contains("Your tiles") && mine.contains("your turn"));
    assert!(svc::rack_button_text(&t, "scrabble-rack:rack-game", "x", B).contains("Waiting for **Alice**"));
    assert!(svc::rack_button_text(&t, "scrabble-rack", "rack-ch", A).contains("Your tiles"), "the bare legacy id resolves by channel");
    svc::finish_game(&mut lock(&g), EndReason::Aborted, None, None);
    assert!(svc::rack_button_text(&t, "scrabble-rack:rack-game", "x", A).contains("no longer active"));
}

#[tokio::test]
async fn the_rendered_board_and_the_finished_embed() {
    let g = mk_game(None, "r1", "r1", 1, None);
    set_rack(&g, A, "C?ATXYZ");
    svc::play_word(&mut lock(&g), A, "C?AT", 7, 7, "h").expect("play");
    let gg = lock(&g);
    let fin = js(&svc::build_game_embed(&gg, true));
    let names: Vec<&str> = fin["fields"].as_array().expect("fields").iter().filter_map(|f| f["name"].as_str()).collect();
    assert!(!names.contains(&"Turn") && names.contains(&"Status"));
    let live = js(&svc::build_game_embed(&gg, false));
    assert!(live["fields"].as_array().expect("fields").iter().any(|f| f["name"] == "Turn" && f["value"].as_str().unwrap_or("").contains(&format!("<@{B}>"))));
    let board = svc::render_board(&gg.board);
    assert!(board.chars().count() < 1100);
    assert!(board.contains("C  a  T"));
    assert!(fin["footer"]["text"].as_str().unwrap_or("").contains("Word check: off"));
    let with = mk_game(None, "r2", "r2", 1, Some(HashSet::from(["CAT".to_string()])));
    assert!(js(&svc::build_game_embed(&lock(&with), false))["footer"]["text"].as_str().unwrap_or("").contains("Word check: on"));
}

#[tokio::test]
async fn the_dictionary_loads_once_from_its_file_and_reports_its_state() {
    let t = TestApp::new();
    let dir = std::env::temp_dir().join(format!("feat-games-dict-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let missing = dir.join("nope.txt");
    svc::testing::set_path(&t, &missing);
    assert!(svc::load_dictionary(&t).await.is_none());
    assert_eq!(svc::dictionary_status(&t).state, svc::DictState::Missing);
    assert!(svc::get_dictionary(&t).is_none());

    let file = dir.join("words.txt");
    std::fs::write(&file, "cat\r\nqi\n#x\n").expect("write");
    svc::testing::reset(&t);
    svc::testing::set_path(&t, &file);
    let set = svc::load_dictionary(&t).await.expect("loaded");
    assert_eq!(set.len(), 2);
    assert_eq!(svc::dictionary_status(&t), svc::DictStatus { state: svc::DictState::Loaded, size: 2 });
    std::fs::write(&file, "dog\n").expect("write");
    assert_eq!(svc::load_dictionary(&t).await.expect("memo").len(), 2, "memoised");

    std::fs::write(&file, "# nothing\n").expect("write");
    svc::testing::reset(&t);
    assert!(svc::load_dictionary(&t).await.is_none());
    assert_eq!(svc::dictionary_status(&t).state, svc::DictState::Error);
    let _ = std::fs::remove_dir_all(&dir);
    // The real path is the repository's assets/dictionaries/words.txt (absent here).
    let fresh = TestApp::new();
    assert_eq!(svc::dictionary_path(&fresh), std::path::PathBuf::from(svc::DICTIONARY_PATH));
}

// ── /games pure helpers against the JS ────────────────────────────────────────

fn games_registry() -> Registry {
    let mut reg = Registry::new();
    feat_games::register(&mut reg);
    reg
}

#[test]
fn games_list_matches_the_js() {
    let gd = &golden()["games"];
    let reg = games_registry();
    let entries = games_cmd::collect_game_commands(reg.commands());
    assert_eq!(serde_json::to_value(&entries).expect("entries"), gd["collected"]);
    let parse = |v: &Value| -> Vec<games_cmd::GameEntry> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|e| games_cmd::GameEntry {
                        name: e["name"].as_str().unwrap_or("").into(),
                        description: e["description"].as_str().unwrap_or("").into(),
                        module: e["module"].as_str().unwrap_or("").into(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let entry = |name: &str, module: &str, desc: &str| games_cmd::GameEntry { name: name.into(), description: desc.into(), module: module.into() };
    let synthetic: Vec<_> = (0..40).map(|n| entry(&format!("synthetic-game-{n}"), "games", &"x".repeat(150))).collect();
    let lists: Vec<(&str, Vec<games_cmd::GameEntry>)> = vec![
        ("real", entries.clone()),
        ("realWithSynthetic", entries.iter().cloned().chain(synthetic).collect()),
        ("empty", vec![]),
        ("other", vec![entry("trivia", "achievements", "Description of trivia"), entry("zzz", "nosuchmodule", "Description of zzz"), entry("games rps", "games", "Description of games rps")]),
        (
            "scrabbleOnly",
            ["scrabble help", "scrabble start", "scrabble odd", "scrabble word"].iter().map(|n| entry(n, "games", &format!("Description of {n}"))).collect(),
        ),
        ("long", (0..30).map(|n| entry(&format!("game-{n}"), if n % 2 == 1 { "games" } else { "economy" }, &"é".repeat(120))).collect()),
    ];
    for (key, list) in lists {
        let _ = parse(&Value::Null);
        let got = js(&games_cmd::build_games_embed(&list, None));
        assert_eq!(got, norm(&gd["embeds"][key]), "{key}");
        let fields = got["fields"].as_array().cloned().unwrap_or_default();
        assert!(fields.len() <= 25 && fields.iter().all(|f| bot_util::js::utf16_len(f["value"].as_str().unwrap_or("")) <= 1024));
    }
    let colored = GuildSettings { embed_color: Some(0x123456), ..Default::default() };
    assert_eq!(js(&games_cmd::build_games_embed(&entries, Some(&colored))), norm(&gd["colored"]));
    // Every real game path has an emoji.
    for e in &entries {
        assert!(games_cmd::game_emoji(&e.name).is_some(), "{} needs a GAME_EMOJI entry", e.name);
    }
}

#[test]
fn rps_tictactoe_and_numguess_helpers_match_the_js() {
    let gd = &golden()["games"];
    assert_eq!(js(&games_cmd::build_rps_row("123456789012345678", false)), norm(&gd["rps"]["row"]));
    assert_eq!(js(&games_cmd::build_rps_row("123456789012345678", true)), norm(&gd["rps"]["disabled"]));
    for p in games_cmd::RPS_CHOICES {
        for b in games_cmd::RPS_CHOICES {
            assert_eq!(games_cmd::decide_rps(p, b), gd["decide"][format!("{p},{b}")].as_str().expect("decide"));
        }
    }
    for pair in gd["guesses"].as_array().expect("guesses") {
        let raw = pair[0].as_str().expect("guess");
        let g: f64 = if raw == "NaN" { f64::NAN } else { raw.parse().expect("number") };
        assert_eq!(games_cmd::evaluate_guess(42, g), pair[1].as_str().expect("verdict"), "{raw}");
    }
    let empty = vec![String::new(); 9];
    assert_eq!(js(&games_cmd::build_board(&empty, "123456789012345678", false)), norm(&gd["board"]["empty"]));
    let some = ["X", "", "O", "", "X", "", "", "O", ""];
    assert_eq!(js(&games_cmd::build_board(&some, "123456789012345678", false)), norm(&gd["board"]["some"]));
    let one = ["X", "", "", "", "", "", "", "", ""];
    assert_eq!(js(&games_cmd::build_board(&one, "id", true)), norm(&gd["board"]["disabled"]));
    for w in gd["winners"].as_array().expect("winners") {
        let board: Vec<String> = w["board"].as_array().expect("board").iter().map(|c| c.as_str().unwrap_or("").to_string()).collect();
        assert_eq!(json!(games_cmd::check_winner(&board)), w["winner"]);
    }
    for [a, b, c] in games_cmd::WIN_CONDITIONS {
        let mut board = vec![String::new(); 9];
        for i in [a, b, c] {
            board[i] = "X".into();
        }
        assert_eq!(games_cmd::check_winner(&board).as_deref(), Some("X"));
    }
    let ids = &gd["customIds"];
    assert_eq!(bot_config::custom_id!("ttt-ch", "123456789012345678", "accept").expect("id"), ids["tttCh"][0].as_str().expect("id"));
    assert_eq!(bot_config::custom_id!("ttt-ch", "123456789012345678", "decline").expect("id"), ids["tttCh"][1].as_str().expect("id"));
    assert_eq!(bot_config::custom_id!("scrabble-ch", "123456789012345678", "accept").expect("id"), ids["scrabbleCh"][0].as_str().expect("id"));
    assert_eq!(bot_config::custom_id!("scrabble-ch", "123456789012345678", "decline").expect("id"), ids["scrabbleCh"][1].as_str().expect("id"));
    assert_eq!(bot_config::custom_id!("bj", "1".repeat(20), "double").expect("id"), ids["bj"].as_str().expect("id"));
}

#[test]
fn the_guess_state_machine() {
    let mut g = games_cmd::Numguess::new(42);
    assert!(matches!(g.step("0", 10_000, None), games_cmd::GuessStep::Say(_)));
    assert!(matches!(g.step("101", 11_000, None), games_cmd::GuessStep::Silent), "warnings are throttled");
    assert!(matches!(g.step("500", 16_000, None), games_cmd::GuessStep::Say(_)));
    assert_eq!(g.guess_count, 0, "out of range does not count");
    for n in 1..=6 {
        assert!(matches!(g.step(&n.to_string(), 20_000, None), games_cmd::GuessStep::Say(_)));
    }
    match g.step("1", 20_000, None) {
        games_cmd::GuessStep::End(reason, e) => {
            assert_eq!(reason, "exhausted");
            assert_eq!(js(&e)["title"], "❌  Out of Guesses!");
        }
        _ => panic!("the seventh wrong guess ends the game"),
    }
    assert!(matches!(g.step("42", 20_000, None), games_cmd::GuessStep::Silent), "nothing after the end (R235)");
    let mut w = games_cmd::Numguess::new(42);
    match w.step(" 042 ", 0, None) {
        games_cmd::GuessStep::End(reason, e) => {
            assert_eq!(reason, "won");
            assert!(js(&e)["description"].as_str().unwrap_or("").contains("**1/7** guesses"));
        }
        _ => panic!("a correct guess wins"),
    }
    let n = games_cmd::Numguess::new(7);
    assert!(js(&n.end_message("idle", 0).expect("idle"))["description"].as_str().unwrap_or("").contains("may not be able to read messages"));
    assert!(!js(&n.end_message("idle", 3).expect("idle"))["description"].as_str().unwrap_or("").contains("may not be able"));
    assert!(js(&n.end_message("time", 3).expect("time"))["description"].as_str().unwrap_or("").contains("10-minute limit"));
    assert!(n.end_message("user", 0).is_none());
    assert!(games_cmd::is_guess_message(" 42 ") && !games_cmd::is_guess_message("hello") && !games_cmd::is_guess_message("1234567"));
}

// ── casino pure helpers against the JS ────────────────────────────────────────

fn card(rank: &str) -> Card {
    Card::new(rank, "♠️")
}

fn hand_of(v: &Value) -> Vec<Card> {
    v.as_array().expect("hand").iter().map(|r| card(r.as_str().expect("rank"))).collect()
}

#[test]
fn blackjack_helpers_match_the_js() {
    let c = &golden()["casino"];
    for h in c["hands"].as_array().expect("hands") {
        let hand = hand_of(&h["hand"]);
        assert_eq!(bj::hand_value(&hand), h["value"].as_i64().expect("value"), "{}", h["hand"]);
        assert_eq!(bj::is_soft(&hand), h["soft"].as_bool().expect("soft"));
        assert_eq!(bj::is_natural(&hand), h["natural"].as_bool().expect("natural"));
        assert_eq!(bj::should_dealer_hit(&hand), h["dealerHits"].as_bool().expect("hits"));
    }
    for o in c["outcomes"].as_array().expect("outcomes") {
        let (p, d) = (hand_of(&o["player"]), hand_of(&o["dealer"]));
        let natural: Vec<Value> = [1, 5, 100].iter().map(|bet| json!(bj::natural_outcome(&p, &d, *bet))).collect();
        assert_eq!(json!(natural), o["natural"]);
        let fin: Vec<Value> = [1, 100, 200].iter().map(|s| json!(bj::final_outcome(&p, &d, *s))).collect();
        assert_eq!(json!(fin), o["final"]);
    }
    let mut rng = mulberry32(7);
    let deck: Vec<String> = bj::build_deck(&mut |n| (rng() * f64::from(n)).floor() as u32).iter().map(|c| format!("{}{}", c.rank, c.suit)).collect();
    assert_eq!(json!(deck), c["deck"]);
    for (i, (can, dis)) in [(true, false), (false, false), (true, true)].into_iter().enumerate() {
        assert_eq!(js(&bj::build_buttons("123456789012345678", can, dis)), norm(&c["buttons"][i]));
    }
}

#[test]
fn blackjack_unit_rules() {
    let h = |r: &[&str]| r.iter().map(|x| card(x)).collect::<Vec<_>>();
    assert_eq!(bj::natural_outcome(&h(&["A", "K"]), &h(&["10", "9"]), 5), Some(bj::Outcome { result: "blackjack", payout: 12 }));
    let big = bj::natural_outcome(&h(&["A", "K"]), &h(&["10", "9"]), i64::from(bot_config::COIN_MAX)).expect("natural");
    assert_eq!(big.payout, 2_500_000_000);
    assert_eq!(bj::natural_outcome(&h(&["10", "5", "6"]), &h(&["A", "K"]), 100).map(|o| o.result), Some("dealer-blackjack"));
    let mut calls = Vec::new();
    bj::build_deck(&mut |n| {
        calls.push(n);
        0
    });
    assert_eq!(calls, (0..51).map(|k| 52 - k).collect::<Vec<u32>>(), "Fisher-Yates calls rng(i + 1) from 52 down to 2");
    let deck = bj::build_deck(&mut feat_games::casino::secure_int);
    assert_eq!(deck.iter().map(|c| format!("{}{}", c.rank, c.suit)).collect::<HashSet<_>>().len(), 52);
    assert_eq!(deck.iter().map(|c| c.value).sum::<i64>(), 4 * (11 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 10 + 10 + 10));
}

#[test]
fn roulette_helpers_match_the_js() {
    let c = &golden()["casino"];
    for s in c["spaces"].as_array().expect("spaces") {
        let got = roulette::parse_space(s["input"].as_str().expect("input"));
        assert_eq!(json!(got), s["space"], "{}", s["input"]);
    }
    for (token, returns) in c["returns"].as_object().expect("returns") {
        let space = roulette::parse_space(token);
        let got: Vec<i64> = (0..37).map(|n| roulette::space_return(space.as_ref(), n, 3)).collect();
        assert_eq!(json!(got), *returns, "{token}");
        assert_eq!(got.iter().sum::<i64>(), 36 * 3, "{token}: one house edge for every bet");
    }
    for (n, col) in c["colors"].as_array().expect("colors").iter().enumerate() {
        let (label, emoji) = roulette::wheel_color(n as u32);
        assert_eq!(json!({ "label": label, "emoji": emoji }), *col);
    }
    for a in c["autocomplete"].as_array().expect("autocomplete") {
        let got: Vec<Value> = roulette::autocomplete_choices(a["query"].as_str().expect("q")).into_iter().map(|(n, v)| json!({ "name": n, "value": v })).collect();
        assert_eq!(json!(got), a["choices"], "{}", a["query"]);
    }
    assert_eq!(roulette::space_return(roulette::parse_space("17").as_ref(), 17, 1_000_000_000), 36_000_000_000);
    let mut seen = Vec::new();
    roulette::spin_number(&mut |n| {
        seen.push(n);
        0
    });
    assert_eq!(seen, [37]);
}

fn sym(e: &str) -> Symbol {
    *SYMBOLS.iter().find(|s| s.emoji == e).expect("symbol")
}

fn grid3(line: &str, other: &str) -> Vec<Symbol> {
    [line, line, line, other, "🍒", other, "🍇", other, "🍒"].iter().map(|e| sym(e)).collect()
}

#[test]
fn slots_helpers_match_the_js() {
    let c = &golden()["casino"];
    for g in c["grids"].as_array().expect("grids") {
        let grid: Vec<Symbol> = g["grid"].as_array().expect("grid").iter().map(|e| sym(e.as_str().expect("emoji"))).collect();
        let p7 = slots::slots_payout(&grid, 7);
        assert_eq!(p7.payout, g["payout7"].as_i64().expect("p7"));
        assert_eq!(slots::slots_payout(&grid, 1).payout, g["payout1"].as_i64().expect("p1"));
        assert_eq!(p7.multiplier, g["multiplier"].as_f64().expect("m"));
        assert_eq!(slots::render_grid(&grid, &p7.wins), g["render"].as_str().expect("render"));
    }
    // The reels drawn with the same rng as the JS fixture.
    let mut reels = mulberry32(99);
    for g in c["grids"].as_array().expect("grids").iter().take(60) {
        let grid = slots::spin_grid(&mut |n| (reels() * f64::from(n)).floor() as u32);
        let emojis: Vec<&str> = grid.iter().map(|s| s.emoji).collect();
        assert_eq!(json!(emojis), g["grid"]);
    }
}

#[test]
fn slots_paytable_returns_about_95_percent() {
    // review-economy.test.js
    let total = f64::from(slots::TOTAL_WEIGHT);
    let rtp = slots::LINES.len() as f64 * SYMBOLS.iter().map(|s| f64::from(s.weight).powi(3) * s.multiplier).sum::<f64>() / total.powi(3);
    assert!((0.94..=0.96).contains(&rtp), "{rtp}");
    assert!((rtp - 8800.0 / 9261.0).abs() < 1e-12);
    for s in SYMBOLS {
        let others: Vec<Symbol> = SYMBOLS.iter().filter(|o| o.emoji != s.emoji).copied().collect();
        let (x, y) = (others[0], others[1]);
        let grid = [s, s, s, x, y, x, y, x, y]; // only the top row lines up
        assert_eq!(slots::check_wins(&grid).len(), 1);
        assert!(s.multiplier.fract() == 0.0 && s.multiplier >= 2.0);
        for bet in [1, 2, 7, 100, 12345] {
            assert_eq!(slots::slots_payout(&grid, bet).payout, bet * s.multiplier as i64);
        }
    }
    let mut counts = std::collections::HashMap::new();
    for r in 0..slots::TOTAL_WEIGHT {
        *counts.entry(slots::spin_symbol(&mut |_| r).emoji).or_insert(0u32) += 1;
    }
    for s in SYMBOLS {
        assert_eq!(counts[s.emoji], s.weight);
    }
    let half = Symbol { emoji: "🍎", weight: 6, multiplier: 1.5 };
    let half_grid: Vec<Symbol> = grid3("🍎", "🍋").into_iter().map(|s| if s.emoji == "🍎" { half } else { s }).collect();
    assert_eq!(slots::slots_payout(&half_grid, 1).payout, 2, "a winning spin returns at least bet + 1");
    assert_eq!(slots::slots_payout(&half_grid, 3).payout, 4, "floor(4.5) = 4");
    let jackpot = slots::slots_payout(&[SYMBOLS[0]; 9], 1_000_000_000);
    assert_eq!((jackpot.wins.len(), jackpot.payout), (5, 75_000_000_000));
}

// ── the casino through the router, against a fake ledger ──────────────────────

const G: &str = "955026000000000001";
const U1: &str = "955026900000000001";
const U2: &str = "955026900000000002";
const CH: &str = "955026000000000500";

/// The fake escrow of economy-casino.test.js: balances move exactly as the real reserveBet / settleBet move them.
struct World {
    balance: Mutex<i64>,
    locked: Mutex<HashSet<String>>,
    reserves: Mutex<Vec<i64>>,
    settles: Mutex<Vec<(i64, i64)>>,
    timeline: Mutex<Vec<String>>,
    fail_settle: AtomicBool,
    fail_reserve: AtomicBool,
    responses: Mutex<Option<Arc<bot_core::interaction::RecordingBackend>>>,
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl World {
    fn new(balance: i64) -> Arc<Self> {
        Arc::new(Self {
            balance: Mutex::new(balance),
            locked: Mutex::default(),
            reserves: Mutex::default(),
            settles: Mutex::default(),
            timeline: Mutex::default(),
            fail_settle: AtomicBool::new(false),
            fail_reserve: AtomicBool::new(false),
            responses: Mutex::new(None),
            gate: Mutex::new(None),
        })
    }

    fn push(&self, e: String) {
        self.timeline.lock().expect("timeline").push(e);
    }

    fn mark(&self) -> String {
        let r = self.responses.lock().expect("r").clone();
        r.map(|r| r.names().join(",")).unwrap_or_default()
    }

    fn balance(&self) -> i64 {
        *self.balance.lock().expect("b")
    }

    fn settles(&self) -> Vec<(i64, i64)> {
        self.settles.lock().expect("s").clone()
    }

    fn reserves(&self) -> Vec<i64> {
        self.reserves.lock().expect("r").clone()
    }

    fn locked(&self) -> usize {
        self.locked.lock().expect("l").len()
    }
}

impl Escrow for World {
    fn try_lock(&self, _: &App, g: &str, u: &str) -> bool {
        let ok = self.locked.lock().expect("l").insert(format!("{g}:{u}"));
        if ok {
            self.push("lock".into());
        }
        ok
    }

    fn unlock(&self, _: &App, g: &str, u: &str) {
        if self.locked.lock().expect("l").remove(&format!("{g}:{u}")) {
            self.push("unlock".into());
        }
    }

    fn reserve<'a>(&'a self, _: &'a App, _: &'a str, _: &'a str, bet: i64, _: Option<&'a GuildSettings>) -> BoxFuture<'a, anyhow::Result<Reserved>> {
        Box::pin(async move {
            self.push(format!("reserve:{bet}|{}", self.mark()));
            self.reserves.lock().expect("r").push(bet);
            let gate = self.gate.lock().expect("g").clone();
            if let Some(g) = gate
                && self.reserves().len() == 2
            {
                let _ = g.acquire().await;
            }
            tokio::task::yield_now().await;
            if self.fail_reserve.load(Ordering::SeqCst) {
                anyhow::bail!("db down");
            }
            let mut b = self.balance.lock().expect("b");
            if bet > *b {
                return Ok(Reserved::Insufficient { balance: *b });
            }
            *b -= bet;
            Ok(Reserved::Ok { balance: *b, before: *b + bet })
        })
    }

    fn settle<'a>(&'a self, _: &'a App, _: &'a str, _: &'a str, payout: i64, settings: Option<&'a GuildSettings>, stake: i64) -> BoxFuture<'a, anyhow::Result<Settled>> {
        Box::pin(async move {
            self.push(format!("settle:{payout}/{stake}"));
            self.settles.lock().expect("s").push((payout, stake));
            tokio::task::yield_now().await;
            if self.fail_settle.load(Ordering::SeqCst) {
                anyhow::bail!("db down");
            }
            let c = clamp_payout(payout, stake, economy_config(settings).max_payout);
            let mut b = self.balance.lock().expect("b");
            *b += c.payout;
            Ok(Settled { balance: *b, capped: false, payout_capped: c.capped })
        })
    }
}

fn settings_row(edit: impl FnOnce(&mut GuildSettings)) -> GuildSettings {
    let mut s = GuildSettings { guild_id: G.into(), games_enabled: true, economy_enabled: true, ..Default::default() };
    edit(&mut s);
    s
}

fn deck_of(ranks: &[&str]) -> Vec<Card> {
    ranks.iter().rev().map(|r| card(r)).collect()
}

struct Casino {
    t: TestApp,
    world: Arc<World>,
}

fn casino(balance: i64, settings: impl FnOnce(&mut GuildSettings), deck: &[&str], spin: u32, grid: Vec<Symbol>, guild: Option<FakeGuild>) -> Casino {
    let t = TestApp::new().with_guilds([G]);
    let t = match guild {
        Some(g) => t.with_fake_guilds(&[g]),
        None => t,
    };
    t.settings_store.put(settings_row(settings));
    let world = World::new(balance);
    *world.responses.lock().expect("r") = Some(t.responses.clone());
    let deck: Vec<Card> = deck_of(deck);
    let deps = CasinoDeps {
        escrow: world.clone(),
        new_deck: Arc::new(move || deck.clone()),
        spin_number: Arc::new(move || Ok(spin)),
        spin_grid: Arc::new(move || Ok(grid.clone())),
        sleep: Arc::new(|_| Box::pin(async {})),
        ..CasinoDeps::default()
    };
    let mut reg = Registry::new();
    reg.command(bj::command_with(deps.clone()));
    reg.command(roulette::command_with(deps.clone()));
    reg.command(slots::command_with(deps));
    t.install(reg);
    Casino { t, world }
}

fn slash(name: &str, bet: i64) -> FakeInteraction {
    FakeInteraction::slash(name).guild(G).user(U1).channel(CH).option("bet", option_type::INTEGER, json!(bet))
}

fn spin_slash(bet: i64, space: &str) -> FakeInteraction {
    slash("roulette", bet).option("space", option_type::STRING, json!(space))
}

async fn run(c: &Casino, i: FakeInteraction) {
    route_interaction(&c.t, i.build()).await;
}

fn last_edit(c: &Casino) -> RecordedCall {
    c.t.responses.calls().into_iter().rfind(|x| x.method == "editReply").expect("an editReply")
}

fn last_private(c: &Casino) -> RecordedCall {
    c.t.responses.calls().into_iter().rfind(|x| x.method == "reply" || x.method == "followUp").expect("a private answer")
}

#[tokio::test]
async fn every_casino_game_acknowledges_before_the_first_database_access() {
    for (name, i) in [("blackjack", slash("blackjack", 100)), ("roulette", spin_slash(100, "red")), ("slots", slash("slots", 100))] {
        let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 17, grid3("🍎", "🍋"), None);
        run(&c, i).await;
        let tl = c.world.timeline.lock().expect("t").clone();
        assert_eq!(tl[0], "lock", "{name}");
        assert!(tl[1].starts_with("reserve:100|deferReply"), "{name}: {tl:?}");
        assert!(calls_named(&c.t.responses.calls(), "reply").is_empty(), "{name}: no ephemeral reply on the happy path");
        c.t.collectors().len(); // blackjack leaves its hand open
    }
}

fn channel_guild(channels: &[&str]) -> FakeGuild {
    let mut g = FakeGuild::new(G, "955026900000000099").everyone(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES).member(TEST_BOT_ID, &[]);
    for ch in channels {
        g = g.channel(ch, 0, "chan", &[]);
    }
    g.channel(CH, 0, "here", &[])
}

#[tokio::test]
async fn channel_rules_max_bet_lock_funds_and_outages_refuse_privately() {
    for (name, mk) in [
        ("blackjack", (|| slash("blackjack", 100)) as fn() -> FakeInteraction),
        ("roulette", || spin_slash(100, "red")),
        ("slots", || slash("slots", 100)),
    ] {
        // Allow list without this channel.
        let c = casino(
            1000,
            |s| {
                s.economy_game_channel_ids = vec!["955026000000000700".into(), "955026000000000701".into()];
                s.economy_game_channel_mode = ChannelListMode::Allow;
            },
            &["10", "9", "10", "8"],
            17,
            grid3("🍎", "🍋"),
            Some(channel_guild(&["955026000000000700", "955026000000000701"])),
        );
        run(&c, mk()).await;
        assert_eq!(c.t.responses.names(), ["reply"], "{name}");
        let r = last_private(&c);
        assert!(r.ephemeral());
        assert_eq!(r.embed_description(), Some("Casino games can only be played in <#955026000000000700>, <#955026000000000701>."));
        assert!(c.world.timeline.lock().expect("t").is_empty());

        // Deny list with this channel.
        let c = casino(
            1000,
            |s| {
                s.economy_game_channel_ids = vec![CH.into()];
                s.economy_game_channel_mode = ChannelListMode::Deny;
            },
            &["10", "9", "10", "8"],
            17,
            grid3("🍎", "🍋"),
            Some(channel_guild(&[])),
        );
        run(&c, mk()).await;
        assert_eq!(last_private(&c).embed_description(), Some("Casino games are not allowed in this channel."), "{name}");

        // Max bet.
        let c = casino(1000, |s| s.economy_max_bet = 50, &["10", "9", "10", "8"], 17, grid3("🍎", "🍋"), None);
        run(&c, mk()).await;
        let r = last_private(&c);
        assert_eq!(r.embed_title(), Some("❌  Bet Too High"));
        assert!(r.embed_description().unwrap_or("").contains("maximum bet on this server is 🪙 **50** coins"));
        assert_eq!(c.t.responses.names(), ["reply"]);

        // A game already running.
        let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 17, grid3("🍎", "🍋"), None);
        c.world.try_lock(&c.t, G, U1);
        run(&c, mk()).await;
        assert!(c.world.reserves().is_empty());
        assert_eq!(last_private(&c).embed_title(), Some("⚠️  Game In Progress"));
        assert!(last_private(&c).ephemeral());

        // Insufficient funds: private after the defer.
        let c = casino(40, |_| {}, &["10", "9", "10", "8"], 17, grid3("🍎", "🍋"), None);
        run(&c, mk()).await;
        assert_eq!(c.t.responses.names(), ["deferReply", "deleteReply", "followUp"], "{name}");
        let f = last_private(&c);
        assert!(f.ephemeral());
        assert_eq!(f.embed_title(), Some("❌  Insufficient Funds"));
        assert!(f.embed_description().unwrap_or("").contains("only have 🪙 **40** coins"));
        assert!(c.world.settles().is_empty());
        assert_eq!((c.world.balance(), c.world.locked()), (40, 0));

        // A database failure while reserving.
        let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 17, grid3("🍎", "🍋"), None);
        c.world.fail_reserve.store(true, Ordering::SeqCst);
        run(&c, mk()).await;
        assert!(c.world.settles().is_empty());
        assert_eq!((c.world.balance(), c.world.locked()), (1000, 0));
        assert_eq!(last_private(&c).embed_title(), Some("❌  Economy Unavailable"));
    }
}

#[tokio::test]
async fn settings_that_cannot_be_loaded_refuse_without_a_lock() {
    let t = TestApp::new().with_guilds([G]);
    t.settings_store.put(settings_row(|_| {}));
    let world = World::new(1000);
    let deps = CasinoDeps { escrow: world.clone(), load_settings: Some(Arc::new(|| None)), ..CasinoDeps::default() };
    let mut reg = Registry::new();
    reg.command(slots::command_with(deps));
    t.install(reg);
    route_interaction(&t, slash("slots", 100).build()).await;
    assert_eq!(t.responses.calls()[0].embed_title(), Some("❌  Temporarily Unavailable"));
    assert_eq!((world.reserves().len(), world.locked()), (0, 0));
}

#[tokio::test]
async fn slots_win_loss_cap_and_failures() {
    let c = casino(1000, |_| {}, &[], 0, grid3("🍎", "🍋"), None);
    run(&c, slash("slots", 100)).await;
    assert_eq!(c.world.settles(), [(300, 100)]);
    assert_eq!(c.world.balance(), 1200);
    let e = last_edit(&c);
    assert_eq!(field(&e, "Bet"), Some("🪙 **100** coins"));
    assert_eq!(field(&e, "Payout"), Some("🪙 **300** coins"));
    assert_eq!(field(&e, "Net"), Some("+🪙 **200** coins"));
    assert_eq!(field(&e, "Balance"), Some("1,000 → 1,200 coins"));
    assert!(e.embed_description().unwrap_or("").contains("1 winning line (3× payout) — +🪙 **200** coins!"));
    assert_eq!(e.body["embeds"][0]["color"], 0x57f287);
    assert_eq!(c.world.locked(), 0);

    let c = casino(1000, |_| {}, &[], 0, grid3("🍎", "🍋"), None);
    run(&c, slash("slots", 1)).await;
    assert_eq!(c.world.settles(), [(3, 1)]);
    assert_eq!(field(&last_edit(&c), "Net"), Some("+🪙 **2** coins"));

    let none: Vec<Symbol> = ["🍎", "🍋", "🍒", "🍇", "💎", "7️⃣", "🍎", "🍋", "🍒"].iter().map(|e| sym(e)).collect();
    let c = casino(1000, |_| {}, &[], 0, none, None);
    run(&c, slash("slots", 100)).await;
    assert_eq!(c.world.settles(), [(0, 100)]);
    let e = last_edit(&c);
    assert_eq!(field(&e, "Net"), Some("−🪙 **100** coins"));
    assert_eq!(field(&e, "Balance"), Some("1,000 → 900 coins"));
    assert_eq!(e.body["embeds"][0]["color"], 0xed4245);

    let c = casino(1000, |s| s.economy_max_payout = 120, &[], 0, grid3("💎", "🍋"), None);
    run(&c, slash("slots", 100)).await;
    assert_eq!(c.world.settles(), [(1500, 100)]);
    assert_eq!(c.world.balance(), 900 + 220);
    let e = last_edit(&c);
    assert_eq!(field(&e, "Payout"), Some("🪙 **220** coins"));
    assert!(e.embed_description().unwrap_or("").contains("Winnings capped at 🪙 **120** coins by the server's max payout."));

    let c = casino(1000, |_| {}, &[], 0, grid3("🍎", "🍋"), None);
    c.world.fail_settle.store(true, Ordering::SeqCst);
    run(&c, slash("slots", 100)).await;
    assert!(field(&last_edit(&c), "Balance").unwrap_or("").contains("Payout failed"));
    assert_eq!(c.world.locked(), 0);

    let c = casino(1000, |_| {}, &[], 0, grid3("🍎", "🍋"), None);
    c.t.responses.fail_next("editReply", 10008);
    run(&c, slash("slots", 100)).await;
    assert_eq!(c.world.settles(), [(300, 100)]);
    assert_eq!(c.world.locked(), 0);

    // A reel jam between the reserve and the settlement refunds the stake once.
    let t = TestApp::new().with_guilds([G]);
    t.settings_store.put(settings_row(|_| {}));
    let world = World::new(1000);
    let deps = CasinoDeps { escrow: world.clone(), spin_grid: Arc::new(|| anyhow::bail!("reel jam")), ..CasinoDeps::default() };
    let mut reg = Registry::new();
    reg.command(slots::command_with(deps));
    t.install(reg);
    route_interaction(&t, slash("slots", 100).build()).await;
    assert_eq!(world.settles(), [(100, 100)]);
    assert_eq!((world.balance(), world.locked()), (1000, 0));
}

#[tokio::test]
async fn roulette_flows() {
    for bad in ["12 red", "3.9", "00", "37", "x"] {
        let c = casino(1000, |_| {}, &[], 5, vec![], None);
        run(&c, spin_slash(10, bad)).await;
        assert_eq!(c.t.responses.names(), ["reply"], "{bad}");
        let e = last_private(&c);
        assert_eq!(e.embed_title(), Some("❌  Invalid Space"));
        assert!(e.embed_description().unwrap_or("").ends_with(
            "is not a valid bet. Use red, black, green, even, odd, low, high, 1-12, 13-24, 25-36, or a number from 0 to 36."
        ));
        assert!(c.world.timeline.lock().expect("t").is_empty());
    }

    let c = casino(1000, |_| {}, &[], 17, vec![], None);
    run(&c, spin_slash(10, "17")).await;
    assert_eq!(c.t.responses.names(), ["deferReply", "editReply", "editReply", "editReply", "editReply"]);
    assert_eq!(c.world.settles(), [(360, 10)]);
    assert_eq!(c.world.balance(), 1000 - 10 + 360);
    let e = last_edit(&c);
    assert_eq!(e.embed_title(), Some("🎰 The ball lands on 17 ⚫ Black"));
    assert_eq!(field(&e, "Your bet"), Some("Number 17 — 🪙 **10** coins"));
    assert_eq!(field(&e, "Net"), Some("+🪙 **350** coins"));
    assert_eq!(field(&e, "Balance"), Some("1,000 → 1,350 coins"));
    assert!(e.embed_description().unwrap_or("").contains("You win +🪙 **350** coins!"));
    assert_eq!(c.world.locked(), 0);
    let tl = c.world.timeline.lock().expect("t").clone();
    assert!(tl.iter().position(|x| x.starts_with("reserve")) < tl.iter().position(|x| x.starts_with("settle")));

    for space in ["green", "0"] {
        let c = casino(1000, |_| {}, &[], 0, vec![], None);
        run(&c, spin_slash(100, space)).await;
        assert_eq!(c.world.settles(), [(3600, 100)], "{space}");
        assert_eq!(last_edit(&c).embed_title(), Some("🎰 The ball lands on 0 🟢 Green"));
        assert_eq!(field(&last_edit(&c), "Your bet"), Some("Green (0) — 🪙 **100** coins"));
    }

    let c = casino(1000, |_| {}, &[], 0, vec![], None);
    run(&c, spin_slash(100, "red")).await;
    assert_eq!(c.world.settles(), [(0, 100)]);
    assert!(last_edit(&c).embed_description().unwrap_or("").contains("You lose"));
    assert_eq!(field(&last_edit(&c), "Balance"), Some("1,000 → 900 coins"));

    let c = casino(1000, |_| {}, &[], 17, vec![], None);
    c.t.responses.set_failing(true);
    run(&c, spin_slash(10, "17")).await;
    assert!(c.world.settles().is_empty(), "a failed acknowledgement takes nothing");
    assert_eq!(c.world.locked(), 0);

    let c = casino(1000, |s| s.economy_max_payout = 100, &[], 17, vec![], None);
    run(&c, spin_slash(10, "17")).await;
    assert_eq!(c.world.balance(), 990 + 110);
    assert!(last_edit(&c).embed_description().unwrap_or("").contains("Winnings capped at 🪙 **100** coins"));

    let c = casino(1000, |_| {}, &[], 17, vec![], None);
    c.world.fail_settle.store(true, Ordering::SeqCst);
    run(&c, spin_slash(10, "17")).await;
    assert!(field(&last_edit(&c), "Balance").unwrap_or("").contains("Payout failed"));

    // Two games by the same member at once: the second is refused, the first settles once.
    let c = casino(1000, |_| {}, &[], 17, vec![], None);
    futures::future::join(run(&c, spin_slash(10, "17")), run(&c, spin_slash(10, "17"))).await;
    assert_eq!((c.world.settles().len(), c.world.reserves().len()), (1, 1));
    assert!(c.t.responses.calls().iter().any(|x| x.embed_title() == Some("⚠️  Game In Progress")));
}

#[tokio::test]
async fn roulette_autocomplete_answers_without_a_database() {
    let c = casino(1000, |_| {}, &[], 0, vec![], None);
    let i = FakeInteraction::autocomplete("roulette").guild(G).user(U1).focused("space", "1").option("bet", option_type::INTEGER, json!(5));
    route_interaction(&c.t, i.build()).await;
    let choices = c.t.responses.calls()[0].choices();
    let values: Vec<&str> = choices.iter().take(4).filter_map(|x| x["value"].as_str()).collect();
    assert_eq!(values, ["1-12", "13-24", "1", "10"]);
}

async fn start_hand(c: &Casino, bet: i64) {
    run(c, slash("blackjack", bet)).await;
}

async fn click(c: &Casino, action: &str, user: &str) {
    let id = hand_id(c);
    route_interaction(&c.t, FakeInteraction::button(&format!("bj:{id}:{action}")).guild(G).user(user).channel(CH).build()).await;
}

fn hand_id(c: &Casino) -> String {
    let edit = c.t.responses.calls().into_iter().rfind(|x| x.method == "editReply" && x.body["components"].as_array().is_some_and(|a| !a.is_empty())).expect("buttons");
    let id = edit.body["components"][0]["components"][0]["custom_id"].as_str().expect("id").to_string();
    id.split(':').nth(1).expect("hand id").to_string()
}

#[tokio::test]
async fn blackjack_naturals_settle_at_once() {
    let c = casino(1000, |_| {}, &["A", "K", "5", "6"], 0, vec![], None);
    start_hand(&c, 100).await;
    assert_eq!(c.t.collectors().len(), 0);
    assert_eq!(c.world.settles(), [(250, 100)]);
    assert_eq!(c.world.balance(), 1150);
    let e = last_edit(&c);
    assert_eq!(e.body["components"], json!([]));
    assert_eq!(e.embed_description(), Some("🃏 Blackjack! Paid 3:2."));
    assert_eq!(field(&e, "Net"), Some("+🪙 **150** coins"));
    assert_eq!(field(&e, "Balance"), Some("1,000 → 1,150 coins"));
    assert_eq!(field(&e, "Dealer"), Some("♠️5  ♠️6"));
    assert_eq!(c.world.locked(), 0);

    let c = casino(1000, |_| {}, &["A", "K", "A", "Q"], 0, vec![], None);
    start_hand(&c, 100).await;
    assert_eq!((c.world.settles(), c.world.balance()), (vec![(100, 100)], 1000));
    assert_eq!(last_edit(&c).embed_description(), Some("🤝 Both have blackjack — push."));

    let c = casino(1000, |_| {}, &["10", "9", "A", "K"], 0, vec![], None);
    start_hand(&c, 100).await;
    assert_eq!((c.world.settles(), c.world.balance()), (vec![(0, 100)], 900));
    assert_eq!(last_edit(&c).embed_description(), Some("🂡 Dealer has blackjack."));
    assert_eq!(c.world.locked(), 0);

    let c = casino(1000, |s| s.economy_max_payout = 100, &["A", "K", "5", "6"], 0, vec![], None);
    start_hand(&c, 100).await;
    assert_eq!(c.world.balance(), 1100, "a natural is capped like any win");
    assert!(last_edit(&c).embed_description().unwrap_or("").contains("Winnings capped"));

    let c = casino(1000, |_| {}, &["A", "K", "5", "6"], 0, vec![], None);
    c.t.responses.fail_next("editReply", 10008);
    start_hand(&c, 100).await;
    assert_eq!((c.world.settles(), c.world.locked()), (vec![(250, 100)], 0));
}

#[tokio::test]
async fn blackjack_stand_double_hit_and_strangers() {
    let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 0, vec![], None);
    start_hand(&c, 100).await;
    assert_eq!(c.t.collectors().len(), 1);
    let e = last_edit(&c);
    assert_eq!(field(&e, "Dealer"), Some("♠️10  🂠"));
    assert!(e.body["embeds"][0]["fields"].as_array().expect("fields").iter().any(|f| f["name"] == "Dealer (?)"));
    assert_eq!(e.embed_description(), Some("Hit, stand or double down."));
    let ids: Vec<&str> = e.body["components"][0]["components"].as_array().expect("buttons").iter().filter_map(|b| b["custom_id"].as_str()).collect();
    let id = hand_id(&c);
    assert_eq!(ids, [format!("bj:{id}:hit"), format!("bj:{id}:stand"), format!("bj:{id}:double")]);
    assert_eq!(c.world.locked(), 1, "the collector owns the lock");

    // A stranger gets a private reply and nothing else happens.
    let before = c.t.responses.calls().len();
    click(&c, "stand", U2).await;
    wait_for("the stranger's answer", || c.t.responses.calls().len() > before).await;
    let r = c.t.responses.calls()[before].clone();
    assert_eq!((r.method, r.content(), r.ephemeral()), ("reply", Some("❌ This isn't your game!"), true));
    assert!(c.world.settles().is_empty());

    click(&c, "stand", U1).await;
    wait_for("the settlement", || c.world.locked() == 0).await;
    assert_eq!(c.world.settles(), [(200, 100)]);
    assert_eq!(c.world.balance(), 1100);
    let after: Vec<&str> = c.t.responses.names()[before + 1..].to_vec();
    assert_eq!(after[0], "deferUpdate", "the acknowledgement comes first");
    let e = last_edit(&c);
    assert_eq!(e.body["components"], json!([]));
    assert_eq!(e.embed_description(), Some("🎉 You win!"));
    assert_eq!(field(&e, "Dealer"), Some("♠️10  ♠️8"));
    assert_eq!(c.world.timeline.lock().expect("t").last().map(String::as_str), Some("unlock"));
    settle_tasks().await;
    assert_eq!(c.t.collectors().len(), 0);

    // Double down.
    let c = casino(1000, |_| {}, &["6", "5", "10", "8", "10"], 0, vec![], None);
    start_hand(&c, 100).await;
    click(&c, "double", U1).await;
    wait_for("the double", || c.world.locked() == 0).await;
    assert_eq!(c.world.reserves(), [100, 100]);
    assert_eq!(c.world.settles(), [(400, 200)]);
    assert_eq!(c.world.balance(), 1200);
    let e = last_edit(&c);
    assert_eq!(e.embed_description(), Some("🎉 You win! (doubled down)"));
    assert_eq!(field(&e, "Bet"), Some("🪙 **200** coins"));
    assert_eq!(field(&e, "Balance"), Some("1,000 → 1,200 coins"));
    assert_eq!(field(&e, "You"), Some("♠️6  ♠️5  ♠️10"));

    // A double down without the coins: refused privately, the hand goes on.
    let c = casino(150, |_| {}, &["6", "5", "10", "8", "10"], 0, vec![], None);
    start_hand(&c, 100).await;
    let before = c.t.responses.calls().len();
    click(&c, "double", U1).await;
    wait_for("the refusal", || c.t.responses.calls().len() >= before + 2).await;
    let calls = c.t.responses.calls();
    assert_eq!((calls[before].method, calls[before + 1].method), ("deferUpdate", "followUp"));
    assert!(calls[before + 1].ephemeral());
    assert!(calls[before + 1].embed_description().unwrap_or("").contains("need another 🪙 **100** coins"));
    assert_eq!((c.world.settles().len(), c.world.locked(), c.world.balance()), (0, 1, 50));

    // Double disabled past the max bet.
    let c = casino(1000, |s| s.economy_max_bet = 150, &["6", "5", "10", "8", "10"], 0, vec![], None);
    start_hand(&c, 100).await;
    let e = last_edit(&c);
    assert_eq!(e.body["components"][0]["components"][2]["disabled"], true);
    assert_eq!(e.embed_description(), Some("Hit or stand."));
    let before = c.t.responses.calls().len();
    click(&c, "double", U1).await;
    wait_for("the ack", || c.t.responses.calls().len() > before).await;
    settle_tasks().await;
    assert_eq!(c.world.reserves(), [100]);

    // Hit, then 21 stands automatically.
    let c = casino(1000, |_| {}, &["10", "5", "10", "7", "2", "4", "K"], 0, vec![], None);
    start_hand(&c, 100).await;
    click(&c, "hit", U1).await;
    wait_for("the hit", || last_edit(&c).embed_description() == Some("Hit or stand.")).await;
    let e = last_edit(&c);
    assert_eq!(e.body["components"][0]["components"][2]["disabled"], true, "no double after a hit");
    assert_eq!(field(&e, "You"), Some("♠️10  ♠️5  ♠️2"));
    settle_tasks().await;
    click(&c, "hit", U1).await;
    wait_for("21 stands", || c.world.locked() == 0).await;
    assert_eq!(c.world.settles(), [(200, 100)]);

    // A bust loses the stake; the dealer does not draw.
    let c = casino(1000, |_| {}, &["10", "6", "10", "7", "K"], 0, vec![], None);
    start_hand(&c, 100).await;
    click(&c, "hit", U1).await;
    wait_for("the bust", || c.world.locked() == 0).await;
    assert_eq!(c.world.settles(), [(0, 100)]);
    assert_eq!(last_edit(&c).embed_description(), Some("💥 You busted."));
    assert_eq!(field(&last_edit(&c), "Dealer"), Some("♠️10  ♠️7"));

    // Soft 17: the dealer hits and stands on the hard 17.
    let c = casino(1000, |_| {}, &["10", "10", "A", "6", "10"], 0, vec![], None);
    start_hand(&c, 100).await;
    click(&c, "stand", U1).await;
    wait_for("the stand", || c.world.locked() == 0).await;
    let e = last_edit(&c);
    assert!(e.body["embeds"][0]["fields"].as_array().expect("fields").iter().any(|f| f["name"] == "Dealer (17)"));
    assert_eq!(field(&e, "Dealer"), Some("♠️A  ♠️6  ♠️10"));
}

#[tokio::test]
async fn blackjack_double_stand_settles_once_and_payout_failures_show() {
    let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 0, vec![], None);
    start_hand(&c, 100).await;
    futures::future::join(click(&c, "stand", U1), click(&c, "stand", U1)).await;
    wait_for("the settlement", || c.world.locked() == 0).await;
    settle_tasks().await;
    assert_eq!(c.world.settles().len(), 1);
    assert_eq!(c.world.balance(), 1100);
    assert_eq!(calls_named(&c.t.responses.calls(), "deferUpdate").len(), 2, "the second click is acknowledged and dropped");

    let c = casino(1000, |s| s.economy_max_payout = 5, &["10", "8", "10", "8"], 0, vec![], None);
    start_hand(&c, 100).await;
    click(&c, "stand", U1).await;
    wait_for("the push", || c.world.locked() == 0).await;
    assert_eq!((c.world.settles(), c.world.balance()), (vec![(100, 100)], 1000));
    assert_eq!(field(&last_edit(&c), "Net"), Some("±🪙 **0** coins"));

    let c = casino(1000, |s| s.economy_max_payout = 50, &["6", "5", "10", "8", "10"], 0, vec![], None);
    start_hand(&c, 100).await;
    click(&c, "double", U1).await;
    wait_for("the capped double", || c.world.locked() == 0).await;
    assert_eq!((c.world.settles(), c.world.balance()), (vec![(400, 200)], 800 + 250));

    let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 0, vec![], None);
    start_hand(&c, 100).await;
    c.world.fail_settle.store(true, Ordering::SeqCst);
    click(&c, "stand", U1).await;
    wait_for("the failed payout", || c.world.locked() == 0).await;
    assert!(field(&last_edit(&c), "Balance").unwrap_or("").contains("Payout failed"));
    assert_eq!(c.world.settles().len(), 1, "no second attempt from the game");

    // The hand cannot be shown after the reserve: refunded once.
    let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 0, vec![], None);
    c.t.responses.fail_next("editReply", 10008);
    start_hand(&c, 100).await;
    assert_eq!((c.world.settles(), c.world.balance(), c.world.locked(), c.t.collectors().len()), (vec![(100, 100)], 1000, 0, 0));
    assert!(last_private(&c).embed_description().unwrap_or("").contains("refunded"));

    // A second /blackjack while a hand is live.
    let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 0, vec![], None);
    start_hand(&c, 100).await;
    start_hand(&c, 100).await;
    assert!(c.t.responses.calls().iter().any(|x| x.embed_title() == Some("⚠️  Game In Progress")));
    assert_eq!(c.world.reserves(), [100]);
}

#[tokio::test(start_paused = true)]
async fn blackjack_an_abandoned_hand_is_forfeited() {
    let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 0, vec![], None);
    start_hand(&c, 100).await;
    assert_eq!(c.world.locked(), 1);
    tokio::time::sleep(Duration::from_millis(bj::IDLE_MS + 10)).await;
    settle_paused().await;
    assert!(c.world.settles().is_empty(), "forfeit is not a settlement");
    assert_eq!((c.world.balance(), c.world.locked()), (900, 0));
    let e = last_edit(&c);
    assert_eq!(e.body["components"], json!([]));
    assert!(e.embed_description().unwrap_or("").contains("walked away from the table. Your bet of 🪙 **100** coins is forfeited."));
    assert_eq!(e.body["embeds"][0]["color"], 0xfee75c);

    // The collector ending while the double-down reservation is in flight refunds the extra stake only.
    let c = casino(1000, |_| {}, &["6", "5", "10", "8", "10"], 0, vec![], None);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *c.world.gate.lock().expect("g") = Some(gate.clone());
    start_hand(&c, 100).await;
    let id = hand_id(&c);
    let t = c.t.app.clone();
    tokio::spawn(async move {
        route_interaction(&t, FakeInteraction::button(&format!("bj:{id}:double")).guild(G).user(U1).channel(CH).build()).await;
    });
    settle_paused().await;
    assert_eq!(c.world.reserves(), [100, 100]);
    tokio::time::sleep(Duration::from_millis(bj::IDLE_MS + 10)).await;
    settle_paused().await;
    gate.add_permits(1);
    settle_paused().await;
    assert_eq!(c.world.settles(), [(100, 100)], "only the extra stake goes back");
    assert_eq!((c.world.balance(), c.world.locked()), (900, 0));
    assert!(last_edit(&c).embed_description().unwrap_or("").contains("forfeited"));
}

// ── /games and /scrabble through the router ───────────────────────────────────

const GG: &str = "955027000000000001";
const P1: &str = "955027900000000001";
const P2: &str = "955027900000000002";
const P3: &str = "955027900000000003";
const GCH: &str = "955027000000000500";

fn games_app(guild: Option<FakeGuild>) -> TestApp {
    let t = TestApp::new().with_guilds([GG]);
    let t = match guild {
        Some(g) => t.with_fake_guilds(&[g]),
        None => t,
    };
    t.settings_store.put(GuildSettings { guild_id: GG.into(), games_enabled: true, economy_enabled: true, embed_color: Some(0x123456), ..Default::default() });
    t.install(games_registry());
    t
}

fn games_slash(sub: &str, user: &str) -> FakeInteraction {
    FakeInteraction::slash("games").guild(GG).user(user).channel(GCH).subcommand(sub)
}

async fn press(t: &TestApp, custom_id: &str, user: &str) {
    route_interaction(t, FakeInteraction::button(custom_id).guild(GG).user(user).channel(GCH).build()).await;
}

fn last(t: &TestApp, method: &str) -> RecordedCall {
    t.responses.calls().into_iter().rfind(|c| c.method == method).unwrap_or_else(|| panic!("no {method}"))
}

fn first_custom_id(t: &TestApp, prefix: &str) -> String {
    for c in t.responses.calls().iter().rev() {
        for row in c.body["components"].as_array().into_iter().flatten() {
            for b in row["components"].as_array().into_iter().flatten() {
                if let Some(id) = b["custom_id"].as_str()
                    && id.starts_with(&format!("{prefix}:"))
                {
                    return id.to_string();
                }
            }
        }
    }
    panic!("no {prefix} button")
}

fn game_id_of(t: &TestApp, prefix: &str) -> String {
    first_custom_id(t, prefix).split(':').nth(1).expect("id").to_string()
}

#[tokio::test]
async fn games_list_hides_modules_that_are_off() {
    let t = games_app(None);
    route_interaction(&t, games_slash("list", P1).build()).await;
    let text = last(&t, "reply").body.to_string();
    assert!(text.contains("/slots") && text.contains("/games rps") && text.contains("🎰 Casino"));
    let t = games_app(None);
    t.settings_store.put(GuildSettings { guild_id: GG.into(), games_enabled: true, economy_enabled: false, ..Default::default() });
    route_interaction(&t, games_slash("list", P1).build()).await;
    let text = last(&t, "reply").body.to_string();
    assert!(text.contains("/games rps") && !text.contains("/slots"), "the casino is hidden while Economy is off");
}

#[tokio::test]
async fn rps_a_double_click_plays_one_round_and_strangers_are_refused() {
    let t = games_app(None);
    route_interaction(&t, games_slash("rps", P1).build()).await;
    assert_eq!(t.collectors().len(), 1);
    let id = game_id_of(&t, "rps");
    route_interaction(&t, games_slash("rps", P1).build()).await;
    assert_eq!(last(&t, "reply").content(), Some("⚠️ You already have an active RPS game!"));

    press(&t, &format!("rps:{id}:rock"), P2).await;
    wait_for("the stranger's answer", || t.responses.calls().iter().any(|c| c.content() == Some("❌ This isn't your game!"))).await;

    futures::future::join(press(&t, &format!("rps:{id}:rock"), P1), press(&t, &format!("rps:{id}:paper"), P1)).await;
    wait_for("the round", || !calls_named(&t.responses.calls(), "update").is_empty()).await;
    settle_tasks().await;
    let updates = calls_named(&t.responses.calls(), "update").len();
    assert_eq!(updates, 1, "one round");
    let u = last(&t, "update");
    assert!(u.embed_description().unwrap_or("").starts_with("You: 🪨 **rock**") || u.embed_description().unwrap_or("").starts_with("You: 📄 **paper**"));
    assert!(u.body["components"][0]["components"].as_array().expect("buttons").iter().all(|b| b["disabled"] == true));
    assert_eq!(t.collectors().len(), 0);
    route_interaction(&t, games_slash("rps", P1).build()).await;
    assert_eq!(t.collectors().len(), 1, "the lock is released");
}

#[tokio::test(start_paused = true)]
async fn rps_times_out_and_a_failed_reply_releases_the_lock() {
    let t = games_app(None);
    route_interaction(&t, games_slash("rps", P1).build()).await;
    tokio::time::sleep(Duration::from_millis(games_cmd::RPS_TIME_MS + 10)).await;
    settle_paused().await;
    assert!(last(&t, "editReply").embed_description().unwrap_or("").contains("Time's up"));
    t.responses.fail_next("reply", 50013);
    route_interaction(&t, games_slash("rps", P1).build()).await;
    assert!(t.responses.calls().iter().any(|c| c.embed_title() == Some("❌  Command Error")));
    route_interaction(&t, games_slash("rps", P1).build()).await;
    assert_eq!(t.collectors().len(), 1);
}

fn ttt_slash(user: &str, target: &str) -> FakeInteraction {
    games_slash("tictactoe", user).option("target", option_type::USER, json!(target))
}

#[tokio::test]
async fn tictactoe_challenge_moves_and_a_win() {
    let t = games_app(None);
    route_interaction(&t, ttt_slash(P1, P1).build()).await;
    assert_eq!(last(&t, "reply").content(), Some("❌ You cannot challenge yourself!"));
    let mut bot = ttt_slash(P1, P3);
    bot.json_mut()["data"]["resolved"] = json!({ "users": { P3: { "id": P3, "username": "bot", "discriminator": "0", "avatar": null, "bot": true } }, "members": { P3: { "roles": [], "joined_at": "2025-01-01T00:00:00.000000+00:00", "flags": 0, "permissions": "0" } } });
    route_interaction(&t, bot.build()).await;
    assert_eq!(last(&t, "reply").content(), Some("❌ You cannot challenge a bot!"));
    let mut absent = ttt_slash(P1, P3);
    absent.json_mut()["data"]["resolved"] = json!({ "users": { P3: { "id": P3, "username": "gone", "discriminator": "0", "avatar": null } } });
    route_interaction(&t, absent.build()).await;
    assert_eq!(last(&t, "reply").content(), Some("❌ That user isn't in this server."));

    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    let challenge = last(&t, "reply");
    assert_eq!(challenge.content(), Some(format!("<@{P2}>").as_str()));
    assert_eq!(challenge.body["allowed_mentions"]["users"], json!([P2]));
    let id = game_id_of(&t, "ttt-ch");
    route_interaction(&t, ttt_slash(P1, P3).build()).await;
    assert_eq!(last(&t, "reply").content(), Some("⚠️ You already have an active Tic-Tac-Toe game!"));
    route_interaction(&t, ttt_slash(P3, P2).build()).await;
    assert!(last(&t, "reply").content().unwrap_or("").ends_with("is already in a game!"));

    press(&t, &format!("ttt-ch:{id}:accept"), P3).await;
    wait_for("not for you", || t.responses.calls().iter().any(|c| c.content() == Some("❌ This challenge isn't for you!"))).await;
    press(&t, &format!("ttt-ch:{id}:accept"), P1).await;
    wait_for("waiting", || t.responses.calls().iter().any(|c| c.content().is_some_and(|s| s.starts_with("⏳ Waiting for")))).await;

    futures::future::join(press(&t, &format!("ttt-ch:{id}:accept"), P2), press(&t, &format!("ttt-ch:{id}:accept"), P2)).await;
    wait_for("the board", || calls_named(&t.responses.calls(), "update").len() == 1).await;
    settle_tasks().await;
    assert_eq!(calls_named(&t.responses.calls(), "update").len(), 1, "one game");
    assert_eq!(t.collectors().len(), 1, "the game collector");
    let board = last(&t, "update");
    assert_eq!(board.body["components"].as_array().expect("rows").len(), 3);

    let mv = |user: &'static str, n: usize| {
        let t = &t;
        let id = id.clone();
        async move { press(t, &format!("ttt:{id}:{n}"), user).await }
    };
    mv(P3, 0).await;
    wait_for("spectator", || t.responses.calls().iter().any(|c| c.content() == Some("❌ You're not a player in this game!"))).await;
    mv(P2, 4).await;
    wait_for("wrong turn", || t.responses.calls().iter().any(|c| c.content().is_some_and(|s| s.contains("'s turn!")))).await;
    for (u, n, k) in [(P1, 0, 2), (P2, 3, 3), (P1, 1, 4), (P2, 4, 5)] {
        mv(u, n).await;
        wait_for("a move", || calls_named(&t.responses.calls(), "update").len() == k).await;
    }
    let taken = calls_named(&t.responses.calls(), "deferUpdate").len();
    mv(P1, 3).await;
    wait_for("taken", || calls_named(&t.responses.calls(), "deferUpdate").len() == taken + 1).await;
    mv(P1, 2).await;
    wait_for("the win", || calls_named(&t.responses.calls(), "update").len() == 6).await;
    let win = last(&t, "update");
    assert_eq!(win.embed_title(), Some("✅  Tic-Tac-Toe — Winner!"));
    assert!(win.embed_description().unwrap_or("").contains("wins with **X**s"));
    settle_tasks().await;
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    assert!(!last(&t, "reply").ephemeral(), "the locks are free again");
}

#[tokio::test]
async fn tictactoe_cancel_and_decline() {
    let t = games_app(None);
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    let id = game_id_of(&t, "ttt-ch");
    press(&t, &format!("ttt-ch:{id}:decline"), P1).await;
    wait_for("cancel", || !calls_named(&t.responses.calls(), "update").is_empty()).await;
    assert!(last(&t, "update").embed_description().unwrap_or("").contains("cancelled the challenge"));
    settle_tasks().await;
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    let id = game_id_of(&t, "ttt-ch");
    press(&t, &format!("ttt-ch:{id}:decline"), P2).await;
    wait_for("decline", || calls_named(&t.responses.calls(), "update").len() == 2).await;
    assert!(last(&t, "update").embed_description().unwrap_or("").contains("declined the challenge"));
}

#[tokio::test(start_paused = true)]
async fn tictactoe_timeouts_release_the_locks() {
    let t = games_app(None);
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    tokio::time::sleep(Duration::from_millis(games_cmd::TTT_CHALLENGE_MS + 10)).await;
    settle_paused().await;
    assert_eq!(last(&t, "editReply").embed_title(), Some("⚠️  Challenge Timed Out"));

    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    let id = game_id_of(&t, "ttt-ch");
    press(&t, &format!("ttt-ch:{id}:accept"), P2).await;
    settle_paused().await;
    tokio::time::sleep(Duration::from_millis(games_cmd::TTT_MOVE_IDLE_MS + 10)).await;
    settle_paused().await;
    assert!(last(&t, "editReply").embed_description().unwrap_or("").contains("didn't move within 60 seconds"));
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    assert_eq!(t.collectors().len(), 1, "every lock is free");
}

fn numguess_guild(view: bool) -> FakeGuild {
    let perms = if view { Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES } else { Permissions::empty() };
    FakeGuild::new(GG, "955027900000000099").everyone(perms).member(TEST_BOT_ID, &[]).channel(GCH, 0, "games", &[])
}

#[tokio::test]
async fn numguess_reads_guesses_through_the_listener() {
    let t = games_app(Some(numguess_guild(false)));
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    assert_eq!(last(&t, "reply").embed_title(), Some("❌  Missing Permissions"));

    let t = games_app(Some(numguess_guild(true)));
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    let intro = last(&t, "reply");
    assert!(intro.embed_description().unwrap_or("").contains("60 seconds"));
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    assert_eq!(last(&t, "reply").content(), Some("⚠️ You already have an active guessing game!"));

    let say = |content: &str, n: u64| {
        let msg = fake_message(&(1_000 + n).to_string(), GG, GCH, P1, content);
        t.run_hooks(bot_core::hooks::events::MessageCreate {
            guild_id: GG.parse().expect("id"),
            message: msg,
            member: None,
            channel_id: GCH.parse().expect("id"),
            channel: None,
        })
    };
    // Binary search; every hint goes through followUp.
    let (mut lo, mut hi, mut won) = (1i64, 100i64, false);
    for n in 0..7 {
        let guess = (lo + hi) / 2;
        let before = calls_named(&t.responses.calls(), "followUp").len();
        say(&guess.to_string(), n).await;
        wait_for("the hint", || calls_named(&t.responses.calls(), "followUp").len() > before).await;
        let title = last(&t, "followUp").embed_title().unwrap_or("").to_string();
        if title.contains("Correct") {
            won = true;
            break;
        } else if title.contains("Higher") {
            lo = guess + 1;
        } else {
            hi = guess - 1;
        }
    }
    assert!(won);
    settle_tasks().await;
    let before = t.responses.calls().len();
    say("50", 99).await;
    settle_tasks().await;
    assert_eq!(t.responses.calls().len(), before, "nothing after the win");
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    assert!(last(&t, "reply").embed_title().unwrap_or("").contains("Number Guessing Game"), "the lock is free again");
}

#[tokio::test(start_paused = true)]
async fn numguess_ends_when_idle_and_a_failed_intro_releases_the_lock() {
    let t = games_app(Some(numguess_guild(true)));
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    tokio::time::sleep(Duration::from_millis(games_cmd::GUESS_IDLE_MS + 10)).await;
    settle_paused().await;
    let end = last(&t, "followUp");
    assert!(end.embed_description().unwrap_or("").contains("didn't guess for 60 seconds"));
    assert!(end.embed_description().unwrap_or("").contains("may not be able to read messages"));
    assert!(!games_cmd::has_guess_game(&t, GCH, P1), "the inbox is gone");

    // R089: a failed intro reply releases the lock.
    t.responses.fail_next("reply", 50013);
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    assert!(t.responses.calls().iter().any(|c| c.embed_title() == Some("❌  Command Error")));
    assert!(!games_cmd::has_guess_game(&t, GCH, P1));
    route_interaction(&t, games_slash("numguess", P1).build()).await;
    assert!(last(&t, "reply").embed_title().unwrap_or("").contains("Number Guessing Game"));
    assert!(games_cmd::has_guess_game(&t, GCH, P1));
}

#[tokio::test]
async fn tictactoe_a_failed_challenge_reply_releases_both_players() {
    let t = games_app(None);
    t.responses.fail_next("reply", 50013);
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    assert!(t.responses.calls().iter().any(|c| c.embed_title() == Some("❌  Command Error")));
    route_interaction(&t, ttt_slash(P1, P2).build()).await;
    assert_eq!(t.collectors().len(), 1, "both players are free");
}

// ── /scrabble ────────────────────────────────────────────────────────────────

fn scrabble_slash(sub: &str, user: &str) -> FakeInteraction {
    FakeInteraction::slash("scrabble").guild(GG).user(user).channel(GCH).subcommand(sub)
}

async fn start_scrabble(t: &TestApp) -> String {
    route_interaction(t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
    let id = game_id_of(t, "scrabble-ch");
    press(t, &format!("scrabble-ch:{id}:accept"), P2).await;
    wait_for("the board", || svc::get_game(t, GCH).is_some() && svc::get_game(t, GCH).is_some_and(|g| lock(&g).last_board.is_some())).await;
    id
}

#[tokio::test]
async fn scrabble_a_challenge_reserves_the_channel_and_accept_starts_one_game() {
    let t = games_app(None);
    route_interaction(&t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
    let id = game_id_of(&t, "scrabble-ch");
    assert_eq!(svc::pending_challenge(&t, GCH), Some(id.clone()));
    assert_eq!(last(&t, "reply").body["allowed_mentions"]["users"], json!([P2]));
    route_interaction(&t, scrabble_slash("start", P3).option("target", option_type::USER, json!(P1)).build()).await;
    assert_eq!(last(&t, "reply").content(), Some("⚠️ There is already a Scrabble game or a pending challenge in this channel!"));

    press(&t, &format!("scrabble-ch:{id}:accept"), P3).await;
    wait_for("not for you", || t.responses.calls().iter().any(|c| c.content() == Some("❌ This challenge isn't for you!"))).await;

    futures::future::join(press(&t, &format!("scrabble-ch:{id}:accept"), P2), press(&t, &format!("scrabble-ch:{id}:accept"), P2)).await;
    wait_for("the board", || svc::get_game(&t, GCH).is_some_and(|g| lock(&g).last_board.is_some())).await;
    settle_tasks().await;
    let game = svc::get_game(&t, GCH).expect("game");
    {
        let g = lock(&game);
        assert_eq!(g.id, id);
        assert!(g.timer.is_some());
        assert!(g.dictionary.is_none());
        assert_eq!(g.settings.as_ref().and_then(|s| s.embed_color), Some(0x123456), "the guild colour is captured");
    }
    assert!(svc::pending_challenge(&t, GCH).is_none());
    let names = t.responses.names();
    assert_eq!(calls_named(&t.responses.calls(), "deferUpdate").len(), 2, "both accepts acknowledged");
    let pos = names.iter().position(|n| *n == "editReply").expect("edit");
    assert_eq!(names[pos + 1], "followUp");
    assert!(last(&t, "editReply").embed_description().unwrap_or("").contains("accepted"));
    let board = last(&t, "followUp");
    assert!(board.content().unwrap_or("").starts_with(&format!("<@{P1}>, you go first")));
    assert_eq!(board.body["allowed_mentions"]["users"], json!([P1]));
    assert_eq!(board.body["components"][0]["components"][0]["custom_id"], format!("scrabble-rack:{id}"));

    // The rack button through the router (the persistent plugin component).
    press(&t, &format!("scrabble-rack:{id}"), P1).await;
    let r = last(&t, "reply");
    assert!(r.ephemeral() && r.content().unwrap_or("").contains("Your tiles"));
}

#[tokio::test]
async fn scrabble_decline_cancel_and_the_moves() {
    let t = games_app(None);
    route_interaction(&t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
    let id = game_id_of(&t, "scrabble-ch");
    press(&t, &format!("scrabble-ch:{id}:decline"), P2).await;
    wait_for("declined", || !calls_named(&t.responses.calls(), "update").is_empty()).await;
    assert!(last(&t, "update").embed_description().unwrap_or("").contains("declined the Scrabble challenge"));
    assert!(!svc::is_channel_busy(&t, GCH));

    let _id = start_scrabble(&t).await;
    let game = svc::get_game(&t, GCH).expect("game");
    set_rack(&game, P1, "CATXYZE");
    set_rack(&game, P2, "OXQJKLM");
    {
        let mut g = lock(&game);
        g.players[0].name = "Alice".into();
        g.players[1].name = "Bob".into();
    }
    let word = |user: &str, w: &str, row: i64, col: &str| {
        scrabble_slash("word", user)
            .option("word", option_type::STRING, json!(w))
            .option("row", option_type::INTEGER, json!(row))
            .option("col", option_type::STRING, json!(col))
            .option("direction", option_type::STRING, json!("h"))
    };
    let before = t.responses.calls().len();
    route_interaction(&t, word(P1, "cat", 8, "H").build()).await;
    let calls: Vec<RecordedCall> = t.responses.calls()[before..].to_vec();
    let names: Vec<&str> = calls.iter().map(|c| c.method).collect();
    assert_eq!(names, ["deferReply", "editReply", "editFollowUp", "followUp"], "the private reply is edited before the public board");
    assert!(calls[0].ephemeral());
    assert_eq!(calls[1].embed_title(), Some("✅  Word Played!"));
    assert!(calls[1].embed_description().unwrap_or("").contains("**CAT** (10) = **10** pts"));
    assert_eq!(calls[2].body["components"], json!([]), "the old board loses its button");
    // ... and keeps its embed: serenity always sends `embeds` on a webhook edit, so the board's own embed goes back.
    assert!(calls[2].body.get("content").is_none());
    assert_eq!(calls[2].body["embeds"].as_array().map(Vec::len), Some(1), "{}", calls[2].body);
    assert_eq!(calls[2].body["embeds"][0]["title"], "🔤 Scrabble");
    let public = &calls[3];
    assert!(public.content().unwrap_or("").starts_with(&format!("<@{P2}>, it's your turn! **Alice** played **CAT** (10) for **10** points")));
    assert_eq!(public.body["allowed_mentions"]["users"], json!([P2]));
    assert!(!public.ephemeral());
    assert_eq!(lock(&game).current_turn, 1);

    // Two concurrent plays by one player apply once.
    let before = t.responses.calls().len();
    futures::future::join(route_interaction(&t, word(P2, "OX", 9, "H").build()), route_interaction(&t, word(P2, "OX", 9, "H").build())).await;
    assert_eq!(lock(&game).moves, 2);
    let edits: Vec<RecordedCall> = t.responses.calls()[before..].iter().filter(|c| c.method == "editReply").cloned().collect();
    assert!(edits.iter().any(|c| c.embed_title() == Some("✅  Word Played!")));
    assert!(edits.iter().any(|c| c.embed_description().unwrap_or("").contains("not your turn")));

    // Errors: invalid move, not a player, no game.
    set_rack(&game, P1, "CATXYZE");
    route_interaction(&t, word(P1, "CAT", 1, "A").build()).await;
    assert!(last(&t, "editReply").embed_description().unwrap_or("").contains("must touch or pass through"));
    route_interaction(&t, word(P3, "CAT", 8, "H").build()).await;
    assert_eq!(last(&t, "editReply").embed_title(), Some("❌  Not Your Game"));
    route_interaction(&t, word(P1, "CAT", 8, "H").channel("955027000000000501").build()).await;
    assert_eq!(last(&t, "editReply").embed_title(), Some("❌  No Active Game"));

    // Pass and exchange (never reveals the tiles), then the six-scoreless-turn ending.
    route_interaction(&t, scrabble_slash("pass", P1).build()).await;
    assert!(last(&t, "followUp").content().unwrap_or("").contains("**Alice** passed."));
    let tiles: String = lock(&game).player(P2).expect("P2").rack.iter().take(2).map(|t| t.letter).collect();
    route_interaction(&t, scrabble_slash("exchange", P2).option("tiles", option_type::STRING, json!(tiles)).build()).await;
    assert!(last(&t, "followUp").content().unwrap_or("").contains("exchanged 2 tiles."));
    assert!(last(&t, "editReply").embed_description().unwrap_or("").contains("swapped **2** tiles"));
    for _ in 0..4 {
        let cur = svc::current_player_id(&lock(&game)).to_string();
        route_interaction(&t, scrabble_slash("pass", &cur).build()).await;
    }
    assert!(lock(&game).ended);
    assert!(svc::get_game(&t, GCH).is_none());
    let result = last(&t, "followUp");
    assert_eq!(result.content(), Some(format!("🏁 <@{P1}> <@{P2}>").as_str()));
    assert_eq!(result.body["embeds"].as_array().expect("embeds").len(), 2);
    assert!(result.body["embeds"][1]["description"].as_str().unwrap_or("").contains("Six scoreless turns"));
}

#[tokio::test]
async fn scrabble_resign_asks_first_and_cancelling_keeps_the_game() {
    let t = games_app(None);
    start_scrabble(&t).await;
    let game = svc::get_game(&t, GCH).expect("game");
    let resign = |answer: &'static str| {
        let app = t.app.clone();
        let responses = t.responses.clone();
        async move {
            let before = responses.calls().len();
            let h = tokio::spawn({
                let app = app.clone();
                async move { route_interaction(&app, scrabble_slash("resign", P2).build()).await }
            });
            for _ in 0..400 {
                if responses.calls()[before..].iter().any(|c| c.body.to_string().contains("confirm:")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let prompt = responses.calls()[before..].iter().find(|c| c.body.to_string().contains("confirm:")).cloned().expect("the confirm prompt");
            let ids: Vec<String> = prompt.body["components"][0]["components"].as_array().expect("buttons").iter().filter_map(|b| b["custom_id"].as_str().map(str::to_string)).collect();
            let id = if answer == "yes" { &ids[0] } else { &ids[1] };
            route_interaction(&app, FakeInteraction::button(id).guild(GG).user(P2).channel(GCH).message_id("1000000000000000100").build()).await;
            let _ = h.await;
        }
    };
    resign("no").await;
    assert!(!lock(&game).ended);
    resign("yes").await;
    assert!(lock(&game).ended);
    let r = lock(&game).result.clone().expect("result");
    assert_eq!(r.reason, EndReason::Resign);
    let post = last(&t, "followUp");
    let desc = post.body["embeds"][1]["description"].as_str().unwrap_or("").to_string();
    assert!(desc.contains("resigned") && desc.contains("🏆 Winner:"));
    route_interaction(&t, scrabble_slash("resign", P1).build()).await;
    assert_eq!(last(&t, "editReply").embed_title(), Some("❌  No Active Game"));
}

#[tokio::test(start_paused = true)]
async fn scrabble_a_challenge_times_out_and_frees_the_channel() {
    let t = games_app(None);
    route_interaction(&t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
    tokio::time::sleep(Duration::from_millis(svc::CHALLENGE_TIMEOUT_MS + 10)).await;
    settle_paused().await;
    assert_eq!(last(&t, "editReply").embed_title(), Some("⚠️  Challenge Timed Out"));
    assert!(!svc::is_channel_busy(&t, GCH));
}

#[tokio::test]
async fn scrabble_a_start_that_fails_leaves_no_game() {
    let t = games_app(None);
    route_interaction(&t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
    let id = game_id_of(&t, "scrabble-ch");
    t.responses.fail_next("editReply", 50013);
    press(&t, &format!("scrabble-ch:{id}:accept"), P2).await;
    wait_for("the notice", || t.responses.calls().iter().any(|c| c.content().is_some_and(|s| s.contains("couldn't start the game")))).await;
    assert!(svc::get_game(&t, GCH).is_none());
    assert!(!svc::is_channel_busy(&t, GCH));

    t.responses.fail_next("reply", 50013);
    route_interaction(&t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
    assert!(!svc::is_channel_busy(&t, GCH), "R089: a failed challenge reply releases the reservation");
}

#[tokio::test]
async fn scrabble_help_defers_publicly_and_names_the_dictionary_state() {
    let t = games_app(None);
    svc::testing::set_dictionary(&t, HashSet::from(["AA".to_string(), "AB".to_string()]));
    route_interaction(&t, scrabble_slash("help", P1).build()).await;
    let calls = t.responses.calls();
    assert_eq!(calls[0].method, "deferReply");
    assert!(!calls[0].ephemeral());
    let rules = calls[1].body["embeds"][0]["fields"].as_array().expect("fields").iter().find(|f| f["name"].as_str().unwrap_or("").contains("Rules")).expect("rules")["value"].as_str().unwrap_or("").to_string();
    assert!(rules.contains("**2-word**"));
}

// ── database: both cross-language directions ──────────────────────────────────

async fn db_app(url: &str, guild: &str) -> DbTestApp {
    let t = DbTestApp::new(url).await.with_guilds([guild]);
    t.reset_guilds(&[guild]).await;
    t
}

fn db_registry(deck: &[&str], spin: u32, grid: Vec<Symbol>) -> Registry {
    let deck = deck_of(deck);
    let deps = CasinoDeps {
        new_deck: Arc::new(move || deck.clone()),
        spin_number: Arc::new(move || Ok(spin)),
        spin_grid: Arc::new(move || Ok(grid.clone())),
        sleep: Arc::new(|_| Box::pin(async {})),
        ..CasinoDeps::default()
    };
    let mut reg = Registry::new();
    reg.command(bj::command_with(deps.clone()));
    reg.command(roulette::command_with(deps.clone()));
    reg.command(slots::command_with(deps));
    reg
}

fn js_balance(g: &str, u: &str) -> Option<i64> {
    run_node("rust/fixtures/gen/feat-games-js.mjs", &["balance", g, u])["balance"].as_i64()
}

fn db_slash(name: &str, g: &str, u: &str, bet: i64) -> FakeInteraction {
    FakeInteraction::slash(name).guild(g).user(u).channel(CH).option("bet", option_type::INTEGER, json!(bet))
}

#[tokio::test]
async fn db_js_seeds_rust_plays_and_js_reads_the_balance() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    let (g, u) = ("955026000000000101", "955026900000000101");
    let t = db_app(&url, g).await;
    let seeded = run_node("rust/fixtures/gen/feat-games-js.mjs", &["seed", g, u, "1000"]);
    assert_eq!(seeded["balance"], 1000, "{seeded}");
    t.app.settings().invalidate_settings(g).await;

    // Slots: an apple line at 100 returns 300.
    let t1 = {
        t.install(db_registry(&["10", "9", "10", "8"], 17, grid3("🍎", "🍋")));
        &t
    };
    route_interaction(t1, db_slash("slots", g, u, 100).build()).await;
    assert_eq!(js_balance(g, u), Some(1200), "{:?}", t.responses.names());
    // Roulette: a straight-up 17 at 10 returns 360.
    route_interaction(t1, db_slash("roulette", g, u, 10).option("space", option_type::STRING, json!("17")).build()).await;
    assert_eq!(js_balance(g, u), Some(1550));
    // Blackjack: stand on 19 against 18 wins.
    route_interaction(t1, db_slash("blackjack", g, u, 100).build()).await;
    assert_eq!(js_balance(g, u), Some(1450), "the bet is escrowed before the player acts");
    let edit = t.responses.calls().into_iter().rfind(|c| c.method == "editReply").expect("hand");
    let id = edit.body["components"][0]["components"][1]["custom_id"].as_str().expect("stand").to_string();
    route_interaction(t1, FakeInteraction::button(&id).guild(g).user(u).channel(CH).build()).await;
    for _ in 0..400 {
        if !feat_economy::economy_service::is_casino_locked(&t, g, u) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(js_balance(g, u), Some(1650));
    // An empty wallet cannot bet.
    route_interaction(t1, db_slash("slots", g, u, 5000).build()).await;
    assert!(t.responses.calls().iter().any(|c| c.embed_title() == Some("❌  Insufficient Funds")));
    assert_eq!(js_balance(g, u), Some(1650));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_js_plays_and_rust_reads_the_balance() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    let (g, u) = ("955026000000000102", "955026900000000102");
    let t = db_app(&url, g).await;
    run_node("rust/fixtures/gen/feat-games-js.mjs", &["seed", g, u, "1000"]);
    let played = run_node("rust/fixtures/gen/feat-games-js.mjs", &["slots", g, u, "100", "💎", "💎", "💎", "🍋", "🍒", "🍋", "🍇", "🍋", "🍒"]);
    assert_eq!(played["balance"], 2400, "{played}");
    let account = feat_economy::economy_service::get_account(&t, g, u, None).await.expect("account");
    assert_eq!(account.balance, 2400);
    let hand = run_node("rust/fixtures/gen/feat-games-js.mjs", &["blackjack", g, u, "100", "stand", "10", "9", "10", "8"]);
    assert_eq!(hand["balance"], 2500, "{hand}");
    let account = feat_economy::economy_service::get_account(&t, g, u, None).await.expect("account");
    assert_eq!(account.balance, 2500);

    // The embeds the two bots show for the same hand agree.
    let rust_hand = {
        t.install(db_registry(&["10", "9", "10", "8"], 0, vec![]));
        route_interaction(&t, db_slash("blackjack", g, u, 100).build()).await;
        let edit = t.responses.calls().into_iter().rfind(|c| c.method == "editReply").expect("hand");
        let id = edit.body["components"][0]["components"][1]["custom_id"].as_str().expect("stand").to_string();
        route_interaction(&t, FakeInteraction::button(&id).guild(g).user(u).channel(CH).build()).await;
        for _ in 0..400 {
            if !feat_economy::economy_service::is_casino_locked(&t, g, u) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        t.responses.calls().into_iter().rfind(|c| c.method == "editReply").expect("result").body["embeds"][0].clone()
    };
    let mut a = norm(&rust_hand);
    let mut b = norm(&hand["last"]);
    for v in [&mut a, &mut b] {
        // The balance line differs only by the balance itself.
        if let Some(fields) = v["fields"].as_array_mut() {
            fields.retain(|f| f["name"] != "Balance");
        }
    }
    assert_eq!(a, b);
    assert_eq!(js_balance(g, u), Some(2600));
    t.reset_guilds(&[g]).await;
}

#[tokio::test]
async fn db_three_games_at_once_with_the_same_coins() {
    let Some(url) = database_url() else {
        eprintln!("skipped: TEST_DATABASE_URL not set");
        return;
    };
    let (g, u) = ("955026000000000103", "955026900000000103");
    let t = db_app(&url, g).await;
    run_node("rust/fixtures/gen/feat-games-js.mjs", &["seed", g, u, "500"]);
    t.install(db_registry(&["10", "9", "10", "8"], 1, ["🍎", "🍋", "🍒", "🍇", "💎", "7️⃣", "🍎", "🍋", "🍒"].iter().map(|e| sym(e)).collect()));
    futures::future::join3(
        route_interaction(&t, db_slash("roulette", g, u, 500).option("space", option_type::STRING, json!("black")).build()),
        route_interaction(&t, db_slash("slots", g, u, 500).build()),
        route_interaction(&t, db_slash("blackjack", g, u, 500).build()),
    )
    .await;
    let refused = t.responses.calls().iter().filter(|c| c.embed_title() == Some("⚠️  Game In Progress")).count();
    assert_eq!(refused, 2, "one plays, the others are refused");
    // Whichever game took the lock, its 500 is gone (a lost spin, a lost reel or a hand in play): never twice.
    assert_eq!(js_balance(g, u), Some(0));
    t.reset_guilds(&[g]).await;
}

// ── audit: JS white space around typed input ─────────────────────────────────

/// JS `\s` and `trim()` are not `char::is_whitespace` (U+0085 is white space only in Rust): the numguess filter and
/// guess, the Scrabble word and exchange parsers and the roulette space answer like the JS for every kind of space.
#[test]
fn typed_input_uses_the_js_white_space_set() {
    let js = run_node("rust/fixtures/gen/feat-games-whitespace.mjs", &[]);
    let cases = js.as_array().expect("cases");
    assert!(cases.len() >= 40, "{js}");
    for c in cases {
        let input = c["input"].as_str().unwrap_or_default();
        assert_eq!(games_cmd::is_guess_message(input), c["guess"].as_bool().unwrap_or(false), "numguess filter {input:?}");
        if let Some(value) = c["value"].as_i64() {
            let mut game = games_cmd::Numguess::new(value);
            assert!(matches!(game.step(input, 0, None), games_cmd::GuessStep::End("won", _)), "numguess guess {input:?}");
        }
        let word = match svc::parse_word_input(input) {
            Ok(tiles) => json!({ "ok": true, "tiles": tiles }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        assert_eq!(word, c["word"], "parseWordInput {input:?}");
        let tiles = match svc::parse_tiles_input(input) {
            Ok(letters) => json!({ "ok": true, "letters": letters.iter().map(|l| l.to_string()).collect::<Vec<_>>() }),
            Err(error) => json!({ "ok": false, "error": error }),
        };
        assert_eq!(tiles, c["tiles"], "parseTilesInput {input:?}");
        assert_eq!(json!(roulette::parse_space(input).map(|s| s.token)), c["space"], "parseSpace {input:?}");
    }
}

// ── audit: concurrent clicks on a multi-thread runtime ───────────────────────

/// The collector spawns each click's handler, so on a multi-thread runtime two clicks are decided in parallel. Each
/// decision (rps `played`, the ttt challenge's `settled`, the ttt turn) must be one test-and-set: a double Accept
/// starts one game, and two quick moves of the same player place one mark (the second is told it is not their turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clicks_are_decided_once_on_a_multi_thread_runtime() {
    for round in 0..25 {
        let t = Arc::new(games_app(None));
        route_interaction(&t, games_slash("rps", P1).build()).await;
        let id = game_id_of(&t, "rps");
        let (a, b) = (t.clone(), t.clone());
        let (ra, rb) = (format!("rps:{id}:rock"), format!("rps:{id}:paper"));
        let x = tokio::spawn(async move { press(&a, &ra, P1).await });
        let y = tokio::spawn(async move { press(&b, &rb, P1).await });
        let _ = futures::future::join(x, y).await;
        wait_for("the round", || !calls_named(&t.responses.calls(), "update").is_empty()).await;
        settle_tasks().await;
        assert_eq!(calls_named(&t.responses.calls(), "update").len(), 1, "rps round {round}: one round");

        route_interaction(&t, ttt_slash(P1, P2).build()).await;
        let id = game_id_of(&t, "ttt-ch");
        t.responses.clear();
        let (a, b) = (t.clone(), t.clone());
        let (ca, cb) = (format!("ttt-ch:{id}:accept"), format!("ttt-ch:{id}:accept"));
        let x = tokio::spawn(async move { press(&a, &ca, P2).await });
        let y = tokio::spawn(async move { press(&b, &cb, P2).await });
        let _ = futures::future::join(x, y).await;
        wait_for("the board", || !calls_named(&t.responses.calls(), "update").is_empty()).await;
        settle_tasks().await;
        assert_eq!(calls_named(&t.responses.calls(), "update").len(), 1, "ttt round {round}: one game");
        assert_eq!(t.collectors().len(), 1, "ttt round {round}: one game collector");

        t.responses.clear();
        let (a, b) = (t.clone(), t.clone());
        let (ma, mb) = (format!("ttt:{id}:0"), format!("ttt:{id}:1"));
        let x = tokio::spawn(async move { press(&a, &ma, P1).await });
        let y = tokio::spawn(async move { press(&b, &mb, P1).await });
        let _ = futures::future::join(x, y).await;
        wait_for("the move", || !calls_named(&t.responses.calls(), "update").is_empty()).await;
        settle_tasks().await;
        let calls = t.responses.calls();
        assert_eq!(calls_named(&calls, "update").len(), 1, "ttt round {round}: one mark");
        let marks = calls_named(&calls, "update")[0].body["components"].as_array().expect("rows").iter()
            .flat_map(|r| r["components"].as_array().cloned().unwrap_or_default())
            .filter(|b| b["label"] == "X")
            .count();
        assert_eq!(marks, 1, "ttt round {round}: X placed once");
        assert!(calls.iter().any(|c| c.content().is_some_and(|s| s.ends_with("'s turn!"))), "ttt round {round}: the second click is refused");

        route_interaction(&t, scrabble_slash("start", P1).option("target", option_type::USER, json!(P2)).build()).await;
        let id = game_id_of(&t, "scrabble-ch");
        t.responses.clear();
        let (a, b) = (t.clone(), t.clone());
        let (sa, sb) = (format!("scrabble-ch:{id}:accept"), format!("scrabble-ch:{id}:accept"));
        let x = tokio::spawn(async move { press(&a, &sa, P2).await });
        let y = tokio::spawn(async move { press(&b, &sb, P2).await });
        let _ = futures::future::join(x, y).await;
        wait_for("the scrabble board", || svc::get_game(&t, GCH).is_some_and(|g| lock(&g).last_board.is_some())).await;
        settle_tasks().await;
        let calls = t.responses.calls();
        assert_eq!(calls_named(&calls, "editReply").len(), 1, "scrabble round {round}: one start");
        assert!(!calls.iter().any(|c| c.content().is_some_and(|s| s.contains("already started"))), "scrabble round {round}: no second game attempt");
    }
}

// ── audit: the casino lock survives no panic ─────────────────────────────────

/// The JS releases the casino lock in `finally`, so even an unexpected throw frees the member. The router turns a
/// panic in `execute` into an error; the lock must be released then too, or the member stays "in a game".
#[tokio::test]
async fn a_panicking_casino_game_still_releases_the_lock() {
    for name in ["blackjack", "roulette", "slots"] {
        let c = casino(1000, |_| {}, &["10", "9", "10", "8"], 17, grid3("🍎", "🍋"), None);
        let deps = CasinoDeps {
            escrow: c.world.clone(),
            new_deck: Arc::new(|| panic!("deck boom")),
            spin_number: Arc::new(|| panic!("wheel boom")),
            spin_grid: Arc::new(|| panic!("reels boom")),
            sleep: Arc::new(|_| Box::pin(async {})),
            ..CasinoDeps::default()
        };
        let mut reg = Registry::new();
        reg.command(bj::command_with(deps.clone()));
        reg.command(roulette::command_with(deps.clone()));
        reg.command(slots::command_with(deps));
        let t = TestApp::new().with_guilds([G]);
        t.settings_store.put(settings_row(|_| {}));
        t.install(reg);
        let i = if name == "roulette" { spin_slash(100, "red") } else { slash(name, 100) };
        route_interaction(&t, i.build()).await;
        assert_eq!(c.world.reserves(), vec![100], "{name}: the bet was reserved before the panic");
        assert_eq!(c.world.settles(), vec![(100, 100)], "{name}: the stake is refunded once, as in the JS catch");
        assert_eq!(c.world.balance(), 1000, "{name}");
        let refunded = t.responses.calls().iter().any(|x| x.embed_title().is_some_and(|s| s.ends_with("Unavailable")));
        assert!(refunded, "{name}: the private refund notice: {:?}", t.responses.names());
        assert_eq!(c.world.locked(), 0, "{name}: the lock is released");
    }
}
