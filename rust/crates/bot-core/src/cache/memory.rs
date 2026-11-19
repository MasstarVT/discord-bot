//! The in-memory cache backend: Redis's observable behaviour for the commands the bot uses (TTL expiry on a
//! controllable clock, NX, counters, sorted-set windows, lists, hashes, glob SCAN, WRONGTYPE errors), plus test
//! controls: make it unavailable (every command fails, `available()` is false), fail chosen commands, inspect the
//! calls and the stored values.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use indexmap::IndexMap;

use super::{CacheBackend, CacheError, redis_glob_match};
use crate::clock::SharedClock;

const WRONGTYPE: &str = "WRONGTYPE Operation against a key holding the wrong kind of value";
const NOT_INTEGER: &str = "ERR value is not an integer or out of range";
/// The error every command gets while the backend is unavailable (what ioredis says with the offline queue off).
pub const UNAVAILABLE: &str = "Stream isn't writeable and enableOfflineQueue options is false";

#[derive(Debug, Clone)]
enum Data {
    Str(String),
    ZSet(Vec<(f64, String)>),
    List(VecDeque<String>),
    Hash(IndexMap<String, String>),
}

#[derive(Debug, Clone)]
struct Entry {
    data: Data,
    expires_at: Option<i64>,
}

/// One command the backend received (for assertions in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheCall {
    pub op: &'static str,
    pub args: Vec<String>,
}

#[derive(Default)]
struct State {
    data: BTreeMap<String, Entry>,
    calls: Vec<CacheCall>,
    unavailable: bool,
    failing: HashSet<String>,
}

/// The in-memory backend. Thread-safe; share it through a [`super::Cache`].
pub struct MemoryBackend {
    clock: SharedClock,
    state: Mutex<State>,
}

impl MemoryBackend {
    pub fn new(clock: SharedClock) -> Self {
        Self { clock, state: Mutex::new(State::default()) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes every command fail (`false`) or work again (`true`); `available()` follows.
    pub fn set_available(&self, available: bool) {
        self.lock().unavailable = !available;
    }

    /// Makes the named commands fail (`"get"`, `"set"`, `"del"`, `"incr"`, `"expire"`, `"slidingWindow"`,
    /// `"pushRecent"`, `"hget"`, `"hset"`, `"hdel"`, `"scan"`, `"unlink"`); an empty list clears the failures.
    pub fn fail_ops(&self, ops: &[&str]) {
        let mut s = self.lock();
        s.failing = ops.iter().map(|o| (*o).to_string()).collect();
    }

    /// Every command received so far.
    pub fn calls(&self) -> Vec<CacheCall> {
        self.lock().calls.clone()
    }

    /// Forgets the recorded calls.
    pub fn clear_calls(&self) {
        self.lock().calls.clear();
    }

    /// Deletes every key (`FLUSHDB`).
    pub fn flush(&self) {
        self.lock().data.clear();
    }

    /// The stored string of `key` (no parsing, no call recorded), `None` when missing, expired or not a string.
    pub fn raw(&self, key: &str) -> Option<String> {
        let now = self.clock.now_ms();
        let s = self.lock();
        match s.data.get(key) {
            Some(Entry { data: Data::Str(v), expires_at }) if expires_at.is_none_or(|t| t > now) => Some(v.clone()),
            _ => None,
        }
    }

    /// Writes a raw string value (as another process would), with an optional TTL in milliseconds.
    pub fn put_raw(&self, key: &str, value: &str, ttl_ms: Option<i64>) {
        let now = self.clock.now_ms();
        self.lock().data.insert(key.to_string(), Entry { data: Data::Str(value.to_string()), expires_at: ttl_ms.map(|t| now + t) });
    }

    /// The remaining TTL of `key` in milliseconds (`PTTL`): `-2` missing, `-1` no TTL.
    pub fn pttl(&self, key: &str) -> i64 {
        let now = self.clock.now_ms();
        let s = self.lock();
        match s.data.get(key) {
            None => -2,
            Some(e) => match e.expires_at {
                Some(t) if t <= now => -2,
                Some(t) => t - now,
                None => -1,
            },
        }
    }

    /// The live keys, sorted.
    pub fn keys(&self) -> Vec<String> {
        let now = self.clock.now_ms();
        self.lock().data.iter().filter(|(_, e)| e.expires_at.is_none_or(|t| t > now)).map(|(k, _)| k.clone()).collect()
    }

    /// The members of a sorted set, lowest score first.
    pub fn zmembers(&self, key: &str) -> Vec<(f64, String)> {
        let now = self.clock.now_ms();
        let s = self.lock();
        match s.data.get(key) {
            Some(Entry { data: Data::ZSet(z), expires_at }) if expires_at.is_none_or(|t| t > now) => z.clone(),
            _ => Vec::new(),
        }
    }

    /// Records the call and checks the failure switches.
    fn begin(&self, s: &mut State, op: &'static str, args: Vec<String>) -> Result<(), CacheError> {
        s.calls.push(CacheCall { op, args });
        if s.unavailable {
            return Err(CacheError::new(UNAVAILABLE));
        }
        if s.failing.contains(op) {
            return Err(CacheError::new(format!("injected failure: {op}")));
        }
        let now = self.clock.now_ms();
        s.data.retain(|_, e| e.expires_at.is_none_or(|t| t > now));
        Ok(())
    }
}

#[async_trait]
impl CacheBackend for MemoryBackend {
    fn is_ready(&self) -> bool {
        !self.lock().unavailable
    }

    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "get", vec![key.into()])?;
        match s.data.get(key) {
            None => Ok(None),
            Some(Entry { data: Data::Str(v), .. }) => Ok(Some(v.clone())),
            Some(_) => Err(CacheError::new(WRONGTYPE)),
        }
    }

    async fn set(&self, key: &str, value: &str, ex_secs: Option<u64>, nx: bool) -> Result<bool, CacheError> {
        let mut s = self.lock();
        let mut args = vec![key.to_string(), value.to_string()];
        if let Some(ex) = ex_secs {
            args.extend(["EX".into(), ex.to_string()]);
        }
        if nx {
            args.push("NX".into());
        }
        self.begin(&mut s, "set", args)?;
        if nx && s.data.contains_key(key) {
            return Ok(false);
        }
        let now = self.clock.now_ms();
        let expires_at = ex_secs.map(|e| now + (e as i64).saturating_mul(1000));
        s.data.insert(key.to_string(), Entry { data: Data::Str(value.to_string()), expires_at });
        Ok(true)
    }

    async fn del(&self, keys: &[String]) -> Result<u64, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "del", keys.to_vec())?;
        Ok(keys.iter().filter(|k| s.data.remove(k.as_str()).is_some()).count() as u64)
    }

    async fn incr(&self, key: &str) -> Result<i64, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "incr", vec![key.into()])?;
        let entry = s.data.entry(key.to_string()).or_insert_with(|| Entry { data: Data::Str("0".into()), expires_at: None });
        let Data::Str(v) = &mut entry.data else { return Err(CacheError::new(WRONGTYPE)) };
        let n: i64 = v.parse().map_err(|_| CacheError::new(NOT_INTEGER))?;
        let n = n.checked_add(1).ok_or_else(|| CacheError::new("ERR increment or decrement would overflow"))?;
        *v = n.to_string();
        Ok(n)
    }

    async fn expire(&self, key: &str, secs: u64) -> Result<bool, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "expire", vec![key.into(), secs.to_string()])?;
        let now = self.clock.now_ms();
        Ok(match s.data.get_mut(key) {
            Some(e) => {
                e.expires_at = Some(now + (secs as i64).saturating_mul(1000));
                true
            }
            None => false,
        })
    }

    async fn sliding_window(&self, key: &str, now_ms: i64, window_ms: i64, member: &str) -> Result<i64, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "slidingWindow", vec![key.into(), now_ms.to_string(), window_ms.to_string(), member.into()])?;
        let entry = s.data.entry(key.to_string()).or_insert_with(|| Entry { data: Data::ZSet(Vec::new()), expires_at: None });
        let Data::ZSet(z) = &mut entry.data else { return Err(CacheError::new(WRONGTYPE)) };
        let score = now_ms as f64;
        match z.iter_mut().find(|(_, m)| m == member) {
            Some(slot) => slot.0 = score,
            None => z.push((score, member.to_string())),
        }
        let cutoff = (now_ms - window_ms) as f64;
        z.retain(|(sc, _)| !(*sc >= 0.0 && *sc <= cutoff));
        z.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(&b.1)));
        let count = z.len() as i64;
        if z.is_empty() {
            s.data.remove(key);
        } else if window_ms > 0 {
            entry.expires_at = Some(now_ms_clock(&self.clock) + window_ms);
        } else {
            s.data.remove(key);
        }
        Ok(count)
    }

    async fn push_recent(&self, key: &str, value: &str, max: i64, ttl_secs: u64) -> Result<Vec<String>, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "pushRecent", vec![key.into(), value.into(), max.to_string(), ttl_secs.to_string()])?;
        let now = self.clock.now_ms();
        let entry = s.data.entry(key.to_string()).or_insert_with(|| Entry { data: Data::List(VecDeque::new()), expires_at: None });
        let Data::List(list) = &mut entry.data else { return Err(CacheError::new(WRONGTYPE)) };
        list.push_front(value.to_string());
        let keep = lrange_len(list.len(), 0, max - 1);
        list.truncate(keep);
        let out: Vec<String> = list.iter().cloned().collect();
        if list.is_empty() {
            s.data.remove(key);
        } else {
            entry.expires_at = Some(now + (ttl_secs as i64).saturating_mul(1000));
        }
        Ok(out)
    }

    async fn hget(&self, hash: &str, field: &str) -> Result<Option<String>, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "hget", vec![hash.into(), field.into()])?;
        match s.data.get(hash) {
            None => Ok(None),
            Some(Entry { data: Data::Hash(h), .. }) => Ok(h.get(field).cloned()),
            Some(_) => Err(CacheError::new(WRONGTYPE)),
        }
    }

    async fn hset(&self, hash: &str, field: &str, value: &str) -> Result<(), CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "hset", vec![hash.into(), field.into(), value.into()])?;
        let entry = s.data.entry(hash.to_string()).or_insert_with(|| Entry { data: Data::Hash(IndexMap::new()), expires_at: None });
        let Data::Hash(h) = &mut entry.data else { return Err(CacheError::new(WRONGTYPE)) };
        h.insert(field.to_string(), value.to_string());
        Ok(())
    }

    async fn hdel(&self, hash: &str, field: &str) -> Result<(), CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "hdel", vec![hash.into(), field.into()])?;
        let remove = match s.data.get_mut(hash) {
            None => false,
            Some(Entry { data: Data::Hash(h), .. }) => {
                h.shift_remove(field);
                h.is_empty()
            }
            Some(_) => return Err(CacheError::new(WRONGTYPE)),
        };
        if remove {
            s.data.remove(hash);
        }
        Ok(())
    }

    async fn scan(&self, cursor: &str, pattern: &str, count: usize) -> Result<(String, Vec<String>), CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "scan", vec![cursor.into(), "MATCH".into(), pattern.into(), "COUNT".into(), count.to_string()])?;
        // The cursor is the last key of the previous page (keys are kept sorted), so keys deleted between pages never
        // make the scan skip any (Redis guarantees every key present for the whole scan is returned).
        let after = cursor.strip_prefix("k:");
        let keys: Vec<&String> = s.data.keys().filter(|k| after.is_none_or(|a| k.as_str() > a)).take(count.max(1)).collect();
        let more = keys.last().is_some_and(|last| s.data.keys().any(|k| k > *last));
        let page: Vec<String> = keys.iter().filter(|k| redis_glob_match(pattern, k)).map(|k| (*k).clone()).collect();
        let next = match keys.last() {
            Some(last) if more => format!("k:{last}"),
            _ => "0".to_string(),
        };
        Ok((next, page))
    }

    async fn unlink(&self, keys: &[String]) -> Result<u64, CacheError> {
        let mut s = self.lock();
        self.begin(&mut s, "unlink", keys.to_vec())?;
        Ok(keys.iter().filter(|k| s.data.remove(k.as_str()).is_some()).count() as u64)
    }
}

fn now_ms_clock(clock: &SharedClock) -> i64 {
    clock.now_ms()
}

/// How many elements `LRANGE 0 stop` / `LTRIM 0 stop` keeps of a list of `len` (negative indexes count from the end).
fn lrange_len(len: usize, start: i64, stop: i64) -> usize {
    let len_i = len as i64;
    let start = if start < 0 { (len_i + start).max(0) } else { start };
    let stop = if stop < 0 { len_i + stop } else { stop.min(len_i - 1) };
    if start > stop || start >= len_i { 0 } else { (stop + 1) as usize }
}
