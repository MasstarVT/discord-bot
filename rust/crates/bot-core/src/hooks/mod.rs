//! Port of `src/handlers/hookRegistry.js`: the hook registry and runner.
//!
//! Plugins register [`Hook`]s; part B's event handlers call [`crate::App::run_hooks`] with a typed payload
//! ([`events`]). [`HookRegistry::run`] never fails. Per hook, in `HOOK_ORDER`:
//! stop when `ctx.stopped`; a sync `filter` returning false (or panicking) skips it; a hook with a module is skipped
//! unless that module is enabled (no settings counts as disabled) unless it is `always`; then `run(ctx)` with a soft
//! timeout (10 s default, or `timeout_ms`). A failing or timed-out hook is logged and the next one still runs; a
//! timed-out hook keeps running in the background (it is abandoned, not cancelled).
//!
//! Ordering guarantee: nothing before the first `run` awaits, so the first hook of a dispatch that passes its filter
//! and is `always`, ungated, or gated on a module the memoised settings (`peek_settings`) show enabled, is started
//! (its `run` is called, then spawned) during the `run` call itself, in the order of the calls. A hook that takes a
//! [`crate::sync::KeyedMutex`] place in its `run` before returning its future therefore keeps the dispatch order.

pub mod events;

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bot_config::{HookPoint, ModuleKey, hook_index};
use serenity::all::GuildId;
use tokio::sync::OnceCell;

use crate::context::App;
use crate::discord::GuildState;
use crate::error::{BotResult, BoxFuture, guard_sync, log_arg, start_guarded};
use crate::settings::{Settings, is_module_enabled};

pub use events::HookEvent;

/// The soft timeout of a hook without `timeout_ms`.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 10_000;
/// How many guildAvailable re-runs of the startup work may run at once.
pub const AVAILABLE_CONCURRENCY: usize = 2;
/// A guild whose startup work ran this recently skips a guildAvailable re-run.
pub const AVAILABLE_COOLDOWN_MS: u64 = 10 * 60_000;

/// The module a hook or component is gated on. JS `module: undefined` inherits the plugin's module, `null` is never
/// gated, a key gates on that module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModuleRef {
    /// Inherit the plugin's module (`module` not declared).
    #[default]
    Inherit,
    /// Never gated (`module: null`).
    Ungated,
    /// Gated on this module.
    Module(ModuleKey),
}

impl ModuleRef {
    /// The effective module given the plugin's.
    pub fn resolve(self, plugin_module: Option<ModuleKey>) -> Option<ModuleKey> {
        match self {
            Self::Inherit => plugin_module,
            Self::Ungated => None,
            Self::Module(m) => Some(m),
        }
    }
}

/// The shared scratch of one dispatch (`ctx.state`; keys in use: `invite`, `captchaPending`, `customCommand`,
/// `automodAction`). Values are typed: read with the type that was stored.
#[derive(Default)]
pub struct HookState(Mutex<HashMap<String, Box<dyn Any + Send + Sync>>>);

impl HookState {
    pub fn insert<T: Any + Send + Sync>(&self, key: &str, value: T) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), Box::new(value));
    }

    /// A clone of the value under `key`, when it holds a `T`.
    pub fn get<T: Any + Clone>(&self, key: &str) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).get(key).and_then(|v| v.downcast_ref::<T>()).cloned()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).contains_key(key)
    }

    pub fn remove(&self, key: &str) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(key).is_some()
    }

    /// `ctx.state.automodAction = { rule, deleted }` (automodService.js). Stored as the JSON object the JS keeps, so
    /// readers that take `get::<serde_json::Value>("automodAction")` keep working.
    pub fn set_automod_action(&self, action: &AutomodAction) {
        self.insert(AUTOMOD_ACTION_KEY, serde_json::json!({ "rule": action.rule, "deleted": action.deleted }));
    }

    /// `ctx.state.automodAction`, stored by [`Self::set_automod_action`], as an [`AutomodAction`], or as a JSON value
    /// with those fields.
    pub fn automod_action(&self) -> Option<AutomodAction> {
        if let Some(a) = self.get::<AutomodAction>(AUTOMOD_ACTION_KEY) {
            return Some(a);
        }
        let v = self.get::<serde_json::Value>(AUTOMOD_ACTION_KEY)?;
        Some(AutomodAction {
            rule: v.get("rule").and_then(serde_json::Value::as_str).unwrap_or_default().to_string(),
            deleted: v.get("deleted").and_then(serde_json::Value::as_bool).unwrap_or(false),
        })
    }
}

/// The [`HookState`] key automod writes after acting on a message.
pub const AUTOMOD_ACTION_KEY: &str = "automodAction";

/// What automod did to a message (`ctx.state.automodAction`): the rule that fired and whether the message was
/// deleted. Later `messageCreate` hooks (translation, automations) skip deleted messages.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct AutomodAction {
    pub rule: String,
    pub deleted: bool,
}

/// The context every hook of one dispatch receives (`createHookContext`). The core keys cannot be overridden.
pub struct HookCtx<E: HookEvent> {
    pub app: App,
    pub event: HookPoint,
    pub guild_id: Option<GuildId>,
    /// The event's fields.
    pub data: E,
    pub state: HookState,
    stopped: AtomicBool,
    stop_reason: Mutex<Option<String>>,
    settings: OnceCell<Option<Settings>>,
}

impl<E: HookEvent> HookCtx<E> {
    pub fn new(app: App, data: E) -> Self {
        Self {
            app,
            event: E::POINT,
            guild_id: data.guild_id(),
            data,
            state: HookState::default(),
            stopped: AtomicBool::new(false),
            stop_reason: Mutex::new(None),
            settings: OnceCell::new(),
        }
    }

    /// `ctx.stop(reason)`: later hooks of this dispatch do not run. Only automod and custom-commands call it.
    pub fn stop(&self, reason: impl Into<String>) {
        self.stopped.store(true, Ordering::SeqCst);
        *self.stop_reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.into());
    }

    /// `ctx.stopped`.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// `ctx.stopReason`.
    pub fn stop_reason(&self) -> Option<String> {
        self.stop_reason.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `ctx.getSettings()`: memoised `getSettingsSafe(guildId)`; `None` without a guild. Never fails.
    pub async fn get_settings(&self) -> Option<Settings> {
        self.settings
            .get_or_init(|| async {
                match self.guild_id {
                    Some(g) => self.app.settings().get_settings_safe(&g.to_string()).await,
                    None => None,
                }
            })
            .await
            .clone()
    }

    /// The guild id as a string (most tables key by it).
    pub fn guild_id_str(&self) -> Option<String> {
        self.guild_id.map(|g| g.to_string())
    }
}

/// A hook's `run(ctx)`.
pub type HookRun<E> = Arc<dyn Fn(Arc<HookCtx<E>>) -> BoxFuture<BotResult<()>> + Send + Sync>;
/// A hook's synchronous `filter(ctx)`.
pub type HookFilter<E> = Arc<dyn Fn(&HookCtx<E>) -> bool + Send + Sync>;

/// A hook declaration: `{ event, id, run, filter?, module?, always?, timeoutMs? }`. The event is the payload type.
pub struct Hook<E: HookEvent> {
    pub id: &'static str,
    run: HookRun<E>,
    filter: Option<HookFilter<E>>,
    pub module: ModuleRef,
    pub always: bool,
    pub timeout_ms: Option<u64>,
}

impl<E: HookEvent> Clone for Hook<E> {
    fn clone(&self) -> Self {
        Self { id: self.id, run: self.run.clone(), filter: self.filter.clone(), module: self.module, always: self.always, timeout_ms: self.timeout_ms }
    }
}

impl<E: HookEvent> Hook<E> {
    /// A hook with id `id` (`<pluginId>` or `<pluginId>:<name>`, listed in `HOOK_ORDER`) running `run`.
    pub fn new<F, Fut>(id: &'static str, run: F) -> Self
    where
        F: Fn(Arc<HookCtx<E>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        Self { id, run: Arc::new(move |ctx| Box::pin(run(ctx))), filter: None, module: ModuleRef::Inherit, always: false, timeout_ms: None }
    }

    /// A cheap synchronous filter (no I/O; use `app.settings().peek_settings` for settings).
    pub fn filter(mut self, f: impl Fn(&HookCtx<E>) -> bool + Send + Sync + 'static) -> Self {
        self.filter = Some(Arc::new(f));
        self
    }

    /// `module: <key>` (or [`ModuleRef::Ungated`] for `module: null`).
    pub fn module(mut self, module: ModuleRef) -> Self {
        self.module = module;
        self
    }

    /// `module: null`: never gated.
    pub fn ungated(self) -> Self {
        self.module(ModuleRef::Ungated)
    }

    /// `always: true`: runs whatever the module toggle says.
    pub fn always(mut self) -> Self {
        self.always = true;
        self
    }

    /// `timeoutMs`.
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = Some(ms);
        self
    }

    pub fn has_filter(&self) -> bool {
        self.filter.is_some()
    }
}

/// A hook of any event, as a plugin holds it.
pub trait AnyHook: Send + Sync {
    fn event(&self) -> HookPoint;
    fn id(&self) -> &'static str;
    fn module(&self) -> ModuleRef;
    fn always(&self) -> bool;
    fn timeout_ms(&self) -> Option<u64>;
    fn has_filter(&self) -> bool;
    /// Registers the hook (`registerHook(hook, { pluginId, pluginModule })`).
    fn register(&self, registry: &HookRegistry, plugin_id: &str, plugin_module: Option<ModuleKey>) -> Result<(), String>;
    /// A copy (the handlers are shared).
    fn clone_box(&self) -> Box<dyn AnyHook>;
}

impl<E: HookEvent> AnyHook for Hook<E> {
    fn event(&self) -> HookPoint {
        E::POINT
    }
    fn id(&self) -> &'static str {
        self.id
    }
    fn module(&self) -> ModuleRef {
        self.module
    }
    fn always(&self) -> bool {
        self.always
    }
    fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }
    fn has_filter(&self) -> bool {
        self.filter.is_some()
    }
    fn register(&self, registry: &HookRegistry, plugin_id: &str, plugin_module: Option<ModuleKey>) -> Result<(), String> {
        registry.register_hook(self.clone(), plugin_id, plugin_module)
    }
    fn clone_box(&self) -> Box<dyn AnyHook> {
        Box::new(self.clone())
    }
}

/// `listHooks(event)` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInfo {
    pub id: String,
    pub plugin_id: String,
    /// The effective module (`None`: never gated).
    pub module: Option<ModuleKey>,
    pub always: bool,
}

struct Registered<E: HookEvent> {
    hook: Hook<E>,
    module: Option<ModuleKey>,
}

struct PointHooks {
    /// `Vec<Arc<Registered<E>>>` for the point's payload type.
    typed: Box<dyn Any + Send + Sync>,
    info: Vec<HookInfo>,
}

/// The hook registry. One per app.
#[derive(Default)]
pub struct HookRegistry {
    points: RwLock<HashMap<HookPoint, PointHooks>>,
}

/// `orderOf(event, id)`: a hook not in HOOK_ORDER sorts after every listed one.
fn order_of(event: HookPoint, id: &str) -> usize {
    hook_index(event.as_str(), id).unwrap_or(usize::MAX)
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `registerHook(hook, { pluginId, pluginModule })`. The effective module is the hook's own when declared,
    /// otherwise the plugin's. A second hook with the same (event, id) is refused and logged.
    pub fn register_hook<E: HookEvent>(&self, hook: Hook<E>, plugin_id: &str, plugin_module: Option<ModuleKey>) -> Result<(), String> {
        let mut points = self.points.write().unwrap_or_else(|e| e.into_inner());
        let entry = points.entry(E::POINT).or_insert_with(|| PointHooks { typed: Box::new(Vec::<Arc<Registered<E>>>::new()), info: Vec::new() });
        if entry.info.iter().any(|h| h.id == hook.id) {
            let msg = format!("Duplicate hook {}:{} ignored", E::POINT, hook.id);
            bot_util::log_error!(msg.clone());
            return Err(msg);
        }
        let Some(list) = entry.typed.downcast_mut::<Vec<Arc<Registered<E>>>>() else {
            return Err(format!("hook {}:{} has the wrong payload type", E::POINT, hook.id));
        };
        let module = hook.module.resolve(plugin_module);
        let info = HookInfo { id: hook.id.to_string(), plugin_id: plugin_id.to_string(), module, always: hook.always };
        list.push(Arc::new(Registered { hook, module }));
        list.sort_by_key(|r| order_of(E::POINT, r.hook.id));
        entry.info.push(info);
        entry.info.sort_by_key(|h| order_of(E::POINT, &h.id));
        Ok(())
    }

    /// Removes every hook (tests).
    pub fn clear(&self) {
        self.points.write().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// `listHooks(event)`: the hooks of an event in execution order.
    pub fn list(&self, event: HookPoint) -> Vec<HookInfo> {
        self.points.read().unwrap_or_else(|e| e.into_inner()).get(&event).map(|p| p.info.clone()).unwrap_or_default()
    }

    fn snapshot<E: HookEvent>(&self) -> Vec<Arc<Registered<E>>> {
        self.points
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&E::POINT)
            .and_then(|p| p.typed.downcast_ref::<Vec<Arc<Registered<E>>>>())
            .cloned()
            .unwrap_or_default()
    }

    /// `runHooks(event, fields)`: runs every hook of the payload's event. Never fails; resolves with the context.
    ///
    /// The synchronous prefix (filters of leading hooks and the start of the first ungated or `always` hook) runs
    /// during this call, before the returned future is polled. Call inside a tokio runtime.
    pub fn run<E: HookEvent>(&self, app: &App, data: E) -> impl Future<Output = Arc<HookCtx<E>>> + Send + 'static {
        let hooks = self.snapshot::<E>();
        let ctx = Arc::new(HookCtx::new(app.clone(), data));
        let event = E::POINT;

        // The synchronous prefix.
        let mut idx = 0;
        let mut filter_passed: Option<usize> = None;
        let mut started: Option<(usize, tokio::task::JoinHandle<BotResult<()>>)> = None;
        while idx < hooks.len() {
            let h = &hooks[idx];
            if !passes_filter(event, h, &ctx) {
                idx += 1;
                continue;
            }
            filter_passed = Some(idx);
            if !h.hook.always && h.module.is_some() {
                // A gated hook needs the settings: decided now when they are memoised, else asynchronously.
                let memo = ctx.guild_id.and_then(|g| app.settings().peek_settings(&g.to_string()));
                match memo {
                    Some(s) if !is_module_enabled(Some(&s), h.module) => {
                        idx += 1;
                        filter_passed = None;
                        continue;
                    }
                    Some(_) => {}
                    None => break, // continue asynchronously
                }
            }
            started = Some((idx, tokio::spawn(start_guarded(|| (h.hook.run)(ctx.clone())))));
            break;
        }

        async move {
            let mut started = started;
            for (i, h) in hooks.iter().enumerate().skip(idx) {
                if ctx.stopped() {
                    break;
                }
                let handle = match started.take() {
                    Some((si, handle)) if si == i => handle,
                    other => {
                        started = other;
                        if filter_passed != Some(i) && !passes_filter(event, h, &ctx) {
                            continue;
                        }
                        if !h.hook.always && h.module.is_some() {
                            let settings = ctx.get_settings().await;
                            if !is_module_enabled(settings.as_deref(), h.module) {
                                continue;
                            }
                        }
                        tokio::spawn(start_guarded(|| (h.hook.run)(ctx.clone())))
                    }
                };
                let timeout = Duration::from_millis(h.hook.timeout_ms.unwrap_or(DEFAULT_HOOK_TIMEOUT_MS));
                match tokio::time::timeout(timeout, handle).await {
                    Err(_) => bot_util::log_warn!(format!("Hook {event}:{} timed out and was abandoned", h.hook.id)),
                    Ok(Ok(Ok(()))) => {}
                    Ok(Ok(Err(e))) => bot_util::log_error!(format!("Hook {event}:{} failed", h.hook.id), log_arg(&e)),
                    Ok(Err(join)) => bot_util::log_error!(format!("Hook {event}:{} failed", h.hook.id), join.to_string()),
                }
            }
            ctx
        }
    }

    /// `runGuildReady(client, guild, reason)`: ensures the guild's settings row, then runs the `guildReady` hooks. An
    /// unavailable (or uncached) guild is skipped. Never fails.
    pub async fn run_guild_ready(&self, app: &App, guild_id: GuildId, reason: events::GuildReadyReason, gate: &ReadyGate) {
        if app.guild_directory().guild_state(&guild_id.to_string()) != GuildState::Available {
            return;
        }
        if reason != events::GuildReadyReason::Available {
            gate.record(&guild_id.to_string()); // run_guild_available checked and recorded it already
        }
        if let Err(e) = app.settings().ensure_guild_settings(&guild_id.to_string()).await {
            bot_util::log_error!(format!("Could not ensure the settings row of guild {guild_id}"), bot_util::logger::LogArg::error(&e));
        }
        self.run(app, events::GuildReady { guild_id, reason }).await;
    }
}

fn passes_filter<E: HookEvent>(event: HookPoint, h: &Registered<E>, ctx: &HookCtx<E>) -> bool {
    let Some(filter) = &h.hook.filter else { return true };
    match guard_sync(|| filter(ctx)) {
        Ok(pass) => pass,
        Err(msg) => {
            bot_util::log_error!(format!("Hook {event}:{} filter failed", h.hook.id), bot_util::logger::LogArg::Error { message: msg, stack: None });
            false
        }
    }
}

/// `createReadyGate`: the cap and per-guild cooldown in front of guildAvailable re-runs. `record(id)` starts a
/// guild's cooldown; it returns false (and keeps the older start) while one is running.
pub struct ReadyGate {
    slots: bot_util::async_util::Semaphore,
    record: bot_util::async_util::Throttle,
}

impl Default for ReadyGate {
    fn default() -> Self {
        Self::new(AVAILABLE_CONCURRENCY, AVAILABLE_COOLDOWN_MS)
    }
}

impl ReadyGate {
    pub fn new(concurrency: usize, cooldown_ms: u64) -> Self {
        Self { slots: bot_util::async_util::create_semaphore(concurrency), record: bot_util::async_util::create_throttle(cooldown_ms) }
    }

    /// Starts the guild's cooldown; `false` while one is running.
    pub fn record(&self, guild_id: &str) -> bool {
        self.record.allow(guild_id)
    }

    /// Runs `f` unless the guild is inside its cooldown (then `false`), at most `concurrency` at once.
    pub async fn run<F, Fut>(&self, guild_id: &str, f: F) -> bool
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if !self.record(guild_id) {
            return false;
        }
        self.slots.run(f).await;
        true
    }
}

/// `runGuildAvailable(client, guild)`: re-runs the startup work of a guild that came back, capped and at most once
/// per guild per cooldown (a startup or join run counts). Never fails; whether it ran.
pub async fn run_guild_available(app: &App, guild_id: GuildId) -> bool {
    if app.guild_directory().guild_state(&guild_id.to_string()) != GuildState::Available {
        return false;
    }
    let gate = app.ready_gate();
    let a = app.clone();
    gate.run(&guild_id.to_string(), move || async move {
        let gate = a.ready_gate();
        a.hooks().run_guild_ready(&a, guild_id, events::GuildReadyReason::Available, &gate).await;
    })
    .await
}

/// `runGuildReady` with the app's gate.
pub async fn run_guild_ready(app: &App, guild_id: GuildId, reason: events::GuildReadyReason) {
    let gate = app.ready_gate();
    app.hooks().run_guild_ready(app, guild_id, reason, &gate).await;
}
