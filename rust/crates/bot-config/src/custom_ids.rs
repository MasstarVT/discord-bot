//! Port of `src/config/customIds.js`: the customId prefix registry. Ids are `prefix:param1:param2`.

pub use crate::tables::CUSTOM_ID_PREFIXES;

/// A component type a prefix is used on (`types` in the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComponentType {
    Button,
    Select,
    Modal,
    Field,
}

impl ComponentType {
    pub const ALL: [ComponentType; 4] = [ComponentType::Button, ComponentType::Select, ComponentType::Modal, ComponentType::Field];

    pub const fn as_str(self) -> &'static str {
        match self {
            ComponentType::Button => "button",
            ComponentType::Select => "select",
            ComponentType::Modal => "modal",
            ComponentType::Field => "field",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// How a prefix is routed (`kind` in the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CustomIdKind {
    /// Routed by the global interaction router (survives restarts).
    Persistent,
    /// Handled by a message component collector only.
    Collector,
    /// A modal text-input id; takes no params.
    Field,
}

impl CustomIdKind {
    pub const ALL: [CustomIdKind; 3] = [CustomIdKind::Persistent, CustomIdKind::Collector, CustomIdKind::Field];

    pub const fn as_str(self) -> &'static str {
        match self {
            CustomIdKind::Persistent => "persistent",
            CustomIdKind::Collector => "collector",
            CustomIdKind::Field => "field",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

crate::string_enum_impls!(ComponentType, CustomIdKind);

/// One `CUSTOM_ID_PREFIXES` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixInfo {
    pub prefix: &'static str,
    pub owner: &'static str,
    pub types: &'static [ComponentType],
    pub kind: CustomIdKind,
}

/// `CUSTOM_ID_MAX_LENGTH` (Discord's cap, counted in UTF-16 code units like JS `length`).
pub const CUSTOM_ID_MAX_LENGTH: usize = 100;

/// `parseCustomId` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCustomId<'a> {
    pub prefix: &'a str,
    pub params: Vec<&'a str>,
}

/// `parseCustomId`: splits on every `:`. `""` gives prefix `""` and no params.
pub fn parse_custom_id(custom_id: &str) -> ParsedCustomId<'_> {
    let mut parts = custom_id.split(':');
    let prefix = parts.next().unwrap_or("");
    ParsedCustomId { prefix, params: parts.collect() }
}

/// `prefixInfo(prefix)`: the registry entry, or `None`.
pub fn prefix_info(prefix: &str) -> Option<&'static PrefixInfo> {
    CUSTOM_ID_PREFIXES.iter().find(|p| p.prefix == prefix)
}

/// Why [`build_custom_id`] refused. `Display` gives the JS error message verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomIdError {
    UnknownPrefix(String),
    FieldWithParams(String),
    ParamContainsColon(String),
    TooLong(usize),
}

impl std::fmt::Display for CustomIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CustomIdError::UnknownPrefix(p) => write!(f, "Unknown customId prefix \"{p}\""),
            CustomIdError::FieldWithParams(p) => write!(f, "customId prefix \"{p}\" is a field id and takes no params"),
            CustomIdError::ParamContainsColon(p) => write!(f, "customId param \"{p}\" contains ':'"),
            CustomIdError::TooLong(n) => write!(f, "customId is {n} characters (max {CUSTOM_ID_MAX_LENGTH})"),
        }
    }
}

impl std::error::Error for CustomIdError {}

/// `buildCustomId(prefix, ...params)`: `prefix:param1:param2`.
/// Errors on an unknown prefix, params on a `field` prefix, a param containing `:`, or a result longer than 100.
pub fn build_custom_id<S: AsRef<str>>(prefix: &str, params: &[S]) -> Result<String, CustomIdError> {
    let info = prefix_info(prefix).ok_or_else(|| CustomIdError::UnknownPrefix(prefix.to_owned()))?;
    if info.kind == CustomIdKind::Field && !params.is_empty() {
        return Err(CustomIdError::FieldWithParams(prefix.to_owned()));
    }
    if let Some(p) = params.iter().find(|p| p.as_ref().contains(':')) {
        return Err(CustomIdError::ParamContainsColon(p.as_ref().to_owned()));
    }
    let mut id = String::from(prefix);
    for p in params {
        id.push(':');
        id.push_str(p.as_ref());
    }
    let len = id.encode_utf16().count();
    if len > CUSTOM_ID_MAX_LENGTH {
        return Err(CustomIdError::TooLong(len));
    }
    Ok(id)
}

/// `buildCustomId(prefix, a, b, ...)` with any `Display` params: `custom_id!("rr", guild_id, role_id)`.
#[macro_export]
macro_rules! custom_id {
    ($prefix:expr $(, $param:expr)* $(,)?) => {
        $crate::custom_ids::build_custom_id::<::std::string::String>($prefix, &[$(::std::string::ToString::to_string(&$param)),*])
    };
}
