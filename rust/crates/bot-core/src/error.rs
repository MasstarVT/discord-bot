//! Error conventions of the framework.
//!
//! Every handler a feature writes (command, autocomplete, component, hook, job, reward provider) returns
//! [`BotResult`], an `anyhow::Result`. The boundary that calls it catches the error (and a panic, see [`guard`]),
//! logs it the way the JS does and carries on: nothing a handler does can take the process down.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;

/// The result type of every feature handler.
pub type BotResult<T = ()> = anyhow::Result<T>;

/// The boxed `'static` future every stored handler returns.
pub type BoxFuture<T> = futures::future::BoxFuture<'static, T>;

/// Throw this from a job handler to fail the job without retries (`PermanentJobError` in scheduler.js).
///
/// ```ignore
/// return Err(PermanentJobError::new("the giveaway is gone").into());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PermanentJobError {
    pub message: String,
}

impl PermanentJobError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    /// `true` when `err` (or anything in its chain) is a [`PermanentJobError`] (`err instanceof PermanentJobError`).
    pub fn is(err: &anyhow::Error) -> bool {
        err.chain().any(|e| e.downcast_ref::<PermanentJobError>().is_some())
    }
}

/// A panic inside a handler, turned into an error so the boundary can log it like a thrown exception.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct HandlerPanic(pub String);

/// The message of a caught panic payload.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "handler panicked".to_string()
    }
}

/// Runs `fut`, turning a panic into an `Err(HandlerPanic)` (a thrown exception in the JS). Every handler boundary in
/// bot-core wraps the feature's future with this before awaiting or spawning it.
pub async fn guard<T, F>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(payload) => Err(HandlerPanic(panic_message(payload.as_ref())).into()),
    }
}

/// Calls a stored handler (`f` builds its future) and guards both the call and the future: a panic while building the
/// future (a closure that unwraps before its `async` block) is caught like one inside it. The call happens now, so
/// whatever the handler does synchronously runs at this point (the hook runner's ordering guarantee relies on it).
pub fn start_guarded<T: Send + 'static>(f: impl FnOnce() -> BoxFuture<anyhow::Result<T>>) -> BoxFuture<anyhow::Result<T>> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(fut) => Box::pin(guard(fut)),
        Err(payload) => {
            let err: anyhow::Error = HandlerPanic(panic_message(payload.as_ref())).into();
            Box::pin(futures::future::ready(Err(err)))
        }
    }
}

/// Runs a synchronous closure, turning a panic into `None`.
pub fn guard_sync<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(|p| panic_message(p.as_ref()))
}

/// The JS `err?.message ?? err` of an error: its own message, without the chain.
pub fn error_message(err: &anyhow::Error) -> String {
    err.to_string()
}

/// A log argument for an `anyhow::Error`: the message inline and the cause chain as the "stack".
pub fn log_arg(err: &anyhow::Error) -> bot_util::logger::LogArg {
    let mut stack = format!("Error: {err}");
    for cause in err.chain().skip(1) {
        stack.push_str(&format!("\n    caused by: {cause}"));
    }
    bot_util::logger::LogArg::Error { message: err.to_string(), stack: Some(stack) }
}
