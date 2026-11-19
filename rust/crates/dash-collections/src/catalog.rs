//! Port of `src/dashboard/catalog.js`: what the dashboard shows and may change: module categories, one-line
//! descriptions, every module's settings groups (dash-schema), its record lists and the placeholder tokens.
//! [`build_catalog`] is the GET /api/catalog payload, JSON-equal (key order included) to the JS `buildCatalog()`.

use std::sync::LazyLock;

use bot_config::modules::ModuleKey;
use serde_json::{Map, Value, json};

use crate::index::Collections;

/// One dashboard group of modules (`CATEGORIES` entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Category {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub modules: &'static [&'static str],
}

/// `CATEGORIES`: the dashboard's groups, in display order. Every MODULES key appears in exactly one.
pub const CATEGORIES: [Category; 4] = [
    Category {
        id: "safety",
        label: "Moderation & Safety",
        description: "Keep the server safe with moderation tools, automatic filters, audit logs and member verification.",
        modules: &["moderation", "automod", "logging", "captcha", "antiraid", "rolepersist", "backups"],
    },
    Category {
        id: "community",
        label: "Community",
        description: "Greet members, hand out roles and run the features your community uses every day.",
        modules: &[
            "welcome", "reactionroles", "invites", "tickets", "tempvoice", "stats", "birthdays", "polls", "giveaways", "starboard",
            "suggestions", "modmail", "applications", "events", "afk", "boosts", "confessions", "voiceroles",
        ],
    },
    Category {
        id: "engagement",
        label: "Levels & Economy",
        description: "Reward activity with XP, achievements, coins and games.",
        modules: &["leveling", "achievements", "economy", "games", "counting"],
    },
    Category {
        id: "tools",
        label: "Content & Utilities",
        description: "Custom commands, automations, scheduled messages, feeds and other handy tools.",
        modules: &[
            "customcommands", "automations", "embeds", "reminders", "feeds", "translation", "search", "liverole", "autotime", "sticky",
            "autothreads", "bumps",
        ],
    },
];

/// `DESCRIPTIONS`: one user-facing sentence per module.
pub const DESCRIPTIONS: [(&str, &str); 42] = [
    ("moderation", "Ban, kick, timeout and warn with case history and escalation."),
    ("automod", "Automatically removes or punishes bad words, spam, invites, links, caps, mass mentions and more."),
    ("logging", "Posts message, member, voice, channel, role and server events to your log channels."),
    ("welcome", "Greets new members with a message, a welcome card or a DM, says goodbye and gives autoroles."),
    ("captcha", "New members solve a captcha before they get access; those who never verify can be kicked."),
    ("reactionroles", "Members pick their own roles from buttons, a dropdown or emoji reactions."),
    ("leveling", "Members earn XP for chatting and voice time, level up and unlock reward roles."),
    ("customcommands", "Your own !commands and /tag replies, plus keyword auto-responders."),
    ("automations", "When-If-Do rules that react to messages, reactions, voice activity, joins and threads."),
    ("translation", "Translates a channel's messages into up to five languages."),
    ("invites", "Tracks who invited whom, with an invite leaderboard and personal invite links."),
    ("embeds", "Build, save, publish and edit rich embed messages with fields and link buttons."),
    ("reminders", "Scheduled channel messages at a time of day, on an interval or once, in the server's time zone."),
    ("giveaways", "Button-entry giveaways with role requirements, weighted odds, rerolls and XP or coin prizes."),
    ("polls", "Single or multiple choice polls with live results and an end time."),
    ("starboard", "Reposts popular messages to a showcase channel once they collect enough reactions."),
    ("tickets", "Support ticket panels with categories, staff claiming and HTML transcripts."),
    ("tempvoice", "Join-to-create voice channels that their owners can lock, rename and resize."),
    ("stats", "Channel names that count members, bots, online members, boosts and more."),
    ("birthdays", "Members save their birthday and the bot announces it, with an optional birthday role."),
    ("achievements", "Tiered achievements for messages, reactions, levels, boosts and purchases, with rewards."),
    ("economy", "A wallet per server with daily rewards, work, payments, a shop and casino games."),
    ("games", "Rock-paper-scissors, number guessing, tic-tac-toe and two-player Scrabble."),
    ("feeds", "Posts new RSS, YouTube, Reddit, Bluesky and podcast items to a channel."),
    ("search", "Look up anime, manga, Pokémon and Urban Dictionary definitions."),
    (
        "liverole",
        "Gives members a role while they stream. Switching it off here leaves it on current streamers; /liverole disable in Discord removes it too.",
    ),
    ("counting", "A channel where members count up from 1 in turns; a wrong number starts the count again."),
    ("autotime", "Answers times typed in chat, like \"fri 5pm\", with a timestamp everyone sees in their own timezone."),
    ("suggestions", "Members post ideas everyone can vote on; staff approve, deny or mark them implemented."),
    ("modmail", "Members DM the bot to reach staff privately; each conversation becomes a staff thread."),
    ("applications", "Forms for staff or role applications, reviewed with Accept and Deny buttons."),
    ("sticky", "Keeps a message at the bottom of a channel, posting it again as chat moves on."),
    ("afk", "Members set an AFK note; whoever mentions them sees it, and it clears when they are back."),
    ("antiraid", "Spots join floods and brand-new accounts, pauses invites and alerts your moderators."),
    ("events", "Schedule server events with Going and Maybe buttons, reminders and an attendee role."),
    ("rolepersist", "Gives members their roles and nickname back when they leave and rejoin."),
    ("boosts", "Thanks members who boost the server and lets boosters make a role of their own."),
    ("backups", "Saves your roles, channels and permissions, and rebuilds what a raid or a mistake deleted."),
    ("confessions", "Members post anonymously; staff can review confessions before they appear."),
    ("autothreads", "Opens a thread on every message in chosen channels and publishes announcement posts."),
    ("voiceroles", "Gives members a role while they are in a voice channel."),
    ("bumps", "Reminds the server to bump it on Disboard when the cooldown ends, and counts who bumps."),
];

/// `CODES`: a two-letter code per module (a capital and a small letter, unique).
pub const CODES: [(&str, &str); 42] = [
    ("moderation", "Md"), ("automod", "Am"), ("logging", "Lg"), ("captcha", "Cp"), ("welcome", "Wl"), ("reactionroles", "Rr"),
    ("invites", "Iv"), ("tickets", "Tk"), ("tempvoice", "Tv"), ("stats", "St"), ("birthdays", "Bd"), ("polls", "Pl"),
    ("giveaways", "Gw"), ("starboard", "Sb"), ("leveling", "Lv"), ("achievements", "Ac"), ("economy", "Ec"), ("games", "Gm"),
    ("counting", "Ct"), ("customcommands", "Cc"), ("automations", "Au"), ("embeds", "Em"), ("reminders", "Rm"), ("feeds", "Fd"),
    ("translation", "Tr"), ("search", "Sr"), ("liverole", "Lr"), ("autotime", "Ts"), ("suggestions", "Sg"), ("modmail", "Mm"),
    ("applications", "Ap"), ("sticky", "Sk"), ("afk", "Af"), ("antiraid", "Ar"), ("events", "Ev"), ("rolepersist", "Rp"),
    ("boosts", "Bo"), ("backups", "Bk"), ("confessions", "Cf"), ("autothreads", "At"), ("voiceroles", "Vr"), ("bumps", "Bm"),
];

/// The boolean sub-settings of each module (`SUB_TOGGLE_FIELDS`; their descriptors live in dash-schema).
pub const SUB_TOGGLE_FIELDS: [(&str, &[&str]); 18] = [
    ("moderation", &["modDmTargets"]),
    ("welcome", &["welcomeMessageEnabled", "welcomeUseEmbed", "welcomeDmEnabled", "welcomeDmCard", "goodbyeEnabled", "goodbyeUseEmbed"]),
    ("leveling", &["voiceXpEnabled", "levelUpUseEmbed", "levelRolesStack", "levelRolesSyncOnDemotion"]),
    ("customcommands", &["autoTriggersEnabled"]),
    ("birthdays", &["birthdayRequireYear"]),
    ("achievements", &["achievementUseEmbed"]),
    ("economy", &["economyImportGlobal"]),
    ("search", &["urbanNsfwOnly"]),
    ("suggestions", &["suggestionThreads", "suggestionAllowAnonymous", "suggestionDmOnUpdate"]),
    ("modmail", &["modmailAnonymousReplies"]),
    ("afk", &["afkNicknamePrefix"]),
    ("antiraid", &["antiRaidPauseInvites", "antiRaidLockChat"]),
    ("events", &["eventCreateDiscordEvent", "eventChannelsEnabled"]),
    ("rolepersist", &["rolePersistNicknames"]),
    ("boosts", &["boostCustomRoles"]),
    ("confessions", &["confessionAllowImages", "confessionBlockLinks"]),
    ("voiceroles", &["voiceRoleIgnoreAfk"]),
    ("bumps", &["bumpThanks"]),
];

const GENERAL_DESCRIPTION: &str = "Time zone, embed colour and the custom command prefix: settings every module uses.";

fn lookup(table: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `DESCRIPTIONS[key]`.
pub fn description(key: &str) -> Option<&'static str> {
    lookup(&DESCRIPTIONS, key)
}

/// `CODES[key]`.
pub fn code(key: &str) -> Option<&'static str> {
    lookup(&CODES, key)
}

/// `SUB_TOGGLES[key]`: `{ field, label, description, default }` of each sub-toggle.
pub fn sub_toggles(key: &str) -> Vec<Value> {
    let fields = SUB_TOGGLE_FIELDS.iter().find(|(k, _)| *k == key).map_or(&[][..], |(_, f)| *f);
    fields
        .iter()
        .map(|field| {
            let d = dash_schema::settings_field(field);
            let mut m = Map::new();
            m.insert("field".into(), json!(field));
            for (k, v) in [
                ("label", d.and_then(|d| d.get("label")).cloned()),
                ("description", d.and_then(|d| d.get("description")).cloned()),
                ("default", d.and_then(|d| d.default_value()).cloned()),
            ] {
                if let Some(v) = v {
                    m.insert(k.into(), v);
                }
            }
            Value::Object(m)
        })
        .collect()
}

/// `placeholderGroup(token)`: `user`, `server` or `channel` for `{user...`, `{server...`, `{channel...` followed by a
/// word boundary, else `other`.
pub fn placeholder_group(token: &str) -> &'static str {
    for ns in ["user", "server", "channel"] {
        if let Some(rest) = token.strip_prefix('{').and_then(|t| t.strip_prefix(ns)) {
            let boundary = rest.bytes().next().is_none_or(|b| !(b.is_ascii_alphanumeric() || b == b'_'));
            if boundary {
                return ns;
            }
        }
    }
    "other"
}

/// `buildCatalog()` with a set of collections (server.js tests inject their own).
pub fn build_catalog_with(collections: &Collections) -> Value {
    let mut modules = Map::new();
    for category in CATEGORIES {
        for key in category.modules {
            let info = key.parse::<ModuleKey>().ok().map(ModuleKey::info);
            let mut m = Map::new();
            m.insert("key".into(), json!(key));
            if let Some(c) = code(key) {
                m.insert("code".into(), json!(c));
            }
            if let Some(i) = info {
                m.insert("label".into(), json!(i.label));
                m.insert("emoji".into(), json!(i.emoji));
                m.insert("flag".into(), json!(i.flag));
            }
            if let Some(d) = description(key) {
                m.insert("description".into(), json!(d));
            }
            if let Some(i) = info {
                m.insert("configCommand".into(), json!(i.config_command));
            }
            m.insert("default".into(), json!(info.is_some_and(|i| i.default)));
            m.insert("subToggles".into(), Value::Array(sub_toggles(key)));
            let groups = dash_schema::module_groups(key).unwrap_or(&[]);
            m.insert("groups".into(), Value::Array(groups.iter().map(dash_schema::Group::public_json).collect()));
            let lists: Vec<Value> = collections.iter().filter(|c| c.module() == *key).map(|c| json!(c.key)).collect();
            m.insert("collections".into(), Value::Array(lists));
            modules.insert((*key).to_string(), Value::Object(m));
        }
    }
    let categories: Vec<Value> = CATEGORIES
        .iter()
        .map(|c| json!({ "id": c.id, "label": c.label, "description": c.description, "modules": c.modules }))
        .collect();
    let general = json!({
        "title": "General",
        "description": GENERAL_DESCRIPTION,
        "groups": dash_schema::general_groups().iter().map(dash_schema::Group::public_json).collect::<Vec<_>>(),
    });
    let mut lists = Map::new();
    for c in collections.iter() {
        lists.insert(c.key.clone(), c.public_json());
    }
    let placeholders: Vec<Value> = bot_util::placeholder_parser::placeholders()
        .into_iter()
        .map(|(token, description)| json!({ "token": token, "description": description, "group": placeholder_group(token) }))
        .collect();
    json!({
        "categories": categories,
        "modules": modules,
        "general": general,
        "collections": lists,
        "placeholders": placeholders,
    })
}

static CATALOG: LazyLock<Value> = LazyLock::new(|| build_catalog_with(Collections::builtin()));
static CATALOG_TEXT: LazyLock<String> = LazyLock::new(|| serde_json::to_string(&*CATALOG).unwrap_or_default());

/// `buildCatalog()`: the GET /api/catalog payload, built once.
pub fn build_catalog() -> &'static Value {
    &CATALOG
}

/// [`build_catalog`] serialised (what the server sends).
pub fn catalog_json() -> &'static str {
    &CATALOG_TEXT
}
