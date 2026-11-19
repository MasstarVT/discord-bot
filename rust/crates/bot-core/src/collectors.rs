//! Collector-style flows (`awaitMessageComponent`, `createMessageComponentCollector`, the paginator, confirm prompts).
//!
//! In the JS a collector listens to every interaction while the router, finding no registered handler for a
//! `collector` prefix (`paginator`, `confirm`, `rps`, `ttt`, `bj`, ...), waits 2.5 s and answers "This menu has
//! expired." unless something acknowledged the click. Here the router hands every click on a collector prefix to the
//! app's [`CollectorHub`]; a live [`Collector`] whose filter matches receives the same [`ComponentCtx`] (sharing the
//! acknowledgement state), and the router's 2.5 s check sees whether the collector answered.
//!
//! Never use serenity's own collectors: the router cannot see them.
//!
//! ```ignore
//! let mut collector = app.collectors().collect(CollectorFilter::message(msg.id).prefixes(["confirm"]));
//! while let Some(click) = collector.next(Duration::from_secs(60)).await {
//!     click.defer_update().await.ok(); // collectors acknowledge first
//!     ...
//! }
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serenity::all::{ComponentInteraction, MessageId, UserId};
use tokio::sync::mpsc;

use crate::interaction::ComponentCtx;

/// A collector's extra check on a click.
pub type ClickPredicate = Arc<dyn Fn(&ComponentInteraction) -> bool + Send + Sync>;

/// Which clicks a collector receives. Every set field must match.
#[derive(Clone, Default)]
pub struct CollectorFilter {
    /// Only clicks on this message (`message.createMessageComponentCollector`).
    pub message_id: Option<MessageId>,
    /// Only these customId prefixes (empty: any collector prefix).
    pub prefixes: Vec<String>,
    /// Only clicks by this user (a filter that ignores other users' clicks; they then get "This menu has expired.").
    pub user_id: Option<UserId>,
    /// Any further check (the JS `filter`); a click it rejects is ignored.
    pub predicate: Option<ClickPredicate>,
}

impl std::fmt::Debug for CollectorFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CollectorFilter")
            .field("message_id", &self.message_id)
            .field("prefixes", &self.prefixes)
            .field("user_id", &self.user_id)
            .field("predicate", &self.predicate.is_some())
            .finish()
    }
}

impl CollectorFilter {
    /// Clicks on one message.
    pub fn message(id: MessageId) -> Self {
        Self { message_id: Some(id), ..Default::default() }
    }

    /// Restricts the prefixes.
    pub fn prefixes<S: Into<String>>(mut self, prefixes: impl IntoIterator<Item = S>) -> Self {
        self.prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }

    /// Restricts the user.
    pub fn user(mut self, id: UserId) -> Self {
        self.user_id = Some(id);
        self
    }

    /// Adds a predicate.
    pub fn filter(mut self, f: impl Fn(&ComponentInteraction) -> bool + Send + Sync + 'static) -> Self {
        self.predicate = Some(Arc::new(f));
        self
    }

    /// Whether a click matches.
    pub fn matches(&self, i: &ComponentInteraction) -> bool {
        if self.message_id.is_some_and(|m| m != i.message.id) {
            return false;
        }
        if !self.prefixes.is_empty() {
            let prefix = bot_config::parse_custom_id(&i.data.custom_id).prefix;
            if !self.prefixes.iter().any(|p| p == prefix) {
                return false;
            }
        }
        if self.user_id.is_some_and(|u| u != i.user.id) {
            return false;
        }
        self.predicate.as_ref().is_none_or(|p| p(i))
    }
}

struct Waiter {
    id: u64,
    filter: CollectorFilter,
    tx: mpsc::UnboundedSender<ComponentCtx>,
}

/// The app's live collectors.
#[derive(Default)]
pub struct CollectorHub {
    waiters: Arc<Mutex<Vec<Waiter>>>,
    next_id: AtomicU64,
}

impl CollectorHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts collecting. The collector stops when dropped.
    pub fn collect(&self, filter: CollectorFilter) -> Collector {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.waiters.lock().unwrap_or_else(|e| e.into_inner()).push(Waiter { id, filter, tx });
        Collector { id, rx, hub: self.waiters.clone() }
    }

    /// `awaitMessageComponent({ filter, time })`: the first matching click within `timeout`, or `None`.
    pub async fn await_component(&self, filter: CollectorFilter, timeout: Duration) -> Option<ComponentCtx> {
        let mut c = self.collect(filter);
        c.next(timeout).await
    }

    /// Hands a click to the first live collector whose filter matches (the router calls this). `true` when one took
    /// it.
    pub fn offer(&self, ctx: ComponentCtx) -> bool {
        let mut waiters = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        waiters.retain(|w| !w.tx.is_closed());
        for w in waiters.iter() {
            if w.filter.matches(&ctx.interaction) && w.tx.send(ctx.clone()).is_ok() {
                return true;
            }
        }
        false
    }

    /// Live collectors (tests).
    pub fn len(&self) -> usize {
        let mut waiters = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        waiters.retain(|w| !w.tx.is_closed());
        waiters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A live collector (`InteractionCollector`).
pub struct Collector {
    id: u64,
    rx: mpsc::UnboundedReceiver<ComponentCtx>,
    hub: Arc<Mutex<Vec<Waiter>>>,
}

impl Collector {
    /// The next matching click within `timeout` (the collector's `idle` / `time`), or `None` when it elapsed.
    pub async fn next(&mut self, timeout: Duration) -> Option<ComponentCtx> {
        tokio::time::timeout(timeout, self.rx.recv()).await.ok().flatten()
    }

    /// Stops collecting (also done on drop).
    pub fn stop(&mut self) {
        self.rx.close();
        let id = self.id;
        self.hub.lock().unwrap_or_else(|e| e.into_inner()).retain(|w| w.id != id);
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop();
    }
}
