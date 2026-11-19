//! Port of `src/services/scrabbleService.js`: the Scrabble rules engine, the game registry, the turn timers, the
//! rendering and the persistent rack button.
//!
//! Games are in memory, per app (the JS keeps them per shard process): a restart ends every game, and the rack button
//! of an old board then answers "no longer active". Nothing is kept in Redis or the database, so there is no state
//! for the JS and Rust bots to share mid-game.
//!
//! The registry (`activeGames`, `pendingChallenges`, `gameMutex`) and the dictionary memo live in [`ScrabbleState`]
//! (`app.ext`). A game is a [`GameRef`] (`Arc<Mutex<Game>>`); every rules function works on the locked [`Game`] and
//! is synchronous, so a move is validated and committed with no await in between (R083), as in the JS.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use bot_core::App;
use bot_core::embed_builder::guild_embed;
use bot_core::interaction::{ComponentCtx, InteractionError, Responder};
use bot_core::interaction_helpers::is_interaction_gone;
use bot_core::settings::Settings;
use bot_core::sync::KeyedMutex;
use bot_util::text::clamp_text;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::Serialize;
use serde_json::{Map, Value, json};
use serenity::all::{ButtonStyle, CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed, CreateEmbedFooter, CreateInteractionResponseFollowup, CreateInteractionResponseMessage, MessageId, UserId};

// ── Constants ─────────────────────────────────────────────────────────────────

pub const BOARD_SIZE: i64 = 15;
pub const CENTER: i64 = 7;
pub const RACK_SIZE: usize = 7;
pub const BINGO_BONUS: i64 = 50;
/// 3 minutes.
pub const TURN_TIMEOUT_MS: u64 = 180_000;
pub const CHALLENGE_TIMEOUT_MS: u64 = 30_000;
pub const MAX_SCORELESS_TURNS: u32 = 6;
pub const MIN_BAG_FOR_EXCHANGE: usize = 7;
pub const COLS: &str = "ABCDEFGHIJKLMNO";
pub const BLANK: char = '?';
pub const MAX_WORD_LENGTH: usize = 15;

/// The dictionary file, relative to the repository root (the JS resolves it from its own source file).
pub const DICTIONARY_PATH: &str = "assets/dictionaries/words.txt";
pub const DICTIONARY_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// `TILE_POINTS[letter]` (`?` is 0; an unknown letter is `undefined` in the JS, 0 here, where `?? 0` applies).
pub fn tile_points(letter: char) -> i64 {
    match letter {
        'A' | 'E' | 'I' | 'L' | 'N' | 'O' | 'R' | 'S' | 'T' | 'U' => 1,
        'D' | 'G' => 2,
        'B' | 'C' | 'M' | 'P' => 3,
        'F' | 'H' | 'V' | 'W' | 'Y' => 4,
        'K' => 5,
        'J' | 'X' => 8,
        'Q' | 'Z' => 10,
        _ => 0,
    }
}

/// `TILE_COUNTS`, in the JS object order (A..Z, then `?`). R280: 98 lettered tiles + 2 blanks = 100.
pub const TILE_COUNTS: [(char, usize); 27] = [
    ('A', 9), ('B', 2), ('C', 2), ('D', 4), ('E', 12), ('F', 2), ('G', 3), ('H', 2), ('I', 9), ('J', 1), ('K', 1),
    ('L', 4), ('M', 2), ('N', 6), ('O', 8), ('P', 2), ('Q', 1), ('R', 6), ('S', 4), ('T', 6), ('U', 4),
    ('V', 2), ('W', 2), ('X', 1), ('Y', 2), ('Z', 1),
    ('?', 2),
];

// ── Board ─────────────────────────────────────────────────────────────────────

const TW_SQUARES: [(usize, usize); 8] = [(0, 0), (0, 7), (0, 14), (7, 0), (7, 14), (14, 0), (14, 7), (14, 14)];
const DW_SQUARES: [(usize, usize); 16] = [
    (1, 1), (2, 2), (3, 3), (4, 4), (10, 10), (11, 11), (12, 12), (13, 13),
    (1, 13), (2, 12), (3, 11), (4, 10), (10, 4), (11, 3), (12, 2), (13, 1),
];
const TL_SQUARES: [(usize, usize); 12] = [(1, 5), (1, 9), (5, 1), (5, 5), (5, 9), (5, 13), (9, 1), (9, 5), (9, 9), (9, 13), (13, 5), (13, 9)];
const DL_SQUARES: [(usize, usize); 24] = [
    (0, 3), (0, 11), (2, 6), (2, 8), (3, 0), (3, 7), (3, 14),
    (6, 2), (6, 6), (6, 8), (6, 12), (7, 3), (7, 11),
    (8, 2), (8, 6), (8, 8), (8, 12), (11, 0), (11, 7), (11, 14),
    (12, 6), (12, 8), (14, 3), (14, 11),
];

/// An empty premium square.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Premium {
    Tw,
    Dw,
    Tl,
    Dl,
    Center,
}

impl Premium {
    pub fn as_str(self) -> &'static str {
        match self {
            Premium::Tw => "TW",
            Premium::Dw => "DW",
            Premium::Tl => "TL",
            Premium::Dl => "DL",
            Premium::Center => "CENTER",
        }
    }
}

/// A tile on the board: `{ letter, points, blank }` (a blank has points 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlacedTile {
    #[serde(serialize_with = "ser_char")]
    pub letter: char,
    pub points: i64,
    pub blank: bool,
}

/// One board cell: `null` (empty), `'TW'|'DW'|'TL'|'DL'|'CENTER'` (an empty premium square) or a played tile (the
/// premium is consumed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Empty,
    Premium(Premium),
    Tile(PlacedTile),
}

impl Cell {
    pub fn tile(&self) -> Option<&PlacedTile> {
        match self {
            Cell::Tile(t) => Some(t),
            _ => None,
        }
    }

    pub fn premium(&self) -> Option<Premium> {
        match self {
            Cell::Premium(p) => Some(*p),
            _ => None,
        }
    }
}

impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Cell::Empty => s.serialize_none(),
            Cell::Premium(p) => s.serialize_str(p.as_str()),
            Cell::Tile(t) => t.serialize(s),
        }
    }
}

fn ser_char<S: serde::Serializer>(c: &char, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&c.to_string())
}

/// 15 rows of 15 cells.
pub type Board = Vec<Vec<Cell>>;

/// `createBoard()`.
pub fn create_board() -> Board {
    let mut b = vec![vec![Cell::Empty; BOARD_SIZE as usize]; BOARD_SIZE as usize];
    for (r, c) in TW_SQUARES {
        b[r][c] = Cell::Premium(Premium::Tw);
    }
    for (r, c) in DW_SQUARES {
        b[r][c] = Cell::Premium(Premium::Dw);
    }
    for (r, c) in TL_SQUARES {
        b[r][c] = Cell::Premium(Premium::Tl);
    }
    for (r, c) in DL_SQUARES {
        b[r][c] = Cell::Premium(Premium::Dl);
    }
    b[CENTER as usize][CENTER as usize] = Cell::Premium(Premium::Center);
    b
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// A tile in the bag or a rack: `{ letter, points }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tile {
    #[serde(serialize_with = "ser_char")]
    pub letter: char,
    pub points: i64,
}

impl Tile {
    pub fn of(letter: char) -> Self {
        Self { letter, points: tile_points(letter) }
    }
}

/// `Math.random`: the plain (not secure) source the JS uses for the bag.
pub type PlainRng = Box<dyn FnMut() -> f64 + Send>;

/// The default `Math.random`.
pub fn math_random() -> PlainRng {
    Box::new(rand::random::<f64>)
}

/// Fisher–Yates, in place: `j = floor(rng() * (i + 1))`.
pub fn shuffle<T>(arr: &mut [T], rng: &mut dyn FnMut() -> f64) {
    let mut i = arr.len();
    while i > 1 {
        i -= 1;
        let j = ((rng() * (i as f64 + 1.0)).floor() as usize).min(i);
        arr.swap(i, j);
    }
}

/// `createTileBag(rng)`: the 100 tiles, shuffled.
pub fn create_tile_bag(rng: &mut dyn FnMut() -> f64) -> Vec<Tile> {
    let mut bag = Vec::with_capacity(100);
    for (letter, count) in TILE_COUNTS {
        for _ in 0..count {
            bag.push(Tile::of(letter));
        }
    }
    shuffle(&mut bag, rng);
    bag
}

/// `drawTiles(bag, n)`: the first `n` tiles (fewer when the bag is short).
pub fn draw_tiles(bag: &mut Vec<Tile>, n: usize) -> Vec<Tile> {
    let n = n.min(bag.len());
    bag.drain(..n).collect()
}

/// `rackValue(rack)`.
pub fn rack_value(rack: &[Tile]) -> i64 {
    rack.iter().map(|t| t.points).sum()
}

/// One tile of the `word` option: `{ letter, blank }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WordTile {
    #[serde(serialize_with = "ser_char")]
    pub letter: char,
    pub blank: bool,
}

pub const WORD_FORMAT_ERROR: &str = "Use letters A–Z only. Put `?` before a letter to play a blank as that letter (e.g. `QU?IZ`).";

/// `String(raw).trim()` with the JS whitespace set.
fn js_trim(s: &str) -> &str {
    bot_util::js::trim(s)
}

/// `parseWordInput(raw)`: `?` before a letter plays a blank as that letter.
pub fn parse_word_input(raw: &str) -> Result<Vec<WordTile>, String> {
    let mut tiles = Vec::new();
    let mut pending_blank = false;
    for ch in js_trim(raw).chars() {
        if ch == BLANK {
            if pending_blank {
                return Err(WORD_FORMAT_ERROR.into());
            }
            pending_blank = true;
        } else if ch.is_ascii_alphabetic() {
            tiles.push(WordTile { letter: ch.to_ascii_uppercase(), blank: pending_blank });
            pending_blank = false;
        } else {
            return Err(WORD_FORMAT_ERROR.into());
        }
    }
    if pending_blank || tiles.is_empty() {
        return Err(WORD_FORMAT_ERROR.into());
    }
    if tiles.len() > MAX_WORD_LENGTH {
        return Err(format!("A word can be at most {MAX_WORD_LENGTH} letters."));
    }
    Ok(tiles)
}

/// `parseTilesInput(raw)`: the tiles of an exchange (letters A–Z, `?` = blank), up to 7.
pub fn parse_tiles_input(raw: &str) -> Result<Vec<char>, String> {
    let s = js_trim(raw);
    let n = s.chars().count();
    if !(1..=7).contains(&n) || !s.chars().all(|c| c.is_ascii_alphabetic() || c == BLANK) {
        return Err("Use letters A–Z and `?` (blank), up to 7 tiles.".into());
    }
    Ok(s.chars().map(|c| c.to_ascii_uppercase()).collect())
}

/// A word a play formed: `{ text, score }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormedWord {
    pub text: String,
    pub score: i64,
}

/// `describeWords(words)`.
pub fn describe_words(words: &[FormedWord]) -> String {
    let parts: Vec<String> = words.iter().map(|w| format!("**{}** ({})", w.text, w.score)).collect();
    clamp_text(&parts.join(" + "), 1000)
}

/// `formatRack(rack)`.
pub fn format_rack(rack: &[Tile]) -> String {
    if rack.is_empty() {
        return "_(empty)_".into();
    }
    rack.iter()
        .map(|t| if t.letter == BLANK { "**?** blank".to_string() } else { format!("**{}** {}", t.letter, t.points) })
        .collect::<Vec<_>>()
        .join(" · ")
}

// ── Game ──────────────────────────────────────────────────────────────────────

/// A player: `{ id, name, rack, score }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Player {
    pub id: String,
    pub name: String,
    pub rack: Vec<Tile>,
    pub score: i64,
}

/// Why a game ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    Out,
    Scoreless,
    Resign,
    Timeout,
    Aborted,
}

/// The result of a finished game, serialised like the JS object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameResult {
    pub reason: EndReason,
    pub winner_id: Option<String>,
    pub loser_id: Option<String>,
    pub mover_id: Option<String>,
    /// `{ [id]: delta }` in the JS insertion order.
    pub adjustments: Vec<(String, i64)>,
    /// `{ [a]: score, [b]: score }`.
    pub scores: Vec<(String, i64)>,
}

impl GameResult {
    pub fn adjustment(&self, id: &str) -> i64 {
        self.adjustments.iter().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0)
    }

    pub fn score(&self, id: &str) -> i64 {
        self.scores.iter().find(|(k, _)| k == id).map(|(_, v)| *v).unwrap_or(0)
    }

    pub fn to_json(&self) -> Value {
        let map = |v: &[(String, i64)]| Value::Object(v.iter().map(|(k, n)| (k.clone(), json!(n))).collect::<Map<_, _>>());
        json!({
            "reason": self.reason,
            "winnerId": self.winner_id,
            "loserId": self.loser_id,
            "moverId": self.mover_id,
            "adjustments": map(&self.adjustments),
            "scores": map(&self.scores),
        })
    }
}

/// A live (or finished) game.
pub struct Game {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    /// The guild settings captured at the start (embed colour only).
    pub settings: Option<Settings>,
    pub dictionary: Option<Arc<HashSet<String>>>,
    pub rng: PlainRng,
    pub board: Board,
    pub bag: Vec<Tile>,
    /// The two players, in turn order.
    pub players: Vec<Player>,
    pub turn_order: [String; 2],
    pub current_turn: usize,
    pub turn_seq: u64,
    pub scoreless_turns: u32,
    pub placed_count: usize,
    pub moves: u32,
    pub timer: Option<tokio::task::AbortHandle>,
    pub ended: bool,
    pub result: Option<GameResult>,
    /// The interaction the turn timer posts its result through.
    pub last_interaction: Option<Responder>,
    /// The board message with the rack button: the interaction that posted it, the message id and its embed (a
    /// webhook edit through serenity always sends `embeds`, so stripping the button resends the embed unchanged).
    pub last_board: Option<(Responder, MessageId, CreateEmbed)>,
    me: Weak<Mutex<Game>>,
    registry: Option<Arc<ScrabbleState>>,
}

impl std::fmt::Debug for Game {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Game").field("id", &self.id).field("channel_id", &self.channel_id).field("state", &self.state_json()).finish()
    }
}

/// A shared game.
pub type GameRef = Arc<Mutex<Game>>;

/// Locks a game (a poisoned lock is recovered: every mutation is synchronous and complete).
pub fn lock(game: &GameRef) -> MutexGuard<'_, Game> {
    game.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `createGame` takes.
pub struct CreateGame {
    pub id: String,
    pub guild_id: String,
    pub channel_id: String,
    /// `[{ id, name }]`, challenger first.
    pub players: [(String, String); 2],
    pub dictionary: Option<Arc<HashSet<String>>>,
    pub settings: Option<Settings>,
    pub rng: PlainRng,
}

/// `createGame(opts)`: builds a game. It does NOT register it and does NOT arm a timer (see [`register_game`]).
pub fn create_game(opts: CreateGame) -> GameRef {
    let CreateGame { id, guild_id, channel_id, players, dictionary, settings, mut rng } = opts;
    let mut bag = create_tile_bag(&mut rng);
    let mut list = Vec::with_capacity(2);
    for (pid, name) in &players {
        let rack = draw_tiles(&mut bag, RACK_SIZE);
        list.push(Player { id: pid.clone(), name: name.clone(), rack, score: 0 });
    }
    let turn_order = [players[0].0.clone(), players[1].0.clone()];
    Arc::new_cyclic(|me| {
        Mutex::new(Game {
            id,
            guild_id,
            channel_id,
            settings,
            dictionary,
            rng,
            board: create_board(),
            bag,
            players: list,
            turn_order,
            current_turn: 0,
            turn_seq: 0,
            scoreless_turns: 0,
            placed_count: 0,
            moves: 0,
            timer: None,
            ended: false,
            result: None,
            last_interaction: None,
            last_board: None,
            me: me.clone(),
            registry: None,
        })
    })
}

impl Game {
    pub fn player(&self, id: &str) -> Option<&Player> {
        self.players.iter().find(|p| p.id == id)
    }

    pub fn player_mut(&mut self, id: &str) -> Option<&mut Player> {
        self.players.iter_mut().find(|p| p.id == id)
    }

    /// The game as the JS object serialises (`board`, `bag`, `players`, counters, `ended`, `result`); timers,
    /// interactions, settings and the dictionary are left out.
    pub fn state_json(&self) -> Value {
        let players: Map<String, Value> =
            self.players.iter().map(|p| (p.id.clone(), serde_json::to_value(p).unwrap_or(Value::Null))).collect();
        json!({
            "id": self.id,
            "guildId": self.guild_id,
            "channelId": self.channel_id,
            "board": self.board,
            "bag": self.bag,
            "players": players,
            "turnOrder": self.turn_order,
            "currentTurn": self.current_turn,
            "turnSeq": self.turn_seq,
            "scorelessTurns": self.scoreless_turns,
            "placedCount": self.placed_count,
            "moves": self.moves,
            "ended": self.ended,
            "result": self.result.as_ref().map(GameResult::to_json),
        })
    }
}

/// `currentPlayerId(game)`.
pub fn current_player_id(game: &Game) -> &str {
    &game.turn_order[game.current_turn]
}

/// `otherPlayerId(game, userId)`.
pub fn other_player_id<'a>(game: &'a Game, user_id: &str) -> &'a str {
    if game.turn_order[0] == user_id { &game.turn_order[1] } else { &game.turn_order[0] }
}

// ── Registry (per app) ────────────────────────────────────────────────────────

/// The registry and the dictionary memo of one app (the JS module-level `activeGames`, `pendingChallenges`,
/// `gameMutex` and dictionary state).
#[derive(Default)]
pub struct ScrabbleState {
    /// channelId → running game.
    active: Mutex<HashMap<String, GameRef>>,
    /// channelId → challengeId while a challenge is pending.
    pending: Mutex<HashMap<String, String>>,
    /// Serialises every move, pass, exchange and result post of one channel.
    pub game_mutex: KeyedMutex,
    dict: Mutex<DictInner>,
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The app's Scrabble state.
pub fn state(app: &App) -> Arc<ScrabbleState> {
    app.ext::<ScrabbleState>()
}

/// `isChannelBusy(channelId)`.
pub fn is_channel_busy(app: &App, channel_id: &str) -> bool {
    let s = state(app);
    guard(&s.active).contains_key(channel_id) || guard(&s.pending).contains_key(channel_id)
}

/// `reserveChannel(channelId, challengeId)`: reserves a channel for a pending challenge; `false` when it is busy.
pub fn reserve_channel(app: &App, channel_id: &str, challenge_id: &str) -> bool {
    let s = state(app);
    let active = guard(&s.active);
    let mut pending = guard(&s.pending);
    if active.contains_key(channel_id) || pending.contains_key(channel_id) {
        return false;
    }
    pending.insert(channel_id.to_string(), challenge_id.to_string());
    true
}

/// `releaseChannel(channelId, challengeId)`: releases a reservation, but only the caller's own.
pub fn release_channel(app: &App, channel_id: &str, challenge_id: &str) {
    let s = state(app);
    let mut pending = guard(&s.pending);
    if pending.get(channel_id).is_some_and(|c| c == challenge_id) {
        pending.remove(channel_id);
    }
}

/// Whether a reservation is pending in the channel (`pendingChallenges.has`).
pub fn pending_challenge(app: &App, channel_id: &str) -> Option<String> {
    guard(&state(app).pending).get(channel_id).cloned()
}

/// `registerGame(game)`: turns a reservation into the running game and arms its first turn timer, so a registered
/// game always has a timer (R087). `false` when the channel is taken.
pub fn register_game(app: &App, game: &GameRef) -> bool {
    let s = state(app);
    let mut g = lock(game);
    {
        let mut active = guard(&s.active);
        if active.contains_key(&g.channel_id) {
            return false;
        }
        let mut pending = guard(&s.pending);
        if pending.get(&g.channel_id).is_some_and(|by| *by != g.id) {
            return false;
        }
        active.insert(g.channel_id.clone(), game.clone());
        pending.remove(&g.channel_id);
    }
    g.registry = Some(s);
    arm_turn_timer(&mut g);
    true
}

/// `getGame(channelId)`: the running game of a channel.
pub fn get_game(app: &App, channel_id: &str) -> Option<GameRef> {
    let g = guard(&state(app).active).get(channel_id).cloned()?;
    if lock(&g).ended { None } else { Some(g) }
}

/// `findGameById(gameId)`.
pub fn find_game_by_id(app: &App, game_id: &str) -> Option<GameRef> {
    let games: Vec<GameRef> = guard(&state(app).active).values().cloned().collect();
    games.into_iter().find(|g| {
        let g = lock(g);
        g.id == game_id && !g.ended
    })
}

/// Whether the game is the one registered in its channel (the JS `activeGames.get(channelId) === game`).
fn is_registered(game: &Game) -> bool {
    let Some(reg) = &game.registry else { return false };
    guard(&reg.active).get(&game.channel_id).is_some_and(|r| std::ptr::eq(Arc::as_ptr(r), game.me.as_ptr()))
}

fn compute_result(game: &mut Game, reason: EndReason, loser_id: Option<&str>, mover_id: Option<&str>) -> GameResult {
    let [a, b] = game.turn_order.clone();
    let mut adjustments: Vec<(String, i64)> = Vec::new();
    let mut winner_id: Option<String> = None;
    let mut loser: Option<String> = None;

    match (reason, mover_id) {
        (EndReason::Out, Some(mover)) => {
            let opponent = other_player_id(game, mover).to_string();
            let value = game.player(&opponent).map(|p| rack_value(&p.rack)).unwrap_or(0);
            if let Some(p) = game.player_mut(mover) {
                p.score += value;
            }
            if let Some(p) = game.player_mut(&opponent) {
                p.score -= value;
            }
            adjustments.push((mover.to_string(), value));
            adjustments.push((opponent, -value));
        }
        (EndReason::Scoreless, _) => {
            for id in [&a, &b] {
                let value = game.player(id).map(|p| rack_value(&p.rack)).unwrap_or(0);
                if let Some(p) = game.player_mut(id) {
                    p.score -= value;
                }
                adjustments.push((id.clone(), -value));
            }
        }
        _ => {}
    }

    if matches!(reason, EndReason::Resign | EndReason::Timeout) {
        loser = loser_id.map(str::to_string);
        winner_id = loser.as_deref().map(|l| other_player_id(game, l).to_string());
    } else if reason != EndReason::Aborted {
        let sa = game.player(&a).map(|p| p.score).unwrap_or(0);
        let sb = game.player(&b).map(|p| p.score).unwrap_or(0);
        if sa != sb {
            let w = if sa > sb { a.clone() } else { b.clone() };
            loser = Some(other_player_id(game, &w).to_string());
            winner_id = Some(w);
        }
    }

    let score = |id: &str| game.player(id).map(|p| p.score).unwrap_or(0);
    GameResult {
        reason,
        winner_id,
        loser_id: loser,
        mover_id: if reason == EndReason::Out { mover_id.map(str::to_string) } else { None },
        adjustments,
        scores: vec![(a.clone(), score(&a)), (b.clone(), score(&b))],
    }
}

/// `finishGame(game, reason, { loserId, moverId })`: ends a game. Synchronous and idempotent. The registry entry is
/// removed only when it is THIS game, so a stale timer can never end a newer game in the same channel (R021, R167).
pub fn finish_game(game: &mut Game, reason: EndReason, loser_id: Option<&str>, mover_id: Option<&str>) -> GameResult {
    if game.ended
        && let Some(r) = &game.result
    {
        return r.clone();
    }
    game.ended = true;
    clear_turn_timer(game);
    if is_registered(game)
        && let Some(reg) = game.registry.clone()
    {
        guard(&reg.active).remove(&game.channel_id);
    }
    let result = compute_result(game, reason, loser_id, mover_id);
    game.result = Some(result.clone());
    result
}

// ── Timers bound to a game (R167) ─────────────────────────────────────────────

/// `clearTurnTimer(game)`.
pub fn clear_turn_timer(game: &mut Game) {
    if let Some(t) = game.timer.take() {
        t.abort();
    }
}

/// `armTurnTimer(game)`: forfeits the current player after [`TURN_TIMEOUT_MS`] (needs a tokio runtime; without one,
/// in a synchronous unit test, no timer is armed).
pub fn arm_turn_timer(game: &mut Game) {
    clear_turn_timer(game);
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    let seq = game.turn_seq;
    let me = game.me.clone();
    let handle = rt.spawn(async move {
        tokio::time::sleep(Duration::from_millis(TURN_TIMEOUT_MS)).await;
        if let Some(g) = me.upgrade() {
            handle_turn_timeout(g, seq).await;
        }
    });
    game.timer = Some(handle.abort_handle());
}

/// `handleTurnTimeout(game, seq)`: the turn timer's callback. The decision and the end of the game happen
/// synchronously, before any await; only the result post waits for the channel mutex. Never fails.
pub async fn handle_turn_timeout(game: GameRef, seq: u64) {
    let (result, responder, registry, channel) = {
        let mut g = lock(&game);
        if g.ended || g.turn_seq != seq || !is_registered(&g) {
            return;
        }
        // The timer that fired is this task: detach it so finishing does not cancel the result post.
        let _ = g.timer.take();
        let loser = current_player_id(&g).to_string();
        let result = finish_game(&mut g, EndReason::Timeout, Some(&loser), None);
        (result, g.last_interaction.clone(), g.registry.clone(), g.channel_id.clone())
    };
    let Some(reg) = registry else { return };
    let g2 = game.clone();
    reg.game_mutex.run(&channel, move || async move { post_result(&g2, responder.as_ref(), Some(&result)).await }).await;
}

fn advance_turn(game: &mut Game) {
    game.current_turn = 1 - game.current_turn;
    game.turn_seq += 1;
    arm_turn_timer(game);
}

// ── Rules engine ──────────────────────────────────────────────────────────────

fn not_your_turn(game: &Game) -> String {
    let name = game.player(current_player_id(game)).map(|p| p.name.as_str()).unwrap_or("your opponent");
    format!("It's not your turn — it's **{name}**'s turn.")
}

fn square(r: i64, c: i64) -> String {
    format!("{}{}", COLS.as_bytes()[c as usize] as char, r + 1)
}

/// A new tile of a play.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Placement {
    pub r: i64,
    pub c: i64,
    #[serde(serialize_with = "ser_char")]
    pub letter: char,
    pub blank: bool,
    pub points: i64,
    /// The square under it before the play (`null` or a premium).
    #[serde(serialize_with = "ser_premium")]
    pub premium: Option<Premium>,
}

fn ser_premium<S: serde::Serializer>(p: &Option<Premium>, s: S) -> Result<S::Ok, S::Error> {
    match p {
        Some(p) => s.serialize_str(p.as_str()),
        None => s.serialize_none(),
    }
}

/// A valid play, scored (`computePlay`'s `{ ok: true, placements, words, total, bingo }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Play {
    pub placements: Vec<Placement>,
    pub words: Vec<FormedWord>,
    pub total: i64,
    pub bingo: bool,
}

/// `computePlay(game, userId, tiles, row0, col0, dir)`: validates and scores a play WITHOUT touching the game. Checks
/// run in a fixed order and the first error wins (turn, range, squares, new tile, rack, placement, words,
/// dictionary). `row0` and `col0` are 0-based; `dir` is `h` or `v`.
pub fn compute_play(game: &Game, user_id: &str, tiles: &[WordTile], row0: i64, col0: i64, dir: &str) -> Result<Play, String> {
    // 1. Turn
    if current_player_id(game) != user_id {
        return Err(not_your_turn(game));
    }

    // 2. Range
    let in_range = |n: i64| (0..BOARD_SIZE).contains(&n);
    if !in_range(row0) || !in_range(col0) || (dir != "h" && dir != "v") || tiles.is_empty() {
        return Err("Invalid position.".into());
    }
    let dr: i64 = if dir == "v" { 1 } else { 0 };
    let dc: i64 = if dir == "h" { 1 } else { 0 };
    let n = tiles.len() as i64;
    if row0 + dr * (n - 1) >= BOARD_SIZE || col0 + dc * (n - 1) >= BOARD_SIZE {
        return Err("That word runs off the board.".into());
    }

    // 3. Squares
    let board = &game.board;
    let cell = |r: i64, c: i64| &board[r as usize][c as usize];
    let mut placements: Vec<Placement> = Vec::new();
    for (i, t) in tiles.iter().enumerate() {
        let r = row0 + dr * i as i64;
        let c = col0 + dc * i as i64;
        match cell(r, c) {
            Cell::Tile(existing) => {
                if t.blank {
                    return Err(format!("Square **{}** already has a tile — only new tiles can be blanks (remove the `?`).", square(r, c)));
                }
                if existing.letter != t.letter {
                    return Err(format!("Square **{}** already has **{}**, not **{}**.", square(r, c), existing.letter, t.letter));
                }
            }
            other => placements.push(Placement {
                r,
                c,
                letter: t.letter,
                blank: t.blank,
                points: if t.blank { 0 } else { tile_points(t.letter) },
                premium: other.premium(),
            }),
        }
    }

    // 4. At least one new tile
    if placements.is_empty() {
        return Err("You must place at least one new tile.".into());
    }

    // 5. Rack (multiset, on a copy)
    let mut rack: Vec<char> = game.player(user_id).map(|p| p.rack.iter().map(|t| t.letter).collect()).unwrap_or_default();
    for p in &placements {
        if p.blank {
            let Some(idx) = rack.iter().position(|l| *l == BLANK) else {
                return Err("You don't have a blank tile.".into());
            };
            rack.remove(idx);
        } else {
            let Some(idx) = rack.iter().position(|l| *l == p.letter) else {
                return Err(if rack.contains(&BLANK) {
                    format!("You don't have **{0}**. To play your blank as {0}, type `?{0}`.", p.letter)
                } else {
                    format!("You don't have the tile **{}** in your rack.", p.letter)
                });
            };
            rack.remove(idx);
        }
    }

    // 6. Placement
    let is_tile_at = |r: i64, c: i64| in_range(r) && in_range(c) && matches!(cell(r, c), Cell::Tile(_));
    if game.placed_count == 0 {
        if !placements.iter().any(|p| p.r == CENTER && p.c == CENTER) {
            return Err("The first word must cover the centre square (**H8**).".into());
        }
    } else {
        let touches = placements.iter().any(|p| [(p.r - 1, p.c), (p.r + 1, p.c), (p.r, p.c - 1), (p.r, p.c + 1)].iter().any(|&(nr, nc)| is_tile_at(nr, nc)));
        let passes_through = placements.len() < tiles.len();
        if !touches && !passes_through {
            return Err("Your word must touch or pass through a tile already on the board.".into());
        }
    }

    // 7. Formed words: the whole run through the declared span (R168) plus every cross-word (R047)
    let fresh: HashMap<i64, &Placement> = placements.iter().map(|p| (p.r * BOARD_SIZE + p.c, p)).collect();
    let occupied = |r: i64, c: i64| in_range(r) && in_range(c) && (fresh.contains_key(&(r * BOARD_SIZE + c)) || matches!(cell(r, c), Cell::Tile(_)));
    let run = |r: i64, c: i64, row_step: i64, col_step: i64| -> Vec<(i64, i64)> {
        let (mut sr, mut sc) = (r, c);
        while occupied(sr - row_step, sc - col_step) {
            sr -= row_step;
            sc -= col_step;
        }
        let mut cells = Vec::new();
        while occupied(sr, sc) {
            cells.push((sr, sc));
            sr += row_step;
            sc += col_step;
        }
        cells
    };

    let mut runs: Vec<Vec<(i64, i64)>> = Vec::new();
    let main = run(row0, col0, dr, dc);
    if main.len() >= 2 {
        runs.push(main);
    }
    for p in &placements {
        let cross = run(p.r, p.c, dc, dr);
        if cross.len() >= 2 {
            runs.push(cross);
        }
    }
    if runs.is_empty() {
        return Err("Your tiles must form a word of at least 2 letters.".into());
    }

    // 8. Score: only NEW tiles use their premium square
    let words: Vec<FormedWord> = runs
        .iter()
        .map(|cells| {
            let mut letters = 0i64;
            let mut multiplier = 1i64;
            let mut text = String::new();
            for &(r, c) in cells {
                if let Some(n) = fresh.get(&(r * BOARD_SIZE + c)) {
                    let mut v = n.points;
                    match n.premium {
                        Some(Premium::Dl) => v *= 2,
                        Some(Premium::Tl) => v *= 3,
                        Some(Premium::Dw) | Some(Premium::Center) => multiplier *= 2,
                        Some(Premium::Tw) => multiplier *= 3,
                        None => {}
                    }
                    letters += v;
                    text.push(n.letter);
                } else if let Cell::Tile(t) = cell(r, c) {
                    letters += t.points;
                    text.push(t.letter);
                }
            }
            FormedWord { text, score: letters * multiplier }
        })
        .collect();

    // 9. Dictionary (R046)
    if let Some(dict) = &game.dictionary {
        let invalid: Vec<String> = words.iter().filter(|w| !dict.contains(&w.text)).map(|w| format!("**{}**", w.text)).collect();
        if !invalid.is_empty() {
            return Err(format!("Not in the word list: {}.", invalid.join(", ")));
        }
    }

    // 10. Total
    let bingo = placements.len() == RACK_SIZE;
    let total = words.iter().map(|w| w.score).sum::<i64>() + if bingo { BINGO_BONUS } else { 0 };
    Ok(Play { placements, words, total, bingo })
}

fn remove_from_rack(rack: &mut Vec<Tile>, letter: char) {
    if let Some(idx) = rack.iter().position(|t| t.letter == letter) {
        rack.remove(idx);
    }
}

/// A committed word play (`playWord`'s `{ ok: true, words, total, bingo, placements, gameOver }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordPlayed {
    pub words: Vec<FormedWord>,
    pub total: i64,
    pub bingo: bool,
    pub placements: Vec<Placement>,
    pub game_over: Option<GameResult>,
}

/// `playWord(game, userId, raw, row0, col0, dir)`: validates and commits a word play SYNCHRONOUSLY (R083).
pub fn play_word(game: &mut Game, user_id: &str, raw: &str, row0: i64, col0: i64, dir: &str) -> Result<WordPlayed, String> {
    if game.ended {
        return Err("This game has already ended.".into());
    }
    if current_player_id(game) != user_id {
        return Err(not_your_turn(game));
    }
    let tiles = parse_word_input(raw)?;
    let res = compute_play(game, user_id, &tiles, row0, col0, dir)?;

    for p in &res.placements {
        game.board[p.r as usize][p.c as usize] = Cell::Tile(PlacedTile { letter: p.letter, points: p.points, blank: p.blank });
        if let Some(player) = game.player_mut(user_id) {
            remove_from_rack(&mut player.rack, if p.blank { BLANK } else { p.letter });
        }
    }
    if let Some(player) = game.player_mut(user_id) {
        player.score += res.total;
    }
    game.placed_count += res.placements.len();
    game.moves += 1;
    game.scoreless_turns = if res.total > 0 { 0 } else { game.scoreless_turns + 1 };
    let rack_len = game.player(user_id).map(|p| p.rack.len()).unwrap_or(0);
    let drawn = draw_tiles(&mut game.bag, RACK_SIZE.saturating_sub(rack_len));
    if let Some(player) = game.player_mut(user_id) {
        player.rack.extend(drawn);
    }

    let rack_empty = game.player(user_id).is_some_and(|p| p.rack.is_empty());
    let game_over = if game.bag.is_empty() && rack_empty {
        Some(finish_game(game, EndReason::Out, None, Some(user_id)))
    } else if game.scoreless_turns >= MAX_SCORELESS_TURNS {
        Some(finish_game(game, EndReason::Scoreless, None, None))
    } else {
        advance_turn(game);
        None
    };
    Ok(WordPlayed { words: res.words, total: res.total, bingo: res.bingo, placements: res.placements, game_over })
}

/// `passTurn(game, userId)`: `Ok(gameOver)` or the error text.
pub fn pass_turn(game: &mut Game, user_id: &str) -> Result<Option<GameResult>, String> {
    if game.ended {
        return Err("This game has already ended.".into());
    }
    if current_player_id(game) != user_id {
        return Err(not_your_turn(game));
    }
    game.scoreless_turns += 1;
    game.moves += 1;
    if game.scoreless_turns >= MAX_SCORELESS_TURNS {
        return Ok(Some(finish_game(game, EndReason::Scoreless, None, None)));
    }
    advance_turn(game);
    Ok(None)
}

/// `exchangeTiles(game, userId, raw)`: `Ok((count, gameOver))` or the error text.
pub fn exchange_tiles(game: &mut Game, user_id: &str, raw: &str) -> Result<(usize, Option<GameResult>), String> {
    if game.ended {
        return Err("This game has already ended.".into());
    }
    if current_player_id(game) != user_id {
        return Err(not_your_turn(game));
    }
    if game.bag.len() < MIN_BAG_FOR_EXCHANGE {
        return Err(format!("You can only exchange when the bag has at least {MIN_BAG_FOR_EXCHANGE} tiles (it has **{}**).", game.bag.len()));
    }
    let letters = parse_tiles_input(raw)?;

    let mut left: Vec<char> = game.player(user_id).map(|p| p.rack.iter().map(|t| t.letter).collect()).unwrap_or_default();
    for letter in &letters {
        let Some(idx) = left.iter().position(|l| l == letter) else {
            let shown = if *letter == BLANK { "**?** (blank)".to_string() } else { format!("**{letter}**") };
            return Err(format!("You don't have the tile {shown} in your rack."));
        };
        left.remove(idx);
    }

    // Draw first, then put the old tiles back, so you can never draw your own tiles again.
    let n = letters.len();
    let drawn = draw_tiles(&mut game.bag, n);
    let mut removed = Vec::with_capacity(n);
    if let Some(player) = game.player_mut(user_id) {
        for letter in &letters {
            if let Some(idx) = player.rack.iter().position(|t| t.letter == *letter) {
                removed.push(player.rack.remove(idx));
            }
        }
        player.rack.extend(drawn);
    }
    game.bag.extend(removed);
    let mut bag = std::mem::take(&mut game.bag);
    shuffle(&mut bag, &mut game.rng);
    game.bag = bag;

    game.scoreless_turns += 1;
    game.moves += 1;
    if game.scoreless_turns >= MAX_SCORELESS_TURNS {
        return Ok((n, Some(finish_game(game, EndReason::Scoreless, None, None))));
    }
    advance_turn(game);
    Ok((n, None))
}

/// `resignGame(game, userId)`: allowed at any time, not only on your turn.
pub fn resign_game(game: &mut Game, user_id: &str) -> GameResult {
    finish_game(game, EndReason::Resign, Some(user_id), None)
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// `renderBoard(board)`: the board as a code block (a blank tile shows as a lowercase letter).
pub fn render_board(board: &Board) -> String {
    let header = format!("    {}", COLS.chars().map(|c| format!("{c} ")).collect::<Vec<_>>().join(" "));
    let rows: Vec<String> = board
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Cell::Empty => "· ".to_string(),
                    Cell::Premium(Premium::Center) => "★ ".to_string(),
                    Cell::Premium(p) => p.as_str().to_string(),
                    Cell::Tile(t) => format!("{} ", if t.blank { t.letter.to_ascii_lowercase() } else { t.letter }),
                })
                .collect();
            format!("{:>2}  {}", r + 1, cells.join(" "))
        })
        .collect();
    format!("```\n{header}\n{}\n```", rows.join("\n"))
}

/// `buildGameEmbed(game, { finished })`: the board embed. `finished` shows "Game over" instead of whose turn it is
/// (R238).
pub fn build_game_embed(game: &Game, finished: bool) -> CreateEmbed {
    let p1 = &game.players[0];
    let p2 = &game.players[1];
    let or = |s: &str, d: &str| if s.is_empty() { d.to_string() } else { s.to_string() };
    let mut embed = guild_embed(game.settings.as_deref(), Some("🔤 Scrabble"), Some(&render_board(&game.board)), None)
        .field(or(&p1.name, "Player 1"), format!("**{}** pts", p1.score), true)
        .field(or(&p2.name, "Player 2"), format!("**{}** pts", p2.score), true)
        .field("Tiles in bag", game.bag.len().to_string(), true);
    embed = if finished {
        embed.field("Status", "Game over", false)
    } else {
        embed.field("Turn", format!("<@{}> · 3 min to play", current_player_id(game)), false)
    };
    embed.footer(CreateEmbedFooter::new(format!(
        "TW/DW = triple/double word · TL/DL = triple/double letter · ★ = centre (DW) · lowercase = blank · Word check: {}",
        if game.dictionary.is_some() { "on" } else { "off" }
    )))
}

/// `buildResultEmbed(game, result)`: the final scores and the winner.
pub fn build_result_embed(game: &Game, result: &GameResult) -> CreateEmbed {
    let mut lines: Vec<String> = Vec::new();
    for id in &game.turn_order {
        let adj = result.adjustment(id);
        let name = game.player(id).map(|p| p.name.as_str()).unwrap_or("");
        let suffix = if adj != 0 { format!(" ({}{adj} from racks)", if adj > 0 { "+" } else { "" }) } else { String::new() };
        lines.push(format!("**{name}**: {} pts{suffix}", result.score(id)));
    }
    lines.push(String::new());
    let name_of = |id: Option<&str>| id.and_then(|i| game.player(i)).map(|p| p.name.clone()).unwrap_or_else(|| "Someone".into());
    match result.reason {
        EndReason::Out => lines.push(format!(
            "**{}** used their last tile with the bag empty.",
            name_of(result.mover_id.as_deref().or(result.winner_id.as_deref()))
        )),
        EndReason::Scoreless => lines.push("Six scoreless turns in a row — remaining tiles were deducted.".into()),
        EndReason::Resign => lines.push(format!("**{}** resigned.", name_of(result.loser_id.as_deref()))),
        EndReason::Timeout => lines.push(format!("**{}** didn't play within 3 minutes.", name_of(result.loser_id.as_deref()))),
        EndReason::Aborted => {}
    }
    lines.push(match &result.winner_id {
        Some(w) => format!("🏆 Winner: **{}**!", name_of(Some(w))),
        None => "🤝 It's a tie!".into(),
    });
    guild_embed(game.settings.as_deref(), Some("🏁 Scrabble — Game Over"), Some(&lines.join("\n")), None)
}

/// `rackRow(gameId)`: one row with the persistent "View My Tiles" button, bound to a game id.
pub fn rack_row(game_id: &str) -> CreateActionRow {
    let id = bot_config::custom_id!("scrabble-rack", game_id).unwrap_or_else(|_| "scrabble-rack".into());
    CreateActionRow::Buttons(vec![CreateButton::new(id).label("🎒 View My Tiles").style(ButtonStyle::Secondary)])
}

fn log_post_error(label: &str, err: &InteractionError) {
    if is_interaction_gone(err) || err.code() == Some(10008) {
        bot_util::log_debug!(format!("Scrabble {label}: {err}"));
    } else {
        bot_util::log_warn!(format!("Scrabble {label} failed"), bot_util::logger::LogArg::Error { message: err.to_string(), stack: None });
    }
}

/// Strips the old board's button, keeping its embed. Never fails.
async fn strip_old_board(game: &GameRef) {
    let last = lock(game).last_board.take();
    let Some((responder, message_id, embed)) = last else { return };
    let edit = CreateInteractionResponseFollowup::new().embeds(vec![embed]).components(Vec::new());
    if let Err(e) = responder.edit_follow_up(message_id, edit).await {
        log_post_error("old board edit", &e);
    }
}

/// `postBoard(game, interaction, { content, mentionIds })`: posts the new public board (with a mention of the next
/// player) and strips the previous board's button. Never fails; returns whether the post succeeded. `responder` is
/// a replied or deferred interaction (a follow-up is used).
pub async fn post_board(game: &GameRef, responder: &Responder, content: &str, mention_ids: &[String]) -> bool {
    strip_old_board(game).await;
    let (embed, row) = {
        let g = lock(game);
        (build_game_embed(&g, false), rack_row(&g.id))
    };
    let users: Vec<UserId> = mention_ids.iter().filter_map(|id| id.parse::<u64>().ok().filter(|n| *n != 0)).map(UserId::new).collect();
    let followup = CreateInteractionResponseFollowup::new()
        .content(clamp_text(content, 2000))
        .embeds(vec![embed.clone()])
        .components(vec![row])
        .allowed_mentions(CreateAllowedMentions::new().users(users));
    match responder.follow_up(followup).await {
        Ok(msg) => {
            let mut g = lock(game);
            g.last_board = Some((responder.clone(), msg.id, embed));
            g.last_interaction = Some(responder.clone());
            true
        }
        Err(e) => {
            log_post_error("board post", &e);
            false
        }
    }
}

/// `postResult(game, interaction, result)`: posts the final board and the result (R166, R238). Never fails; nothing
/// is posted for an aborted game or without an interaction.
pub async fn post_result(game: &GameRef, responder: Option<&Responder>, result: Option<&GameResult>) {
    let (Some(result), Some(responder)) = (result, responder) else { return };
    if result.reason == EndReason::Aborted {
        return;
    }
    strip_old_board(game).await;
    let (a, b, embeds) = {
        let g = lock(game);
        let [a, b] = g.turn_order.clone();
        (a, b, vec![build_game_embed(&g, true), build_result_embed(&g, result)])
    };
    let users: Vec<UserId> = [&a, &b].iter().filter_map(|id| id.parse::<u64>().ok().filter(|n| *n != 0)).map(UserId::new).collect();
    let followup = CreateInteractionResponseFollowup::new()
        .content(format!("🏁 <@{a}> <@{b}>"))
        .embeds(embeds)
        .allowed_mentions(CreateAllowedMentions::new().users(users));
    if let Err(e) = responder.follow_up(followup).await {
        log_post_error("result post", &e);
    }
}

/// The text of the rack button's answer, or `None` when the game is gone.
pub fn rack_button_text(app: &App, custom_id: &str, channel_id: &str, user_id: &str) -> String {
    let parsed = bot_config::parse_custom_id(custom_id);
    let game = match parsed.params.first().filter(|p| !p.is_empty()) {
        Some(id) => find_game_by_id(app, id),
        None => get_game(app, channel_id),
    };
    let Some(game) = game else {
        return "This Scrabble game is no longer active (it ended, or the bot restarted). Start a new one with `/scrabble start`.".into();
    };
    let g = lock(&game);
    let Some(p) = g.player(user_id) else { return "You're not a player in this game.".into() };
    let current = current_player_id(&g);
    let turn = if current == p.id {
        "✅ It's your turn — use `/scrabble word`.".to_string()
    } else {
        format!("⏳ Waiting for **{}**.", g.player(current).map(|q| q.name.as_str()).unwrap_or(""))
    };
    [format!("🎒 Your tiles: {}", format_rack(&p.rack)), format!("📊 Your score: **{}** pts · 👜 Bag: **{}**", p.score, g.bag.len()), turn].join("\n")
}

/// `handleRackButton(interaction)`: the persistent "View My Tiles" button (`scrabble-rack:{gameId}`; the bare
/// legacy `scrabble-rack` of older boards is resolved by channel). Never fails.
pub async fn handle_rack_button(ctx: ComponentCtx) -> bot_core::BotResult<()> {
    let text = rack_button_text(&ctx.app, ctx.custom_id(), &ctx.interaction.channel_id.to_string(), &ctx.interaction.user.id.to_string());
    if let Err(e) = ctx.reply(CreateInteractionResponseMessage::new().content(text).ephemeral(true)).await {
        if is_interaction_gone(&e) {
            bot_util::log_debug!("Scrabble rack button: interaction gone");
        } else {
            bot_util::log_error!("Scrabble rack button failed", bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
        }
    }
    Ok(())
}

// ── Optional dictionary (R046) ────────────────────────────────────────────────

/// The dictionary's load state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DictState {
    #[default]
    Unloaded,
    Loading,
    Loaded,
    Missing,
    Error,
}

/// `dictionaryStatus()`: `{ state, size }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct DictStatus {
    pub state: DictState,
    pub size: usize,
}

type DictFuture = Shared<BoxFuture<'static, Option<Arc<HashSet<String>>>>>;

#[derive(Default)]
struct DictInner {
    status: DictStatus,
    set: Option<Arc<HashSet<String>>>,
    promise: Option<DictFuture>,
    path: Option<PathBuf>,
}

/// `parseDictionary(text)`: one word per line; trimmed; blank lines and `#` lines ignored; keeps `^[A-Z]{2,15}$`.
pub fn parse_dictionary(text: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let w = js_trim(line).to_uppercase();
        if w.is_empty() || w.starts_with('#') {
            continue;
        }
        if (2..=15).contains(&w.len()) && w.bytes().all(|b| b.is_ascii_uppercase()) {
            set.insert(w);
        }
    }
    set
}

fn set_status(s: &ScrabbleState, status: DictStatus, set: Option<Arc<HashSet<String>>>) {
    let mut d = guard(&s.dict);
    d.status = status;
    if set.is_some() {
        d.set = set;
    }
}

async fn read_dictionary(s: Arc<ScrabbleState>, path: PathBuf) -> Option<Arc<HashSet<String>>> {
    let info = match tokio::fs::metadata(&path).await {
        Ok(i) => i,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            set_status(&s, DictStatus { state: DictState::Missing, size: 0 }, None);
            bot_util::log_info!("Scrabble dictionary not found at assets/dictionaries/words.txt; word checking is off.");
            return None;
        }
        Err(e) => {
            set_status(&s, DictStatus { state: DictState::Error, size: 0 }, None);
            bot_util::log_warn!(
                "Scrabble dictionary could not be read; word checking is off.",
                bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
            );
            return None;
        }
    };
    if info.len() > DICTIONARY_MAX_BYTES {
        set_status(&s, DictStatus { state: DictState::Error, size: 0 }, None);
        bot_util::log_warn!(format!(
            "Scrabble dictionary is {} bytes (limit {DICTIONARY_MAX_BYTES}); word checking is off.",
            info.len()
        ));
        return None;
    }
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            set_status(&s, DictStatus { state: DictState::Error, size: 0 }, None);
            bot_util::log_warn!(
                "Scrabble dictionary load failed; word checking is off.",
                bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
            );
            return None;
        }
    };
    let set = parse_dictionary(&String::from_utf8_lossy(&bytes));
    if set.is_empty() {
        set_status(&s, DictStatus { state: DictState::Error, size: 0 }, None);
        bot_util::log_warn!("Scrabble dictionary is empty; word checking is off.");
        return None;
    }
    let size = set.len();
    let set = Arc::new(set);
    set_status(&s, DictStatus { state: DictState::Loaded, size }, Some(set.clone()));
    bot_util::log_info!(format!("Scrabble dictionary loaded: {size} words"));
    Some(set)
}

/// The dictionary path the app reads (the repository-relative [`DICTIONARY_PATH`] unless a test set another).
pub fn dictionary_path(app: &App) -> PathBuf {
    guard(&state(app).dict).path.clone().unwrap_or_else(|| PathBuf::from(DICTIONARY_PATH))
}

/// `loadDictionary()`: loads the dictionary once per app (memoised). Never fails; resolves `None` when there is
/// none. The read starts at once (like the JS promise), whether or not the result is awaited. Needs a tokio runtime.
pub fn load_dictionary(app: &App) -> DictFuture {
    let s = state(app);
    let mut d = guard(&s.dict);
    if let Some(p) = &d.promise {
        return p.clone();
    }
    d.status = DictStatus { state: DictState::Loading, size: 0 };
    let path = d.path.clone().unwrap_or_else(|| PathBuf::from(DICTIONARY_PATH));
    let s2 = s.clone();
    let handle = tokio::spawn(read_dictionary(s2, path));
    let s3 = s.clone();
    let fut: BoxFuture<'static, Option<Arc<HashSet<String>>>> = async move {
        match handle.await {
            Ok(v) => v,
            Err(e) => {
                set_status(&s3, DictStatus { state: DictState::Error, size: 0 }, None);
                bot_util::log_warn!(
                    "Scrabble dictionary load failed; word checking is off.",
                    bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
                );
                None
            }
        }
    }
    .boxed();
    let shared = fut.shared();
    d.promise = Some(shared.clone());
    shared
}

/// `getDictionary()`: the loaded dictionary, or `None` (synchronous).
pub fn get_dictionary(app: &App) -> Option<Arc<HashSet<String>>> {
    let s = state(app);
    let d = guard(&s.dict);
    if d.status.state == DictState::Loaded { d.set.clone() } else { None }
}

/// `dictionaryStatus()`.
pub fn dictionary_status(app: &App) -> DictStatus {
    guard(&state(app).dict).status
}

/// Test hooks (`__testing`): reset the memoised dictionary, install one, or point the loader at another file.
pub mod testing {
    use super::*;

    /// `__testing.reset()`.
    pub fn reset(app: &App) {
        let s = state(app);
        let mut d = guard(&s.dict);
        let path = d.path.take();
        *d = DictInner { path, ..Default::default() };
    }

    /// `__testing.setDictionary(set)`.
    pub fn set_dictionary(app: &App, set: HashSet<String>) {
        let s = state(app);
        let mut d = guard(&s.dict);
        let size = set.len();
        let set = Arc::new(set);
        d.status = DictStatus { state: DictState::Loaded, size };
        d.set = Some(set.clone());
        let fut: BoxFuture<'static, Option<Arc<HashSet<String>>>> = futures::future::ready(Some(set)).boxed();
        d.promise = Some(fut.shared());
    }

    /// Reads the dictionary from `path` instead of [`DICTIONARY_PATH`].
    pub fn set_path(app: &App, path: &Path) {
        guard(&state(app).dict).path = Some(path.to_path_buf());
    }

    /// Every registered game (`activeGames.clear()` in a test's cleanup).
    pub fn clear_registry(app: &App) {
        let s = state(app);
        let games: Vec<GameRef> = guard(&s.active).drain().map(|(_, g)| g).collect();
        guard(&s.pending).clear();
        for g in games {
            clear_turn_timer(&mut lock(&g));
        }
    }
}
