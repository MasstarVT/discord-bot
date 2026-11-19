//! Port of `src/handlers/componentRegistry.js`: one handler map per component kind, keyed by the customId prefix.
//! The first registration of a prefix wins; a different second handler is refused and logged, re-registering the same
//! handler (`Arc::ptr_eq`) is a no-op.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock, RwLock};

use crate::error::{BotResult, BoxFuture};
use crate::interaction::{ComponentCtx, ComponentKind, ModalCtx};

/// A button or select handler.
pub type ComponentFn = Arc<dyn Fn(ComponentCtx) -> BoxFuture<BotResult<()>> + Send + Sync>;
/// A modal submit handler.
pub type ModalFn = Arc<dyn Fn(ModalCtx) -> BoxFuture<BotResult<()>> + Send + Sync>;

/// Wraps an async closure as a [`ComponentFn`].
pub fn component_fn<F, Fut>(f: F) -> ComponentFn
where
    F: Fn(ComponentCtx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = BotResult<()>> + Send + 'static,
{
    Arc::new(move |ctx| Box::pin(f(ctx)))
}

/// Wraps an async closure as a [`ModalFn`].
pub fn modal_fn<F, Fut>(f: F) -> ModalFn
where
    F: Fn(ModalCtx) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = BotResult<()>> + Send + 'static,
{
    Arc::new(move |ctx| Box::pin(f(ctx)))
}

/// `HandlerMap`: prefix -> handler, first registration wins.
pub struct HandlerMap<H: Clone> {
    kind: ComponentKind,
    map: RwLock<HashMap<String, H>>,
    same: fn(&H, &H) -> bool,
}

impl<H: Clone> HandlerMap<H> {
    fn with(kind: ComponentKind, same: fn(&H, &H) -> bool) -> Self {
        Self { kind, map: RwLock::new(HashMap::new()), same }
    }

    pub fn kind(&self) -> ComponentKind {
        self.kind
    }

    /// `set(prefix, handler)`: `false` (and an error log) when a different handler already has the prefix.
    pub fn set(&self, prefix: &str, handler: H) -> bool {
        let mut map = self.map.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = map.get(prefix) {
            if (self.same)(existing, &handler) {
                return true;
            }
            bot_util::log_error!(format!("Duplicate {} handler for customId prefix \"{prefix}\" ignored", self.kind.as_str()));
            return false;
        }
        map.insert(prefix.to_string(), handler);
        true
    }

    pub fn get(&self, prefix: &str) -> Option<H> {
        self.map.read().unwrap_or_else(|e| e.into_inner()).get(prefix).cloned()
    }

    pub fn has(&self, prefix: &str) -> bool {
        self.map.read().unwrap_or_else(|e| e.into_inner()).contains_key(prefix)
    }

    pub fn len(&self) -> usize {
        self.map.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The registered prefixes, sorted.
    pub fn prefixes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.read().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
        v.sort();
        v
    }
}

/// `client.buttonHandlers`, `client.selectHandlers`, `client.modalHandlers` (`createHandlerMaps`).
pub struct ComponentRegistry {
    pub buttons: HandlerMap<ComponentFn>,
    pub selects: HandlerMap<ComponentFn>,
    pub modals: HandlerMap<ModalFn>,
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self {
            buttons: HandlerMap::with(ComponentKind::Button, Arc::ptr_eq),
            selects: HandlerMap::with(ComponentKind::Select, Arc::ptr_eq),
            modals: HandlerMap::with(ComponentKind::Modal, Arc::ptr_eq),
        }
    }
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The button or select map.
    pub fn message_map(&self, kind: ComponentKind) -> Option<&HandlerMap<ComponentFn>> {
        match kind {
            ComponentKind::Button => Some(&self.buttons),
            ComponentKind::Select => Some(&self.selects),
            ComponentKind::Modal => None,
        }
    }
}

/// The `noop` button handler: `interaction.deferUpdate()` (errors ignored). One shared instance, so a second
/// registration is the same handler.
pub fn noop_handler() -> ComponentFn {
    static NOOP: OnceLock<ComponentFn> = OnceLock::new();
    NOOP.get_or_init(|| {
        component_fn(|ctx: ComponentCtx| async move {
            let _ = ctx.defer_update().await;
            Ok(())
        })
    })
    .clone()
}

/// `registerCoreComponents(client)`: the persistent components the core owns (`noop`: a button that does nothing).
pub fn register_core_components(registry: &ComponentRegistry) {
    registry.buttons.set("noop", noop_handler());
}
