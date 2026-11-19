//! The dashboard's rate limits: the pure helpers of `src/dashboard/server.js` (`createRateLimiter`, `clientAddress`,
//! `loginSourceKey`, the limit constants and the login callback's overall allowance with its "unresolved source"
//! marks), so `dash-server` only wires them into its handlers.

use std::collections::HashSet;
use std::net::Ipv6Addr;
use std::sync::LazyLock;

use bot_util::js::{trim, utf16_len};
use bot_util::jsre::JsRegex;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::config::normalize_ip;
use crate::js_ip::is_ip;

/// `{ max, windowMs }` of one limiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub max: usize,
    pub window_ms: u64,
}

/// Settings writes per user.
pub const WRITE_RATE_LIMIT: RateLimit = RateLimit { max: 20, window_ms: 10_000 };
/// Member searches (user pickers search as the admin types).
pub const SEARCH_RATE_LIMIT: RateLimit = RateLimit { max: 30, window_ms: 10_000 };
/// Login callbacks that reach Discord's token endpoint, per source (an IPv6 /64 counts as one).
pub const LOGIN_RATE_LIMIT: RateLimit = RateLimit { max: 10, window_ms: 60_000 };
/// The overall allowance, spent only by a source whose earlier callback has not (yet) succeeded.
pub const LOGIN_GLOBAL_RATE_LIMIT: RateLimit = RateLimit { max: 60, window_ms: 60_000 };
/// The hard ceiling on first callbacks of every source together.
pub const LOGIN_FIRST_ATTEMPT_RATE_LIMIT: RateLimit = RateLimit { max: 600, window_ms: 60_000 };
/// How many keys a limiter (and the unresolved-login map) keeps; the oldest goes first.
pub const RATE_LIMIT_KEYS_MAX: usize = 10_000;
/// Peers whose X-Forwarded-For is always believed (a reverse proxy on the same host).
pub const LOOPBACK_ADDRESSES: [&str; 3] = ["127.0.0.1", "::1", "::ffff:127.0.0.1"];

/// The answer of one hit: allowed, or refused with the seconds until the oldest hit leaves the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    Ok,
    Limited { retry_after: u64 },
}

impl RateDecision {
    pub fn is_ok(self) -> bool {
        matches!(self, RateDecision::Ok)
    }
}

/// `createRateLimiter({ max, windowMs, now })`: a sliding window of hit times per key, at most
/// [`RATE_LIMIT_KEYS_MAX`] keys (least recently hit dropped first). Times are ms since epoch, passed per hit.
#[derive(Debug)]
pub struct RateLimiter {
    limit: RateLimit,
    hits: Mutex<IndexMap<String, Vec<f64>>>,
}

impl RateLimiter {
    pub fn new(limit: RateLimit) -> Self {
        Self { limit, hits: Mutex::new(IndexMap::new()) }
    }

    /// One hit of `key` at `now_ms`.
    pub fn hit(&self, key: &str, now_ms: f64) -> RateDecision {
        let window = self.limit.window_ms as f64;
        let mut hits = self.hits.lock();
        let mut recent: Vec<f64> =
            hits.shift_remove(key).unwrap_or_default().into_iter().filter(|&at| at > now_ms - window).collect();
        if recent.len() >= self.limit.max {
            let retry = ((recent[0] + window - now_ms) / 1000.0).ceil().max(1.0);
            hits.insert(key.to_string(), recent);
            return RateDecision::Limited { retry_after: retry as u64 };
        }
        recent.push(now_ms);
        hits.insert(key.to_string(), recent);
        while hits.len() > RATE_LIMIT_KEYS_MAX {
            hits.shift_remove_index(0);
        }
        RateDecision::Ok
    }
}

/// The client address the login limit counts (`clientAddress`). A request is counted under its socket address; only
/// when that is a loopback address or one of the trusted proxies (normalised, `config.trustedProxies`) is the last
/// `X-Forwarded-For` entry used instead (when it is non-empty and at most 64 characters).
/// `peer` is the socket's remote address (`None` = unknown); `forwarded` the X-Forwarded-For header as Node joins it.
pub fn client_address(peer: Option<&str>, forwarded: Option<&str>, trusted: &HashSet<String>) -> String {
    let peer = peer.unwrap_or("unknown");
    if !LOOPBACK_ADDRESSES.contains(&peer) && !trusted.contains(&normalize_ip(peer)) {
        return peer.to_string();
    }
    let last = forwarded.map(|h| trim(h.rsplit(',').next().unwrap_or(""))).unwrap_or("");
    if !last.is_empty() && utf16_len(last) <= 64 {
        last.to_string()
    } else {
        peer.to_string()
    }
}

static BRACKETED_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^\[([^\]]*)\](?::\d{1,5})?$", ""));
static V4_PORT_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"^(\d{1,3}(?:\.\d{1,3}){3}):\d{1,5}$", ""));
static ZONE_RE: LazyLock<JsRegex> = LazyLock::new(|| JsRegex::new(r"%.*$", ""));

/// The key a login callback is counted under (`loginSourceKey`): an IPv6 address stands for its whole /64 (written
/// `xxxx:xxxx:xxxx:xxxx::/64`), a port and a zone id are dropped, an IPv4-mapped address counts as its IPv4 address,
/// and anything that is not an IP address is used as it is.
pub fn login_source_key(address: &str) -> String {
    let group1 = |m: &bot_util::jsre::JsMatch, t: &[u16]| m.group(t, 1).unwrap_or_default();
    let bare = BRACKETED_RE.replace_first_with(trim(address), group1);
    let bare = V4_PORT_RE.replace_first_with(&bare, group1);
    let bare = ZONE_RE.replace_first_with(&bare, |_, _| String::new());
    let ip = normalize_ip(&bare);
    if is_ip(&ip) != 6 {
        return ip;
    }
    // isIP allowed it, so std parses it (no zone id is left; std accepts every form Node's regex does).
    let Ok(addr) = ip.parse::<Ipv6Addr>() else { return ip };
    let n = addr.segments();
    if n[..5].iter().all(|&x| x == 0) && n[5] == 0xffff {
        return format!("{}.{}.{}.{}", n[6] >> 8, n[6] & 255, n[7] >> 8, n[7] & 255);
    }
    format!("{:04x}:{:04x}:{:04x}:{:04x}::/64", n[0], n[1], n[2], n[3])
}

/// The login callback's limits (`GET /auth/callback` in server.js): per source, the overall allowance for sources
/// with an unresolved callback, the ceiling for first callbacks, and the unresolved marks.
///
/// Call [`LoginLimiter::check`] once a callback has a matching state and a well-formed code (before the token
/// exchange); on `Limited` answer 429 `Too many login attempts. Try again in {retry_after} s.` with `Retry-After`.
/// After a successful login call [`LoginLimiter::resolved`].
#[derive(Debug)]
pub struct LoginLimiter {
    by_address: RateLimiter,
    overall: RateLimiter,
    first: RateLimiter,
    unresolved: Mutex<IndexMap<String, f64>>,
    /// The JS checks and marks a source in one synchronous step; this lock makes [`LoginLimiter::check`] one step too.
    step: Mutex<()>,
}

/// What [`LoginLimiter::check`] decided; `overall` tells the log line which limit refused it (`overall limit` when the
/// source itself was under its cap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginCheck {
    pub decision: RateDecision,
    pub overall: bool,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self {
            by_address: RateLimiter::new(LOGIN_RATE_LIMIT),
            overall: RateLimiter::new(LOGIN_GLOBAL_RATE_LIMIT),
            first: RateLimiter::new(LOGIN_FIRST_ATTEMPT_RATE_LIMIT),
            unresolved: Mutex::new(IndexMap::new()),
            step: Mutex::new(()),
        }
    }

    fn has_unresolved(&self, key: &str, now_ms: f64) -> bool {
        let mut map = self.unresolved.lock();
        let Some(&at) = map.get(key) else { return false };
        if at > now_ms - LOGIN_RATE_LIMIT.window_ms as f64 {
            return true;
        }
        map.shift_remove(key);
        false
    }

    fn mark(&self, key: &str, now_ms: f64) {
        let mut map = self.unresolved.lock();
        map.shift_remove(key);
        map.insert(key.to_string(), now_ms);
        while map.len() > RATE_LIMIT_KEYS_MAX {
            map.shift_remove_index(0);
        }
    }

    /// Counts one callback of `source` (a [`login_source_key`]) at `now_ms`, and marks the source unresolved when it
    /// is let through as a first callback (before the exchange is awaited, so concurrent requests cannot all be first).
    pub fn check(&self, source: &str, now_ms: f64) -> LoginCheck {
        let _step = self.step.lock();
        let limited = self.by_address.hit(source, now_ms);
        let marked = limited.is_ok() && self.has_unresolved(source, now_ms);
        let decision = if !limited.is_ok() {
            limited
        } else if marked {
            self.overall.hit("all", now_ms)
        } else {
            self.first.hit("first", now_ms)
        };
        if decision.is_ok() && !marked {
            self.mark(source, now_ms);
        }
        LoginCheck { decision, overall: limited.is_ok() }
    }

    /// A login of `source` succeeded: its next callback is a first one again.
    pub fn resolved(&self, source: &str) {
        self.unresolved.lock().shift_remove(source);
    }
}
