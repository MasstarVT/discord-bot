//! The explicit registry that replaces the JS file-system discovery (`src/commands/<category>/*.js`,
//! `src/plugins/<id>.js`).
//!
//! Every feature crate exposes exactly one function:
//!
//! ```ignore
//! pub fn register(reg: &mut bot_core::Registry) {
//!     reg.command(commands::ban::command());
//!     reg.plugin(plugin::plugin());
//! }
//! ```
//!
//! The bot binary calls every crate's `register`, then [`Registry::install`], which validates the plugins
//! (`collectPlugins`), installs them (`installPlugins`), registers the core components (`registerCoreComponents`) and
//! loads the commands (`loadCommands`), in the JS boot order.

use crate::command::{CommandDef, load_commands};
use crate::component_registry::register_core_components;
use crate::context::App;
use crate::plugin::{InstallReport, Plugin, collect_plugins, install_plugins};

/// A feature crate's registration function.
pub type RegisterFn = fn(&mut Registry);

/// A boot step registered with [`Registry::on_boot`].
type BootFn = Box<dyn FnOnce(&App) -> anyhow::Result<()> + Send>;

/// Commands and plugins, before installation.
#[derive(Default)]
pub struct Registry {
    commands: Vec<CommandDef>,
    plugins: Vec<Plugin>,
    boot: Vec<(String, BootFn)>,
}

/// What [`Registry::install`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootReport {
    pub plugins: InstallReport,
    pub commands: usize,
    /// Command validation problems (they are logged; the commands still load, like the JS).
    pub command_errors: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs feature registration functions.
    pub fn with(fns: &[RegisterFn]) -> Self {
        let mut reg = Self::new();
        for f in fns {
            f(&mut reg);
        }
        reg
    }

    /// Adds a slash command.
    pub fn command(&mut self, def: CommandDef) -> &mut Self {
        self.commands.push(def);
        self
    }

    /// Adds a plugin.
    pub fn plugin(&mut self, plugin: Plugin) -> &mut Self {
        self.plugins.push(plugin);
        self
    }

    /// Runs `f` once when the registry is installed into an app (after the plugins and commands), outside any
    /// plugin, so it does not count as a plugin `init` (`hasInit` in `plugins.json` stays as the JS has it). An error
    /// is logged (`Boot step <label> failed`) and the boot goes on.
    pub fn on_boot(&mut self, label: &str, f: impl FnOnce(&App) -> anyhow::Result<()> + Send + 'static) -> &mut Self {
        self.boot.push((label.to_string(), Box::new(f)));
        self
    }

    /// Registers a shutdown flush ([`crate::client::on_shutdown`]) for the app this registry is installed into, from
    /// a feature's `register` (no plugin `init` needed): `f` gets the app and runs after the scheduler stops, lowest
    /// `order` first, bounded by `timeout_ms`; a failure logs `<warn>: <error>`.
    pub fn on_shutdown<F, Fut>(&mut self, order: u32, label: &'static str, timeout_ms: u64, warn: &'static str, f: F) -> &mut Self
    where
        F: Fn(App) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.on_boot(label, move |app| {
            let a = app.clone();
            crate::client::on_shutdown(app, order, label, timeout_ms, warn, move || f(a.clone()));
            Ok(())
        })
    }

    pub fn commands(&self) -> &[CommandDef] {
        &self.commands
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn command_named(&self, name: &str) -> Option<&CommandDef> {
        self.commands.iter().find(|c| c.name() == name)
    }

    pub fn plugin_named(&self, id: &str) -> Option<&Plugin> {
        self.plugins.iter().find(|p| p.id == id)
    }

    /// Every command's validation problems, plus duplicate names.
    pub fn validate_commands(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for c in &self.commands {
            if !seen.insert(c.name()) {
                errors.push(format!("Duplicate command name \"{}\"", c.name()));
            }
            errors.extend(c.validate());
        }
        errors
    }

    /// Takes the commands and plugins by value (registration order). Boot steps registered with
    /// [`Registry::on_boot`] / [`Registry::on_shutdown`] are dropped; install the registry to run them.
    pub fn into_parts(self) -> (Vec<CommandDef>, Vec<Plugin>) {
        (self.commands, self.plugins)
    }

    /// Reorders the commands (a stable sort), e.g. into the JS `walkDir` order of `src/commands/<category>/<file>.js`.
    pub fn sort_commands_by<F>(&mut self, compare: F) -> &mut Self
    where
        F: FnMut(&CommandDef, &CommandDef) -> std::cmp::Ordering,
    {
        self.commands.sort_by(compare);
        self
    }

    /// Every deploy payload, in registration order.
    pub fn payloads(&self) -> Vec<serde_json::Value> {
        self.commands.iter().map(CommandDef::payload).collect()
    }

    /// Validates and installs everything into `app`.
    pub fn install(self, app: &App) -> BootReport {
        let command_errors = self.validate_commands();
        for e in &command_errors {
            bot_util::log_warn!(format!("Command problem: {e}"));
        }
        let collected = collect_plugins(self.plugins);
        let plugins = install_plugins(app, &collected);
        register_core_components(app.components());
        let commands = load_commands(app.commands(), self.commands);
        for (label, f) in self.boot {
            if let Err(e) = f(app) {
                bot_util::log_error!(format!("Boot step {label} failed"), bot_util::logger::LogArg::Error { message: e.to_string(), stack: None });
            }
        }
        BootReport { plugins, commands, command_errors }
    }
}
