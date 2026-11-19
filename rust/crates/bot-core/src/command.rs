//! Port of `src/handlers/commandLoader.js` and of the discord.js slash command builders.
//!
//! A feature declares each command as a [`CommandDef`]: the [`SlashCommand`] data (whose JSON is byte-for-byte what
//! discord.js's `toJSON()` produced, key order included, so the deploy hash of an unchanged command set is the same
//! as the JS bot's), the routing metadata (`module`, `gated`, `permissionLevel`, `subcommandPermissions`,
//! `allowInDMs`), its category (the JS folder, for `/help`) and its handlers.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bot_config::{ModuleKey, PermissionLevel};
use indexmap::IndexMap;
use serde_json::{Map, Number, Value, json};
use serenity::all::{GuildId, Http, Permissions};
use sha2::{Digest, Sha256};

use crate::error::{BotResult, BoxFuture};
use crate::interaction::{AutocompleteCtx, CommandCtx};

/// Discord's application command option types.
pub mod option_type {
    pub const SUB_COMMAND: u8 = 1;
    pub const SUB_COMMAND_GROUP: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTEGER: u8 = 4;
    pub const BOOLEAN: u8 = 5;
    pub const USER: u8 = 6;
    pub const CHANNEL: u8 = 7;
    pub const ROLE: u8 = 8;
    pub const MENTIONABLE: u8 = 9;
    pub const NUMBER: u8 = 10;
    pub const ATTACHMENT: u8 = 11;
}

/// Discord channel types (`ChannelType` in discord.js), for [`CommandOption::channel_types`].
pub mod channel_type {
    pub const GUILD_TEXT: u8 = 0;
    pub const DM: u8 = 1;
    pub const GUILD_VOICE: u8 = 2;
    pub const GROUP_DM: u8 = 3;
    pub const GUILD_CATEGORY: u8 = 4;
    pub const GUILD_ANNOUNCEMENT: u8 = 5;
    pub const ANNOUNCEMENT_THREAD: u8 = 10;
    pub const PUBLIC_THREAD: u8 = 11;
    pub const PRIVATE_THREAD: u8 = 12;
    pub const GUILD_STAGE_VOICE: u8 = 13;
    pub const GUILD_DIRECTORY: u8 = 14;
    pub const GUILD_FORUM: u8 = 15;
    pub const GUILD_MEDIA: u8 = 16;
}

/// `InteractionContextType.Guild` and `ApplicationIntegrationType.GuildInstall`.
pub const CONTEXT_GUILD: u8 = 0;
pub const INTEGRATION_GUILD_INSTALL: u8 = 0;

/// A JSON number written like `JSON.stringify` writes it (integral values without a fraction).
pub fn js_number(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        Value::Number(Number::from(n as i64))
    } else {
        Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null)
    }
}

/// A numeric bound or choice value (`i64` or `f64`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Num(pub f64);

impl From<i64> for Num {
    fn from(v: i64) -> Self {
        Num(v as f64)
    }
}
impl From<i32> for Num {
    fn from(v: i32) -> Self {
        Num(f64::from(v))
    }
}
impl From<u32> for Num {
    fn from(v: u32) -> Self {
        Num(f64::from(v))
    }
}
impl From<f64> for Num {
    fn from(v: f64) -> Self {
        Num(v)
    }
}

/// One option of a slash command (any type, subcommands and groups included).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOption {
    pub kind: u8,
    pub name: String,
    pub description: String,
    pub required: bool,
    pub autocomplete: bool,
    pub choices: Vec<(String, Value)>,
    pub min_value: Option<Num>,
    pub max_value: Option<Num>,
    pub min_length: Option<u16>,
    pub max_length: Option<u16>,
    pub channel_types: Vec<u8>,
    pub options: Vec<CommandOption>,
}

impl CommandOption {
    pub fn new(kind: u8, name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
            description: description.into(),
            required: false,
            autocomplete: false,
            choices: Vec::new(),
            min_value: None,
            max_value: None,
            min_length: None,
            max_length: None,
            channel_types: Vec::new(),
            options: Vec::new(),
        }
    }

    /// `addSubcommand`.
    pub fn sub_command(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::SUB_COMMAND, name, description)
    }
    /// `addSubcommandGroup`.
    pub fn sub_command_group(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::SUB_COMMAND_GROUP, name, description)
    }
    pub fn string(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::STRING, name, description)
    }
    pub fn integer(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::INTEGER, name, description)
    }
    pub fn number(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::NUMBER, name, description)
    }
    pub fn boolean(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::BOOLEAN, name, description)
    }
    pub fn user(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::USER, name, description)
    }
    pub fn channel(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::CHANNEL, name, description)
    }
    pub fn role(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::ROLE, name, description)
    }
    pub fn mentionable(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::MENTIONABLE, name, description)
    }
    pub fn attachment(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self::new(option_type::ATTACHMENT, name, description)
    }

    /// `setRequired(true)`.
    pub fn required(mut self, required: bool) -> Self {
        self.required = required;
        self
    }
    /// `setAutocomplete(true)`.
    pub fn autocomplete(mut self, on: bool) -> Self {
        self.autocomplete = on;
        self
    }
    /// `addChoices({ name, value })` for a string option.
    pub fn choice(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.choices.push((name.into(), Value::String(value.into())));
        self
    }
    /// `addChoices({ name, value })` for an integer or number option.
    pub fn num_choice(mut self, name: impl Into<String>, value: impl Into<Num>) -> Self {
        self.choices.push((name.into(), js_number(value.into().0)));
        self
    }
    /// `setMinValue`.
    pub fn min_value(mut self, v: impl Into<Num>) -> Self {
        self.min_value = Some(v.into());
        self
    }
    /// `setMaxValue`.
    pub fn max_value(mut self, v: impl Into<Num>) -> Self {
        self.max_value = Some(v.into());
        self
    }
    /// `setMinLength`.
    pub fn min_length(mut self, n: u16) -> Self {
        self.min_length = Some(n);
        self
    }
    /// `setMaxLength`.
    pub fn max_length(mut self, n: u16) -> Self {
        self.max_length = Some(n);
        self
    }
    /// `addChannelTypes(...)` (numbers from [`channel_type`]).
    pub fn channel_types(mut self, types: &[u8]) -> Self {
        self.channel_types.extend_from_slice(types);
        self
    }
    /// Adds an option (to a subcommand) or a subcommand (to a group).
    pub fn option(mut self, option: CommandOption) -> Self {
        self.options.push(option);
        self
    }

    fn is_container(&self) -> bool {
        self.kind == option_type::SUB_COMMAND || self.kind == option_type::SUB_COMMAND_GROUP
    }

    /// `toJSON()`, with discord.js's key order per option class.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let numeric = self.kind == option_type::INTEGER || self.kind == option_type::NUMBER;
        if self.is_container() {
            m.insert("type".into(), json!(self.kind));
            m.insert("name".into(), json!(self.name));
            m.insert("description".into(), json!(self.description));
            m.insert("options".into(), Value::Array(self.options.iter().map(CommandOption::to_json).collect()));
            return Value::Object(m);
        }
        if numeric {
            if let Some(v) = self.max_value {
                m.insert("max_value".into(), js_number(v.0));
            }
            if let Some(v) = self.min_value {
                m.insert("min_value".into(), js_number(v.0));
            }
        }
        if self.kind == option_type::STRING || numeric {
            if self.autocomplete {
                m.insert("autocomplete".into(), json!(true));
            }
            m.insert("type".into(), json!(self.kind));
            if !self.choices.is_empty() {
                let choices = self.choices.iter().map(|(n, v)| json!({ "name": n, "value": v })).collect();
                m.insert("choices".into(), Value::Array(choices));
            }
            m.insert("name".into(), json!(self.name));
            m.insert("description".into(), json!(self.description));
            m.insert("required".into(), json!(self.required));
            if self.kind == option_type::STRING {
                if let Some(n) = self.max_length {
                    m.insert("max_length".into(), json!(n));
                }
                if let Some(n) = self.min_length {
                    m.insert("min_length".into(), json!(n));
                }
            }
            return Value::Object(m);
        }
        if self.kind == option_type::CHANNEL && !self.channel_types.is_empty() {
            m.insert("channel_types".into(), json!(self.channel_types));
        }
        m.insert("name".into(), json!(self.name));
        m.insert("description".into(), json!(self.description));
        m.insert("required".into(), json!(self.required));
        m.insert("type".into(), json!(self.kind));
        Value::Object(m)
    }

}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The `smoke.test.js` name rule (`^[-_\p{L}\p{N}...]{1,32}$`, lowercase).
fn valid_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=32).contains(&len) && name.chars().all(|c| c == '-' || c == '_' || c.is_alphanumeric()) && name.to_lowercase() == name
}

/// `checkOptions(path, options, problems, depth)` from test/smoke.test.js.
fn check_options(path: &str, options: &[CommandOption], problems: &mut Vec<String>, depth: usize) {
    if options.len() > 25 {
        problems.push(format!("{path}: {} options (max 25)", options.len()));
    }
    let mut names = std::collections::HashSet::new();
    let mut seen_optional = false;
    for o in options {
        let p = format!("{path} {}", o.name);
        if !valid_name(&o.name) {
            problems.push(format!("{p}: invalid name"));
        }
        if !names.insert(o.name.as_str()) {
            problems.push(format!("{p}: duplicate option name"));
        }
        if o.description.is_empty() || utf16_len(&o.description) > 100 {
            problems.push(format!("{p}: description must be 1-100 chars"));
        }
        if o.choices.len() > 25 {
            problems.push(format!("{p}: {} choices (max 25)", o.choices.len()));
        }
        if !o.choices.is_empty() && o.autocomplete {
            problems.push(format!("{p}: choices and autocomplete are exclusive"));
        }
        for (name, value) in &o.choices {
            if name.is_empty() || utf16_len(name) > 100 {
                problems.push(format!("{p}: choice name \"{name}\" must be 1-100 chars"));
            }
            if value.as_str().is_some_and(|v| utf16_len(v) > 100) {
                problems.push(format!("{p}: choice value too long"));
            }
        }
        if o.is_container() {
            if o.kind == option_type::SUB_COMMAND_GROUP && depth > 0 {
                problems.push(format!("{p}: subcommand group nested too deep"));
            }
            check_options(&p, &o.options, problems, depth + 1);
        } else if o.required {
            if seen_optional {
                problems.push(format!("{p}: required option after an optional one"));
            }
        } else {
            seen_optional = true;
        }
    }
    // newTargets companions: `new_<x>` (string) must sit next to a role or channel picker `<x>`.
    for o in options {
        let Some(base_name) = o.name.strip_prefix("new_") else { continue };
        let base = options.iter().find(|b| b.name == base_name);
        if o.kind != option_type::STRING || !base.is_some_and(|b| b.kind == option_type::CHANNEL || b.kind == option_type::ROLE) {
            problems.push(format!("{path} {}: a new_ option must be a string next to a role or channel option \"{base_name}\"", o.name));
        }
    }
    let has_sub = options.iter().any(CommandOption::is_container);
    if has_sub && options.iter().any(|o| !o.is_container()) {
        problems.push(format!("{path}: mixes subcommands with plain options"));
    }
}

/// `charCount(node)`: Discord caps the combined length of every name, description and choice (name and value).
fn char_count_option(o: &CommandOption) -> usize {
    let mut n = utf16_len(&o.name) + utf16_len(&o.description);
    for (name, value) in &o.choices {
        n += utf16_len(name) + utf16_len(&value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()));
    }
    n + o.options.iter().map(char_count_option).sum::<usize>()
}

impl CommandOption {
    /// Reads an option back from its Discord JSON (the inverse of [`CommandOption::to_json`]).
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let kind = v.get("type").and_then(Value::as_u64).ok_or("option without a type")? as u8;
        let name = v.get("name").and_then(Value::as_str).ok_or("option without a name")?;
        let description = v.get("description").and_then(Value::as_str).unwrap_or_default();
        let mut o = CommandOption::new(kind, name, description);
        o.required = v.get("required").and_then(Value::as_bool).unwrap_or(false);
        o.autocomplete = v.get("autocomplete").and_then(Value::as_bool).unwrap_or(false);
        o.min_value = v.get("min_value").and_then(Value::as_f64).map(Num);
        o.max_value = v.get("max_value").and_then(Value::as_f64).map(Num);
        o.min_length = v.get("min_length").and_then(Value::as_u64).map(|n| n as u16);
        o.max_length = v.get("max_length").and_then(Value::as_u64).map(|n| n as u16);
        if let Some(types) = v.get("channel_types").and_then(Value::as_array) {
            o.channel_types = types.iter().filter_map(Value::as_u64).map(|t| t as u8).collect();
        }
        if let Some(choices) = v.get("choices").and_then(Value::as_array) {
            for c in choices {
                let n = c.get("name").and_then(Value::as_str).ok_or("choice without a name")?;
                o.choices.push((n.to_string(), c.get("value").cloned().unwrap_or(Value::Null)));
            }
        }
        if let Some(opts) = v.get("options").and_then(Value::as_array) {
            for sub in opts {
                o.options.push(CommandOption::from_json(sub)?);
            }
        }
        Ok(o)
    }
}

/// A slash command's data (`SlashCommandBuilder`).
#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub options: Vec<CommandOption>,
    pub default_member_permissions: Option<Permissions>,
    /// Set only when the command sets its own contexts (otherwise guild-only unless `allowInDMs`).
    pub contexts: Option<Vec<u8>>,
    pub integration_types: Option<Vec<u8>>,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            options: Vec::new(),
            default_member_permissions: None,
            contexts: None,
            integration_types: None,
        }
    }

    /// `addXOption(...)` / `addSubcommand(...)` / `addSubcommandGroup(...)`.
    pub fn option(mut self, option: CommandOption) -> Self {
        self.options.push(option);
        self
    }

    /// `setDefaultMemberPermissions(bits)` (ADMIN commands: `MANAGE_GUILD`).
    pub fn default_member_permissions(mut self, permissions: Permissions) -> Self {
        self.default_member_permissions = Some(permissions);
        self
    }

    /// `setContexts(...)`.
    pub fn contexts(mut self, contexts: &[u8]) -> Self {
        self.contexts = Some(contexts.to_vec());
        self
    }

    /// `setIntegrationTypes(...)`.
    pub fn integration_types(mut self, types: &[u8]) -> Self {
        self.integration_types = Some(types.to_vec());
        self
    }

    /// `data.toJSON()` (discord.js key order).
    pub fn to_json(&self) -> Value {
        self.to_json_with(self.contexts.clone(), self.integration_types.clone())
    }

    fn to_json_with(&self, contexts: Option<Vec<u8>>, integration_types: Option<Vec<u8>>) -> Value {
        let mut m = Map::new();
        m.insert("options".into(), Value::Array(self.options.iter().map(CommandOption::to_json).collect()));
        m.insert("name".into(), json!(self.name));
        m.insert("description".into(), json!(self.description));
        if let Some(c) = contexts {
            m.insert("contexts".into(), json!(c));
        }
        if let Some(p) = self.default_member_permissions {
            m.insert("default_member_permissions".into(), json!(p.bits().to_string()));
        }
        if let Some(t) = integration_types {
            m.insert("integration_types".into(), json!(t));
        }
        m.insert("type".into(), json!(1));
        Value::Object(m)
    }

    /// Reads a command back from its Discord payload (the inverse of [`SlashCommand::to_json`]).
    pub fn from_payload(v: &Value) -> Result<Self, String> {
        let name = v.get("name").and_then(Value::as_str).ok_or("command without a name")?;
        let description = v.get("description").and_then(Value::as_str).unwrap_or_default();
        let mut c = SlashCommand::new(name, description);
        if let Some(opts) = v.get("options").and_then(Value::as_array) {
            for o in opts {
                c.options.push(CommandOption::from_json(o)?);
            }
        }
        if let Some(p) = v.get("default_member_permissions").and_then(Value::as_str) {
            let bits: u64 = p.parse().map_err(|_| format!("bad default_member_permissions {p:?}"))?;
            c.default_member_permissions = Some(Permissions::from_bits_retain(bits));
        }
        let bytes = |k: &str| v.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|n| n as u8).collect::<Vec<u8>>());
        c.contexts = bytes("contexts");
        c.integration_types = bytes("integration_types");
        Ok(c)
    }

    /// Every `subcommand` and `group subcommand` path.
    pub fn subcommand_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        for o in &self.options {
            match o.kind {
                option_type::SUB_COMMAND => out.push(o.name.clone()),
                option_type::SUB_COMMAND_GROUP => {
                    for s in &o.options {
                        out.push(format!("{} {}", o.name, s.name));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// `charCount(json)`: names, descriptions and choices, summed (Discord allows 4000).
    pub fn char_count(&self) -> usize {
        utf16_len(&self.name) + utf16_len(&self.description) + self.options.iter().map(char_count_option).sum::<usize>()
    }

    /// The limits `test/smoke.test.js` checks: names, description lengths, option counts, choices, order, `new_`
    /// companions, no mixing of subcommands and options, at most 4000 combined characters.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let at = format!("/{}", self.name);
        if !valid_name(&self.name) {
            problems.push(format!("{at}: invalid name"));
        }
        if self.description.is_empty() || utf16_len(&self.description) > 100 {
            problems.push(format!("{at}: description must be 1-100 chars"));
        }
        check_options(&at, &self.options, &mut problems, 0);
        let chars = self.char_count();
        if chars > 4000 {
            problems.push(format!("{at}: {chars} combined characters (max 4000)"));
        }
        problems
    }
}

/// A command's `execute(interaction, client)`.
pub type CommandFn = Arc<dyn Fn(CommandCtx) -> BoxFuture<BotResult<()>> + Send + Sync>;
/// A command's `autocomplete(interaction, client)`.
pub type AutocompleteFn = Arc<dyn Fn(AutocompleteCtx) -> BoxFuture<BotResult<()>> + Send + Sync>;

/// A slash command module: `data`, `execute`, `module`, `gated`, `permissionLevel`, `subcommandPermissions`,
/// `allowInDMs`, `autocomplete`, plus the category folder.
#[derive(Clone)]
pub struct CommandDef {
    pub data: SlashCommand,
    /// The `MODULES` key, or `None` for a core command. Required and explicit (constructor argument).
    pub module: Option<ModuleKey>,
    /// Refused while the module is off (default `true`; ADMIN config commands set `false`).
    pub gated: bool,
    pub permission_level: PermissionLevel,
    /// `{ 'sub': level, 'group sub': level }`, in declaration order.
    pub subcommand_permissions: Vec<(String, PermissionLevel)>,
    /// DM-safe; every other command fails closed outside a cached guild and is registered for servers only.
    pub allow_in_dms: bool,
    /// The JS folder under `src/commands/` (`admin`, `moderation`, `utility`, ...): `/help` groups by it.
    pub category: &'static str,
    pub execute: CommandFn,
    pub autocomplete: Option<AutocompleteFn>,
}

impl std::fmt::Debug for CommandDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandDef")
            .field("name", &self.data.name)
            .field("module", &self.module)
            .field("gated", &self.gated)
            .field("permission_level", &self.permission_level)
            .field("subcommand_permissions", &self.subcommand_permissions)
            .field("allow_in_dms", &self.allow_in_dms)
            .field("category", &self.category)
            .field("autocomplete", &self.autocomplete.is_some())
            .finish()
    }
}

impl CommandDef {
    /// A command in `category` with its data, its module (`None` = core) and its `execute`.
    pub fn new<F, Fut>(category: &'static str, data: SlashCommand, module: Option<ModuleKey>, execute: F) -> Self
    where
        F: Fn(CommandCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        Self {
            data,
            module,
            gated: true,
            permission_level: PermissionLevel::Member,
            subcommand_permissions: Vec::new(),
            allow_in_dms: false,
            category,
            execute: Arc::new(move |ctx| Box::pin(execute(ctx))),
            autocomplete: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.data.name
    }

    /// `gated = false`.
    pub fn gated(mut self, gated: bool) -> Self {
        self.gated = gated;
        self
    }

    /// `permissionLevel`.
    pub fn permission_level(mut self, level: PermissionLevel) -> Self {
        self.permission_level = level;
        self
    }

    /// One `subcommandPermissions` entry (`"sub"` or `"group sub"`).
    pub fn subcommand_permission(mut self, path: impl Into<String>, level: PermissionLevel) -> Self {
        self.subcommand_permissions.push((path.into(), level));
        self
    }

    /// `allowInDMs = true`.
    pub fn allow_in_dms(mut self) -> Self {
        self.allow_in_dms = true;
        self
    }

    /// `autocomplete(interaction, client)`.
    pub fn autocomplete<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(AutocompleteCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = BotResult<()>> + Send + 'static,
    {
        self.autocomplete = Some(Arc::new(move |ctx| Box::pin(f(ctx))));
        self
    }

    /// The required level for a subcommand path: `'group sub'` beats `'sub'` beats `permissionLevel`.
    pub fn required_level(&self, group: Option<&str>, sub: Option<&str>) -> PermissionLevel {
        let lookup = |key: &str| self.subcommand_permissions.iter().find(|(k, _)| k == key).map(|(_, l)| *l);
        if let (Some(g), Some(s)) = (group, sub)
            && let Some(l) = lookup(&format!("{g} {s}"))
        {
            return l;
        }
        if let Some(s) = sub
            && let Some(l) = lookup(s)
        {
            return l;
        }
        self.permission_level
    }

    /// `buildCommandPayload(mod)`: servers only (`contexts: [0]`, `integration_types: [0]`) unless `allowInDMs`; a
    /// command that sets its own contexts or integration types keeps them.
    pub fn payload(&self) -> Value {
        if self.allow_in_dms {
            return self.data.to_json();
        }
        let contexts = self.data.contexts.clone().or_else(|| Some(vec![CONTEXT_GUILD]));
        let types = self.data.integration_types.clone().or_else(|| Some(vec![INTEGRATION_GUILD_INSTALL]));
        self.data.to_json_with(contexts, types)
    }

    /// The routing metadata as `commands.json` lists it.
    pub fn metadata(&self) -> Value {
        let mut perms = Map::new();
        for (k, l) in &self.subcommand_permissions {
            perms.insert(k.clone(), json!(l.as_u8()));
        }
        json!({
            "category": self.category,
            "name": self.data.name,
            "module": self.module.map(|m| m.as_str()),
            "gated": self.gated,
            "permissionLevel": self.permission_level.as_u8(),
            "subcommandPermissions": Value::Object(perms),
            "allowInDMs": self.allow_in_dms,
            "hasAutocomplete": self.autocomplete.is_some(),
        })
    }

    /// The rules the JS tests check: valid data (`smoke.test.js`), guild-only contexts unless `allowInDMs`, an
    /// `autocomplete` handler when an option autocompletes, and `subcommandPermissions` keys that are real
    /// subcommands (`registry.test.js`).
    pub fn validate(&self) -> Vec<String> {
        let mut errors = self.data.validate();
        let name = &self.data.name;
        let paths = self.data.subcommand_paths();
        for (k, _) in &self.subcommand_permissions {
            if !paths.contains(k) {
                errors.push(format!("/{name}: subcommandPermissions key {k:?} is not a subcommand"));
            }
        }
        let payload = self.payload();
        if !self.allow_in_dms {
            if payload.get("contexts") != Some(&json!([0])) {
                errors.push(format!("/{name}: guild-only command must have contexts [0], got {}", payload.get("contexts").cloned().unwrap_or(Value::Null)));
            }
            if payload.get("integration_types") != Some(&json!([0])) {
                errors.push(format!("/{name}: guild-only command must have integration_types [0]"));
            }
        }
        let uses_autocomplete = serde_json::to_string(&payload).is_ok_and(|s| s.contains("\"autocomplete\":true"));
        if uses_autocomplete && self.autocomplete.is_none() {
            errors.push(format!("/{name}: has autocomplete options but no autocomplete()"));
        }
        errors
    }
}

/// `client.commands` and `client.commandCategories`.
#[derive(Default)]
pub struct CommandRegistry {
    commands: RwLock<IndexMap<String, Arc<CommandDef>>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a command; a duplicate name is refused (`false`).
    pub fn insert(&self, def: CommandDef) -> bool {
        let mut map = self.commands.write().unwrap_or_else(|e| e.into_inner());
        if map.contains_key(def.name()) {
            return false;
        }
        map.insert(def.name().to_string(), Arc::new(def));
        true
    }

    pub fn get(&self, name: &str) -> Option<Arc<CommandDef>> {
        self.commands.read().unwrap_or_else(|e| e.into_inner()).get(name).cloned()
    }

    /// Every command, in load order.
    pub fn all(&self) -> Vec<Arc<CommandDef>> {
        self.commands.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    /// `client.commandCategories.get(name)`.
    pub fn category(&self, name: &str) -> Option<&'static str> {
        self.get(name).map(|c| c.category)
    }

    pub fn len(&self) -> usize {
        self.commands.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every deploy payload.
    pub fn payloads(&self) -> Vec<Value> {
        self.all().iter().map(|c| c.payload()).collect()
    }
}

/// `loadCommands(client)`: registers the commands (a duplicate name is skipped with a warning). The commands loaded.
pub fn load_commands(registry: &CommandRegistry, defs: Vec<CommandDef>) -> usize {
    let mut loaded = 0;
    for def in defs {
        let name = def.name().to_string();
        let category = def.category;
        if !registry.insert(def) {
            bot_util::log_warn!(format!("Duplicate command name \"{name}\" in {category} — skipping."));
            continue;
        }
        loaded += 1;
    }
    bot_util::log_success!(format!("Loaded {loaded} command(s)."));
    loaded
}

/// `a.localeCompare(b)` for command names (ICU root collation over the characters names use: space, `_`, `-`, `.`,
/// digits, then letters case-insensitively, lowercase first on a tie; other characters by code point after those).
pub fn command_name_compare(a: &str, b: &str) -> std::cmp::Ordering {
    fn primary(c: char) -> u32 {
        match c {
            ' ' => 1,
            '_' => 2,
            '-' => 3,
            ',' => 4,
            ';' => 5,
            ':' => 6,
            '!' => 7,
            '?' => 8,
            '.' => 9,
            '0'..='9' => 100 + (c as u32 - '0' as u32),
            'a'..='z' => 200 + (c as u32 - 'a' as u32),
            'A'..='Z' => 200 + (c as u32 - 'A' as u32),
            other => 1_000 + other as u32,
        }
    }
    let pa: Vec<u32> = a.chars().map(primary).collect();
    let pb: Vec<u32> = b.chars().map(primary).collect();
    pa.cmp(&pb).then_with(|| {
        // Tertiary: lowercase before uppercase at the first difference.
        for (x, y) in a.chars().zip(b.chars()) {
            if x != y {
                return x.is_uppercase().cmp(&y.is_uppercase());
            }
        }
        std::cmp::Ordering::Equal
    })
}

/// The payloads sorted by name the way the JS sorts them (`localeCompare`).
pub fn sort_payloads(payloads: &mut [Value]) {
    payloads.sort_by(|a, b| {
        let na = a.get("name").and_then(Value::as_str).unwrap_or("");
        let nb = b.get("name").and_then(Value::as_str).unwrap_or("");
        command_name_compare(na, nb)
    });
}

/// `computeDeployHash(clientId, guildId, payloads)`: sha256 hex of `"{clientId}|{guildId || 'global'}|"` and the
/// `JSON.stringify` of the payloads sorted by name.
pub fn compute_deploy_hash(client_id: &str, guild_id: Option<&str>, payloads: &[Value]) -> String {
    let mut sorted = payloads.to_vec();
    sort_payloads(&mut sorted);
    let mut h = Sha256::new();
    h.update(format!("{client_id}|{}|", guild_id.filter(|g| !g.is_empty()).unwrap_or("global")).as_bytes());
    h.update(serde_json::to_string(&sorted).unwrap_or_default().as_bytes());
    hex::encode(h.finalize())
}

/// Where and as whom commands are deployed (`DISCORD_TOKEN`, `CLIENT_ID`, `DEPLOY_GUILD_ID`, `DEPLOY_HASH_FILE`).
#[derive(Debug, Clone, Default)]
pub struct DeployConfig {
    pub token: Option<String>,
    pub client_id: Option<String>,
    pub guild_id: Option<String>,
    pub hash_file: PathBuf,
}

impl DeployConfig {
    /// From the environment. `DEPLOY_HASH_FILE` defaults to `./.deploy-hash` (the repository root when the bot runs
    /// from it, which is where the JS keeps it).
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            token: get("DISCORD_TOKEN"),
            client_id: get("CLIENT_ID"),
            guild_id: get("DEPLOY_GUILD_ID"),
            hash_file: get("DEPLOY_HASH_FILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".deploy-hash")),
        }
    }

    /// From the environment, with the default hash file in `root` (the JS `join(ROOT, '.deploy-hash')`, ROOT being the
    /// repository root). A set `DEPLOY_HASH_FILE` is used as given (relative to the working directory, as the JS
    /// `resolve()` does).
    pub fn from_env_in(root: &std::path::Path) -> Self {
        let mut cfg = Self::from_env();
        if std::env::var("DEPLOY_HASH_FILE").ok().filter(|v| !v.is_empty()).is_none() {
            cfg.hash_file = root.join(".deploy-hash");
        }
        cfg
    }
}

/// Why a deploy did not happen.
#[derive(Debug, thiserror::Error)]
pub enum DeployError {
    #[error("DISCORD_TOKEN and CLIENT_ID must be set in .env to deploy commands.")]
    MissingCredentials,
    #[error("CLIENT_ID is not a valid application id")]
    BadClientId,
    #[error("{0}")]
    Http(String),
}

async fn put_commands(cfg: &DeployConfig, payloads: &[Value]) -> Result<usize, DeployError> {
    let (Some(token), Some(client_id)) = (&cfg.token, &cfg.client_id) else { return Err(DeployError::MissingCredentials) };
    let app_id: u64 = client_id.parse().map_err(|_| DeployError::BadClientId)?;
    if app_id == 0 {
        return Err(DeployError::BadClientId);
    }
    let http = Http::new(token);
    let application_id = serenity::all::ApplicationId::new(app_id);
    let route = match cfg.guild_id.as_deref().and_then(|g| g.parse::<u64>().ok()).filter(|g| *g != 0) {
        Some(g) => serenity::http::Route::GuildCommands { application_id, guild_id: GuildId::new(g) },
        None => serenity::http::Route::Commands { application_id },
    };
    // The bulk overwrite (PUT), with the raw payloads: the body is byte-for-byte what was hashed, and the answer is
    // counted, not parsed into serenity models (a model change there must not turn a successful deploy into a failure).
    let body = serde_json::to_vec(payloads).map_err(|e| DeployError::Http(e.to_string()))?;
    let request = serenity::http::Request::new(route, serenity::http::LightMethod::Put).body(Some(body));
    let response = http.request(request).await.map_err(|e| DeployError::Http(e.to_string()))?;
    let bytes = response.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    let answer: Vec<Value> = serde_json::from_slice(&bytes).unwrap_or_default();
    Ok(if answer.is_empty() { payloads.len() } else { answer.len() })
}

fn target_label(cfg: &DeployConfig) -> String {
    match &cfg.guild_id {
        Some(g) => format!("to guild {g}"),
        None => "globally".to_string(),
    }
}

fn save_hash(file: &Path, hash: &str) {
    if let Err(e) = std::fs::write(file, hash) {
        bot_util::log_warn!(format!("Could not save the command hash to {} ({e}) — the next start will deploy again.", file.display()));
    }
}

/// `deployCommands()` (`index.js --deploy`): PUTs every payload and records the hash. The JS exits with code 1 on a
/// failure; the caller does that with the returned error.
pub async fn deploy_commands(cfg: &DeployConfig, payloads: Vec<Value>) -> Result<usize, DeployError> {
    if cfg.token.is_none() || cfg.client_id.is_none() {
        bot_util::log_error!("DISCORD_TOKEN and CLIENT_ID must be set in .env to deploy commands.");
        return Err(DeployError::MissingCredentials);
    }
    if payloads.is_empty() {
        bot_util::log_warn!("No command payloads found — deploying 0 commands.");
    }
    bot_util::log_info!(format!("Deploying {} command(s)...", payloads.len()));
    bot_util::log_debug!("Payload:", serde_json::to_string_pretty(&payloads).unwrap_or_default());
    match put_commands(cfg, &payloads).await {
        Ok(n) => bot_util::log_success!(format!("Successfully registered {n} command(s) {}.", target_label(cfg))),
        Err(e) => {
            bot_util::log_error!("Command deployment failed", bot_util::logger::LogArg::error(&e));
            return Err(e);
        }
    }
    let hash = compute_deploy_hash(cfg.client_id.as_deref().unwrap_or(""), cfg.guild_id.as_deref(), &payloads);
    save_hash(&cfg.hash_file, &hash);
    Ok(payloads.len())
}

/// What `autoDeployCommands` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDeployResult {
    /// No credentials: nothing to do.
    Skipped,
    /// The hash matches the hash file.
    Unchanged,
    Deployed,
    /// The PUT failed (the bot still starts).
    Failed,
}

/// `autoDeployCommands()`: hashes the payloads (and the target) and deploys only when the hash differs from the hash
/// file. Never fails.
pub async fn auto_deploy_commands(cfg: &DeployConfig, payloads: Vec<Value>) -> AutoDeployResult {
    if cfg.token.is_none() || cfg.client_id.is_none() {
        bot_util::log_warn!("DISCORD_TOKEN / CLIENT_ID not set — skipping auto-deploy.");
        return AutoDeployResult::Skipped;
    }
    let mut payloads = payloads;
    sort_payloads(&mut payloads);
    let hash = compute_deploy_hash(cfg.client_id.as_deref().unwrap_or(""), cfg.guild_id.as_deref(), &payloads);
    let stored = std::fs::read_to_string(&cfg.hash_file).map(|s| s.trim().to_string()).unwrap_or_default();
    if hash == stored {
        bot_util::log_info!("Commands unchanged — skipping deploy.");
        return AutoDeployResult::Unchanged;
    }
    bot_util::log_info!(format!("Commands changed ({} total) — deploying...", payloads.len()));
    match put_commands(cfg, &payloads).await {
        Ok(n) => bot_util::log_success!(format!("Auto-deployed {n} command(s) {}.", target_label(cfg))),
        Err(e) => {
            bot_util::log_error!("Auto-deploy failed — bot will still start", bot_util::logger::LogArg::error(&e));
            return AutoDeployResult::Failed;
        }
    }
    save_hash(&cfg.hash_file, &hash);
    AutoDeployResult::Deployed
}

/// `AUTO_DEPLOY_TIMEOUT_MS`.
pub const AUTO_DEPLOY_TIMEOUT_MS: u64 = 120_000;

/// The outcome of [`auto_deploy_with_timeout`] (`autoDeployInWorker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDeployOutcome {
    Done,
    Failed,
    Timeout,
}

/// `autoDeployInWorker()`: runs the auto-deploy on its own task, bounded by `timeout_ms`. Never fails: a failed or
/// stuck deploy is logged and the bot starts with the commands Discord already has.
pub async fn auto_deploy_with_timeout<F>(deploy: F, timeout_ms: u64) -> AutoDeployOutcome
where
    F: Future<Output = AutoDeployResult> + Send + 'static,
{
    let handle = tokio::spawn(deploy);
    let abort = handle.abort_handle();
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), handle).await {
        Ok(Ok(AutoDeployResult::Failed)) => AutoDeployOutcome::Failed,
        Ok(Ok(_)) => AutoDeployOutcome::Done,
        Ok(Err(e)) => {
            bot_util::log_error!("Auto-deploy worker crashed — bot will still start", e.to_string());
            AutoDeployOutcome::Failed
        }
        Err(_) => {
            bot_util::log_error!(format!("Auto-deploy did not finish within {} s — stopping it; bot will still start", timeout_ms as f64 / 1000.0));
            abort.abort();
            AutoDeployOutcome::Timeout
        }
    }
}

/// Command names (diagnostics).
pub fn names(defs: &[CommandDef]) -> HashSet<String> {
    defs.iter().map(|d| d.name().to_string()).collect()
}
