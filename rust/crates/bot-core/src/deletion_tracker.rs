//! Port of `src/utils/deletionTracker.js`: which message (or channel) deletions the bot caused, so log hooks can tell
//! them apart from deletions by members. In memory, per app (the JS keeps it per shard). Never fails.

use std::sync::Mutex;

use indexmap::IndexMap;

use crate::context::App;

const MAX_ENTRIES: usize = 10_000;
const SWEEP_AFTER_SIZE: usize = 1_000;
const SWEEP_INTERVAL_MS: i64 = 60_000;
/// The default lifetime of a mark.
pub const DEFAULT_TTL_MS: f64 = 60_000.0;
const MAX_TTL_MS: f64 = 3_600_000.0;

#[derive(Default)]
struct Store {
    /// Insertion order = age order.
    entries: IndexMap<String, (String, i64)>,
    last_sweep: i64,
}

/// The tracker (one per app: `app.ext::<DeletionTracker>()`, or the free functions below).
#[derive(Default)]
pub struct DeletionTracker(Mutex<Store>);

impl DeletionTracker {
    /// `markBotDeletion(ids, reason, ttlMs)`: marks ids as deleted by the bot. `reason` defaults to `'bot'`; the
    /// TTL is clamped to 1 ms .. 1 h (60 s when not a number).
    pub fn mark<S: AsRef<str>>(&self, now_ms: i64, ids: &[S], reason: Option<&str>, ttl_ms: Option<f64>) {
        let why = reason.unwrap_or("bot").to_string();
        let mut ttl = ttl_ms.unwrap_or(DEFAULT_TTL_MS).floor();
        if !ttl.is_finite() {
            ttl = DEFAULT_TTL_MS;
        }
        let ttl = ttl.clamp(1.0, MAX_TTL_MS) as i64;
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for id in ids {
            let id = id.as_ref();
            if id.is_empty() {
                continue;
            }
            s.entries.shift_remove(id); // refresh the age order
            s.entries.insert(id.to_string(), (why.clone(), now_ms + ttl));
        }
        while s.entries.len() > MAX_ENTRIES {
            s.entries.shift_remove_index(0);
        }
        if s.entries.len() > SWEEP_AFTER_SIZE && now_ms - s.last_sweep > SWEEP_INTERVAL_MS {
            s.last_sweep = now_ms;
            s.entries.retain(|_, (_, exp)| *exp > now_ms);
        }
    }

    /// `wasDeletedByBot(id)`: the reason when the bot deleted it recently. Non-destructive (several log hooks may
    /// read one mark); an expired mark is removed.
    pub fn was_deleted(&self, now_ms: i64, id: &str) -> Option<String> {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (reason, expires) = s.entries.get(id).cloned()?;
        if expires <= now_ms {
            s.entries.shift_remove(id);
            return None;
        }
        Some(reason)
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `markBotDeletion(ids, reason, ttlMs)` on the app's tracker. Call it before the bot deletes.
pub fn mark_bot_deletion<S: AsRef<str>>(app: &App, ids: &[S], reason: Option<&str>, ttl_ms: Option<f64>) {
    app.ext::<DeletionTracker>().mark(app.clock().now_ms(), ids, reason, ttl_ms);
}

/// `wasDeletedByBot(id)` on the app's tracker.
pub fn was_deleted_by_bot(app: &App, id: &str) -> Option<String> {
    app.ext::<DeletionTracker>().was_deleted(app.clock().now_ms(), id)
}
