//! Port of `src/config/constants.js`.

pub use crate::tables::{DEFAULT_MODULE_SETTINGS, LIMITS, limits};
use crate::tables::ModuleKey;

/// `COLORS`: embed colours.
pub mod colors {
    pub const SUCCESS: u32 = 0x57f287;
    pub const ERROR: u32 = 0xed4245;
    pub const WARNING: u32 = 0xfee75c;
    pub const INFO: u32 = 0x5865f2;
    pub const PRIMARY: u32 = 0x2b2d31;
    pub const NEUTRAL: u32 = 0x99aab5;
}

/// `COLORS` as a name/value table in declaration order.
pub static COLORS: [(&str, u32); 6] = [
    ("SUCCESS", colors::SUCCESS),
    ("ERROR", colors::ERROR),
    ("WARNING", colors::WARNING),
    ("INFO", colors::INFO),
    ("PRIMARY", colors::PRIMARY),
    ("NEUTRAL", colors::NEUTRAL),
];

/// `PERMISSION_LEVELS`. Ordered: `level >= PermissionLevel::Moderator` works as in the JS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(u8)]
pub enum PermissionLevel {
    #[default]
    Member = 0,
    Moderator = 1,
    Admin = 2,
    Owner = 3,
}

impl PermissionLevel {
    /// Every level, lowest first (the JS declaration order).
    pub const ALL: [PermissionLevel; 4] =
        [PermissionLevel::Member, PermissionLevel::Moderator, PermissionLevel::Admin, PermissionLevel::Owner];

    /// The JS number (`PERMISSION_LEVELS.MODERATOR === 1`).
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The level for a JS number; `None` outside 0..=3.
    pub const fn from_u8(n: u8) -> Option<Self> {
        match n {
            0 => Some(PermissionLevel::Member),
            1 => Some(PermissionLevel::Moderator),
            2 => Some(PermissionLevel::Admin),
            3 => Some(PermissionLevel::Owner),
            _ => None,
        }
    }

    /// The JS key (`"MEMBER"`, `"MODERATOR"`, `"ADMIN"`, `"OWNER"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            PermissionLevel::Member => "MEMBER",
            PermissionLevel::Moderator => "MODERATOR",
            PermissionLevel::Admin => "ADMIN",
            PermissionLevel::Owner => "OWNER",
        }
    }

    /// Parses the JS key; `None` for anything else.
    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "MEMBER" => Some(PermissionLevel::Member),
            "MODERATOR" => Some(PermissionLevel::Moderator),
            "ADMIN" => Some(PermissionLevel::Admin),
            "OWNER" => Some(PermissionLevel::Owner),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `UserLevel.xp` clamp (Postgres int4).
pub const XP_MAX: i32 = 1_000_000_000;
/// Absolute economy clamp (`GuildSettings.economyMaxBalance` may be lower).
pub const COIN_MAX: i32 = 1_000_000_000;

/// `XP_DEFAULTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XpDefaults {
    pub rate: i32,
    pub min: i32,
    pub max: i32,
    pub cooldown: i32,
    /// `levelUpChannel: null`.
    pub level_up_channel: Option<&'static str>,
}

/// `XP_DEFAULTS`.
pub const XP_DEFAULTS: XpDefaults = XpDefaults { rate: 15, min: 15, max: 25, cooldown: 60, level_up_channel: None };

/// `xpForLevel`: `5 * level^2 + 50 * level + 100` (saturating instead of growing past `i64`).
pub const fn xp_for_level(level: i64) -> i64 {
    level.saturating_mul(level).saturating_mul(5).saturating_add(level.saturating_mul(50)).saturating_add(100)
}

/// `DEFAULT_MODULE_SETTINGS[key]`.
pub fn default_module_setting(key: ModuleKey) -> bool {
    DEFAULT_MODULE_SETTINGS.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).unwrap_or(false)
}
