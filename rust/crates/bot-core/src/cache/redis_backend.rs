//! The Redis backend: the redis crate with the JS client's options.
//!
//! ioredis was created with `enableOfflineQueue: false, maxRetriesPerRequest: 1, commandTimeout: 750,
//! connectTimeout: 5000, retryStrategy: min(times * 200, 5000), lazyConnect: false`. Here:
//! * construction never fails and never waits: a background task connects (5 s timeout) and reconnects with the same
//!   back-off, logging `Redis connected`, `Redis ready`, `Redis error` (at most once per 30 s), `Redis connection
//!   closed` and `Redis reconnecting in Nms` like the JS event handlers;
//! * while there is no connection every command fails at once (no offline queue), so the cache falls back;
//! * every command is bounded by 750 ms (`Command timed out`);
//! * a command that fails because the connection dropped marks the backend not ready and wakes the reconnect loop.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, RedisError};
use tokio::sync::Notify;

use super::{COMMAND_TIMEOUT_MS, CacheBackend, CacheError, LOG_INTERVAL_MS};
use super::memory::UNAVAILABLE;

/// Connection options (the JS values by default).
#[derive(Debug, Clone)]
pub struct RedisOptions {
    pub url: String,
    pub command_timeout_ms: u64,
    pub connect_timeout_ms: u64,
}

impl RedisOptions {
    /// `REDIS_URL` with the JS defaults (`redis://localhost:6379` when empty).
    pub fn new(url: Option<&str>) -> Self {
        let url = url.filter(|u| !u.is_empty()).unwrap_or("redis://localhost:6379").to_string();
        Self { url, command_timeout_ms: COMMAND_TIMEOUT_MS, connect_timeout_ms: 5_000 }
    }
}

/// `retryStrategy(times)`: `min(times * 200, 5000)` ms.
pub fn retry_delay_ms(times: u32) -> u64 {
    (u64::from(times) * 200).min(5_000)
}

struct Inner {
    client: redis::Client,
    options: RedisOptions,
    conn: RwLock<Option<MultiplexedConnection>>,
    ready: AtomicBool,
    dropped: Notify,
    stopped: AtomicBool,
    stop: Notify,
    last_error_log_at: AtomicI64,
}

/// The Redis backend. Cheap to clone.
#[derive(Clone)]
pub struct RedisBackend {
    inner: Arc<Inner>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl RedisBackend {
    /// Parses the URL and starts the connection task (call inside a tokio runtime). Fails only on a URL the redis
    /// crate cannot parse.
    pub fn connect(options: RedisOptions) -> Result<Self, CacheError> {
        let client = redis::Client::open(options.url.as_str()).map_err(|e| CacheError::new(e.to_string()))?;
        let inner = Arc::new(Inner {
            client,
            options,
            conn: RwLock::new(None),
            ready: AtomicBool::new(false),
            dropped: Notify::new(),
            stopped: AtomicBool::new(false),
            stop: Notify::new(),
            last_error_log_at: AtomicI64::new(i64::MIN / 2),
        });
        tokio::spawn(supervise(inner.clone()));
        Ok(Self { inner })
    }

    /// Closes the connection and stops reconnecting (`redis.disconnect()` / `quit()` at shutdown).
    pub fn disconnect(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        self.inner.ready.store(false, Ordering::SeqCst);
        *self.inner.conn.write().unwrap_or_else(|e| e.into_inner()) = None;
        self.inner.stop.notify_waiters();
        self.inner.dropped.notify_waiters();
    }

    /// Waits until the connection is ready or `timeout_ms` passes (startup and tests); `true` when ready.
    pub async fn wait_ready(&self, timeout_ms: u64) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            if self.is_ready() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.is_ready()
    }

    fn connection(&self) -> Result<MultiplexedConnection, CacheError> {
        if !self.inner.ready.load(Ordering::SeqCst) {
            return Err(CacheError::new(UNAVAILABLE));
        }
        self.inner.conn.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| CacheError::new(UNAVAILABLE))
    }

    async fn run<T, F, Fut>(&self, f: F) -> Result<T, CacheError>
    where
        F: FnOnce(MultiplexedConnection) -> Fut,
        Fut: Future<Output = Result<T, RedisError>>,
    {
        let conn = self.connection()?;
        match tokio::time::timeout(Duration::from_millis(self.inner.options.command_timeout_ms), f(conn)).await {
            Err(_) => Err(CacheError::new("Command timed out")),
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                if e.is_connection_dropped() || e.is_io_error() || e.is_connection_refusal() || e.is_unrecoverable_error() {
                    mark_down(&self.inner);
                }
                Err(CacheError::new(e.to_string()))
            }
        }
    }
}

fn mark_down(inner: &Inner) {
    if inner.ready.swap(false, Ordering::SeqCst) {
        *inner.conn.write().unwrap_or_else(|e| e.into_inner()) = None;
        inner.dropped.notify_one();
    }
}

fn log_error(inner: &Inner, err: &str) {
    let now = now_ms();
    let last = inner.last_error_log_at.load(Ordering::Relaxed);
    if now - last < LOG_INTERVAL_MS {
        return;
    }
    inner.last_error_log_at.store(now, Ordering::Relaxed);
    bot_util::log_error!("Redis error", bot_util::logger::LogArg::Error { message: err.to_string(), stack: None });
}

async fn supervise(inner: Arc<Inner>) {
    let mut times: u32 = 0;
    loop {
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(Duration::from_millis(inner.options.connect_timeout_ms)))
            .set_response_timeout(Some(Duration::from_millis(inner.options.command_timeout_ms)));
        let attempt = tokio::time::timeout(
            Duration::from_millis(inner.options.connect_timeout_ms),
            inner.client.get_multiplexed_async_connection_with_config(&config),
        )
        .await;
        match attempt {
            Ok(Ok(mut conn)) => {
                bot_util::log_info!("Redis connected");
                // The ready check: the connection answers a command.
                let ping: Result<String, RedisError> = redis::cmd("PING").query_async(&mut conn).await;
                match ping {
                    Ok(_) => {
                        times = 0;
                        *inner.conn.write().unwrap_or_else(|e| e.into_inner()) = Some(conn);
                        inner.ready.store(true, Ordering::SeqCst);
                        bot_util::log_success!("Redis ready");
                        tokio::select! {
                            _ = inner.dropped.notified() => {}
                            _ = inner.stop.notified() => {}
                        }
                        if inner.stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        inner.ready.store(false, Ordering::SeqCst);
                        *inner.conn.write().unwrap_or_else(|e| e.into_inner()) = None;
                        bot_util::log_warn!("Redis connection closed");
                    }
                    Err(e) => log_error(&inner, &e.to_string()),
                }
            }
            Ok(Err(e)) => log_error(&inner, &e.to_string()),
            Err(_) => log_error(&inner, "connect ETIMEDOUT"),
        }
        if inner.stopped.load(Ordering::SeqCst) {
            return;
        }
        times = times.saturating_add(1);
        let delay = retry_delay_ms(times);
        bot_util::log_info!(format!("Redis reconnecting in {delay}ms"));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {}
            _ = inner.stop.notified() => return,
        }
    }
}

impl RedisBackend {
    /// `client.status === 'ready'`: connected (false after [`RedisBackend::disconnect`] or while reconnecting).
    pub fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CacheBackend for RedisBackend {
    fn is_ready(&self) -> bool {
        self.inner.ready.load(Ordering::SeqCst)
    }

    async fn get(&self, key: &str) -> Result<Option<String>, CacheError> {
        self.run(|mut c| async move { redis::cmd("GET").arg(key).query_async::<Option<String>>(&mut c).await }).await
    }

    async fn set(&self, key: &str, value: &str, ex_secs: Option<u64>, nx: bool) -> Result<bool, CacheError> {
        self.run(|mut c| async move {
            let mut cmd = redis::cmd("SET");
            cmd.arg(key).arg(value);
            if let Some(ex) = ex_secs {
                cmd.arg("EX").arg(ex);
            }
            if nx {
                cmd.arg("NX");
            }
            let reply: Option<String> = cmd.query_async(&mut c).await?;
            Ok(reply.as_deref() == Some("OK"))
        })
        .await
    }

    async fn del(&self, keys: &[String]) -> Result<u64, CacheError> {
        self.run(|mut c| async move { redis::cmd("DEL").arg(keys).query_async::<u64>(&mut c).await }).await
    }

    async fn incr(&self, key: &str) -> Result<i64, CacheError> {
        self.run(|mut c| async move { redis::cmd("INCR").arg(key).query_async::<i64>(&mut c).await }).await
    }

    async fn expire(&self, key: &str, secs: u64) -> Result<bool, CacheError> {
        self.run(|mut c| async move { redis::cmd("EXPIRE").arg(key).arg(secs).query_async::<i64>(&mut c).await.map(|n| n == 1) }).await
    }

    async fn sliding_window(&self, key: &str, now_ms: i64, window_ms: i64, member: &str) -> Result<i64, CacheError> {
        self.run(|mut c| async move {
            let (count,): (i64,) = redis::pipe()
                .atomic()
                .cmd("ZADD").arg(key).arg(now_ms).arg(member).ignore()
                .cmd("ZREMRANGEBYSCORE").arg(key).arg(0).arg(now_ms - window_ms).ignore()
                .cmd("ZCARD").arg(key)
                .cmd("PEXPIRE").arg(key).arg(window_ms).ignore()
                .query_async(&mut c)
                .await?;
            Ok(count)
        })
        .await
    }

    async fn push_recent(&self, key: &str, value: &str, max: i64, ttl_secs: u64) -> Result<Vec<String>, CacheError> {
        self.run(|mut c| async move {
            let (list,): (Vec<String>,) = redis::pipe()
                .atomic()
                .cmd("LPUSH").arg(key).arg(value).ignore()
                .cmd("LTRIM").arg(key).arg(0).arg(max - 1).ignore()
                .cmd("EXPIRE").arg(key).arg(ttl_secs).ignore()
                .cmd("LRANGE").arg(key).arg(0).arg(max - 1)
                .query_async(&mut c)
                .await?;
            Ok(list)
        })
        .await
    }

    async fn hget(&self, hash: &str, field: &str) -> Result<Option<String>, CacheError> {
        self.run(|mut c| async move { redis::cmd("HGET").arg(hash).arg(field).query_async::<Option<String>>(&mut c).await }).await
    }

    async fn hset(&self, hash: &str, field: &str, value: &str) -> Result<(), CacheError> {
        self.run(|mut c| async move { redis::cmd("HSET").arg(hash).arg(field).arg(value).query_async::<i64>(&mut c).await.map(|_| ()) }).await
    }

    async fn hdel(&self, hash: &str, field: &str) -> Result<(), CacheError> {
        self.run(|mut c| async move { redis::cmd("HDEL").arg(hash).arg(field).query_async::<i64>(&mut c).await.map(|_| ()) }).await
    }

    async fn scan(&self, cursor: &str, pattern: &str, count: usize) -> Result<(String, Vec<String>), CacheError> {
        self.run(|mut c| async move {
            redis::cmd("SCAN").arg(cursor).arg("MATCH").arg(pattern).arg("COUNT").arg(count).query_async::<(String, Vec<String>)>(&mut c).await
        })
        .await
    }

    async fn unlink(&self, keys: &[String]) -> Result<u64, CacheError> {
        self.run(|mut c| async move { redis::cmd("UNLINK").arg(keys).query_async::<u64>(&mut c).await }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_strategy_matches_the_js() {
        assert_eq!(retry_delay_ms(1), 200);
        assert_eq!(retry_delay_ms(10), 2000);
        assert_eq!(retry_delay_ms(25), 5000);
        assert_eq!(retry_delay_ms(1000), 5000);
    }
}
