//! Port of `src/services/countingService.js`: the counting game's rules and state.
//!
//! Members count up from 1 in one channel, one number per message and never twice in a row. A number can also be
//! written as maths (`2+2`). A wrong number (or a second count in a row by the same member) resets the count to 0.
//! State lives in the `CountingState` table (one row per guild); the channel is `GuildSettings.countingChannelId`.
//! The message handling is in [`crate::plugin`].

use bot_core::App;
use bot_db::{COUNTING_STATE_INFO, CountingState, InsertBuilder, PgPool, SelectBuilder};
use serde_json::json;

/// Largest number a message can count (9 digits, so it always fits the 32-bit Int column).
pub const COUNT_MAX: i64 = 999_999_999;

/// Longer literals are no longer exact as doubles.
const MATH_LITERAL_DIGITS: usize = 15;
/// How close to a whole number a result must be to count as it (`1/49*49` is not exactly 1 in floating point).
const MATH_EPSILON: f64 = 1e-9;

/// The guild's counting lock (`createKeyedMutex()`), one per app.
#[derive(Default)]
pub struct CountingLock(pub bot_core::sync::KeyedMutex);

/// `runLocked(guildId, fn)`: runs `f` while holding the guild's counting lock. Message handling and `/counting` both
/// use it. The job is queued when this is called (not when the future is awaited), so callers keep their order.
pub fn run_locked<T, F, Fut>(app: &App, guild_id: &str, f: F) -> impl std::future::Future<Output = T> + Send + 'static + use<T, F, Fut>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    app.ext::<CountingLock>().0.run(guild_id, f)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Num(f64),
    Op(char),
}

/// `MATH_TOKENS`: the characters maths may use, and what they mean.
fn math_token(c: char) -> Option<char> {
    Some(match c {
        '+' => '+',
        '-' | '−' => '-',
        '*' | '×' | 'x' | 'X' => '*',
        '/' | '÷' => '/',
        '%' => '%',
        '^' => '^',
        '(' => '(',
        ')' => ')',
        _ => return None,
    })
}

fn math_tokens(text: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j - i > MATH_LITERAL_DIGITS {
                return None;
            }
            let lit: String = chars[i..j].iter().collect();
            tokens.push(Tok::Num(lit.parse::<f64>().ok()?));
            i = j;
        } else if bot_util::js::is_js_space(c) {
            i += 1;
        } else if c == '\\' && chars.get(i + 1) == Some(&'*') {
            tokens.push(Tok::Op('*'));
            i += 2;
        } else {
            tokens.push(Tok::Op(math_token(c)?));
            i += 1;
        }
    }
    Some(tokens)
}

/// JS `a ** b` (`Math.pow`): differs from `powf` for a base of ±1 with an infinite exponent, which is NaN.
pub fn js_pow(base: f64, exp: f64) -> f64 {
    if exp.is_nan() {
        return f64::NAN;
    }
    if exp == 0.0 {
        return 1.0;
    }
    if base.abs() == 1.0 && exp.is_infinite() {
        return f64::NAN;
    }
    base.powf(exp)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
    bad: bool,
}

impl Parser {
    fn take(&mut self, op: char) -> bool {
        if self.tokens.get(self.pos) == Some(&Tok::Op(op)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn sum(&mut self) -> f64 {
        let mut v = self.product();
        loop {
            if self.take('+') {
                v += self.product();
            } else if self.take('-') {
                v -= self.product();
            } else {
                return v;
            }
        }
    }

    fn product(&mut self) -> f64 {
        let mut v = self.unary();
        loop {
            if self.take('*') {
                v *= self.unary();
            } else if self.take('/') {
                v /= self.unary();
            } else if self.take('%') {
                v %= self.unary();
            } else {
                return v;
            }
        }
    }

    fn unary(&mut self) -> f64 {
        if self.take('-') { -self.unary() } else { self.power() }
    }

    fn power(&mut self) -> f64 {
        let base = self.primary();
        if self.take('^') { js_pow(base, self.unary()) } else { base }
    }

    fn primary(&mut self) -> f64 {
        if let Some(Tok::Num(n)) = self.tokens.get(self.pos) {
            let n = *n;
            self.pos += 1;
            return n;
        }
        if self.take('(') {
            let v = self.sum();
            if self.take(')') {
                return v;
            }
        }
        self.bad = true;
        f64::NAN
    }
}

/// `evaluateMath(text)`: the whole number `text` comes to as maths, or `None` when it is not maths, does not come to
/// a whole number or is not finite (`1/0`). `^` binds tightest and from the right, and `-2^2` is -4. The caller caps
/// the length, which also bounds the recursion.
pub fn evaluate_math(text: &str) -> Option<f64> {
    let tokens = math_tokens(text)?;
    if tokens.is_empty() {
        return None;
    }
    let len = tokens.len();
    let mut p = Parser { tokens, pos: 0, bad: false };
    let value = p.sum();
    if p.bad || p.pos != len || !value.is_finite() {
        return None;
    }
    let whole = bot_util::text::js_round(value);
    if (value - whole).abs() > MATH_EPSILON * 1f64.max(whole.abs()) {
        return None;
    }
    Some(if whole == 0.0 { 0.0 } else { whole }) // never -0
}

/// `COUNT_RE` (`^\d{1,9}$`).
fn is_plain_count(text: &str) -> bool {
    (1..=9).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit())
}

/// `parseCount(content)`: the number a message counts, or `None` when it is chat. The message is either just the
/// number or maths that comes to it; maths that comes to a fraction, a negative number or more than [`COUNT_MAX`] is
/// chat.
pub fn parse_count(content: &str) -> Option<i64> {
    let text = bot_util::js::trim(content);
    if is_plain_count(text) {
        return text.parse::<i64>().ok();
    }
    if bot_util::js::utf16_len(text) > bot_config::limits::COUNTING_MATH_LENGTH {
        return None;
    }
    let value = evaluate_math(text)?;
    (value >= 0.0 && value <= COUNT_MAX as f64).then_some(value as i64)
}

/// The counting state that applies in a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub count: i64,
    pub last_user_id: Option<String>,
    pub last_message_id: Option<String>,
    pub best: i64,
}

/// `currentState(row, channelId)`: a missing row, or one left over from another channel, starts from 0. The best
/// run is per server, so it carries over.
pub fn current_state(row: Option<&CountingState>, channel_id: Option<&str>) -> State {
    match row {
        Some(r) if Some(r.channel_id.as_str()) == channel_id => State {
            count: i64::from(r.count),
            last_user_id: r.last_user_id.clone(),
            last_message_id: r.last_message_id.clone(),
            best: i64::from(r.best),
        },
        _ => State { count: 0, last_user_id: None, last_message_id: None, best: row.map(|r| i64::from(r.best)).unwrap_or(0) },
    }
}

/// Why a count does not continue the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Not the next number.
    Wrong,
    /// The same member twice in a row.
    Twice,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wrong => "wrong",
            Self::Twice => "twice",
        }
    }
}

/// `judgeCount(state, userId, number)`: `{ ok, reason?, expected }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub ok: bool,
    pub reason: Option<Reason>,
    pub expected: i64,
}

/// `judgeCount(state, userId, number)`: whether `user_id` posting `number` continues the count.
pub fn judge_count(count: i64, last_user_id: Option<&str>, user_id: &str, number: i64) -> Verdict {
    let expected = count + 1;
    if number != expected {
        return Verdict { ok: false, reason: Some(Reason::Wrong), expected };
    }
    if last_user_id.is_some_and(|u| u == user_id) {
        return Verdict { ok: false, reason: Some(Reason::Twice), expected };
    }
    Verdict { ok: true, reason: None, expected }
}

/// `loadState(guildId)`: the guild's `CountingState` row (`prisma.countingState.findUnique`).
pub async fn load_state(pool: &PgPool, guild_id: &str) -> anyhow::Result<Option<CountingState>> {
    let q = SelectBuilder::new(&COUNTING_STATE_INFO).filter("guildId", json!(guild_id))?.build()?;
    Ok(q.fetch_optional::<CountingState, _>(pool).await?)
}

/// What [`save_state`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewState {
    pub channel_id: String,
    pub count: i64,
    pub last_user_id: Option<String>,
    pub last_message_id: Option<String>,
    pub best: i64,
}

/// `saveState(guildId, state)`: writes the guild's counting state, creating the row on first use (the Prisma upsert:
/// `INSERT ... ON CONFLICT ("guildId") DO UPDATE`, `updatedAt` set either way). Ensures the settings row first.
pub async fn save_state(app: &App, guild_id: &str, state: &NewState) -> anyhow::Result<CountingState> {
    let clamp = |n: i64| json!(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)));
    let data = [
        ("channelId", json!(state.channel_id)),
        ("count", clamp(state.count)),
        ("lastUserId", json!(state.last_user_id)),
        ("lastMessageId", json!(state.last_message_id)),
        ("best", clamp(state.best)),
    ];
    app.settings().ensure_guild_settings(guild_id).await?;
    let q = InsertBuilder::new(&COUNTING_STATE_INFO)
        .value("guildId", json!(guild_id))?
        .values(data.clone())?
        .on_conflict_update(&["guildId"], data)?
        .build()?;
    Ok(q.fetch_one::<CountingState, _>(app.db()).await?)
}
