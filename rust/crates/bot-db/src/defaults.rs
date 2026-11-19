//! Rust-side conveniences for the generated models: Prisma's column defaults as [`Default`] values, and the
//! module-flag lookup of `bot_config::ModuleFlags` on a typed settings row.

use serde_json::{Map, Value};

use crate::models::GuildSettings;
use crate::schema::{ClientDefault, Model, ModelInfo};

/// A row of `info` as Prisma's `create` with no data would write it, as JSON (Prisma field names): literal defaults
/// (`@default(true)`, `@default("UTC")`, `@default("[]")` for JSON, enum defaults), `null` for optional columns, `[]`
/// for lists, a new cuid / uuid for generated ids, now for `@default(now())` and `@updatedAt`, and `""` / `0` for a
/// required column without a default (the caller fills it, e.g. `guildId`).
pub fn default_row_json(info: &ModelInfo) -> Value {
    let now = crate::serde_js::format_js_date(&chrono::Utc::now());
    let mut m = Map::new();
    for c in info.columns {
        let v = if let Some(j) = c.default_json {
            let v: Value = serde_json::from_str(j).unwrap_or(Value::Null);
            // A `Json` column's DMMF default is the JSON text (`@default("[]")`).
            match (&c.kind, &v) {
                (crate::schema::ScalarKind::Json, Value::String(text)) => serde_json::from_str(text).unwrap_or(v),
                _ => v,
            }
        } else {
            match c.client_default {
                ClientDefault::Cuid => Value::String(crate::ids::new_cuid()),
                ClientDefault::Uuid => Value::String(crate::ids::new_uuid()),
                ClientDefault::Now | ClientDefault::UpdatedAt => Value::String(now.clone()),
                _ if c.list => Value::Array(Vec::new()),
                _ if c.nullable => Value::Null,
                _ => match c.kind {
                    crate::schema::ScalarKind::String => Value::String(String::new()),
                    crate::schema::ScalarKind::Boolean => Value::Bool(false),
                    crate::schema::ScalarKind::DateTime => Value::String(now.clone()),
                    crate::schema::ScalarKind::Json | crate::schema::ScalarKind::Enum(_) => Value::Null,
                    _ => Value::from(0),
                },
            }
        };
        m.insert(c.field.to_string(), v);
    }
    Value::Object(m)
}

/// Prisma's defaults: what `prisma.guildSettings.create({ data: { guildId } })` writes (every toggle, number, enum and
/// JSON default of `prisma/schema.prisma`), with `guildId` empty, a fresh cuid `id` and both timestamps now. For
/// tests and pure code that need a settings row: `GuildSettings { guild_id: "1".into(), xp_rate: 20, ..Default::default() }`.
impl Default for GuildSettings {
    fn default() -> Self {
        serde_json::from_value(default_row_json(GuildSettings::info())).unwrap_or_else(|e| panic!("GuildSettings defaults deserialize: {e}"))
    }
}

impl bot_config::ModuleFlags for GuildSettings {
    /// A boolean column by its Prisma name (`"modEnabled"`); `None` for any other name.
    fn module_flag(&self, flag_column: &str) -> Option<bool> {
        Some(match flag_column {
            "modEnabled" => self.mod_enabled,
            "loggingEnabled" => self.logging_enabled,
            "welcomeEnabled" => self.welcome_enabled,
            "reactionRolesEnabled" => self.reaction_roles_enabled,
            "levelingEnabled" => self.leveling_enabled,
            "customCommandsEnabled" => self.custom_commands_enabled,
            "autoTriggersEnabled" => self.auto_triggers_enabled,
            "automodEnabled" => self.automod_enabled,
            "translationEnabled" => self.translation_enabled,
            "captchaEnabled" => self.captcha_enabled,
            "invitesEnabled" => self.invites_enabled,
            "embedsEnabled" => self.embeds_enabled,
            "remindersEnabled" => self.reminders_enabled,
            "giveawaysEnabled" => self.giveaways_enabled,
            "pollsEnabled" => self.polls_enabled,
            "starboardEnabled" => self.starboard_enabled,
            "ticketsEnabled" => self.tickets_enabled,
            "tempVoiceEnabled" => self.temp_voice_enabled,
            "statsEnabled" => self.stats_enabled,
            "birthdaysEnabled" => self.birthdays_enabled,
            "achievementsEnabled" => self.achievements_enabled,
            "automationsEnabled" => self.automations_enabled,
            "economyEnabled" => self.economy_enabled,
            "gamesEnabled" => self.games_enabled,
            "feedsEnabled" => self.feeds_enabled,
            "searchEnabled" => self.search_enabled,
            "liveRoleEnabled" => self.live_role_enabled,
            "countingEnabled" => self.counting_enabled,
            "autoTimeEnabled" => self.auto_time_enabled,
            "suggestionsEnabled" => self.suggestions_enabled,
            "modmailEnabled" => self.modmail_enabled,
            "applicationsEnabled" => self.applications_enabled,
            "stickyEnabled" => self.sticky_enabled,
            "afkEnabled" => self.afk_enabled,
            "antiRaidEnabled" => self.anti_raid_enabled,
            "eventsEnabled" => self.events_enabled,
            "rolePersistEnabled" => self.role_persist_enabled,
            "boostsEnabled" => self.boosts_enabled,
            "backupsEnabled" => self.backups_enabled,
            "confessionsEnabled" => self.confessions_enabled,
            "autoThreadsEnabled" => self.auto_threads_enabled,
            "voiceRolesEnabled" => self.voice_roles_enabled,
            "bumpsEnabled" => self.bumps_enabled,
            "modDmTargets" => self.mod_dm_targets,
            "welcomeMessageEnabled" => self.welcome_message_enabled,
            "welcomeUseEmbed" => self.welcome_use_embed,
            "welcomeDmEnabled" => self.welcome_dm_enabled,
            "welcomeDmCard" => self.welcome_dm_card,
            "goodbyeEnabled" => self.goodbye_enabled,
            "goodbyeUseEmbed" => self.goodbye_use_embed,
            "xpEnabled" => self.xp_enabled,
            "levelRolesStack" => self.level_roles_stack,
            "voiceXpEnabled" => self.voice_xp_enabled,
            "levelUpUseEmbed" => self.level_up_use_embed,
            "levelRolesSyncOnDemotion" => self.level_roles_sync_on_demotion,
            "birthdayRequireYear" => self.birthday_require_year,
            "economyImportGlobal" => self.economy_import_global,
            "achievementUseEmbed" => self.achievement_use_embed,
            "urbanNsfwOnly" => self.urban_nsfw_only,
            "suggestionThreads" => self.suggestion_threads,
            "suggestionAllowAnonymous" => self.suggestion_allow_anonymous,
            "suggestionDmOnUpdate" => self.suggestion_dm_on_update,
            "modmailAnonymousReplies" => self.modmail_anonymous_replies,
            "afkNicknamePrefix" => self.afk_nickname_prefix,
            "antiRaidPauseInvites" => self.anti_raid_pause_invites,
            "antiRaidLockChat" => self.anti_raid_lock_chat,
            "eventCreateDiscordEvent" => self.event_create_discord_event,
            "eventChannelsEnabled" => self.event_channels_enabled,
            "rolePersistNicknames" => self.role_persist_nicknames,
            "boostCustomRoles" => self.boost_custom_roles,
            "confessionAllowImages" => self.confession_allow_images,
            "confessionBlockLinks" => self.confession_block_links,
            "voiceRoleIgnoreAfk" => self.voice_role_ignore_afk,
            "bumpThanks" => self.bump_thanks,
            _ => return None,
        })
    }
}
