//! Port of `src/handlers/pluginLoader.js`: the plugin model, its validation and installation.
//!
//! A plugin is `{ id, module, hooks, components, jobs, provides, init }`. The JS discovers `src/plugins/<id>.js`;
//! Rust feature crates add their plugins to the [`crate::Registry`] in `register(reg)`. [`collect_plugins`] applies
//! the JS validation rules and drops only the offending entry (the whole plugin when its id or module is wrong),
//! recording an error for each; [`install_plugins`] registers hooks, then components (module-gated), job handlers,
//! reward providers, then each plugin's synchronous `init`.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;

use bot_config::{CustomIdKind, JobType, ModuleKey, prefix_info};

use crate::component_registry::{ComponentFn, ModalFn, component_fn, modal_fn};
use crate::context::App;
use crate::error::{BotResult, guard_sync};
use crate::hooks::{AnyHook, Hook, HookEvent, ModuleRef};
use crate::interaction::{ComponentCtx, ComponentKind, ModalCtx};
use crate::rewards::{RewardKind, RewardProvider};
use crate::scheduler::JobHandler;
use crate::settings::is_module_enabled;

/// A component handler of either shape.
#[derive(Clone)]
pub enum ComponentHandler {
    /// Buttons and selects.
    Message(ComponentFn),
    /// Modal submits.
    Modal(ModalFn),
}

/// `{ type, prefix, handler, module? }`.
#[derive(Clone)]
pub struct ComponentDef {
    pub kind: ComponentKind,
    pub prefix: &'static str,
    pub handler: ComponentHandler,
    pub module: ModuleRef,
}

/// `init(client)`: synchronous, run once at install.
pub type InitFn = Arc<dyn Fn(&App) -> anyhow::Result<()> + Send + Sync>;

/// A plugin declaration.
pub struct Plugin {
    pub id: &'static str,
    pub module: Option<ModuleKey>,
    pub hooks: Vec<Box<dyn AnyHook>>,
    pub components: Vec<ComponentDef>,
    pub jobs: Vec<(JobType, JobHandler)>,
    pub provides: Vec<(RewardKind, Arc<dyn RewardProvider>)>,
    pub init: Option<InitFn>,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("id", &self.id)
            .field("module", &self.module)
            .field("hooks", &self.hooks.iter().map(|h| format!("{}:{}", h.event(), h.id())).collect::<Vec<_>>())
            .field("components", &self.components.iter().map(|c| format!("{}:{}", c.kind.as_str(), c.prefix)).collect::<Vec<_>>())
            .field("jobs", &self.jobs.iter().map(|j| j.0.as_str()).collect::<Vec<_>>())
            .field("provides", &self.provides.iter().map(|p| p.0.as_str()).collect::<Vec<_>>())
            .field("init", &self.init.is_some())
            .finish()
    }
}

impl Plugin {
    /// `export const plugin = { id, module }`: `id` is the JS file name (`reaction-roles`), `module` a `MODULES` key or
    /// `None` for a core plugin.
    pub fn new(id: &'static str, module: Option<ModuleKey>) -> Self {
        Self { id, module, hooks: Vec::new(), components: Vec::new(), jobs: Vec::new(), provides: Vec::new(), init: None }
    }

    /// Adds a hook.
    pub fn hook<E: HookEvent>(mut self, hook: Hook<E>) -> Self {
        self.hooks.push(Box::new(hook));
        self
    }

    /// Adds a button handler for a persistent prefix this plugin owns.
    pub fn button<F, Fut>(self, prefix: &'static str, f: F) -> Self
    where
        F: Fn(ComponentCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        self.component(ComponentDef { kind: ComponentKind::Button, prefix, handler: ComponentHandler::Message(component_fn(f)), module: ModuleRef::Inherit })
    }

    /// Adds a select menu handler (every select kind).
    pub fn select<F, Fut>(self, prefix: &'static str, f: F) -> Self
    where
        F: Fn(ComponentCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        self.component(ComponentDef { kind: ComponentKind::Select, prefix, handler: ComponentHandler::Message(component_fn(f)), module: ModuleRef::Inherit })
    }

    /// Adds a modal submit handler.
    pub fn modal<F, Fut>(self, prefix: &'static str, f: F) -> Self
    where
        F: Fn(ModalCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        self.component(ComponentDef { kind: ComponentKind::Modal, prefix, handler: ComponentHandler::Modal(modal_fn(f)), module: ModuleRef::Inherit })
    }

    /// Adds a component (use this form to set its own `module`).
    pub fn component(mut self, def: ComponentDef) -> Self {
        self.components.push(def);
        self
    }

    /// Sets the module of the last added component (`module: null` is [`ModuleRef::Ungated`]).
    pub fn component_module(mut self, module: ModuleRef) -> Self {
        if let Some(c) = self.components.last_mut() {
            c.module = module;
        }
        self
    }

    /// Adds a job handler for a job type this plugin owns.
    pub fn job(mut self, job_type: JobType, handler: JobHandler) -> Self {
        self.jobs.push((job_type, handler));
        self
    }

    /// `provides: { xp | coins }`.
    pub fn provides(mut self, kind: RewardKind, provider: Arc<dyn RewardProvider>) -> Self {
        self.provides.push((kind, provider));
        self
    }

    /// `init(client)`.
    pub fn init(mut self, f: impl Fn(&App) -> anyhow::Result<()> + Send + Sync + 'static) -> Self {
        self.init = Some(Arc::new(f));
        self
    }
}

/// A plugin id: lowercase words joined by `-` (a JS file name).
pub fn is_valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('_')
        && id.split('-').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// What [`collect_plugins`] returns.
#[derive(Debug, Default)]
pub struct Collected {
    pub plugins: Vec<Plugin>,
    pub errors: Vec<String>,
}

/// `collectPlugins()`: validates every plugin. Each violation becomes an `errors` entry and drops only the offending
/// entry (the whole plugin when its id is malformed or taken). Plugins are kept in id order, like the sorted files.
pub fn collect_plugins(plugins: Vec<Plugin>) -> Collected {
    let mut out = Collected::default();
    let mut sorted = plugins;
    sorted.sort_by(|a, b| a.id.cmp(b.id));
    let mut seen_ids = HashSet::new();
    let mut seen_hooks = HashSet::new();
    let mut seen_components = HashSet::new();
    let mut seen_jobs = HashSet::new();
    let mut seen_providers = HashSet::new();

    for p in sorted {
        let id = p.id;
        let where_ = format!("plugin {id}");
        if !is_valid_plugin_id(id) {
            out.errors.push(format!("{where_}: plugin.id \"{id}\" must be a file-name id (lowercase words joined by '-')"));
            continue;
        }
        if !seen_ids.insert(id) {
            out.errors.push(format!("{where_}: plugin.id \"{id}\" is already taken by another plugin"));
            continue;
        }
        let mut normalized = Plugin::new(id, p.module);

        for hook in p.hooks {
            let event = hook.event();
            let tag = format!("{where_}: hook {}:{}", event, hook.id());
            if !event.order().contains(&hook.id()) {
                out.errors.push(format!("{tag}: id is not listed in HOOK_ORDER.{event}"));
                continue;
            }
            if hook.id() != id && !hook.id().starts_with(&format!("{id}:")) {
                out.errors.push(format!("{tag}: id must be \"{id}\" or start with \"{id}:\""));
                continue;
            }
            if !seen_hooks.insert(format!("{event}:{}", hook.id())) {
                out.errors.push(format!("{tag}: duplicate hook"));
                continue;
            }
            normalized.hooks.push(hook);
        }

        for c in p.components {
            let tag = format!("{where_}: component {} \"{}\"", c.kind.as_str(), c.prefix);
            let Some(info) = prefix_info(c.prefix) else {
                out.errors.push(format!("{tag}: prefix is not in CUSTOM_ID_PREFIXES"));
                continue;
            };
            if info.owner != id {
                out.errors.push(format!("{tag}: prefix is owned by \"{}\", not \"{id}\"", info.owner));
                continue;
            }
            if info.kind != CustomIdKind::Persistent {
                out.errors.push(format!("{tag}: prefix is \"{}\", only persistent prefixes get a registered handler", info.kind.as_str()));
                continue;
            }
            if !info.types.contains(&c.kind.component_type()) {
                let types: Vec<&str> = info.types.iter().map(|t| t.as_str()).collect();
                out.errors.push(format!("{tag}: the registry lists this prefix for {}", types.join("/")));
                continue;
            }
            let handler_ok = matches!((&c.handler, c.kind), (ComponentHandler::Modal(_), ComponentKind::Modal) | (ComponentHandler::Message(_), ComponentKind::Button | ComponentKind::Select));
            if !handler_ok {
                out.errors.push(format!("{tag}: handler must be a function"));
                continue;
            }
            if !seen_components.insert(format!("{}:{}", c.kind.as_str(), c.prefix)) {
                out.errors.push(format!("{tag}: duplicate component"));
                continue;
            }
            normalized.components.push(c);
        }

        for (job_type, handler) in p.jobs {
            let tag = format!("{where_}: job \"{job_type}\"");
            let owner = job_type.info().owner;
            if owner != id {
                out.errors.push(format!("{tag}: owned by \"{owner}\", not \"{id}\""));
                continue;
            }
            if !seen_jobs.insert(job_type) {
                out.errors.push(format!("{tag}: duplicate job handler"));
                continue;
            }
            normalized.jobs.push((job_type, handler));
        }

        for (kind, provider) in p.provides {
            let tag = format!("{where_}: provides.{kind}");
            if !seen_providers.insert(kind) {
                out.errors.push(format!("{tag}: duplicate provider"));
                continue;
            }
            normalized.provides.push((kind, provider));
        }

        normalized.init = p.init;
        out.plugins.push(normalized);
    }
    out
}

/// What [`install_plugins`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub plugins: usize,
    pub hooks: usize,
    pub components: usize,
    pub jobs: usize,
    pub providers: usize,
    pub errors: Vec<String>,
}

async fn ephemeral_notice(responder: &crate::interaction::Responder, content: String) {
    use serenity::builder::{CreateInteractionResponseFollowup, CreateInteractionResponseMessage};
    let _ = if responder.is_acknowledged() {
        responder.follow_up(CreateInteractionResponseFollowup::new().content(content).ephemeral(true)).await.map(|_| ())
    } else {
        responder.reply(CreateInteractionResponseMessage::new().content(content).ephemeral(true)).await
    };
}

/// The module check of a gated component: memo first, then a cold load bounded at 1000 ms (or what is left of the
/// acknowledgement window). `Ok(())` lets the handler run; `Err(notice)` is the text to answer instead.
async fn component_gate(app: &App, guild_id: Option<serenity::all::GuildId>, budget_ms: u64, module: ModuleKey) -> Result<(), String> {
    let Some(g) = guild_id else { return Ok(()) };
    let gid = g.to_string();
    let mut settings = app.settings().peek_settings(&gid);
    if settings.is_none() {
        let a = app.clone();
        let id = gid.clone();
        let load = async move { Ok::<_, std::convert::Infallible>(a.settings().get_settings_safe(&id).await) };
        settings = bot_util::async_util::with_timeout(load, budget_ms.min(1000), "component settings").await.ok().flatten();
    }
    if is_module_enabled(settings.as_deref(), Some(module)) {
        return Ok(());
    }
    Err(if settings.is_some() {
        format!("The **{}** module is turned off on this server.", module.info().label)
    } else {
        "Server settings are temporarily unavailable. Please try again in a moment.".to_string()
    })
}

/// Wraps a button or select handler with the module toggle (`gateComponent`).
pub fn gate_component(handler: ComponentFn, module: ModuleKey) -> ComponentFn {
    component_fn(move |ctx: ComponentCtx| {
        let handler = handler.clone();
        async move {
            match component_gate(&ctx.app, ctx.interaction.guild_id, ctx.ack_budget_ms(), module).await {
                Ok(()) => handler(ctx).await,
                Err(notice) => {
                    ephemeral_notice(&ctx.responder, notice).await;
                    Ok(())
                }
            }
        }
    })
}

/// Wraps a modal handler with the module toggle (`gateComponent`).
pub fn gate_modal(handler: ModalFn, module: ModuleKey) -> ModalFn {
    modal_fn(move |ctx: ModalCtx| {
        let handler = handler.clone();
        async move {
            match component_gate(&ctx.app, ctx.interaction.guild_id, ctx.ack_budget_ms(), module).await {
                Ok(()) => handler(ctx).await,
                Err(notice) => {
                    ephemeral_notice(&ctx.responder, notice).await;
                    Ok(())
                }
            }
        }
    })
}

/// `installPlugins(client, collected)`: hooks, then components (module-gated), job handlers, reward providers, then
/// each `init`. A failing registration is recorded and the rest still install. Never fails.
pub fn install_plugins(app: &App, collected: &Collected) -> InstallReport {
    let mut r = InstallReport { plugins: collected.plugins.len(), errors: collected.errors.clone(), ..Default::default() };
    let plugins = &collected.plugins;

    for p in plugins {
        for hook in &p.hooks {
            match hook.register(app.hooks(), p.id, p.module) {
                Ok(()) => r.hooks += 1,
                Err(e) => r.errors.push(format!("{}: hook {}:{} not registered: {e}", p.id, hook.event(), hook.id())),
            }
        }
    }

    for p in plugins {
        for c in &p.components {
            let module = c.module.resolve(p.module);
            let ok = match (&c.handler, c.kind) {
                (ComponentHandler::Message(h), kind) => {
                    let handler = match module {
                        Some(m) => gate_component(h.clone(), m),
                        None => h.clone(),
                    };
                    app.components().message_map(kind).is_some_and(|map| map.set(c.prefix, handler))
                }
                (ComponentHandler::Modal(h), _) => {
                    let handler = match module {
                        Some(m) => gate_modal(h.clone(), m),
                        None => h.clone(),
                    };
                    app.components().modals.set(c.prefix, handler)
                }
            };
            // Like the JS `HandlerMap.set`: a prefix already taken is refused and logged by the map, the component is
            // still counted and no plugin error is recorded.
            let _registered = ok;
            r.components += 1;
        }
    }

    for p in plugins {
        for (job_type, handler) in &p.jobs {
            match app.scheduler().register_job_handler(*job_type, handler.clone()) {
                Ok(()) => r.jobs += 1,
                Err(e) => r.errors.push(format!("{}: job handler {job_type} not registered: {e}", p.id)),
            }
        }
    }

    for p in plugins {
        for (kind, provider) in &p.provides {
            match app.rewards().register_provider(*kind, provider.clone(), Some(p.id)) {
                Ok(()) => r.providers += 1,
                Err(e) => r.errors.push(format!("{}: reward provider \"{kind}\" not registered: {e}", p.id)),
            }
        }
    }

    for p in plugins {
        let Some(init) = &p.init else { continue };
        match guard_sync(|| init(app)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => r.errors.push(format!("{}: init failed: {e}", p.id)),
            Err(panic) => r.errors.push(format!("{}: init failed: {panic}", p.id)),
        }
    }

    bot_util::log_info!(format!(
        "Plugins: {} loaded ({} hook(s), {} component(s), {} job handler(s), {} provider(s)), {} error(s).",
        plugins.len(),
        r.hooks,
        r.components,
        r.jobs,
        r.providers,
        r.errors.len()
    ));
    for e in &r.errors {
        bot_util::log_error!(format!("Plugin error: {e}"));
    }
    r
}
