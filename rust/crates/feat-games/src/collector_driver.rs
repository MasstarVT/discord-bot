//! The discord.js `createMessageComponentCollector({ filter, idle, time })` semantics the games rely on, on top of
//! bot-core's [`CollectorHub`](bot_core::collectors::CollectorHub).
//!
//! * Every click that passes the filter is "collected": it resets the `idle` timer (strangers included, as in the
//!   JS, whose filters check only the customId) and its handler is spawned at once, so the handlers of two
//!   overlapping clicks run concurrently and decide with their own synchronous flags (`settled`, `busy`, `played`).
//! * [`Stopper::stop`] ends the collector at once with a reason (`collector.stop(reason)`); the first reason wins.
//! * When the collector ends (stopped, `idle` or `time`), the clicks already queued for it (in flight when it stopped)
//!   are still handed to the handler, then `on_end(reason, collected)` runs, like the JS `end` event.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bot_core::App;
use bot_core::collectors::CollectorFilter;
use bot_core::interaction::ComponentCtx;
use tokio::sync::Notify;
use tokio::time::Instant;

/// `collector.stop(reason)` for a running [`drive`].
#[derive(Clone, Default)]
pub struct Stopper {
    inner: Arc<StopInner>,
}

#[derive(Default)]
struct StopInner {
    reason: Mutex<Option<String>>,
    notify: Notify,
}

impl Stopper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ends the collector (only the first call counts).
    pub fn stop(&self, reason: &str) {
        let mut r = self.inner.reason.lock().unwrap_or_else(|e| e.into_inner());
        if r.is_none() {
            *r = Some(reason.to_string());
            self.inner.notify.notify_one();
        }
    }

    /// `collector.ended`.
    pub fn ended(&self) -> bool {
        self.reason().is_some()
    }

    /// The end reason, once ended.
    pub fn reason(&self) -> Option<String> {
        self.inner.reason.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The collector's timers.
#[derive(Debug, Clone, Copy)]
pub struct Timers {
    /// `idle`: ends after this long without a collected click.
    pub idle: Option<Duration>,
    /// `time`: ends after this long in total.
    pub time: Duration,
}

/// Starts a collector now (synchronously, so no click can be missed) and drives it in a background task.
pub fn drive<C, CF, E, EF>(app: &App, filter: CollectorFilter, timers: Timers, stopper: Stopper, on_collect: C, on_end: E) -> tokio::task::JoinHandle<()>
where
    C: Fn(ComponentCtx) -> CF + Send + Sync + 'static,
    CF: Future<Output = ()> + Send + 'static,
    E: FnOnce(String, usize) -> EF + Send + 'static,
    EF: Future<Output = ()> + Send + 'static,
{
    let mut collector = app.collectors().collect(filter);
    tokio::spawn(async move {
        let start = Instant::now();
        let total = start + timers.time;
        let mut idle_deadline = timers.idle.map(|d| start + d);
        let mut collected = 0usize;
        let reason = loop {
            let deadline = idle_deadline.map_or(total, |d| d.min(total));
            let left = deadline.saturating_duration_since(Instant::now());
            tokio::select! {
                biased;
                _ = stopper.inner.notify.notified() => {
                    break stopper.reason().unwrap_or_else(|| "user".into());
                }
                click = collector.next(left) => match click {
                    Some(click) => {
                        collected += 1;
                        if let Some(d) = timers.idle {
                            idle_deadline = Some(Instant::now() + d);
                        }
                        tokio::spawn(on_collect(click));
                    }
                    None => {
                        let why = if Instant::now() >= total { "time" } else { "idle" };
                        stopper.stop(why);
                        break stopper.reason().unwrap_or_else(|| why.into());
                    }
                },
            }
        };
        // Clicks already queued when the collector ended were in flight: their handlers still run (and see the flags).
        while let Some(click) = collector.next(Duration::ZERO).await {
            tokio::spawn(on_collect(click));
        }
        collector.stop();
        on_end(reason, collected).await;
    })
}

/// A customId filter: the prefix and the first parameter (the game's interaction id), as the JS
/// `i.customId.startsWith('<prefix>:<id>:')` does.
pub fn id_filter(prefix: &'static str, id: String) -> CollectorFilter {
    let start = format!("{prefix}:{id}:");
    CollectorFilter::default().prefixes([prefix]).filter(move |i| i.data.custom_id.starts_with(&start))
}
