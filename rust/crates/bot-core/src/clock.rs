//! The time source shared by the settings memo, the in-memory cache and the in-memory job store.
//!
//! The JS reads `Date.now()` everywhere. Rust code in bot-core reads [`Clock::now_ms`] from the [`crate::App`] instead,
//! so tests can move time forward with a [`ManualClock`] (TTL expiry, the 30 s settings memo, job back-off).
//! Database time (`SQL_NOW`) is never affected: SQL statements always use the server's clock, as in the JS.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, TimeZone, Utc};

/// A source of "now" in epoch milliseconds (what `Date.now()` returns).
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> i64;

    /// The current time as a UTC date at millisecond precision.
    fn now(&self) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(self.now_ms()).single().unwrap_or_else(Utc::now)
    }
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        Utc::now().timestamp_millis()
    }
}

/// A clock that only moves when told to. Clones share the same time.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    /// Starts at `start_ms` (epoch milliseconds).
    pub fn new(start_ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(start_ms)))
    }

    /// Starts at the real current time.
    pub fn starting_now() -> Self {
        Self::new(Utc::now().timestamp_millis())
    }

    /// Moves the clock forward by `ms`.
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// Sets the clock to `ms`.
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A shared clock handle.
pub type SharedClock = Arc<dyn Clock>;

/// `createThrottle(intervalMs)` on this clock (the JS throttles read `Date.now()`), so a [`ManualClock`] in tests
/// moves it: `clock_throttle(app.clock(), 60_000)`.
pub fn clock_throttle(clock: &SharedClock, interval_ms: u64) -> bot_util::async_util::Throttle {
    let c = clock.clone();
    bot_util::async_util::Throttle::with_clock(interval_ms, Arc::new(move || c.now_ms()))
}
