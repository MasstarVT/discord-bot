//! bot-core: the framework every feature crate is written against. Read `rust/docs/FRAMEWORK.md` first.
//!
//! | JS | Rust |
//! |---|---|
//! | `src/services/redis.js` | [`cache`] |
//! | `src/services/settingsService.js` | [`settings`] |
//! | `src/services/scheduler.js` | [`scheduler`] |
//! | `src/services/rewards.js` | [`rewards`] |
//! | `src/handlers/pluginLoader.js` | [`plugin`], [`registry`] |
//! | `src/handlers/hookRegistry.js` | [`hooks`] (payloads in [`hooks::events`]) |
//! | `src/handlers/componentRegistry.js` | [`component_registry`] |
//! | `src/handlers/interactionRouter.js` | [`router`], [`interaction`], [`collectors`] |
//! | `src/handlers/commandLoader.js`, `autoDeployWorker.js` | [`command`] |
//! | `src/handlers/eventLoader.js` | [`event_loader`] |
//! | `src/utils/permissionChecker.js` | [`permission_checker`] |
//! | `src/utils/embedBuilder.js` | [`embed_builder`] |
//!
//! The shared context is [`App`]; [`testing`] builds one offline and holds the golden-file helpers.

pub mod adapters;
pub mod cache;
pub mod client;
pub mod clock;
pub mod collectors;
pub mod command;
pub mod component_registry;
pub mod context;
pub mod custom_embed;
pub mod deletion_tracker;
pub mod discord;
pub mod embed_builder;
pub mod error;
pub mod event_loader;
pub mod gateway;
pub mod guild_members;
pub mod hooks;
pub mod interaction;
pub mod interaction_helpers;
pub mod markdown;
pub mod new_targets;
pub mod paginator;
pub mod permission_checker;
pub mod permissions;
pub mod plugin;
pub mod privacy_guards;
pub mod registry;
pub mod rewards;
pub mod router;
pub mod scheduler;
pub mod settings;
pub mod sync;
pub mod testing;

pub use cache::Cache;
pub use command::{CommandDef, CommandOption, SlashCommand};
pub use context::{App, AppBuilder, AppConfig};
pub use error::{BotResult, BoxFuture, PermanentJobError};
pub use hooks::{Hook, HookCtx, HookEvent, ModuleRef};
pub use interaction::{AutocompleteCtx, CommandCtx, ComponentCtx, ModalCtx, Responder};
pub use plugin::Plugin;
pub use registry::{RegisterFn, Registry};
pub use settings::{Settings, SettingsPatch};

/// Everything a feature crate usually needs: `use bot_core::prelude::*;`.
pub mod prelude {
    pub use crate::cache::Cache;
    pub use crate::collectors::{Collector, CollectorFilter};
    pub use crate::command::{CommandDef, CommandOption, SlashCommand, channel_type};
    pub use crate::context::App;
    pub use crate::embed_builder::{error_embed, guild_embed, info_embed, neutral_embed, success_embed, warn_embed};
    pub use crate::error::{BotResult, PermanentJobError};
    pub use crate::hooks::events::*;
    pub use crate::hooks::{Hook, HookCtx, HookEvent, ModuleRef};
    pub use crate::interaction::{AutocompleteCtx, CommandCtx, ComponentCtx, ComponentKind, ModalCtx, Responder};
    pub use crate::permission_checker::{MemberAccess, has_permission_level, resolve_permission_level};
    pub use crate::plugin::Plugin;
    pub use crate::registry::Registry;
    pub use crate::rewards::{RewardArgs, RewardKind, RewardProvider, RewardResult};
    pub use crate::scheduler::{CancelJobs, Job, JobCtx, JobOutcome, OnConflict, ScheduleJob, job_handler};
    pub use crate::settings::{Settings, SettingsPatch, is_module_enabled};
    pub use bot_config::{JobType, ModuleKey, PermissionLevel};
}
