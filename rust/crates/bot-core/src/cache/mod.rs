//! Port of `src/services/redis.js`: the never-failing cache.
//!
//! [`Cache`] is the `cache` object of the JS. Every method returns the documented fallback instead of an error
//! (`get` -> `None`, `set`/`del`/`hset`/`hdel` -> `false`, `set_nx` -> `None`, `incr`/`sliding_window`/`push_recent`
//! -> `None`, `invalidate` -> the keys deleted so far) and logs at most one "Redis unavailable" warning per 30 s. The
//! caller decides fail-open or fail-closed on a fallback: for `set_nx`, fail-open callers test `!= Some(false)`,
//! fail-closed callers test `== Some(true)`.
//!
//! Values are JSON, exactly what `JSON.stringify` wrote: serialize Rust values with the field names and formats the JS
//! used (see `bot_db::serde_js` for dates and numbers). [`Cache::get`] returns the parsed JSON, or the raw string when it
//! is not JSON; [`Cache::get_as`] deserializes and treats anything it cannot read as a miss.
//!
//! Two backends implement [`CacheBackend`]: [`RedisBackend`] (the redis crate, the JS client's options: 750 ms command
//! timeout, no offline queue, reconnect back-off `min(n * 200, 5000)` ms) and [`MemoryBackend`] (the same observable
//! behaviour in process, on a controllable [`Clock`](crate::clock::Clock), with failure injection for tests).

mod glob;
mod memory;
mod redis_backend;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::clock::SharedClock;

pub use glob::{glob_to_regex, redis_glob_match};
pub use memory::{CacheCall, MemoryBackend};
pub use redis_backend::{RedisBackend, RedisOptions};

/// Keys SCAN looks at per call (`SCAN_COUNT`).
pub const SCAN_COUNT: usize = 1000;
/// Log lines for a flapping connection are rate-limited to one per this interval.
pub const LOG_INTERVAL_MS: i64 = 30_000;
/// Every command times out after this long (`commandTimeout`).
pub const COMMAND_TIMEOUT_MS: u64 = 750;

/// A backend error: its message is what the JS logs (`err.message`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CacheError(pub String);

impl CacheError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// The raw Redis operations the cache needs. Values are the stored strings; errors become fallbacks in [`Cache`].
#[async_trait]
pub trait CacheBackend: AsAny + Send + Sync + 'static {
    /// `client.status === 'ready'`.
    fn is_ready(&self) -> bool;
    /// `GET key`.
    async fn get(&self, key: &str) -> Result<Option<String>, CacheError>;
    /// `SET key value [EX secs] [NX]`; `true` when the reply was OK (always for a plain SET).
    async fn set(&self, key: &str, value: &str, ex_secs: Option<u64>, nx: bool) -> Result<bool, CacheError>;
    /// `DEL key...`; the number removed.
    async fn del(&self, keys: &[String]) -> Result<u64, CacheError>;
    /// `INCR key`.
    async fn incr(&self, key: &str) -> Result<i64, CacheError>;
    /// `EXPIRE key secs`.
    async fn expire(&self, key: &str, secs: u64) -> Result<bool, CacheError>;
    /// `MULTI ZADD key now member, ZREMRANGEBYSCORE key 0 now-window, ZCARD key, PEXPIRE key window EXEC`; the ZCARD.
    async fn sliding_window(&self, key: &str, now_ms: i64, window_ms: i64, member: &str) -> Result<i64, CacheError>;
    /// `MULTI LPUSH key value, LTRIM key 0 max-1, EXPIRE key ttl, LRANGE key 0 max-1 EXEC`; the LRANGE.
    async fn push_recent(&self, key: &str, value: &str, max: i64, ttl_secs: u64) -> Result<Vec<String>, CacheError>;
    /// `HGET hash field`.
    async fn hget(&self, hash: &str, field: &str) -> Result<Option<String>, CacheError>;
    /// `HSET hash field value`.
    async fn hset(&self, hash: &str, field: &str, value: &str) -> Result<(), CacheError>;
    /// `HDEL hash field`.
    async fn hdel(&self, hash: &str, field: &str) -> Result<(), CacheError>;
    /// `SCAN cursor MATCH pattern COUNT count`; the next cursor and the keys.
    async fn scan(&self, cursor: &str, pattern: &str, count: usize) -> Result<(String, Vec<String>), CacheError>;
    /// `UNLINK key...`; the number removed.
    async fn unlink(&self, keys: &[String]) -> Result<u64, CacheError>;
}

/// How the JS treats a TTL argument (`if (ttl) ... 'EX', Math.ceil(ttl)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlArg {
    /// Falsy (missing, zero or NaN): no expiry is sent.
    None,
    /// `EX ceil(ttl)`.
    Secs(u64),
    /// Truthy but not positive (a negative number): the JS sends `EX <n <= 0>`, which Redis refuses (`SET` fails,
    /// and `EXPIRE` with it deletes the key).
    Invalid,
}

/// A TTL in seconds as the JS takes it: any number, rounded up (`Math.ceil`); zero or NaN means none. A negative TTL
/// is what the JS sends to Redis as is ([`IntoTtl::ttl_arg`] tells it apart; [`IntoTtl::into_ttl`] reads it as none).
pub trait IntoTtl {
    fn into_ttl(self) -> Option<u64>;

    /// The TTL as the JS would send it, telling a refused negative TTL apart from none.
    fn ttl_arg(self) -> TtlArg
    where
        Self: Sized,
    {
        match self.into_ttl() {
            Some(s) => TtlArg::Secs(s),
            None => TtlArg::None,
        }
    }
}

macro_rules! ttl_int {
    ($($t:ty),*) => {$(impl IntoTtl for $t {
        #[allow(unused_comparisons)]
        fn into_ttl(self) -> Option<u64> { if self > 0 { Some(self as u64) } else { None } }
        #[allow(unused_comparisons)]
        fn ttl_arg(self) -> TtlArg {
            if self > 0 { TtlArg::Secs(self as u64) } else if self == 0 { TtlArg::None } else { TtlArg::Invalid }
        }
    })*};
}
ttl_int!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl IntoTtl for f64 {
    fn into_ttl(self) -> Option<u64> {
        if self.is_nan() || self <= 0.0 { None } else { Some(self.ceil().min(u64::MAX as f64) as u64) }
    }
    fn ttl_arg(self) -> TtlArg {
        if self.is_nan() || self == 0.0 {
            TtlArg::None
        } else if self < 0.0 {
            TtlArg::Invalid
        } else {
            TtlArg::Secs(self.ceil().min(u64::MAX as f64) as u64)
        }
    }
}
impl IntoTtl for f32 {
    fn into_ttl(self) -> Option<u64> {
        f64::from(self).into_ttl()
    }
    fn ttl_arg(self) -> TtlArg {
        f64::from(self).ttl_arg()
    }
}
impl<T: IntoTtl> IntoTtl for Option<T> {
    fn into_ttl(self) -> Option<u64> {
        self.and_then(IntoTtl::into_ttl)
    }
    fn ttl_arg(self) -> TtlArg {
        match self {
            Some(t) => t.ttl_arg(),
            None => TtlArg::None,
        }
    }
}

/// Redis's answer to `SET ... EX <n <= 0>`.
const INVALID_EXPIRE: &str = "ERR invalid expire time in 'set' command";

/// `parse(raw)`: the JSON value, or the raw string when it is not JSON.
pub fn parse_cached(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// The cache. Cheap to clone (shared state).
#[derive(Clone)]
pub struct Cache {
    backend: Arc<dyn CacheBackend>,
    clock: SharedClock,
    last_unavailable_log_at: Arc<AtomicI64>,
}

impl Cache {
    /// A cache over `backend`, rate-limiting its log lines with `clock`.
    pub fn new(backend: Arc<dyn CacheBackend>, clock: SharedClock) -> Self {
        Self { backend, clock, last_unavailable_log_at: Arc::new(AtomicI64::new(i64::MIN / 2)) }
    }

    /// The backend (tests reach the in-memory one through [`Cache::memory`]).
    pub fn backend(&self) -> &Arc<dyn CacheBackend> {
        &self.backend
    }

    /// Logs at most one "Redis unavailable" warning per 30 s, whatever the operation (`noteFailure`).
    fn note_failure(&self, op: &str, err: &CacheError) {
        let now = self.clock.now_ms();
        let last = self.last_unavailable_log_at.load(Ordering::Relaxed);
        if now - last < LOG_INTERVAL_MS {
            return;
        }
        if self.last_unavailable_log_at.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        bot_util::log_warn!(format!("Redis unavailable: {op} ({})", err.0));
    }

    fn serialise<T: Serialize + ?Sized>(&self, op: &str, key: &str, value: &T) -> Option<String> {
        match serde_json::to_string(value) {
            Ok(s) => Some(s),
            Err(e) => {
                bot_util::log_error!(
                    format!("cache.{op} could not serialise \"{key}\""),
                    bot_util::logger::LogArg::Error { message: e.to_string(), stack: None }
                );
                None
            }
        }
    }

    /// `true` when the Redis connection is ready.
    pub fn available(&self) -> bool {
        self.backend.is_ready()
    }

    /// The parsed JSON value, the raw string (as `Value::String`) when it is not JSON, or `None` (miss or error).
    pub async fn get(&self, key: &str) -> Option<Value> {
        match self.backend.get(key).await {
            Ok(raw) => raw.map(|r| parse_cached(&r)),
            Err(e) => {
                self.note_failure("get", &e);
                None
            }
        }
    }

    /// [`Cache::get`] deserialized as `T`; a value that does not fit `T` is a miss.
    pub async fn get_as<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.get(key).await.and_then(|v| serde_json::from_value(v).ok())
    }

    /// `SET key json [EX ttl]`. `true` when written.
    pub async fn set<T: Serialize + ?Sized>(&self, key: &str, value: &T, ttl: impl IntoTtl) -> bool {
        let Some(serialised) = self.serialise("set", key, value) else { return false };
        let ttl = match ttl.ttl_arg() {
            TtlArg::None => None,
            TtlArg::Secs(s) => Some(s),
            TtlArg::Invalid => {
                // `SET key value EX -n`: Redis refuses it, nothing is written.
                self.note_failure("set", &CacheError::new(INVALID_EXPIRE));
                return false;
            }
        };
        match self.backend.set(key, &serialised, ttl, false).await {
            Ok(_) => true,
            Err(e) => {
                self.note_failure("set", &e);
                false
            }
        }
    }

    /// `SET key json EX ttl NX` for cooldowns and locks: `Some(true)` (set), `Some(false)` (already exists), `None`
    /// (Redis unavailable). Fail-open callers test `!= Some(false)`, fail-closed callers `== Some(true)`.
    pub async fn set_nx<T: Serialize + ?Sized>(&self, key: &str, value: &T, ttl: impl IntoTtl) -> Option<bool> {
        let serialised = self.serialise("setNX", key, value)?;
        let ttl = match ttl.ttl_arg() {
            TtlArg::None => None,
            TtlArg::Secs(s) => Some(s),
            TtlArg::Invalid => {
                self.note_failure("setNX", &CacheError::new(INVALID_EXPIRE));
                return None;
            }
        };
        match self.backend.set(key, &serialised, ttl, true).await {
            Ok(ok) => Some(ok),
            Err(e) => {
                self.note_failure("setNX", &e);
                None
            }
        }
    }

    /// `DEL keys...` (empty keys are ignored; nothing to delete is `true`). `true` when the command succeeded.
    pub async fn del<S: AsRef<str>>(&self, keys: &[S]) -> bool {
        let flat: Vec<String> = keys.iter().map(|k| k.as_ref()).filter(|k| !k.is_empty()).map(str::to_string).collect();
        if flat.is_empty() {
            return true;
        }
        match self.backend.del(&flat).await {
            Ok(_) => true,
            Err(e) => {
                self.note_failure("del", &e);
                false
            }
        }
    }

    /// `DEL key`.
    pub async fn del_one(&self, key: &str) -> bool {
        self.del(&[key]).await
    }

    /// `INCR`, then `EXPIRE` when the result is 1. The new count, or `None`. A counter is never left without a TTL:
    /// when the EXPIRE fails the key is deleted.
    pub async fn incr(&self, key: &str, ttl: impl IntoTtl) -> Option<i64> {
        let ttl = ttl.ttl_arg();
        let result: Result<i64, CacheError> = async {
            let n = self.backend.incr(key).await?;
            if n == 1 {
                match ttl {
                    TtlArg::Secs(secs) => {
                        if let Err(e) = self.backend.expire(key, secs).await {
                            let _ = self.backend.del(&[key.to_string()]).await;
                            return Err(e);
                        }
                    }
                    // `EXPIRE key <n <= 0>` deletes the key: the next INCR starts at 1 again.
                    TtlArg::Invalid => {
                        self.backend.del(&[key.to_string()]).await?;
                    }
                    TtlArg::None => {}
                }
            }
            Ok(n)
        }
        .await;
        match result {
            Ok(n) => Some(n),
            Err(e) => {
                self.note_failure("incr", &e);
                None
            }
        }
    }

    /// A sliding-window counter on a sorted set: hits in the window including this one, or `None`. `member` defaults
    /// to a random UUID (pass one to count a hit once).
    pub async fn sliding_window(&self, key: &str, window_ms: i64, member: Option<&str>) -> Option<i64> {
        let now = self.clock.now_ms();
        let member = member.map(str::to_string).unwrap_or_else(bot_db::new_uuid);
        match self.backend.sliding_window(key, now, window_ms, &member).await {
            Ok(n) => Some(n),
            Err(e) => {
                self.note_failure("slidingWindow", &e);
                None
            }
        }
    }

    /// Pushes `value` onto a capped list (newest first); the list after the push, or `None`.
    pub async fn push_recent(&self, key: &str, value: &str, max: i64, ttl_secs: impl IntoTtl) -> Option<Vec<String>> {
        // `expire(key, Math.ceil(ttl))`: the JS always sends an EXPIRE here; a missing TTL would be EXPIRE NaN (an error).
        let Some(ttl) = ttl_secs.into_ttl() else {
            self.note_failure("pushRecent", &CacheError::new("ERR value is not an integer or out of range"));
            return None;
        };
        match self.backend.push_recent(key, value, max, ttl).await {
            Ok(list) => Some(list),
            Err(e) => {
                self.note_failure("pushRecent", &e);
                None
            }
        }
    }

    /// `HGET hash field`, parsed like [`Cache::get`].
    pub async fn hget(&self, hash: &str, field: &str) -> Option<Value> {
        match self.backend.hget(hash, field).await {
            Ok(raw) => raw.map(|r| parse_cached(&r)),
            Err(e) => {
                self.note_failure("hget", &e);
                None
            }
        }
    }

    /// `HSET hash field json`. `true` when written.
    pub async fn hset<T: Serialize + ?Sized>(&self, hash: &str, field: &str, value: &T) -> bool {
        let serialised = match serde_json::to_string(value) {
            Ok(s) => s,
            Err(e) => {
                self.note_failure("hset", &CacheError::new(e.to_string()));
                return false;
            }
        };
        match self.backend.hset(hash, field, &serialised).await {
            Ok(()) => true,
            Err(e) => {
                self.note_failure("hset", &e);
                false
            }
        }
    }

    /// `HDEL hash field`. `true` when the command succeeded.
    pub async fn hdel(&self, hash: &str, field: &str) -> bool {
        match self.backend.hdel(hash, field).await {
            Ok(()) => true,
            Err(e) => {
                self.note_failure("hdel", &e);
                false
            }
        }
    }

    /// Deletes every key matching the glob `pattern` with SCAN (COUNT 1000, never KEYS) and UNLINK. Only keys `keep`
    /// returns true for are deleted (one pass can then serve several patterns). The keys deleted; on a failure, the
    /// keys deleted before it.
    pub async fn invalidate_with(&self, pattern: &str, keep: Option<&(dyn Fn(&str) -> bool + Sync)>) -> u64 {
        let mut deleted = 0u64;
        let mut cursor = "0".to_string();
        loop {
            let (next, found) = match self.backend.scan(&cursor, pattern, SCAN_COUNT).await {
                Ok(page) => page,
                Err(e) => {
                    self.note_failure("invalidate", &e);
                    return deleted;
                }
            };
            cursor = next;
            let keys: Vec<String> = match keep {
                Some(f) => found.into_iter().filter(|k| f(k)).collect(),
                None => found,
            };
            if !keys.is_empty() {
                match self.backend.unlink(&keys).await {
                    Ok(n) => deleted += n,
                    Err(e) => {
                        self.note_failure("invalidate", &e);
                        return deleted;
                    }
                }
            }
            if cursor == "0" {
                return deleted;
            }
        }
    }

    /// `invalidate(pattern)`: deletes every key matching the glob.
    pub async fn invalidate(&self, pattern: &str) -> u64 {
        self.invalidate_with(pattern, None).await
    }

    /// `invalidateAny(match, patterns)`: one SCAN pass over `match_glob` (a glob that covers all `patterns`), deleting
    /// the keys that match any of `patterns` (checked with [`glob_to_regex`]).
    pub async fn invalidate_any<S: AsRef<str>>(&self, match_glob: &str, patterns: &[S]) -> u64 {
        let tests: Vec<regex::Regex> = patterns.iter().map(|p| glob_to_regex(p.as_ref())).collect();
        let keep = move |key: &str| tests.iter().any(|re| re.is_match(key));
        self.invalidate_with(match_glob, Some(&keep)).await
    }

    /// The in-memory backend, when this cache uses one (tests).
    pub fn memory(&self) -> Option<&MemoryBackend> {
        self.backend.as_ref().as_any().downcast_ref::<MemoryBackend>()
    }
}

/// Downcasting support for backends (lets tests reach the [`MemoryBackend`] behind a [`Cache`]).
pub trait AsAny {
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<T: std::any::Any> AsAny for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
