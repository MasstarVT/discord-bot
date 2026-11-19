//! What the starboard and statistics engines need from Discord's REST API, behind a trait per service
//! ([`crate::starboard_io::StarboardIo`], [`crate::stats_io::StatsIo`]), so the engines can be driven by a fake in tests
//! the way the JS tests drive them with fake discord.js objects. This module holds the error type both share and a
//! small insertion-ordered map with a cap (the JS `Map` plus `memoSet` eviction).

use indexmap::IndexMap;

/// A failed Discord call: the REST error code (`err.code ?? err.rawError.code`), the HTTP status, and whether it was
/// a timeout or a rate limit (discord.js `RateLimitError`, with its `retryAfter` in ms).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiscordError {
    pub code: Option<u64>,
    pub status: Option<u16>,
    pub timeout: bool,
    pub rate_limited: Option<i64>,
    pub message: String,
}

impl DiscordError {
    /// A REST error with this JSON error code (`10003`, `10008`, `50001`, `50013`, ...).
    pub fn code(code: u64) -> Self {
        Self { code: Some(code), status: None, timeout: false, rate_limited: None, message: format!("Discord {code}") }
    }

    /// Anything else (a network failure, an unexpected answer).
    pub fn other(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Default::default() }
    }

    /// `TimeoutError` (`withTimeout` gave up).
    pub fn timeout(label: impl Into<String>) -> Self {
        Self { timeout: true, message: format!("{} timed out", label.into()), ..Default::default() }
    }

    /// discord.js `RateLimitError` with `retryAfter` (ms).
    pub fn rate_limited(retry_after_ms: i64) -> Self {
        Self { rate_limited: Some(retry_after_ms), status: Some(429), message: "Rate limited".into(), ..Default::default() }
    }

    /// From a serenity error (code and status read through `bot_core::discord`).
    pub fn from_serenity(err: &serenity::Error) -> Self {
        let status = bot_core::discord::error_status(err);
        Self {
            code: bot_core::discord::error_code(err),
            status,
            timeout: false,
            rate_limited: if status == Some(429) { Some(0) } else { None },
            message: bot_core::discord::error_message(err),
        }
    }

    /// 50001 / 50013.
    pub fn is_no_access(&self) -> bool {
        matches!(self.code, Some(50001) | Some(50013))
    }

    /// 10008 / 10003 (`isGone` of the starboard service).
    pub fn is_gone(&self) -> bool {
        matches!(self.code, Some(10008) | Some(10003))
    }
}

impl std::fmt::Display for DiscordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DiscordError {}

impl From<serenity::Error> for DiscordError {
    fn from(err: serenity::Error) -> Self {
        Self::from_serenity(&err)
    }
}

/// A `Map` with the JS `memoSet` rule: a set moves the key to the newest position, and the oldest entries are evicted
/// past `cap`.
#[derive(Debug, Clone)]
pub struct CappedMap<V> {
    map: IndexMap<String, V>,
    cap: usize,
}

impl<V> CappedMap<V> {
    pub fn new(cap: usize) -> Self {
        Self { map: IndexMap::new(), cap }
    }

    /// `map.delete(key); map.set(key, value); evict`.
    pub fn set(&mut self, key: &str, value: V) {
        self.map.shift_remove(key);
        self.map.insert(key.to_string(), value);
        while self.map.len() > self.cap {
            self.map.shift_remove_index(0);
        }
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<V> {
        self.map.shift_remove(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Keeps only the entries for which `f` is true (insertion order is kept).
    pub fn retain(&mut self, f: impl FnMut(&String, &mut V) -> bool) {
        self.map.retain(f);
    }

    /// Drops the oldest entry.
    pub fn pop_oldest(&mut self) {
        self.map.shift_remove_index(0);
    }
}

/// `true` for a string of 17 to 20 ASCII digits (`/^\d{17,20}$/`).
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

/// A snowflake string as a non-zero `u64`.
pub fn parse_id(s: &str) -> Option<u64> {
    s.parse::<u64>().ok().filter(|v| *v != 0)
}

/// `Date.now()` from the system clock (used where the JS spaces real Discord calls, not for memo TTLs).
pub fn system_now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// `Math.random()`: a float in [0, 1) without a dependency on a random-number crate.
pub fn js_random() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_i64(system_now_ms());
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}
