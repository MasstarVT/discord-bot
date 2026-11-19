//! dash-collections: the web dashboard's per-server record collections and the seams around them.
//!
//! - [`index`] ports `src/dashboard/collections/index.js`: the 22 collection definitions ([`Collections::builtin`]),
//!   `publicCollection`, `validateRecordInput` and `lintCollections`. The descriptors are generated data
//!   (`collections_data.json`); the server-only hooks are Rust ([`collections`]).
//! - [`collections`] ports every `src/dashboard/collections/<key>.js` (not the `*.store.js` files): constants,
//!   cross-field checks, item builders and the stored <-> form converters the stores use.
//! - [`errors`] ports `errors.js` (RecordError, taskOutcome, withMessage), [`channel_access`] `channelAccess.js`,
//!   [`catalog`] `src/dashboard/catalog.js`.
//! - The seams: [`RecordStore`] (what each `*.store.js` exports) with its contexts and [`StoreRegistry`],
//!   [`RecordsBridge`] / [`SettingsBridge`] (what server.js calls), [`TaskRunner`] (tasks.js) and
//!   [`DashboardDiscord`] (the REST client the stores use). [`testing`] has in-memory implementations.
//!
//! Pure: no database, cache or network access.

pub mod bridge;
pub mod catalog;
pub mod channel_access;
pub mod collections;
pub mod discord;
pub mod errors;
pub mod index;
pub mod perms;
pub mod store;
pub mod testing;
pub mod util;

pub use bridge::{RecordsBridge, SettingsBridge, SettingsWriteCtx, SettingsWriteError, SettingsWritten};
pub use catalog::{build_catalog, build_catalog_with, catalog_json};
pub use channel_access::{ChannelCheck, SEND_NEEDS, VIEW_NEEDS, is_channel_exempt, require_channel_access};
pub use discord::{
    DashboardDiscord, DiscordApiError, DiscordMethod, DiscordResources, InviteRef, MemberProfile, MemberRoles, MessageRef, ResBot, ResChannel, ResEmoji,
    ResGuild, ResRole,
};
pub use errors::{RECORD_ERROR_STATUS, RecordError, RecordErrorCode, Saved, TaskOutcome, TaskResult, TaskStatus, task_outcome};
pub use index::{
    CheckCtx, CheckFn, Collection, CollectionAction, Collections, Mode, Problem, RefKeyOfFn, RefKeysFn, ValidInput, is_collection_key,
    is_record_id, lint_collections, public_collection, validate_record_input,
};
pub use store::{
    ActionAnswer, Actor, DashboardMember, DashboardUser, ListAnswer, ListOpts, ReadCtx, RecordStore, StoreCtx, StoreError,
    StoreRegistry, TaskOpts, TaskRunner,
};
