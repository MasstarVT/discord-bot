//! Constants, the trigger / condition / action tables, the user-facing error texts, the cache keys and the raw SQL of
//! `src/services/automationService.js`.

use bot_config::limits;

pub const AUTOMATION_NAME_MAX: usize = 32;
pub const IMPORT_MAX_BYTES: usize = 32_768;
pub const EXPORT_FORMAT: &str = "automation";
pub const EXPORT_VERSION: i64 = 1;
pub const MAX_CHAIN_DEPTH: u32 = 3;
pub const CAUSE_TTL_MS: i64 = 15_000;
pub const RUN_BUDGET_MAX: usize = 30;
pub const RUN_BUDGET_WINDOW_MS: i64 = 10_000;
pub const MAX_PENDING_RUNS: usize = 25;
pub const RUN_TIMEOUT_MS: u64 = 30_000;
pub const MEMO_TTL_MS: i64 = 60_000;
pub const CACHE_TTL_SECONDS: u64 = 300;
pub const REPOST_DEDUPE_TTL_SECONDS: u64 = 2_592_000;
pub const PROBLEM_TTL_SECONDS: u64 = 604_800;
pub const MAX_COOLDOWN_SECONDS: i64 = 604_800;
pub const MAX_REWARD_AMOUNT: i64 = 1_000_000;
pub const THREAD_ARCHIVE_MINUTES: [i64; 4] = [60, 1440, 4320, 10080];
pub const DEFAULT_THREAD_NAME: &str = "{user.name}'s thread";
pub const WELCOMER_DEFAULT_TEXT: &str = "Hey {user}, welcome to **{server}**!";
pub const PRESETS: [&str; 4] = ["welcomer", "starboard", "thread-joiner", "commands-only"];
pub const MAX_BUTTON_INDEX: i64 = 99;

pub(crate) const MEMO_MAX_GUILDS: usize = 5_000;
pub(crate) const MAX_IMPORT_MEMBER_FETCHES: usize = 25;
/// A stored array longer than this is a corrupt row: the tail is ignored.
pub(crate) const MAX_STORED_ELEMENTS: usize = 100;
pub(crate) const CONTENT_MAX: usize = 2000;
pub(crate) const THREAD_NAME_MAX: usize = 100;
pub(crate) const BUTTON_LABEL_MAX: usize = 80;
pub(crate) const BUTTON_URL_MAX: usize = 512;
pub(crate) const DEFAULT_STAR: &str = "⭐";
pub(crate) const DEFAULT_THRESHOLD: i64 = 3;
pub(crate) const BUTTON_STYLES: [&str; 4] = ["PRIMARY", "SECONDARY", "SUCCESS", "DANGER"];

/// `guild:{g}:automations`: the enabled automations by trigger (`{ v: 1, byTrigger }`, 300 s).
pub fn automations_cache_key(guild_id: &str) -> String {
    format!("guild:{guild_id}:automations")
}

/// `automation:cd:{g}:{automationId}:{userId}`: a per-user cooldown (`"1"`).
pub fn cooldown_key(guild_id: &str, automation_id: &str, user_id: &str) -> String {
    format!("automation:cd:{guild_id}:{automation_id}:{user_id}")
}

/// `automation:repost:{automationId}:{messageId}`: one repost per source message (30 days).
pub fn repost_key(automation_id: &str, message_id: &str) -> String {
    format!("automation:repost:{automation_id}:{message_id}")
}

/// `automation:problem:{automationId}`: the last runtime problem (`{ at, text }`, 7 days).
pub fn problem_key(automation_id: &str) -> String {
    format!("automation:problem:{automation_id}")
}

/// What an [`AutomationError`] is about; the command picks the embed title from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Invalid,
    NotFound,
    Limit,
    NoActions,
    Buttons,
    Name,
    NoSuch,
    Hierarchy,
}

impl ErrorCode {
    /// The JS `code` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "INVALID",
            Self::NotFound => "NOTFOUND",
            Self::Limit => "LIMIT",
            Self::NoActions => "NOACTIONS",
            Self::Buttons => "BUTTONS",
            Self::Name => "NAME",
            Self::NoSuch => "NOSUCH",
            Self::Hierarchy => "HIERARCHY",
        }
    }
}

/// `AutomationError`: an error whose message is shown to the admin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationError {
    pub message: String,
    pub code: ErrorCode,
}

impl AutomationError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: ErrorCode::Invalid }
    }

    pub fn with_code(message: impl Into<String>, code: ErrorCode) -> Self {
        Self { message: message.into(), code }
    }
}

impl std::fmt::Display for AutomationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AutomationError {}

/// The `AutomationError` inside an `anyhow::Error`.
pub fn as_automation_error(err: &anyhow::Error) -> Option<&AutomationError> {
    err.downcast_ref::<AutomationError>()
}

/// `isUniqueViolation(err)`: a Prisma P2002 (PostgreSQL 23505) inside an `anyhow::Error`.
pub fn is_unique_violation(err: &anyhow::Error) -> bool {
    err.downcast_ref::<bot_db::DbError>().is_some_and(bot_db::DbError::is_unique_violation)
}

/// `ERROR_TEXTS`: the exact texts shared by the service and the command.
pub mod error_texts {
    use super::limits;

    pub const NAME: &str = "Automation names can use letters, numbers, - and _ (up to 32 characters).";
    pub const BUTTONS_USED: &str = "This automation has used all 99 button numbers. Create a new automation for more buttons.";
    pub const NEEDS_ACTIONS: &str = "Add at least one action before turning it on.";

    pub fn not_found(name: &str) -> String {
        format!("There is no automation called **{name}**. See `/automation list`.")
    }

    pub fn taken(name: &str) -> String {
        format!("An automation called **{name}** already exists.")
    }

    pub fn limit() -> String {
        format!("This server already has {} automations, the maximum. Delete one with `/automation delete` first.", limits::AUTOMATIONS)
    }

    pub fn max_conditions() -> String {
        format!("An automation can have at most {} conditions.", limits::AUTOMATION_CONDITIONS)
    }

    pub fn max_actions() -> String {
        format!("An automation can have at most {} actions.", limits::AUTOMATION_ACTIONS)
    }
}

// ── Raw SQL ──────────────────────────────────────────────────────────────────

pub(crate) const LOCK_GUILD_SQL: &str = r#"SELECT 1 AS ok FROM "GuildSettings" WHERE "guildId" = $1 FOR UPDATE"#;
pub(crate) const LOCK_BY_NAME_SQL: &str = r#"SELECT "id" FROM "Automation" WHERE "guildId" = $1 AND "name" = $2 FOR UPDATE"#;
pub(crate) const LOCK_BY_ID_SQL: &str = r#"SELECT "id" FROM "Automation" WHERE "guildId" = $1 AND "id" = $2 FOR UPDATE"#;
/// `uses` is an int4 column: the maths run in bigint and the result is clamped (no updatedAt change).
pub(crate) const USES_SQL: &str = r#"UPDATE "Automation" SET "uses" = LEAST("uses"::bigint + 1, 2147483647)::int WHERE "id" = $1"#;

/// One raw statement with sample parameters (`RAW_SQL`).
#[derive(Debug, Clone, Copy)]
pub struct RawSql {
    pub name: &'static str,
    pub sql: &'static str,
    pub params: &'static [&'static str],
    /// `true` for a statement that returns rows (`$queryRaw`), `false` for `$executeRaw`.
    pub query: bool,
}

/// `RAW_SQL`.
pub const RAW_SQL: [RawSql; 4] = [
    RawSql { name: "automation.lockGuildSettings", sql: LOCK_GUILD_SQL, params: &["0"], query: true },
    RawSql { name: "automation.lockByName", sql: LOCK_BY_NAME_SQL, params: &["0", "x"], query: true },
    RawSql { name: "automation.lockById", sql: LOCK_BY_ID_SQL, params: &["0", "x"], query: true },
    RawSql { name: "automation.incrementUses", sql: USES_SQL, params: &["0"], query: false },
];

// ── Triggers ─────────────────────────────────────────────────────────────────

/// One trigger. `channel`: whether it happens in a channel (the trigger channel is the default send target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerDef {
    pub key: &'static str,
    pub label: &'static str,
    pub channel: bool,
}

/// `TRIGGERS`, in the JS order.
pub const TRIGGERS: [TriggerDef; 11] = [
    TriggerDef { key: "MESSAGE_SENT", label: "Message sent", channel: true },
    TriggerDef { key: "MESSAGE_DELETED", label: "Message deleted", channel: true },
    TriggerDef { key: "MESSAGE_EDITED", label: "Message edited", channel: true },
    TriggerDef { key: "BUTTON_CLICKED", label: "Button clicked", channel: true },
    TriggerDef { key: "REACTION_ADDED", label: "Reaction added", channel: true },
    TriggerDef { key: "ROLE_GAINED", label: "Role gained", channel: false },
    TriggerDef { key: "ROLE_LOST", label: "Role lost", channel: false },
    TriggerDef { key: "VOICE_JOINED", label: "Joined voice", channel: true },
    TriggerDef { key: "VOICE_LEFT", label: "Left voice", channel: true },
    TriggerDef { key: "THREAD_CREATED", label: "Thread created", channel: true },
    TriggerDef { key: "MEMBER_JOINED", label: "Member joined", channel: false },
];

pub const T_ALL: &[&str] = &[
    "MESSAGE_SENT",
    "MESSAGE_DELETED",
    "MESSAGE_EDITED",
    "BUTTON_CLICKED",
    "REACTION_ADDED",
    "ROLE_GAINED",
    "ROLE_LOST",
    "VOICE_JOINED",
    "VOICE_LEFT",
    "THREAD_CREATED",
    "MEMBER_JOINED",
];
pub const T_CHANNEL: &[&str] =
    &["MESSAGE_SENT", "MESSAGE_DELETED", "MESSAGE_EDITED", "BUTTON_CLICKED", "REACTION_ADDED", "VOICE_JOINED", "VOICE_LEFT", "THREAD_CREATED"];
pub const T_CONTENT: &[&str] = &["MESSAGE_SENT", "MESSAGE_DELETED", "MESSAGE_EDITED", "REACTION_ADDED"];
/// The user message still exists.
pub const T_LIVE: &[&str] = &["MESSAGE_SENT", "MESSAGE_EDITED", "REACTION_ADDED"];
/// Reaction counts are meaningful.
pub const T_COUNTS: &[&str] = &["MESSAGE_DELETED", "MESSAGE_EDITED", "REACTION_ADDED"];
pub const T_THREADY: &[&str] = &["THREAD_CREATED", "MESSAGE_SENT", "MESSAGE_EDITED", "REACTION_ADDED", "BUTTON_CLICKED"];
pub(crate) const T_REPLYABLE: &[&str] = &["MESSAGE_SENT", "MESSAGE_EDITED", "REACTION_ADDED", "BUTTON_CLICKED"];
const T_REACTION: &[&str] = &["REACTION_ADDED"];
const T_ROLE_GAINED: &[&str] = &["ROLE_GAINED"];
const T_ROLE_LOST: &[&str] = &["ROLE_LOST"];
const T_BUTTON: &[&str] = &["BUTTON_CLICKED"];

/// `isTrigger(t)`.
pub fn is_trigger(t: &str) -> bool {
    TRIGGERS.iter().any(|d| d.key == t)
}

/// The trigger's definition.
pub fn trigger_def(t: &str) -> Option<&'static TriggerDef> {
    TRIGGERS.iter().find(|d| d.key == t)
}

/// `TRIGGERS[t].label`.
pub fn trigger_label(t: &str) -> Option<&'static str> {
    trigger_def(t).map(|d| d.label)
}

/// The static key of a trigger name.
pub fn trigger_key(t: &str) -> Option<&'static str> {
    trigger_def(t).map(|d| d.key)
}

// ── Conditions and actions ───────────────────────────────────────────────────

/// One condition type. `list` names the array a repeated `condition add` of the same type extends; `max` is its cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConditionDef {
    pub key: &'static str,
    pub label: &'static str,
    pub triggers: &'static [&'static str],
    pub list: Option<&'static str>,
    pub max: Option<usize>,
    pub no_params: bool,
}

impl ConditionDef {
    pub fn allows(&self, trigger: &str) -> bool {
        self.triggers.contains(&trigger)
    }
}

const fn cdef(
    key: &'static str,
    label: &'static str,
    triggers: &'static [&'static str],
    list: Option<&'static str>,
    max: Option<usize>,
    no_params: bool,
) -> ConditionDef {
    ConditionDef { key, label, triggers, list, max, no_params }
}

/// `CONDITION_TYPES`, in the JS order.
pub const CONDITION_TYPES: [ConditionDef; 24] = [
    cdef("CHANNEL_IN", "Channel is one of", T_CHANNEL, Some("channelIds"), Some(25), false),
    cdef("CHANNEL_NOT_IN", "Channel is not one of", T_CHANNEL, Some("channelIds"), Some(25), false),
    cdef("CATEGORY_IN", "Category is one of", T_CHANNEL, Some("categoryIds"), Some(25), false),
    cdef("CATEGORY_NOT_IN", "Category is not one of", T_CHANNEL, Some("categoryIds"), Some(25), false),
    cdef("CONTAINS", "Message contains words", T_CONTENT, Some("words"), Some(25), false),
    cdef("EXACT", "Message is exactly", T_CONTENT, Some("texts"), Some(25), false),
    cdef("NOT_CONTAINS", "Message does not contain words", T_CONTENT, Some("words"), Some(25), false),
    cdef("MATCHES_REGEX", "Message matches a regex", T_CONTENT, None, None, false),
    cdef("ATTACHMENT_AUDIO", "Has an audio attachment", T_CONTENT, None, None, true),
    cdef("ATTACHMENT_IMAGE", "Has an image attachment", T_CONTENT, None, None, true),
    cdef("ATTACHMENT_VIDEO", "Has a video attachment", T_CONTENT, None, None, true),
    cdef("ATTACHMENT_TEXT", "Has a text-file attachment", T_CONTENT, None, None, true),
    cdef("REACTED_WITH", "Reacted with emoji", T_REACTION, Some("emojis"), Some(10), false),
    cdef("HAS_REACTIONS", "Message has at least N reactions", T_COUNTS, None, None, false),
    cdef("IS_REPLY", "Message is a reply", T_CONTENT, None, None, true),
    cdef("IS_NOT_REPLY", "Message is not a reply", T_CONTENT, None, None, true),
    cdef("HAS_ANY_ROLE", "Member has any of these roles", T_ALL, Some("roleIds"), Some(25), false),
    cdef("HAS_ALL_ROLES", "Member has all of these roles", T_ALL, Some("roleIds"), Some(25), false),
    cdef("HAS_NOT_ANY_ROLE", "Member has none of these roles", T_ALL, Some("roleIds"), Some(25), false),
    cdef("HAS_NOT_ALL_ROLES", "Member lacks one of these roles", T_ALL, Some("roleIds"), Some(25), false),
    cdef("GAINED_ROLE", "Gained one of these roles", T_ROLE_GAINED, Some("roleIds"), Some(25), false),
    cdef("LOST_ROLE", "Lost one of these roles", T_ROLE_LOST, Some("roleIds"), Some(25), false),
    cdef("USER_IS", "User is one of", T_ALL, Some("userIds"), Some(25), false),
    cdef("BUTTON_IS", "Clicked this button", T_BUTTON, Some("buttons"), Some(25), false),
];

/// `CONDITION_TYPES[t]` (`isConditionType`).
pub fn condition_def(t: &str) -> Option<&'static ConditionDef> {
    CONDITION_TYPES.iter().find(|d| d.key == t)
}

/// Conditions that read the message (content, attachments, reply flag or reaction counts).
pub(crate) const NEEDS_MESSAGE: &[&str] = &[
    "CONTAINS",
    "EXACT",
    "NOT_CONTAINS",
    "MATCHES_REGEX",
    "ATTACHMENT_AUDIO",
    "ATTACHMENT_IMAGE",
    "ATTACHMENT_VIDEO",
    "ATTACHMENT_TEXT",
    "IS_REPLY",
    "IS_NOT_REPLY",
    "HAS_REACTIONS",
];

/// One action type. Three types have extra rules in `validate_automation_shape` (see the JS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionDef {
    pub key: &'static str,
    pub label: &'static str,
    pub triggers: &'static [&'static str],
    pub button: bool,
    pub thread: bool,
    /// `'coins'` or `'xp'`.
    pub reward: Option<&'static str>,
}

impl ActionDef {
    pub fn allows(&self, trigger: &str) -> bool {
        self.triggers.contains(&trigger)
    }
}

const fn adef(key: &'static str, label: &'static str, triggers: &'static [&'static str], button: bool, thread: bool, reward: Option<&'static str>) -> ActionDef {
    ActionDef { key, label, triggers, button, thread, reward }
}

/// `ACTION_TYPES`, in the JS order.
pub const ACTION_TYPES: [ActionDef; 17] = [
    adef("CREATE_THREAD", "Create a thread", T_ALL, false, false, None),
    adef("SEND_MESSAGE", "Send a message", T_ALL, true, false, None),
    adef("REPLY", "Reply", T_REPLYABLE, true, false, None),
    adef("REPOST", "Repost the message", T_CONTENT, false, false, None),
    adef("PIN", "Pin the message", T_LIVE, false, false, None),
    adef("DELETE", "Delete the message", T_LIVE, false, false, None),
    adef("REACT", "React with an emoji", T_LIVE, false, false, None),
    adef("REMOVE_REACTION", "Remove the reaction", T_REACTION, false, false, None),
    adef("GIVE_ROLES", "Give roles", T_ALL, false, false, None),
    adef("TAKE_ROLES", "Take roles", T_ALL, false, false, None),
    adef("ADD_TO_THREAD", "Add members to the thread", T_ALL, false, true, None),
    adef("SEND_IN_THREAD", "Send a message in the thread", T_ALL, true, true, None),
    adef("GIVE_COINS", "Give coins", T_ALL, false, false, Some("coins")),
    adef("TAKE_COINS", "Take coins", T_ALL, false, false, Some("coins")),
    adef("GIVE_XP", "Give XP", T_ALL, false, false, Some("xp")),
    adef("TAKE_XP", "Take XP", T_ALL, false, false, Some("xp")),
    adef("SEND_EMBED", "Send a saved embed", T_ALL, false, false, None),
];

/// `ACTION_TYPES[t]` (`isActionType`).
pub fn action_def(t: &str) -> Option<&'static ActionDef> {
    ACTION_TYPES.iter().find(|d| d.key == t)
}

/// `PRESET_OPTIONS`: which options each preset reads (the others are reported as ignored).
pub fn preset_options(preset: &str) -> &'static [&'static str] {
    match preset {
        "welcomer" => &["channel", "message", "role"],
        "starboard" => &["channel", "emoji", "threshold"],
        "thread-joiner" => &["user", "channel"],
        "commands-only" => &["channel", "role"],
        _ => &[],
    }
}

/// `EXECUTOR_CHANNEL_NEEDS`: what the member who sets a target channel needs there, by action type.
pub fn executor_channel_needs(action_type: &str) -> Option<&'static [&'static str]> {
    match action_type {
        "SEND_MESSAGE" | "SEND_EMBED" | "REPOST" => Some(&["ViewChannel", "SendMessages"]),
        "CREATE_THREAD" => Some(&["ViewChannel", "CreatePublicThreads"]),
        _ => None,
    }
}
