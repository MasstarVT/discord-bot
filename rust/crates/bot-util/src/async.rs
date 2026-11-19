//! Port of `src/utils/async.js`: timeouts, the interaction acknowledgement budget, bounded concurrency, a per-key
//! mutex, a counting semaphore and a per-key throttle, on tokio with the JS semantics (a timed-out operation keeps
//! running and its later failure is logged; `run` links work into a key's chain at call time, like a JS promise).

use futures::future::{FutureExt, Shared};
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, VecDeque};
use std::fmt::Display;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// `TimeoutError`: its message is "`label` timed out".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{label} timed out")]
pub struct TimeoutError {
    pub label: String,
}

/// The error of [`with_timeout`]: the deadline passed, or the operation failed.
#[derive(Debug, thiserror::Error)]
pub enum WithTimeoutError<E> {
    #[error(transparent)]
    Timeout(TimeoutError),
    #[error("{0}")]
    Inner(E),
}

/// `sleep(ms)`.
pub async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Resolves with `fut`, or fails with `TimeoutError(label)` after `ms`. The operation is spawned, so when the timeout
/// wins it keeps running; a later failure is logged ("`label` failed after timeout"), never lost.
pub async fn with_timeout<T, E, F>(fut: F, ms: u64, label: &str) -> Result<T, WithTimeoutError<E>>
where
    F: Future<Output = Result<T, E>> + Send + 'static,
    T: Send + 'static,
    E: Display + Send + 'static,
{
    let mut handle = tokio::spawn(fut);
    tokio::select! {
        res = &mut handle => match res {
            Ok(r) => r.map_err(WithTimeoutError::Inner),
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        },
        _ = tokio::time::sleep(Duration::from_millis(ms)) => {
            let label_owned = label.to_string();
            tokio::spawn(async move {
                if let Ok(Err(e)) = handle.await {
                    crate::logger::error(&[format!("{label_owned} failed after timeout").into(), crate::logger::LogArg::Error { message: e.to_string(), stack: None }]);
                }
            });
            Err(WithTimeoutError::Timeout(TimeoutError { label: label.to_string() }))
        }
    }
}

/// Time left for lookups before an interaction must be acknowledged: Discord allows 3 s, 2600 ms leaves network
/// slack. `created_timestamp_ms` is the interaction's creation time; at 0 skip optional lookups.
pub fn ack_budget_ms(created_timestamp_ms: f64, reserve_ms: f64) -> u64 {
    ack_budget_ms_at(created_timestamp_ms, reserve_ms, crate::js::now_ms())
}

/// [`ack_budget_ms`] at a given clock.
pub fn ack_budget_ms_at(created_timestamp_ms: f64, reserve_ms: f64, now_ms: f64) -> u64 {
    let v = 2600.0 - (now_ms - created_timestamp_ms) - reserve_ms;
    if v.is_nan() || v <= 0.0 { 0 } else { v as u64 }
}

/// The default reserve of [`ack_budget_ms`].
pub const ACK_RESERVE_MS: f64 = 400.0;

/// Runs `f` over `items` with at most `limit` in flight (at least 1); results keep the input order and every
/// failure is kept as an `Err` (never short-circuits).
pub async fn map_limit<T, R, E, F, Fut>(items: Vec<T>, limit: usize, f: F) -> Vec<Result<R, E>>
where
    F: Fn(T, usize) -> Fut,
    Fut: Future<Output = Result<R, E>>,
{
    let n = items.len();
    let workers = limit.max(1).min(n.max(1));
    let mut indexed: Vec<(usize, Result<R, E>)> = stream::iter(items.into_iter().enumerate())
        .map(|(i, item)| {
            let fut = f(item, i);
            async move { (i, fut.await) }
        })
        .buffer_unordered(workers)
        .collect()
        .await;
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, r)| r).collect()
}

type Tail = Shared<oneshot::Receiver<()>>;

/// Serialises work per key; the entry is removed when the key is idle. `run` links the work into the key's chain
/// when it is called (not when it is awaited), so callers keep gateway order; a failing job releases the key and
/// its failure reaches only its own caller.
#[derive(Clone, Default)]
pub struct KeyedMutex {
    tails: Arc<Mutex<HashMap<String, (u64, Tail)>>>,
    next_id: Arc<AtomicU64>,
}

impl KeyedMutex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` after every earlier job of `key`. The job starts (is spawned) now; await the returned future for
    /// its result.
    pub fn run<T, F, Fut>(&self, key: &str, f: F) -> impl Future<Output = T> + Send + 'static
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<()>();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let prev = {
            let mut tails = self.tails.lock().unwrap_or_else(|e| e.into_inner());
            tails.insert(key.to_string(), (id, rx.shared())).map(|(_, t)| t)
        };
        let tails = self.tails.clone();
        let key = key.to_string();
        let handle = tokio::spawn(async move {
            if let Some(prev) = prev {
                let _ = prev.await;
            }
            // Released when this job ends, panics included (the sender is dropped).
            let _release = Release { tx: Some(tx), tails, key, id };
            f().await
        });
        async move {
            match handle.await {
                Ok(v) => v,
                Err(e) => std::panic::resume_unwind(e.into_panic()),
            }
        }
    }

    /// Keys with work queued or running.
    pub fn len(&self) -> usize {
        self.tails.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// True when no key is busy.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Release {
    tx: Option<oneshot::Sender<()>>,
    tails: Arc<Mutex<HashMap<String, (u64, Tail)>>>,
    key: String,
    id: u64,
}

impl Drop for Release {
    fn drop(&mut self) {
        let mut tails = self.tails.lock().unwrap_or_else(|e| e.into_inner());
        if tails.get(&self.key).is_some_and(|(id, _)| *id == self.id) {
            tails.remove(&self.key);
        }
        drop(tails);
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
    }
}

/// `createKeyedMutex()`.
pub fn create_keyed_mutex() -> KeyedMutex {
    KeyedMutex::new()
}

struct SemState {
    active: usize,
    queue: VecDeque<oneshot::Sender<()>>,
}

/// A counting semaphore with a FIFO queue (`createSemaphore`).
#[derive(Clone)]
pub struct Semaphore {
    limit: usize,
    state: Arc<Mutex<SemState>>,
}

struct Permit {
    state: Arc<Mutex<SemState>>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        release(&self.state);
    }
}

fn release(state: &Arc<Mutex<SemState>>) {
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    s.active = s.active.saturating_sub(1);
    while let Some(next) = s.queue.pop_front() {
        s.active += 1;
        if next.send(()).is_ok() {
            break;
        }
        // That caller went away before its turn.
        s.active -= 1;
    }
}

impl Semaphore {
    pub fn new(limit: usize) -> Self {
        Self { limit, state: Arc::new(Mutex::new(SemState { active: 0, queue: VecDeque::new() })) }
    }

    /// Runs `f` once fewer than `limit` jobs are active. The caller takes its place in the queue when `run` is
    /// called.
    pub fn run<T, F, Fut>(&self, f: F) -> impl Future<Output = T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let wait = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.active < self.limit {
                s.active += 1;
                None
            } else {
                let (tx, rx) = oneshot::channel();
                s.queue.push_back(tx);
                Some(rx)
            }
        };
        let state = self.state.clone();
        async move {
            if let Some(rx) = wait {
                let _ = rx.await;
            }
            let _permit = Permit { state };
            f().await
        }
    }

    /// Jobs running now.
    pub fn active(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active
    }

    /// Jobs waiting.
    pub fn queued(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).queue.len()
    }
}

/// `createSemaphore(limit)`.
pub fn create_semaphore(limit: usize) -> Semaphore {
    Semaphore::new(limit)
}

/// The time source of a [`Throttle`]: epoch (or any monotonic) milliseconds.
pub type NowMs = Arc<dyn Fn() -> i64 + Send + Sync>;

/// At most one call per interval per key (`createThrottle`); the map is swept lazily. By default it runs on
/// `tokio::time::Instant` (so paused tokio time drives it); [`Throttle::with_clock`] runs it on another clock (the
/// app clock, as the JS `Date.now()`), and [`Throttle::allow_at`] takes the time explicitly.
pub struct Throttle {
    interval_ms: i64,
    now: Option<NowMs>,
    base: tokio::time::Instant,
    state: Mutex<(HashMap<String, i64>, i64)>,
}

impl Throttle {
    pub fn new(interval_ms: u64) -> Self {
        let base = tokio::time::Instant::now();
        Self { interval_ms: interval_ms.min(i64::MAX as u64) as i64, now: None, base, state: Mutex::new((HashMap::new(), 0)) }
    }

    /// A throttle on `now` (e.g. `move || clock.now_ms()` with the app clock).
    pub fn with_clock(interval_ms: u64, now: NowMs) -> Self {
        let start = now();
        Self { interval_ms: interval_ms.min(i64::MAX as u64) as i64, now: Some(now), base: tokio::time::Instant::now(), state: Mutex::new((HashMap::new(), start)) }
    }

    fn now_ms(&self) -> i64 {
        match &self.now {
            Some(f) => f(),
            None => self.base.elapsed().as_millis().min(i64::MAX as u128) as i64,
        }
    }

    /// True when a call for `key` is allowed now (and records it).
    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, self.now_ms())
    }

    /// [`Self::allow`] at time `now_ms` (same clock as the throttle's).
    pub fn allow_at(&self, key: &str, now_ms: i64) -> bool {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (last, last_sweep) = &mut *g;
        let interval = self.interval_ms;
        if now_ms.saturating_sub(*last_sweep) > interval {
            last.retain(|_, t| now_ms.saturating_sub(*t) < interval);
            *last_sweep = now_ms;
        }
        if let Some(prev) = last.get(key)
            && now_ms.saturating_sub(*prev) < interval
        {
            return false;
        }
        last.insert(key.to_string(), now_ms);
        true
    }
}

/// `createThrottle(intervalMs)`.
pub fn create_throttle(interval_ms: u64) -> Throttle {
    Throttle::new(interval_ms)
}
