//! dash-schema: the web dashboard's field descriptors and settings registry.
//!
//! - [`schema`] ports src/dashboard/schema.js: the descriptor type ([`Field`]), [`validate_value`], [`read_value`],
//!   the channel and role reference checks ([`check_refs`]) and [`public_descriptor`] (the JSON the front end gets).
//! - [`fields`] ports src/dashboard/fields.js: every GuildSettings value an admin may edit, grouped per module
//!   ([`registry`]), with the commands' rules: [`validate_settings_changes`], [`to_stored_changes`],
//!   [`check_cross_fields`], [`read_settings_values`].
//!
//! Pure: no database, cache or network access.

pub mod fields;
pub mod js;
pub mod schema;

pub use fields::{
    CrossProblem, Draft, Group, Problem, Registry, SettingsError, ValidatedChanges, automod_config_of, check_cross_fields,
    collect_settings_ref_keys, collect_settings_refs, constants, embed_has_content, general_groups, is_json_column,
    module_groups, normalize_automod_domain, normalize_automod_word, normalize_escalation, read_settings_values, registry,
    row_json, settings_column, settings_field, to_stored_changes, validate_setting, validate_settings_changes,
};
pub use schema::{
    AssignableRole, ChannelInfo, ChannelRef, FieldError, Field, GuildResources, KeepRefs, Refs, RoleInfo, RoleRef, StoredRefs,
    channel_kind, check_refs, collect_assignable_roles, collect_stored_refs, empty_refs, is_plain_object, is_snowflake,
    lint_descriptors, public_descriptor, read_value, ref_key, stored_ref_keys, validate_value,
};
