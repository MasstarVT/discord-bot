//! `createKeyedMutex()` for handlers: per-key serialisation whose job is queued when [`KeyedMutex::run`] is called and
//! whose future is `'static` (it borrows neither the mutex nor the key), so a hook can take its place in the queue
//! synchronously and return the future. That is how a hook keeps gateway order (`runLocked(guildId, ...)` in the JS
//! counting plugin). Keep one per app in a feature state struct (`app.ext::<T>()`).

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::Shared;
use tokio::sync::oneshot;

type Tail = Shared<oneshot::Receiver<()>>;

/// Serialises work per key; the key's entry is removed when it is idle. A failing (or panicking) job releases the
/// key, and its failure reaches only its own caller.
#[derive(Clone, Default)]
pub struct KeyedMutex {
    tails: Arc<Mutex<HashMap<String, (u64, Tail)>>>,
    next_id: Arc<AtomicU64>,
}

impl KeyedMutex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs `f` after every earlier job of `key`. The job is queued and spawned now; await the returned future for
    /// its result.
    pub fn run<T, F, Fut>(&self, key: &str, f: F) -> impl Future<Output = T> + Send + 'static + use<T, F, Fut>
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
        let release = Release { tx: Some(tx), tails: self.tails.clone(), key: key.to_string(), id };
        let handle = tokio::spawn(async move {
            if let Some(prev) = prev {
                let _ = prev.await;
            }
            let _release = release;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn jobs_of_one_key_run_in_call_order() {
        let m = KeyedMutex::new();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut futs = Vec::new();
        for (i, delay) in [30u64, 0, 15, 0].into_iter().enumerate() {
            let log = log.clone();
            futs.push(m.run("g", move || async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                log.lock().unwrap().push(i);
                i
            }));
        }
        let out = futures::future::join_all(futs).await;
        assert_eq!(out, vec![0, 1, 2, 3]);
        assert_eq!(*log.lock().unwrap(), vec![0, 1, 2, 3]);
        assert!(m.is_empty());
    }
}
