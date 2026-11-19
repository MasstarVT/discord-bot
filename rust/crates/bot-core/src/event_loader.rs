//! Port of `src/handlers/eventLoader.js`: the boundary around every gateway event handler.
//!
//! The JS wraps each event file's `execute` so a failing handler (Redis or Postgres briefly unreachable) is logged
//! instead of crashing the shard. Part B's serenity `EventHandler` methods run their body through [`run_event`], which
//! does the same for errors and panics.
//!
//! Serenity spawns a task per gateway event, so events can run concurrently and out of gateway order. Where the JS
//! relies on gateway order (`guildMemberAdd` and the invites hook: "nothing awaited before runHooks"), part B must
//! dispatch those events through an ordered queue per guild (for example `bot_util::async_util::KeyedMutex` keyed by
//! the guild id) and call [`crate::App::run_hooks`] without awaiting anything first.

use std::future::Future;

use crate::error::{BotResult, guard, log_arg};

/// Runs one event handler body, logging `Event handler "<name>" failed` on an error or panic. Never fails.
pub async fn run_event<F>(name: &str, body: F)
where
    F: Future<Output = BotResult<()>>,
{
    if let Err(e) = guard(body).await {
        bot_util::log_error!(format!("Event handler \"{name}\" failed"), log_arg(&e));
    }
}

/// `Registered N event listener(s).`: part B logs this once its handler is installed.
pub fn log_registered(count: usize) {
    bot_util::log_success!(format!("Registered {count} event listener(s)."));
}
