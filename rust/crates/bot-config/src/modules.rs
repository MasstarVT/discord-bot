//! Port of `src/config/modules.js`: the module registry.

pub use crate::tables::{MODULES, ModuleKey};

crate::string_enum_impls!(ModuleKey);

/// One `MODULES` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleInfo {
    /// The `MODULES` key.
    pub key: ModuleKey,
    /// The `GuildSettings` toggle column (`leveling` maps to `xpEnabled`).
    pub flag: &'static str,
    pub label: &'static str,
    pub emoji: &'static str,
    pub mee6: &'static str,
    /// `configCommand`.
    pub config_command: &'static str,
    /// `default` (from `DEFAULT_MODULE_SETTINGS`).
    pub default: bool,
}

impl ModuleKey {
    /// The registry entry (`MODULES[key]`).
    pub fn info(self) -> &'static ModuleInfo {
        // MODULES is generated in the same order as the enum (checked by the tests).
        &MODULES[self as usize]
    }

    /// The `GuildSettings` toggle column (`MODULE_FLAGS[key]`).
    pub fn flag(self) -> &'static str {
        self.info().flag
    }
}

/// `MODULE_NAMES`: every module key in declaration order.
pub static MODULE_NAMES: [&str; ModuleKey::ALL.len()] = {
    let mut names = [""; ModuleKey::ALL.len()];
    let mut i = 0;
    while i < names.len() {
        names[i] = ModuleKey::ALL[i].as_str();
        i += 1;
    }
    names
};

/// `MODULE_FLAGS`: (module key, `GuildSettings` toggle column), in declaration order.
pub fn module_flags() -> impl Iterator<Item = (&'static str, &'static str)> {
    MODULES.iter().map(|m| (m.key.as_str(), m.flag))
}

/// `getModule(key)`: the registry entry for a module key, or `None` (never a prototype key).
pub fn get_module(key: &str) -> Option<&'static ModuleInfo> {
    ModuleKey::from_name(key).map(ModuleKey::info)
}

/// `isModuleKey(key)`.
pub fn is_module_key(key: &str) -> bool {
    get_module(key).is_some()
}

/// Read access to a `GuildSettings` row's boolean toggle columns, by column name (`"modEnabled"`).
/// `None` when the column is missing or not a boolean.
pub trait ModuleFlags {
    fn module_flag(&self, flag_column: &str) -> Option<bool>;
}

impl ModuleFlags for serde_json::Map<String, serde_json::Value> {
    fn module_flag(&self, flag_column: &str) -> Option<bool> {
        self.get(flag_column).and_then(serde_json::Value::as_bool)
    }
}

impl ModuleFlags for serde_json::Value {
    fn module_flag(&self, flag_column: &str) -> Option<bool> {
        self.as_object().and_then(|o| o.module_flag(flag_column))
    }
}

impl<T: ModuleFlags + ?Sized> ModuleFlags for &T {
    fn module_flag(&self, flag_column: &str) -> Option<bool> {
        (**self).module_flag(flag_column)
    }
}

/// `isModuleEnabled(settings, key)` with the settings row abstracted as a flag lookup.
///
/// `key == None` -> true (core, ungated; `lookup` is not called); unknown key -> false; otherwise
/// `lookup(flag_column)` must return exactly `Some(true)`. With no settings row, pass a lookup that returns `None`.
pub fn is_module_enabled(key: Option<&str>, lookup: impl FnOnce(&'static str) -> Option<bool>) -> bool {
    let Some(key) = key else { return true };
    let Some(module) = get_module(key) else { return false };
    lookup(module.flag) == Some(true)
}

/// [`is_module_enabled`] for a typed key (`None` is core and always on).
pub fn is_module_key_enabled(key: Option<ModuleKey>, lookup: impl FnOnce(&'static str) -> Option<bool>) -> bool {
    match key {
        None => true,
        Some(k) => lookup(k.flag()) == Some(true),
    }
}

/// [`is_module_enabled`] against a settings row (`None` = no row, which is off for any module key).
pub fn is_module_enabled_in<S: ModuleFlags + ?Sized>(settings: Option<&S>, key: Option<&str>) -> bool {
    is_module_enabled(key, |flag| settings.and_then(|s| s.module_flag(flag)))
}
